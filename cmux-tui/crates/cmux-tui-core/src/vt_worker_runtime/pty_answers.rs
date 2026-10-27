//! The fixed set of answers a VT worker may have the daemon write to a PTY.
//!
//! A worker's PTY_WRITE is what its parser answers to queries in the
//! output (ghostty-vt `on_pty_write`). Written as is, a compromised worker
//! could type into the shell (`rm -rf ~\r`, cursor keys, a forged paste),
//! so the daemon writes only bytes that parse as whole answers of the forms
//! below, and a worker that sends anything else is a bad message:
//!
//! - CSI with an optional `?`, `>` or `=` prefix, digits, `;` and `:`, then
//!   a final from the answer finals only: `c` (DA1, DA2), `n` (DSR, color
//!   scheme), `R` (CPR, DECXCPR), `$y` (DECRPM), `?...u` (Kitty keyboard
//!   flags), `t` (size reports).
//! - OSC 4, 5, 10-19 and 21 (color reports), printable ASCII, ended by BEL
//!   or ST.
//! - DCS `>|` (XTVERSION), `!|` (DA3), `0$r`/`1$r` (DECRQSS), `0+r`/`1+r`
//!   (XTGETTCAP), printable ASCII, ended by ST.
//! - APC `G` (Kitty graphics replies), printable ASCII, ended by ST.
//!
//! OSC 52 is never a worker's answer: a clipboard read reaches a PTY only
//! through the daemon's clipboard-read broker after the person's grant
//! (ghostty-vt `terminal/clipboard_read.rs`, CLIPBOARD-READ-BROKER), and a
//! worker's parser has no clipboard (its `on_clipboard_read` is unset).
//!
//! The wire splits a long answer run across frames at
//! [`cmux_vt_worker::wire::MAX_EVENT_TEXT`], so an answer may end in the
//! next PTY_WRITE: [`AnswerScanner`] keeps an unfinished one (at most
//! [`MAX_ANSWER_BYTES`]) and releases only whole answers.

/// The longest single answer; a longer one is a bad message.
pub const MAX_ANSWER_BYTES: usize = 4096;
/// The longest CSI parameter run.
const MAX_CSI_PARAMS: usize = 64;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Bytes that are not an answer the daemon writes for a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerError {
    /// The bytes break the answer grammar (the reason names the rule).
    NotAnAnswer(&'static str),
    /// An OSC 52 clipboard reply.
    Osc52,
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnAnswer(why) => write!(f, "not a terminal answer: {why}"),
            Self::Osc52 => f.write_str("an OSC 52 clipboard reply"),
        }
    }
}

impl std::error::Error for AnswerError {}

/// One terminal's answer stream: whole answers out, an unfinished one kept.
#[derive(Debug, Default)]
pub struct AnswerScanner {
    pending: Vec<u8>,
}

/// What [`scan`] found at the start of a buffer.
enum Scan {
    /// A whole answer of this many bytes.
    Answer(usize),
    /// A valid start of an answer that the buffer ends inside.
    Unfinished,
}

impl AnswerScanner {
    /// Append `bytes` and move every whole answer they finish to `out`.
    /// An unfinished answer stays until the next call.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), AnswerError> {
        self.pending.extend_from_slice(bytes);
        let mut at = 0;
        while at < self.pending.len() {
            match scan(&self.pending[at..])? {
                Scan::Answer(len) => {
                    out.extend_from_slice(&self.pending[at..at + len]);
                    at += len;
                }
                Scan::Unfinished => break,
            }
        }
        self.pending.drain(..at);
        Ok(())
    }

    /// Whether an unfinished answer is held.
    pub fn is_holding(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// The answer at the start of `bytes`.
fn scan(bytes: &[u8]) -> Result<Scan, AnswerError> {
    let scanned = match bytes {
        [] | [ESC] => Ok(Scan::Unfinished),
        [ESC, b'[', rest @ ..] => csi(rest).map(|scan| plus(scan, 2)),
        [ESC, b']', rest @ ..] => osc(rest).map(|scan| plus(scan, 2)),
        [ESC, b'P', rest @ ..] => {
            string(rest, &[b">|", b"!|", b"0$r", b"1$r", b"0+r", b"1+r"]).map(|scan| plus(scan, 2))
        }
        [ESC, b'_', rest @ ..] => string(rest, &[b"G"]).map(|scan| plus(scan, 2)),
        [ESC, ..] => Err(AnswerError::NotAnAnswer("escape kind")),
        _ => Err(AnswerError::NotAnAnswer("bytes outside an answer")),
    }?;
    match scanned {
        Scan::Answer(len) if len > MAX_ANSWER_BYTES => {
            Err(AnswerError::NotAnAnswer("answer too long"))
        }
        Scan::Unfinished if bytes.len() > MAX_ANSWER_BYTES => {
            Err(AnswerError::NotAnAnswer("answer too long"))
        }
        scanned => Ok(scanned),
    }
}

fn plus(scan: Scan, introducer: usize) -> Scan {
    match scan {
        Scan::Answer(len) => Scan::Answer(len + introducer),
        Scan::Unfinished => Scan::Unfinished,
    }
}

/// A CSI answer after `ESC [`.
fn csi(bytes: &[u8]) -> Result<Scan, AnswerError> {
    let prefix = match bytes.first() {
        None => return Ok(Scan::Unfinished),
        Some(&byte @ (b'?' | b'>' | b'=')) => Some(byte),
        Some(_) => None,
    };
    let start = usize::from(prefix.is_some());
    let params = bytes[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        .count();
    if params > MAX_CSI_PARAMS {
        return Err(AnswerError::NotAnAnswer("CSI parameters too long"));
    }
    let at = start + params;
    let (dollar, final_at) = match bytes.get(at) {
        None => return Ok(Scan::Unfinished),
        Some(b'$') => (true, at + 1),
        Some(_) => (false, at),
    };
    let Some(&last) = bytes.get(final_at) else { return Ok(Scan::Unfinished) };
    let allowed = matches!(
        (last, dollar, prefix),
        (b'c', false, Some(_))
            | (b'n' | b'R', false, None | Some(b'?'))
            | (b'y', true, None | Some(b'?'))
            | (b'u', false, Some(b'?'))
            | (b't', false, None)
    );
    if !allowed {
        return Err(AnswerError::NotAnAnswer("CSI final"));
    }
    Ok(Scan::Answer(final_at + 1))
}

/// The OSC numbers a worker's parser answers: palette (4), special colors
/// (5), dynamic colors (10-19) and the Kitty color protocol (21).
fn osc_answers(number: u32) -> bool {
    matches!(number, 4 | 5 | 10..=19 | 21)
}

/// An OSC answer after `ESC ]`.
fn osc(bytes: &[u8]) -> Result<Scan, AnswerError> {
    let digits = bytes.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits > 5 {
        return Err(AnswerError::NotAnAnswer("OSC number"));
    }
    let Some(&separator) = bytes.get(digits) else { return Ok(Scan::Unfinished) };
    if digits == 0 || separator != b';' {
        return Err(AnswerError::NotAnAnswer("OSC number"));
    }
    let number = bytes[..digits].iter().fold(0u32, |n, digit| n * 10 + u32::from(digit - b'0'));
    if number == 52 {
        return Err(AnswerError::Osc52);
    }
    if !osc_answers(number) {
        return Err(AnswerError::NotAnAnswer("OSC number"));
    }
    let body = digits + 1;
    for (index, &byte) in bytes[body..].iter().enumerate() {
        let at = body + index;
        match byte {
            BEL => return Ok(Scan::Answer(at + 1)),
            ESC => return terminator(bytes, at),
            0x20..=0x7e => {}
            _ => return Err(AnswerError::NotAnAnswer("OSC byte")),
        }
    }
    Ok(Scan::Unfinished)
}

/// A DCS or APC answer after its introducer: one of `heads`, then
/// printable ASCII, then ST.
fn string(bytes: &[u8], heads: &[&[u8]]) -> Result<Scan, AnswerError> {
    let head = heads.iter().find(|head| {
        let n = head.len().min(bytes.len());
        bytes[..n] == head[..n]
    });
    let Some(head) = head else { return Err(AnswerError::NotAnAnswer("string answer kind")) };
    if bytes.len() < head.len() {
        return Ok(Scan::Unfinished);
    }
    for (index, &byte) in bytes[head.len()..].iter().enumerate() {
        let at = head.len() + index;
        match byte {
            ESC => return terminator(bytes, at),
            0x20..=0x7e => {}
            _ => return Err(AnswerError::NotAnAnswer("string answer byte")),
        }
    }
    Ok(Scan::Unfinished)
}

/// ST (`ESC \`) at `at`: the answer ends after it.
fn terminator(bytes: &[u8], at: usize) -> Result<Scan, AnswerError> {
    match bytes.get(at + 1) {
        None => Ok(Scan::Unfinished),
        Some(b'\\') => Ok(Scan::Answer(at + 2)),
        Some(_) => Err(AnswerError::NotAnAnswer("string terminator")),
    }
}

#[cfg(test)]
#[path = "pty_answers_tests.rs"]
mod tests;
