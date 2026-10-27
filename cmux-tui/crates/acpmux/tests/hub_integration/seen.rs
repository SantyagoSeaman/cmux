//! cx-n8ou: acpmux owns what the person has seen. A client says whether it
//! shows a chat (`_acpmux/attach` with `visible: false`, then `_acpmux/view`);
//! a turn that ends while no attached client shows the chat leaves it
//! `unread`, and showing it is seeing it. A client that never says counts
//! as showing every chat it attaches (the TUI, `acpmux attach`).

use super::*;
#[cfg(unix)]
use acpmux::hub::freeze::ProcessControl;
#[cfg(unix)]
use std::sync::{Mutex, Weak};

/// The chat's `unread` once its turn has settled (the prompt's answer can
/// come before the turn records its end).
async fn unread(hub: &Hub, id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let summary = hub.session_summary(&hub.resolve(id).unwrap());
        if summary["status"] != "running" {
            return summary["unread"].clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "never settled: {summary}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_turn_ending_in_a_hidden_tab_stays_unread_until_shown() {
    let (hub, mut c) = setup(PermissionPolicy::ApproveAll).await;
    let id = new_session(&mut c, "background").await;
    c.request("_acpmux/view", json!({"sessionId": id, "visible": false})).await.unwrap();
    c.request(method::SESSION_PROMPT, prompt(&id, "hello", None)).await.unwrap();
    assert_eq!(unread(&hub, &id).await, true);
    c.request("_acpmux/view", json!({"sessionId": id, "visible": true})).await.unwrap();
    assert_eq!(unread(&hub, &id).await, false);
    // Shown while it ends: seen.
    c.request(method::SESSION_PROMPT, prompt(&id, "again", None)).await.unwrap();
    assert_eq!(unread(&hub, &id).await, false);
}

#[tokio::test]
async fn a_hidden_attach_does_not_mark_a_chat_seen() {
    let (hub, mut c) = setup(PermissionPolicy::ApproveAll).await;
    let id = new_session(&mut c, "unseen").await;
    c.request("_acpmux/view", json!({"sessionId": id, "visible": false})).await.unwrap();
    c.request(method::SESSION_PROMPT, prompt(&id, "hello", None)).await.unwrap();
    c.request(method::MUX_DETACH, json!({"sessionId": id})).await.unwrap();
    assert_eq!(unread(&hub, &id).await, true);
    let mut tab = connect(&hub).await;
    tab.request(method::MUX_ATTACH, json!({"sessionId": id, "limit": 0, "visible": false}))
        .await
        .unwrap();
    assert_eq!(unread(&hub, &id).await, true);
    assert_eq!(hub.session_summary(&hub.resolve(&id).unwrap())["attached"], 1);
    tab.request("_acpmux/view", json!({"sessionId": id, "visible": true})).await.unwrap();
    assert_eq!(unread(&hub, &id).await, false);
}

#[tokio::test]
async fn one_client_showing_a_chat_is_enough() {
    let (hub, mut c) = setup(PermissionPolicy::ApproveAll).await;
    let id = new_session(&mut c, "shared").await;
    let mut tab = connect(&hub).await;
    tab.request(method::MUX_ATTACH, json!({"sessionId": id, "limit": 0, "visible": false}))
        .await
        .unwrap();
    c.request(method::SESSION_PROMPT, prompt(&id, "hello", None)).await.unwrap();
    assert_eq!(unread(&hub, &id).await, false);
    // The showing client hides it: the next turn's end is unseen.
    c.request("_acpmux/view", json!({"sessionId": id, "visible": false})).await.unwrap();
    c.request(method::SESSION_PROMPT, prompt(&id, "again", None)).await.unwrap();
    assert_eq!(unread(&hub, &id).await, true);
}

/// Mark Unread (cx-ncc.78, the Codex app's Mark unread): the person marks a chat they have seen;
/// it stays unread while a client still shows it, and showing it again after hiding it is seeing it.
#[tokio::test]
async fn a_chat_marked_unread_stays_unread_until_shown_again() {
    let (hub, mut c) = setup(PermissionPolicy::ApproveAll).await;
    let id = new_session(&mut c, "read").await;
    c.request(method::SESSION_PROMPT, prompt(&id, "hello", None)).await.unwrap();
    assert_eq!(unread(&hub, &id).await, false);
    c.request("_acpmux/markUnread", json!({"sessionId": id})).await.unwrap();
    assert_eq!(unread(&hub, &id).await, true);
    c.request("_acpmux/view", json!({"sessionId": id, "visible": true})).await.unwrap();
    assert_eq!(unread(&hub, &id).await, true);
    c.request("_acpmux/view", json!({"sessionId": id, "visible": false})).await.unwrap();
    c.request("_acpmux/view", json!({"sessionId": id, "visible": true})).await.unwrap();
    assert_eq!(unread(&hub, &id).await, false);
    assert!(c.request("_acpmux/markUnread", json!({"sessionId": "no-such-chat"})).await.is_err());
}

// p14-energy-117 end to end: the pane hides its chat (`_acpmux/visibility`),
// the chat's idle harness stops after `freezeAfterSeconds`, and a prompt
// continues it before a single byte is sent to it. The process control
// records instead of signalling, with the count of records sent so far.

#[cfg(unix)]
type Log = Arc<Mutex<Vec<(&'static str, usize)>>>;

#[cfg(unix)]
struct Recorder {
    hub: Weak<Hub>,
    session: String,
    log: Log,
}

#[cfg(unix)]
impl Recorder {
    fn note(&self, what: &'static str) {
        let sent = self.hub.upgrade().map_or(0, |hub| {
            let events = hub.events(&self.session, 0, 100_000).unwrap();
            events.iter().filter(|e| e.dir == "out").count()
        });
        self.log.lock().unwrap().push((what, sent));
    }
}

#[cfg(unix)]
impl ProcessControl for Recorder {
    fn stop(&self, _group: u32) -> bool {
        self.note("stop");
        true
    }

    fn resume(&self, _group: u32) {
        self.note("cont");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn freeze_a_hidden_pane_stops_the_idle_harness_and_a_prompt_continues_it_first() {
    let (hub, mut c) = setup(PermissionPolicy::ApproveAll).await;
    let clock = acpmux::clock::ManualClock::new();
    hub.set_clock(clock.clone());
    let id = new_session(&mut c, "hidden-pane").await;
    let log: Log = Default::default();
    let recorder = Recorder { hub: Arc::downgrade(&hub), session: id.clone(), log: log.clone() };
    hub.set_process_control(Arc::new(recorder));
    hub.set_harness_freeze(true);
    c.request(method::MUX_VISIBILITY, json!({"sessionId": id, "visible": false})).await.unwrap();
    let frozen = |hub: &Hub| hub.session_summary(&hub.resolve(&id).unwrap())["frozen"].clone();
    clock.advance(Duration::from_secs(299));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(log.lock().unwrap().is_empty(), "stopped before its quiet period ended");
    clock.advance(Duration::from_secs(2));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while frozen(&hub) != true {
        assert!(std::time::Instant::now() < deadline, "the hidden harness never stopped");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(c.request(method::MUX_STATUS, json!({})).await.unwrap()["frozen"], 1);
    let r = c.request(method::SESSION_PROMPT, prompt(&id, "wake up", None)).await.unwrap();
    assert_eq!(r["stopReason"], "end_turn");
    let log = log.lock().unwrap().clone();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!((log[0].0, log[1].0), ("stop", "cont"));
    assert_eq!(log[1].1, log[0].1, "bytes went to the stopped harness before it continued");
    assert_eq!(frozen(&hub), false);
}

#[tokio::test]
async fn freeze_visibility_needs_an_attachment_and_combines_with_the_page_view() {
    let (hub, mut c) = setup(PermissionPolicy::ApproveAll).await;
    let id = new_session(&mut c, "pane").await;
    let shown = |hub: &Hub| hub.session_summary(&hub.resolve(&id).unwrap())["shown"].clone();
    c.request(method::MUX_VISIBILITY, json!({"sessionId": id, "visible": false})).await.unwrap();
    assert_eq!(shown(&hub), false);
    // The page shows the chat; the pane still hides it.
    c.request(method::MUX_VIEW, json!({"sessionId": id, "visible": true})).await.unwrap();
    assert_eq!(shown(&hub), false);
    // Attaching again keeps what the pane said.
    c.request(method::MUX_ATTACH, json!({"sessionId": id, "limit": 1})).await.unwrap();
    assert_eq!(shown(&hub), false);
    c.request(method::MUX_VISIBILITY, json!({"sessionId": id, "visible": true})).await.unwrap();
    assert_eq!(shown(&hub), true);
    c.request(method::MUX_DETACH, json!({"sessionId": id})).await.unwrap();
    let error = c
        .request(method::MUX_VISIBILITY, json!({"sessionId": id, "visible": false}))
        .await
        .unwrap_err();
    assert!(error.contains("attach to the session before _acpmux/visibility"), "{error}");
}
