use std::sync::{Arc, Mutex};

use super::*;

fn recording() -> (BadMessages, Arc<Mutex<Vec<serde_json::Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink_seen = seen.clone();
    let ledger = BadMessages::with_sink(Box::new(move |line| sink_seen.lock().unwrap().push(line)));
    (ledger, seen)
}

#[test]
fn bad_message_reports_once_per_reason_and_counts_every_one() {
    let (ledger, seen) = recording();
    assert_eq!(ledger.report_bad_message("client 1 unix", "bad_magic", &"0x41"), Reported::First);
    assert_eq!(ledger.report_bad_message("client 2 unix", "bad_magic", &"0x42"), Reported::Repeat);
    assert_eq!(ledger.report_bad_message("client 3 ws", "invalid_utf8", &"at 4"), Reported::First);
    assert_eq!(ledger.count("bad_magic"), 2);
    assert_eq!(ledger.count("invalid_utf8"), 1);
    assert_eq!(ledger.count("unknown_kind"), 0);
    assert_eq!(ledger.counts(), vec![("bad_magic", 2), ("invalid_utf8", 1)]);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one record per reason");
    assert_eq!(seen[0]["kind"], RECORD_KIND);
    assert_eq!(seen[0]["peer"], "client 1 unix");
    assert_eq!(seen[0]["reason"], "bad_magic");
    assert_eq!(seen[0]["message"], "bad_message bad_magic from client 1 unix: 0x41");
    assert_eq!(seen[0]["test"], false);
    assert!(seen[0].get("backtrace").is_none());
    assert_eq!(seen[1]["reason"], "invalid_utf8");
}

#[test]
fn bad_message_ledgers_are_independent() {
    let (first, first_seen) = recording();
    let (second, second_seen) = recording();
    first.report_bad_message("client 1 unix", "oversize", &"");
    assert_eq!(second.report_bad_message("client 1 unix", "oversize", &""), Reported::First);
    assert_eq!(first_seen.lock().unwrap().len(), 1);
    assert_eq!(second_seen.lock().unwrap().len(), 1);
}

#[test]
fn bad_message_reasons_past_the_table_count_as_other() {
    let (ledger, seen) = recording();
    let reasons: Vec<&'static str> =
        (0..=MAX_REASONS).map(|n| &*Box::leak(format!("reason_{n}").into_boxed_str())).collect();
    for reason in &reasons {
        ledger.report_bad_message("client 1 unix", reason, &"");
    }
    ledger.report_bad_message("client 2 unix", reasons[MAX_REASONS], &"");
    assert_eq!(ledger.count(reasons[0]), 1);
    assert_eq!(ledger.count(reasons[MAX_REASONS]), 0, "it has no slot of its own");
    assert_eq!(ledger.count(OVERFLOW_REASON), 2);
    let counts = ledger.counts();
    assert_eq!(counts.len(), MAX_REASONS + 1);
    assert_eq!(counts.last(), Some(&(OVERFLOW_REASON, 2)));
    // MAX_REASONS named reasons plus the overflow's first.
    assert_eq!(seen.lock().unwrap().len(), MAX_REASONS + 1);
}

#[test]
fn bad_message_concurrent_reports_claim_one_slot_per_reason() {
    let (ledger, seen) = recording();
    let ledger = Arc::new(ledger);
    let threads: Vec<_> = (0..8)
        .map(|n| {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    let reason = if n % 2 == 0 { "truncated" } else { "unknown_flags" };
                    ledger.report_bad_message("client 1 unix", reason, &"");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(ledger.count("truncated"), 400);
    assert_eq!(ledger.count("unknown_flags"), 400);
    assert_eq!(ledger.counts().len(), 2);
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn bad_message_file_sink_adds_the_process_fields() {
    let dir = std::env::temp_dir().join(format!("cmux-bad-message-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("panics.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut fields = crate::Fields::new();
    fields.insert("session".into(), "s".into());
    let ledger = BadMessages::with_sink(file_sink(path.clone(), fields));
    ledger.report_bad_message("client 9 unix", "unexpected_fds", &"a file descriptor arrived");
    ledger.report_bad_message("client 9 unix", "unexpected_fds", &"a file descriptor arrived");
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> =
        text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["session"], "s");
    assert_eq!(lines[0]["kind"], RECORD_KIND);
    assert_eq!(lines[0]["reason"], "unexpected_fds");
    let _ = std::fs::remove_dir_all(&dir);
}
