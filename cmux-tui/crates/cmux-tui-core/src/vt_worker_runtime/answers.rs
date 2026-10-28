//! The query answers a worker may have the daemon write to a PTY: device
//! attributes (DA1, DA2, DA3) and device status reports (DSR 5, the cursor
//! position reports of DSR 6 and DECXCPR).
//!
//! A worker parses untrusted output in a sandbox; what it sends back is
//! untrusted too (Chromium's rule: the browser validates every IPC from a
//! renderer and kills one that sends a bad message,
//! content/browser/bad_message.h). An answer is input to the terminal's
//! shell, so a compromised worker that could write arbitrary answers could
//! type commands. The broker therefore writes only the canonical encoding
//! of an answer it parsed from this fixed set, never the worker's bytes.

use std::fmt;

/// The longest answer of the fixed set, with room: a worker's PTY_WRITE
/// longer than this is not one answer.
pub const MAX_ANSWER: usize = 64;

/// Parameters a DA1 or DA2 answer may carry.
const MAX_PARAMS: usize = 16;
/// The largest parameter value (`CSI` parameters are at most 4 digits here).
const MAX_PARAM: u16 = 9999;

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
    /// DSR 6: `CSI row ; col R`, inside the terminal.
    CursorPosition { row: u16, col: u16 },
    /// DECXCPR: `CSI ? row ; col R` or `CSI ? row ; col ; page R`.
    ExtendedCursorPosition { row: u16, col: u16, page: Option<u16> },
}

/// Why an answer was not written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerRejected {
    /// Longer than [`MAX_ANSWER`].
    TooLong(usize),
    /// Not one answer of the fixed set.
    NotAnAnswer,
    /// A cursor report outside the terminal's `cols` x `rows`.
    CursorOutside { row: u16, col: u16 },
    /// The terminal is not one of the sending worker's shard.
    NotInShard,
}

impl fmt::Display for AnswerRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong(len) => write!(f, "query answer of {len} bytes"),
            Self::NotAnAnswer => f.write_str("not a DA or DSR answer"),
            Self::CursorOutside { row, col } => {
                write!(f, "cursor report {row};{col} outside the terminal")
            }
            Self::NotInShard => f.write_str("answer for a terminal outside the worker's shard"),
        }
    }
}

impl std::error::Error for AnswerRejected {}

impl QueryAnswer {
    /// Every answer in `bytes`, in order: a worker sends the answers one
    /// parse slice made as one PTY_WRITE. Each sequence is checked on its
    /// own; bytes that start no sequence end the split as one rejection.
    pub fn parse_all(bytes: &[u8], cols: u16, rows: u16) -> Vec<Result<Self, AnswerRejected>> {
        let mut answers = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let len = sequence_len(rest).unwrap_or(rest.len());
            let (one, after) = rest.split_at(len);
            answers.push(Self::parse(one, cols, rows));
            rest = after;
        }
        answers
    }

    /// The answer `bytes` say, for a terminal of `cols` x `rows`.
    pub fn parse(bytes: &[u8], cols: u16, rows: u16) -> Result<Self, AnswerRejected> {
        if bytes.len() > MAX_ANSWER {
            return Err(AnswerRejected::TooLong(bytes.len()));
        }
        if let Some(hex) =
            bytes.strip_prefix(b"\x1bP!|").and_then(|rest| rest.strip_suffix(b"\x1b\\"))
        {
            let hex: [u8; 8] = hex.try_into().map_err(|_| AnswerRejected::NotAnAnswer)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return Err(AnswerRejected::NotAnAnswer);
            }
            return Ok(Self::TertiaryAttributes(hex));
        }
        let body = bytes.strip_prefix(b"\x1b[").ok_or(AnswerRejected::NotAnAnswer)?;
        let (&last, body) = body.split_last().ok_or(AnswerRejected::NotAnAnswer)?;
        let (marker, params) = match body.first() {
            Some(&marker @ (b'?' | b'>')) => (Some(marker), &body[1..]),
            _ => (None, body),
        };
        let params = parse_params(params)?;
        let answer = match (marker, last, params.as_slice()) {
            (Some(b'?'), b'c', [_, ..]) => Self::PrimaryAttributes(params),
            (Some(b'>'), b'c', [_] | [_, _] | [_, _, _]) => Self::SecondaryAttributes(params),
            (None, b'n', [0]) => Self::OperatingStatus,
            (None, b'R', &[row, col]) => Self::CursorPosition { row, col },
            (Some(b'?'), b'R', &[row, col]) => {
                Self::ExtendedCursorPosition { row, col, page: None }
            }
            (Some(b'?'), b'R', &[row, col, page]) => {
                Self::ExtendedCursorPosition { row, col, page: Some(page) }
            }
            _ => return Err(AnswerRejected::NotAnAnswer),
        };
        if let Self::CursorPosition { row, col } | Self::ExtendedCursorPosition { row, col, .. } =
            &answer
            && (!(1..=rows).contains(row) || !(1..=cols).contains(col))
        {
            return Err(AnswerRejected::CursorOutside { row: *row, col: *col });
        }
        Ok(answer)
    }

    /// The canonical bytes of this answer: what the broker writes.
    pub fn encode(&self) -> Vec<u8> {
        let csi = |marker: &str, params: &[u16], last: char| {
            let params: Vec<String> = params.iter().map(u16::to_string).collect();
            format!("\x1b[{marker}{}{last}", params.join(";")).into_bytes()
        };
        match self {
            Self::PrimaryAttributes(params) => csi("?", params, 'c'),
            Self::SecondaryAttributes(params) => csi(">", params, 'c'),
            Self::TertiaryAttributes(hex) => {
                let mut out = b"\x1bP!|".to_vec();
                out.extend_from_slice(hex);
                out.extend_from_slice(b"\x1b\\");
                out
            }
            Self::OperatingStatus => b"\x1b[0n".to_vec(),
            Self::CursorPosition { row, col } => csi("", &[*row, *col], 'R'),
            Self::ExtendedCursorPosition { row, col, page: None } => csi("?", &[*row, *col], 'R'),
            Self::ExtendedCursorPosition { row, col, page: Some(page) } => {
                csi("?", &[*row, *col, *page], 'R')
            }
        }
    }
}

/// The length of the escape sequence `bytes` start with: a CSI to its
/// final byte, a DCS to its ST. `None` when they start none, or it does not
/// end within [`MAX_ANSWER`] bytes.
fn sequence_len(bytes: &[u8]) -> Option<usize> {
    let window = &bytes[..bytes.len().min(MAX_ANSWER + 1)];
    match window {
        [0x1b, b'[', body @ ..] => {
            body.iter().position(|byte| (0x40..=0x7e).contains(byte)).map(|at| at + 3)
        }
        [0x1b, b'P', body @ ..] => {
            body.windows(2).position(|pair| pair == b"\x1b\\").map(|at| at + 4)
        }
        _ => None,
    }
}

/// `Ps ; Ps ; ...`: decimal parameters, none empty, at most [`MAX_PARAMS`]
/// of at most [`MAX_PARAM`] each.
fn parse_params(bytes: &[u8]) -> Result<Vec<u16>, AnswerRejected> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = Vec::new();
    for part in bytes.split(|&byte| byte == b';') {
        if part.is_empty() || part.len() > 4 || !part.iter().all(u8::is_ascii_digit) {
            return Err(AnswerRejected::NotAnAnswer);
        }
        let value = part.iter().fold(0u16, |value, digit| value * 10 + u16::from(digit - b'0'));
        if value > MAX_PARAM || params.len() == MAX_PARAMS {
            return Err(AnswerRejected::NotAnAnswer);
        }
        params.push(value);
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Result<QueryAnswer, AnswerRejected> {
        QueryAnswer::parse(bytes, 80, 24)
    }

    #[test]
    fn the_fixed_set_parses_and_encodes_canonically() {
        for bytes in [
            &b"\x1b[?62;22c"[..],
            b"\x1b[?1;2c",
            b"\x1b[>1;10;0c",
            b"\x1b[>0c",
            b"\x1bP!|00000000\x1b\\",
            b"\x1b[0n",
            b"\x1b[24;80R",
            b"\x1b[1;1R",
            b"\x1b[?3;7R",
            b"\x1b[?3;7;1R",
        ] {
            let answer = parse(bytes).unwrap_or_else(|error| panic!("{bytes:?}: {error}"));
            assert_eq!(answer.encode(), bytes, "{answer:?}");
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for bytes in [
            &b""[..],
            b"ls -la\r",
            b"\x1b[?62;22c\recho pwned\r",
            b"\x1b[?c",
            b"\x1b[>1;2;3;4c",
            b"\x1b[5n",
            b"\x1b[6n",
            b"\x1b[;1R",
            b"\x1b[1;;1R",
            b"\x1b[01234;1R",
            b"\x1b[?1$y",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
            b"\x1bP!|0000000\x1b\\",
            b"\x1bP!|0000000g\x1b\\",
            b"\x1b[?62;22c\x1b[0n",
            b"\x1b[?1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17c",
        ] {
            assert!(parse(bytes).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn cursor_reports_stay_inside_the_terminal() {
        assert_eq!(parse(b"\x1b[25;1R"), Err(AnswerRejected::CursorOutside { row: 25, col: 1 }));
        assert_eq!(parse(b"\x1b[1;81R"), Err(AnswerRejected::CursorOutside { row: 1, col: 81 }));
        assert_eq!(parse(b"\x1b[0;1R"), Err(AnswerRejected::CursorOutside { row: 0, col: 1 }));
        assert_eq!(parse(b"\x1b[?9;99;1R"), Err(AnswerRejected::CursorOutside { row: 9, col: 99 }));
    }

    #[test]
    fn the_answers_of_one_slice_are_split_and_checked_one_by_one() {
        let all = |bytes: &[u8]| QueryAnswer::parse_all(bytes, 80, 24);
        assert_eq!(
            all(b"\x1b[?62;22c\x1bP!|0000abcd\x1b\\\x1b[0n\x1b[3;4R"),
            [
                Ok(QueryAnswer::PrimaryAttributes(vec![62, 22])),
                Ok(QueryAnswer::TertiaryAttributes(*b"0000abcd")),
                Ok(QueryAnswer::OperatingStatus),
                Ok(QueryAnswer::CursorPosition { row: 3, col: 4 }),
            ]
        );
        // A bad sequence is refused on its own; bytes that start none end the split.
        assert_eq!(
            all(b"\x1b[?1$y\x1b[0nrm -rf ~\r\x1b[0n"),
            [
                Err(AnswerRejected::NotAnAnswer),
                Ok(QueryAnswer::OperatingStatus),
                Err(AnswerRejected::NotAnAnswer),
            ]
        );
        assert!(all(b"").is_empty());
    }

    #[test]
    fn long_answers_are_refused_before_parsing() {
        let long = [b'0'; MAX_ANSWER + 1];
        assert_eq!(parse(&long), Err(AnswerRejected::TooLong(MAX_ANSWER + 1)));
    }
}
