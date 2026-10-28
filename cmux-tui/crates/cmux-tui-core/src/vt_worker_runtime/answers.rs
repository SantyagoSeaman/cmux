//! The query answers a worker may have the daemon write to a PTY.
//!
//! A worker's PTY_WRITE is what its parser answers to queries in the output
//! (ghostty-vt `on_pty_write`). It is input to the terminal's shell, so a
//! compromised worker that could write any bytes could type commands
//! (`rm -rf ~`, or free text the person's next Return runs). The broker
//! therefore never writes a worker's bytes: it parses each answer into a
//! [`QueryAnswer`] of the fixed set below and writes that answer's own
//! encoding ([`QueryAnswer::encode`]). This is Chromium's rule for what a
//! renderer sends: the browser validates every message and acts on the
//! typed values, never on the sender's bytes (content/browser/bad_message.h).
//!
//! The fixed set: device attributes (DA1, DA2, DA3), device status (DSR 5,
//! the color scheme and visibility reports), cursor position reports (CPR,
//! DECXCPR) inside the terminal's largest grid, DECRPM mode reports, the
//! Kitty keyboard flags, XTWINOPS size reports, OSC 4/10/11/12 color reports
//! as `rgb:` hex, the OSC 7501 support reply and Kitty graphics replies
//! (numeric keys and `OK` or an error code). Every byte of these is a
//! digit, a hex digit or a fixed word.
//!
//! The rest of what a worker may send is sorted in two:
//!
//! - **Bad messages** ([`AnswerError`]): bytes outside an escape sequence,
//!   a sequence a terminal never answers with, a sequence of the fixed set
//!   whose body breaks its grammar (free text inside a color report), an
//!   OSC 52 clipboard reply (a worker's parser has no clipboard: a read
//!   reaches a PTY only through the daemon's clipboard-read broker after the
//!   person's grant), or an answer longer than [`MAX_ANSWER`]. A worker that
//!   only parses never sends one; the broker ends a worker that does.
//! - **Refused** ([`AnswerRejected`]): a whole, well-formed reply outside the
//!   fixed set (XTVERSION, DECRQSS, XTGETTCAP, OSC 21, the Kitty clipboard
//!   and drag replies, the glyph protocol).
//!   libghostty-vt sends these, so they end no worker, but their bodies are
//!   text the daemon cannot check, so they are not written.
//!
//! The wire splits a long answer run across frames at
//! [`cmux_vt_worker::wire::MAX_EVENT_TEXT`], so an answer may end in the next
//! PTY_WRITE: [`AnswerScanner`] keeps an unfinished one and releases only
//! whole answers.

use std::fmt;

/// The longest single escape sequence a worker may send as an answer: an
/// unfinished one past it is a bad message.
pub const MAX_ANSWER: usize = 4096;

/// Parameters a DA1 answer may carry.
const MAX_PARAMS: usize = 16;
/// Digits of one CSI parameter (size reports count pixels).
const MAX_DIGITS: usize = 5;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Kitty graphics error codes (kitty's graphics protocol and libghostty-vt
/// `kitty/graphics_*.zig`).
const KITTY_ERRORS: &[&str] = &[
    "EINVAL",
    "ENOENT",
    "ENODATA",
    "ENOMEM",
    "ENOSPC",
    "EBADF",
    "EFBIG",
    "EPERM",
    "ETOODEEP",
    "ECYCLE",
    "ENOPARENT",
];

/// How a string answer ended: the encoding keeps it (an answer ends as its
/// query did).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringEnd {
    Bel,
    St,
}

impl StringEnd {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Bel => b"\x07",
            Self::St => b"\x1b\\",
        }
    }
}

/// The color an OSC color report names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorTarget {
    /// OSC 4: a palette entry.
    Palette(u8),
    /// OSC 10.
    Foreground,
    /// OSC 11.
    Background,
    /// OSC 12.
    Cursor,
}

/// The status of a Kitty graphics reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyStatus {
    Ok,
    /// An error code of [`KITTY_ERRORS`]; the message after it is text and
    /// is not written.
    Error(&'static str),
}

/// One answer of the fixed set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryAnswer {
    /// DA1: `CSI ? Ps ; ... c`.
    PrimaryAttributes(Vec<u16>),
    /// DA2: `CSI > Pp ; Pv ; Pc c` (one to three parameters).
    SecondaryAttributes(Vec<u16>),
    /// DA3: `DCS ! | D...D ST`, eight hex digits.
    TertiaryAttributes([u8; 8]),
    /// DSR 5: `CSI 0 n`.
    OperatingStatus,
    /// `CSI ? 997 ; 1|2 n` (dark, light).
    ColorScheme(u8),
    /// `CSI ? 999 ; 1|2 n` (visible, not visible).
    Visibility(u8),
    /// DSR 6: `CSI row ; col R`, inside the terminal.
    CursorPosition { row: u16, col: u16 },
    /// DECXCPR: `CSI ? row ; col R` or `CSI ? row ; col ; page R`.
    ExtendedCursorPosition { row: u16, col: u16, page: Option<u16> },
    /// DECRPM: `CSI [?] mode ; state $ y`, state 0 to 4.
    ModeReport { private: bool, mode: u16, state: u8 },
    /// The Kitty keyboard flags: `CSI ? flags u` (five bits).
    KittyKeyboard(u8),
    /// XTWINOPS: `CSI 4|6|8 ; a ; b t`, or the in-band report
    /// `CSI 48 ; rows ; cols ; height ; width t`.
    SizeReport(Vec<u32>),
    /// `OSC 4 ; index ; rgb:R/G/B`, `OSC 10|11|12 ; rgb:R/G/B`, 16-bit hex.
    ColorReport { target: ColorTarget, rgb: [u16; 3], end: StringEnd },
    /// `OSC 7501 ; ?`: the terminal takes program status reports.
    ProgramStatusSupport(StringEnd),
    /// `APC G key=value,... ; OK|ECODE ST` with the keys `i`, `I`, `p`, `r`.
    KittyGraphics { keys: Vec<(u8, u32)>, status: KittyStatus },
}

/// A worker's bytes that no worker that only parses sends: a bad message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerError {
    /// The bytes break the answer grammar (the reason names the rule).
    NotAnAnswer(&'static str),
    /// An OSC 52 clipboard reply.
    Osc52,
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnAnswer(why) => write!(f, "not a terminal answer: {why}"),
            Self::Osc52 => f.write_str("an OSC 52 clipboard reply"),
        }
    }
}

impl std::error::Error for AnswerError {}

/// A whole reply outside the fixed set: not written, and no bad message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerRejected {
    /// The reply's kind (`"XTVERSION"`, `"OSC 21"`...).
    OutsideSet(&'static str),
}

impl fmt::Display for AnswerRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideSet(kind) => write!(f, "{kind} reply outside the fixed answer set"),
        }
    }
}

impl std::error::Error for AnswerRejected {}

/// What one whole answer is.
pub type Scanned = Result<QueryAnswer, AnswerRejected>;

/// The grid cursor reports must fall inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnswerGrid {
    pub cols: u16,
    pub rows: u16,
}

/// One terminal's answer stream: whole answers out, an unfinished one kept.
#[derive(Debug, Default)]
pub struct AnswerScanner {
    pending: Vec<u8>,
}

/// What [`scan`] found at the start of a buffer.
enum Scan {
    /// A whole sequence of this many bytes.
    Whole(usize, Scanned),
    /// A valid start that the buffer ends inside.
    Unfinished,
}

impl AnswerScanner {
    /// Append `bytes` and move every whole answer they finish to `out`. An
    /// unfinished answer stays until the next call. On an error the scanner
    /// is spent: the broker ends the worker.
    pub fn feed(
        &mut self,
        bytes: &[u8],
        grid: AnswerGrid,
        out: &mut Vec<Scanned>,
    ) -> Result<(), AnswerError> {
        self.pending.extend_from_slice(bytes);
        let mut at = 0;
        while at < self.pending.len() {
            match scan(&self.pending[at..], grid)? {
                Scan::Whole(len, answer) => {
                    out.push(answer);
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

impl QueryAnswer {
    /// This answer's own encoding: what the broker writes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::PrimaryAttributes(params) => csi(&mut out, "?", params, "c"),
            Self::SecondaryAttributes(params) => csi(&mut out, ">", params, "c"),
            Self::TertiaryAttributes(hex) => {
                out.extend_from_slice(b"\x1bP!|");
                out.extend_from_slice(hex);
                out.extend_from_slice(b"\x1b\\");
            }
            Self::OperatingStatus => out.extend_from_slice(b"\x1b[0n"),
            Self::ColorScheme(scheme) => csi(&mut out, "?", &[997, u16::from(*scheme)], "n"),
            Self::Visibility(visible) => csi(&mut out, "?", &[999, u16::from(*visible)], "n"),
            Self::CursorPosition { row, col } => csi(&mut out, "", &[*row, *col], "R"),
            Self::ExtendedCursorPosition { row, col, page: None } => {
                csi(&mut out, "?", &[*row, *col], "R");
            }
            Self::ExtendedCursorPosition { row, col, page: Some(page) } => {
                csi(&mut out, "?", &[*row, *col, *page], "R");
            }
            Self::ModeReport { private, mode, state } => {
                csi(&mut out, if *private { "?" } else { "" }, &[*mode, u16::from(*state)], "$y");
            }
            Self::KittyKeyboard(flags) => csi(&mut out, "?", &[*flags], "u"),
            Self::SizeReport(params) => csi(&mut out, "", params, "t"),
            Self::ColorReport { target, rgb: [r, g, b], end } => {
                let head = match target {
                    ColorTarget::Palette(index) => format!("\x1b]4;{index};"),
                    ColorTarget::Foreground => "\x1b]10;".to_owned(),
                    ColorTarget::Background => "\x1b]11;".to_owned(),
                    ColorTarget::Cursor => "\x1b]12;".to_owned(),
                };
                out.extend_from_slice(head.as_bytes());
                out.extend_from_slice(format!("rgb:{r:04x}/{g:04x}/{b:04x}").as_bytes());
                out.extend_from_slice(end.bytes());
            }
            Self::ProgramStatusSupport(end) => {
                out.extend_from_slice(b"\x1b]7501;?");
                out.extend_from_slice(end.bytes());
            }
            Self::KittyGraphics { keys, status } => {
                out.extend_from_slice(b"\x1b_G");
                let keys: Vec<String> =
                    keys.iter().map(|(key, value)| format!("{}={value}", *key as char)).collect();
                out.extend_from_slice(keys.join(",").as_bytes());
                out.push(b';');
                out.extend_from_slice(match status {
                    KittyStatus::Ok => b"OK",
                    KittyStatus::Error(code) => code.as_bytes(),
                });
                out.extend_from_slice(b"\x1b\\");
            }
        }
        out
    }
}

/// `CSI <marker> p;p;... <last>`.
fn csi<P: fmt::Display>(out: &mut Vec<u8>, marker: &str, params: &[P], last: &str) {
    let params: Vec<String> = params.iter().map(ToString::to_string).collect();
    out.extend_from_slice(format!("\x1b[{marker}{}{last}", params.join(";")).as_bytes());
}

/// The answer at the start of `bytes`.
fn scan(bytes: &[u8], grid: AnswerGrid) -> Result<Scan, AnswerError> {
    let scanned = match bytes {
        [] | [ESC] => Ok(Scan::Unfinished),
        [ESC, b'[', rest @ ..] => csi_answer(rest, grid).map(|scan| plus(scan, 2)),
        [ESC, b']', rest @ ..] => osc_answer(rest).map(|scan| plus(scan, 2)),
        [ESC, b'P', rest @ ..] => dcs_answer(rest).map(|scan| plus(scan, 2)),
        [ESC, b'_', rest @ ..] => apc_answer(rest).map(|scan| plus(scan, 2)),
        [ESC, ..] => Err(AnswerError::NotAnAnswer("escape kind")),
        _ => Err(AnswerError::NotAnAnswer("bytes outside an answer")),
    }?;
    match scanned {
        Scan::Whole(len, _) if len > MAX_ANSWER => Err(AnswerError::NotAnAnswer("answer too long")),
        Scan::Unfinished if bytes.len() > MAX_ANSWER => {
            Err(AnswerError::NotAnAnswer("answer too long"))
        }
        scanned => Ok(scanned),
    }
}

fn plus(scan: Scan, introducer: usize) -> Scan {
    match scan {
        Scan::Whole(len, answer) => Scan::Whole(len + introducer, answer),
        Scan::Unfinished => Scan::Unfinished,
    }
}

/// A CSI answer after `ESC [`: an optional `?`, `>` or `=`, digits and
/// `;`, an optional `$`, then the final byte.
fn csi_answer(bytes: &[u8], grid: AnswerGrid) -> Result<Scan, AnswerError> {
    let prefix = match bytes.first() {
        None => return Ok(Scan::Unfinished),
        Some(&byte @ (b'?' | b'>' | b'=')) => Some(byte),
        Some(_) => None,
    };
    let start = usize::from(prefix.is_some());
    let params =
        bytes[start..].iter().take_while(|byte| byte.is_ascii_digit() || **byte == b';').count();
    if params > MAX_PARAMS * (MAX_DIGITS + 1) {
        return Err(AnswerError::NotAnAnswer("CSI parameters too long"));
    }
    let at = start + params;
    let (dollar, final_at) = match bytes.get(at) {
        None => return Ok(Scan::Unfinished),
        Some(b'$') => (true, at + 1),
        Some(_) => (false, at),
    };
    let Some(&last) = bytes.get(final_at) else { return Ok(Scan::Unfinished) };
    if !(0x40..=0x7e).contains(&last) {
        return Err(AnswerError::NotAnAnswer("CSI byte"));
    }
    let params = &bytes[start..at];
    let answer = csi_kind(prefix, dollar, last, params, grid)?;
    Ok(Scan::Whole(final_at + 1, answer))
}

/// The CSI answer `prefix params [$] last` names. A kind of the fixed set
/// with other parameters is a bad message; any other kind is a query or a
/// key, never an answer.
fn csi_kind(
    prefix: Option<u8>,
    dollar: bool,
    last: u8,
    params: &[u8],
    grid: AnswerGrid,
) -> Result<Scanned, AnswerError> {
    let bad = |why| Err(AnswerError::NotAnAnswer(why));
    let Some(values) = parse_params(params) else { return bad("CSI parameters") };
    let small = |value: u32| u16::try_from(value).ok().filter(|value| *value <= 9999);
    let answer = match (prefix, dollar, last) {
        (Some(b'?'), false, b'c') => {
            let params: Option<Vec<u16>> = values.iter().map(|value| small(*value)).collect();
            match params {
                Some(params) if !params.is_empty() => QueryAnswer::PrimaryAttributes(params),
                _ => return bad("DA1"),
            }
        }
        (Some(b'>'), false, b'c') => {
            let params: Option<Vec<u16>> = values.iter().map(|value| small(*value)).collect();
            match params {
                Some(params) if (1..=3).contains(&params.len()) => {
                    QueryAnswer::SecondaryAttributes(params)
                }
                _ => return bad("DA2"),
            }
        }
        (None, false, b'n') => match values.as_slice() {
            [0] => QueryAnswer::OperatingStatus,
            _ => return bad("DSR"),
        },
        (Some(b'?'), false, b'n') => match *values.as_slice() {
            [997, scheme @ (1 | 2)] => QueryAnswer::ColorScheme(scheme as u8),
            [999, visible @ (1 | 2)] => QueryAnswer::Visibility(visible as u8),
            _ => return bad("DSR"),
        },
        (None | Some(b'?'), false, b'R') => {
            let (row, col, page) = match *values.as_slice() {
                [row, col] => (row, col, None),
                [row, col, page] if prefix.is_some() => (row, col, Some(page)),
                _ => return bad("cursor report"),
            };
            let (Some(row), Some(col)) = (small(row), small(col)) else {
                return bad("cursor report");
            };
            if !(1..=grid.rows).contains(&row) || !(1..=grid.cols).contains(&col) {
                return bad("cursor report outside the grid");
            }
            match (prefix, page.map(small)) {
                (None, _) => QueryAnswer::CursorPosition { row, col },
                (Some(_), None) => QueryAnswer::ExtendedCursorPosition { row, col, page: None },
                (Some(_), Some(Some(page))) => {
                    QueryAnswer::ExtendedCursorPosition { row, col, page: Some(page) }
                }
                (Some(_), Some(None)) => return bad("cursor report"),
            }
        }
        (None | Some(b'?'), true, b'y') => match *values.as_slice() {
            [mode, state @ 0..=4] => match u16::try_from(mode) {
                Ok(mode) => {
                    QueryAnswer::ModeReport { private: prefix.is_some(), mode, state: state as u8 }
                }
                Err(_) => return bad("DECRPM"),
            },
            _ => return bad("DECRPM"),
        },
        (Some(b'?'), false, b'u') => match *values.as_slice() {
            [flags @ 0..=31] => QueryAnswer::KittyKeyboard(flags as u8),
            _ => return bad("Kitty keyboard flags"),
        },
        (None, false, b't') => match values.as_slice() {
            [4 | 6 | 8, _, _] | [48, _, _, _, _] => QueryAnswer::SizeReport(values),
            _ => return bad("size report"),
        },
        _ => return bad("CSI kind"),
    };
    Ok(Ok(answer))
}

/// `p;p;...`: decimal parameters, none empty, at most [`MAX_PARAMS`] of at
/// most [`MAX_DIGITS`] digits. `None` for anything else.
fn parse_params(bytes: &[u8]) -> Option<Vec<u32>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let mut params = Vec::new();
    for part in bytes.split(|&byte| byte == b';') {
        if part.is_empty() || part.len() > MAX_DIGITS || params.len() == MAX_PARAMS {
            return None;
        }
        let digits = std::str::from_utf8(part).ok()?;
        params.push(digits.parse().ok()?);
    }
    Some(params)
}

/// The body of a string answer: printable ASCII up to its end. `Ok(None)`
/// when the buffer ends first.
fn string_body(bytes: &[u8], bel: bool) -> Result<Option<(usize, StringEnd)>, AnswerError> {
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            BEL if bel => return Ok(Some((at, StringEnd::Bel))),
            ESC => {
                return match bytes.get(at + 1) {
                    None => Ok(None),
                    Some(b'\\') => Ok(Some((at, StringEnd::St))),
                    Some(_) => Err(AnswerError::NotAnAnswer("string terminator")),
                };
            }
            0x20..=0x7e => {}
            _ => return Err(AnswerError::NotAnAnswer("string answer byte")),
        }
    }
    Ok(None)
}

fn end_len(end: StringEnd) -> usize {
    end.bytes().len()
}

/// An OSC answer after `ESC ]`.
fn osc_answer(bytes: &[u8]) -> Result<Scan, AnswerError> {
    let digits = bytes.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits > 5 {
        return Err(AnswerError::NotAnAnswer("OSC number"));
    }
    let Some(&separator) = bytes.get(digits) else { return Ok(Scan::Unfinished) };
    if digits == 0 || separator != b';' {
        return Err(AnswerError::NotAnAnswer("OSC number"));
    }
    let number: u32 = std::str::from_utf8(&bytes[..digits])
        .ok()
        .and_then(|digits| digits.parse().ok())
        .ok_or(AnswerError::NotAnAnswer("OSC number"))?;
    if number == 52 {
        return Err(AnswerError::Osc52);
    }
    let body_at = digits + 1;
    let Some((len, end)) = string_body(&bytes[body_at..], true)? else {
        return Ok(Scan::Unfinished);
    };
    let body = &bytes[body_at..body_at + len];
    let whole = body_at + len + end_len(end);
    let bad = |why| Err(AnswerError::NotAnAnswer(why));
    let answer = match number {
        4 => {
            let Some((index, color)) = split_once(body, b';') else { return bad("OSC 4") };
            let index = std::str::from_utf8(index).ok().filter(|index| index.len() <= 3);
            let Some(index) = index.and_then(|index| index.parse::<u8>().ok()) else {
                return bad("OSC 4");
            };
            let Some(rgb) = rgb16(color) else { return bad("OSC 4") };
            Ok(QueryAnswer::ColorReport { target: ColorTarget::Palette(index), rgb, end })
        }
        10..=12 => {
            let target = match number {
                10 => ColorTarget::Foreground,
                11 => ColorTarget::Background,
                _ => ColorTarget::Cursor,
            };
            let Some(rgb) = rgb16(body) else { return bad("OSC color report") };
            Ok(QueryAnswer::ColorReport { target, rgb, end })
        }
        7501 if body == b"?" => Ok(QueryAnswer::ProgramStatusSupport(end)),
        7501 => return bad("OSC 7501"),
        21 => Err(AnswerRejected::OutsideSet("OSC 21")),
        72 => Err(AnswerRejected::OutsideSet("OSC 72")),
        5522 => Err(AnswerRejected::OutsideSet("OSC 5522")),
        _ => return bad("OSC kind"),
    };
    Ok(Scan::Whole(whole, answer))
}

/// `rgb:RRRR/GGGG/BBBB`, four hex digits each (libghostty-vt
/// `RGB.encodeRgb16`).
fn rgb16(body: &[u8]) -> Option<[u16; 3]> {
    let body = body.strip_prefix(b"rgb:")?;
    let mut parts = body.split(|&byte| byte == b'/');
    let mut rgb = [0u16; 3];
    for channel in &mut rgb {
        let part = parts.next()?;
        if part.len() != 4 || !part.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        *channel = u16::from_str_radix(std::str::from_utf8(part).ok()?, 16).ok()?;
    }
    parts.next().is_none().then_some(rgb)
}

fn split_once(bytes: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let at = bytes.iter().position(|&byte| byte == separator)?;
    Some((&bytes[..at], &bytes[at + 1..]))
}

/// A DCS answer after `ESC P`.
fn dcs_answer(bytes: &[u8]) -> Result<Scan, AnswerError> {
    let Some((len, end)) = string_body(bytes, false)? else { return Ok(Scan::Unfinished) };
    let body = &bytes[..len];
    let whole = len + end_len(end);
    let answer = if let Some(hex) = body.strip_prefix(b"!|") {
        let hex: [u8; 8] = hex.try_into().map_err(|_| AnswerError::NotAnAnswer("DA3"))?;
        if !hex.iter().all(u8::is_ascii_hexdigit) {
            return Err(AnswerError::NotAnAnswer("DA3"));
        }
        Ok(QueryAnswer::TertiaryAttributes(hex))
    } else if body.starts_with(b">|") {
        Err(AnswerRejected::OutsideSet("XTVERSION"))
    } else if body.starts_with(b"0$r") || body.starts_with(b"1$r") {
        Err(AnswerRejected::OutsideSet("DECRQSS"))
    } else if body.starts_with(b"0+r") || body.starts_with(b"1+r") {
        Err(AnswerRejected::OutsideSet("XTGETTCAP"))
    } else {
        return Err(AnswerError::NotAnAnswer("DCS kind"));
    };
    Ok(Scan::Whole(whole, answer))
}

/// An APC answer after `ESC _`.
fn apc_answer(bytes: &[u8]) -> Result<Scan, AnswerError> {
    let Some((len, end)) = string_body(bytes, false)? else { return Ok(Scan::Unfinished) };
    let body = &bytes[..len];
    let whole = len + end_len(end);
    let answer = if let Some(reply) = body.strip_prefix(b"G") {
        kitty_graphics(reply)?
    } else if body.starts_with(b"25a1;") {
        Err(AnswerRejected::OutsideSet("glyph protocol"))
    } else {
        return Err(AnswerError::NotAnAnswer("APC kind"));
    };
    Ok(Scan::Whole(whole, answer))
}

/// `key=value,...;OK` or `key=value,...;ECODE[:message]` (libghostty-vt
/// `kitty/graphics_command.zig`, `Response.encode`).
fn kitty_graphics(reply: &[u8]) -> Result<Scanned, AnswerError> {
    let bad = || AnswerError::NotAnAnswer("Kitty graphics reply");
    let (keys, message) = split_once(reply, b';').ok_or_else(bad)?;
    let mut parsed = Vec::new();
    for pair in keys.split(|&byte| byte == b',') {
        let (key, value) = split_once(pair, b'=').ok_or_else(bad)?;
        let &[key @ (b'i' | b'I' | b'p' | b'r')] = key else { return Err(bad()) };
        if value.is_empty() || value.len() > 10 || !value.iter().all(u8::is_ascii_digit) {
            return Err(bad());
        }
        let value: u32 =
            std::str::from_utf8(value).ok().and_then(|value| value.parse().ok()).ok_or_else(bad)?;
        if parsed.iter().any(|(seen, _)| *seen == key) {
            return Err(bad());
        }
        parsed.push((key, value));
    }
    if message == b"OK" {
        return Ok(Ok(QueryAnswer::KittyGraphics { keys: parsed, status: KittyStatus::Ok }));
    }
    let code = message.split(|&byte| byte == b':').next().unwrap_or_default();
    if code.len() < 2 || code[0] != b'E' || !code.iter().all(u8::is_ascii_uppercase) {
        return Err(bad());
    }
    let known = KITTY_ERRORS.iter().find(|known| known.as_bytes() == code);
    Ok(match known {
        Some(code) => {
            Ok(QueryAnswer::KittyGraphics { keys: parsed, status: KittyStatus::Error(code) })
        }
        None => Err(AnswerRejected::OutsideSet("Kitty graphics error")),
    })
}

#[cfg(test)]
#[path = "answers_tests.rs"]
mod tests;
