use super::*;

/// A peer's bad message is not an owner panic: its log is apart from the
/// panic log the app forwards to Sentry as `RustPanic`.
#[test]
fn bad_messages_have_their_own_log_beside_the_panics() {
    let (Some(panics), Some(bad), Some(hangs)) = (
        owner_panic_log_path("main"),
        owner_bad_message_log_path("main"),
        owner_hang_log_path("main"),
    ) else {
        panic!("the test host has a state directory");
    };
    assert_eq!(
        bad.file_name().and_then(|name| name.to_str()),
        Some("owner-bad-messages-main.jsonl")
    );
    assert_ne!(bad, panics);
    assert_ne!(bad, hangs);
    assert_eq!(bad.parent(), panics.parent());
}
