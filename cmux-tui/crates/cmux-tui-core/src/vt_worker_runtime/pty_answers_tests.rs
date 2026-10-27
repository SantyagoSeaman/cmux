use super::*;

fn written(chunks: &[&[u8]]) -> Result<Vec<u8>, AnswerError> {
    let mut scanner = AnswerScanner::default();
    let mut out = Vec::new();
    for chunk in chunks {
        scanner.feed(chunk, &mut out)?;
    }
    assert!(!scanner.is_holding(), "an unfinished answer is left");
    Ok(out)
}

/// What libghostty-vt answers to the queries programs send (DA, DSR, CPR,
/// DECRQM, Kitty keyboard, size reports, colors, XTVERSION, DECRQSS,
/// XTGETTCAP, DA3, Kitty graphics).
const ANSWERS: &[&[u8]] = &[
    b"\x1b[?62;22;52c",
    b"\x1b[>1;10;0c",
    b"\x1b[0n",
    b"\x1b[?997;1n",
    b"\x1b[24;80R",
    b"\x1b[?24;80;1R",
    b"\x1b[?2004;1$y",
    b"\x1b[4;2$y",
    b"\x1b[?5u",
    b"\x1b[8;24;80t",
    b"\x1b[48;24;80;480;640t",
    b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\",
    b"\x1b]4;1;rgb:cccc/0000/0000\x07",
    b"\x1b]21;foreground=rgb:ff/ff/ff\x1b\\",
    b"\x1bP>|libghostty 1.3.0\x1b\\",
    b"\x1bP1$r0;1m\x1b\\",
    b"\x1bP0+r\x1b\\",
    b"\x1bP!|00000000\x1b\\",
    b"\x1b_Gi=31;OK\x1b\\",
];

#[test]
fn every_answer_form_is_written_whole() {
    for answer in ANSWERS {
        assert_eq!(written(&[answer]).as_deref(), Ok(*answer), "{}", answer.escape_ascii());
    }
    let all: Vec<u8> = ANSWERS.concat();
    assert_eq!(written(&[&all]), Ok(all.clone()));
}

#[test]
fn an_answer_split_across_writes_waits_for_its_end() {
    let all: Vec<u8> = ANSWERS.concat();
    for cut in 0..=all.len() {
        let (head, tail) = all.split_at(cut);
        let mut scanner = AnswerScanner::default();
        let mut out = Vec::new();
        scanner.feed(head, &mut out).expect("head");
        assert!(all.starts_with(&out) && out.len() <= cut, "cut {cut}");
        scanner.feed(tail, &mut out).expect("tail");
        assert_eq!(out, all, "cut {cut}");
        assert!(!scanner.is_holding());
    }
}

#[test]
fn typed_input_and_other_sequences_are_refused() {
    let refused: &[&[u8]] = &[
        b"rm -rf ~\r",
        b"\x1b[0n\r",
        b"\x1b[A",
        b"\x1b[200~paste\x1b[201~",
        b"\x1b[0c",
        b"\x1b[?5t",
        b"\x1b[5$u",
        b"\x1bOA",
        b"\x1b]0;title\x07",
        b"\x1b]7;file:///tmp\x07",
        b"\x1b]11;rgb:00/00/00\x1bX",
        b"\x1b]11;\r\x07",
        b"\x1bPq#0;2;0;0;0\x1b\\",
        b"\x1bP>|ok\r\x1b\\",
        b"\x1b_Pnot-graphics\x1b\\",
        b"\x9b0n",
    ];
    for bytes in refused {
        assert!(
            matches!(written(&[bytes]), Err(AnswerError::NotAnAnswer(_))),
            "{} -> {:?}",
            bytes.escape_ascii(),
            written(&[bytes])
        );
    }
}

#[test]
fn an_osc_52_reply_is_refused_by_name() {
    assert_eq!(written(&[b"\x1b]52;c;aGVsbG8=\x07"]), Err(AnswerError::Osc52));
    assert_eq!(written(&[b"\x1b]5", b"2;c;"]), Err(AnswerError::Osc52));
}

#[test]
fn an_answer_that_never_ends_is_refused() {
    let mut scanner = AnswerScanner::default();
    let mut out = Vec::new();
    scanner.feed(b"\x1b]11;", &mut out).expect("start");
    let body = vec![b'f'; MAX_ANSWER_BYTES];
    assert_eq!(scanner.feed(&body, &mut out), Err(AnswerError::NotAnAnswer("answer too long")));
    let params = vec![b'1'; MAX_CSI_PARAMS + 1];
    let mut long = b"\x1b[".to_vec();
    long.extend_from_slice(&params);
    long.push(b'n');
    assert!(written(&[&long]).is_err());
}
