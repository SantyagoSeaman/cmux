//! The hidden idle harness freeze (`cargo test -p acpmux freeze`), on a real
//! hub with a manual clock and a recording process control: a stand-in
//! harness (`cat`) is never signalled; the recorder and the harness's write
//! tap share one log, so the order of stop, continue and bytes is checked.

use super::*;
use crate::agent::Direction;
use crate::clock::ManualClock;

type Log = Arc<StdMutex<Vec<String>>>;

/// Records stops and continues in the log the write tap also writes.
struct Recorder(Log);

impl ProcessControl for Recorder {
    fn stop(&self, _group: u32) -> bool {
        self.0.lock().unwrap().push("stop".into());
        true
    }

    fn resume(&self, _group: u32) {
        self.0.lock().unwrap().push("cont".into());
    }
}

struct Fixture {
    hub: Arc<Hub>,
    clock: Arc<ManualClock>,
    log: Log,
    session: Arc<Session>,
    child: Arc<ChildAgent>,
    _inbound: mpsc::Receiver<crate::agent::Inbound>,
}

impl Fixture {
    /// A ready chat whose harness runs, attached by one client that hides
    /// it, with the freeze on.
    async fn hidden() -> Self {
        let mut cfg = crate::config::Config::default();
        cfg.store.mode = crate::config::StoreMode::Memory;
        let store = crate::store::open(&cfg.store, std::path::Path::new("/nonexistent")).unwrap();
        let hub = Hub::new(cfg, store);
        let clock = ManualClock::new();
        hub.set_clock(clock.clone());
        let log: Log = Default::default();
        hub.set_process_control(Arc::new(Recorder(log.clone())));
        hub.set_harness_freeze(true);
        let meta: SessionMeta = serde_json::from_value(json!({
            "schema": "acpmux.session.v1", "id": "chat", "name": "chat", "harness": "stand-in",
            "cwd": "/tmp", "status": "ready", "createdAt": 1, "updatedAt": 2,
        }))
        .unwrap();
        let session = hub.make_session(meta);
        hub.sessions.lock().unwrap().insert("chat".into(), session.clone());
        let profile: HarnessProfile = serde_json::from_value(json!({"argv": ["cat"]})).unwrap();
        let (inbound_tx, inbound) = mpsc::channel(64);
        let tap_log = log.clone();
        let tap: crate::agent::Tap = Arc::new(move |direction, _msg, _| {
            if direction == Direction::Out {
                tap_log.lock().unwrap().push("bytes".into());
            }
            None
        });
        let child =
            ChildAgent::spawn("stand-in", &profile, std::path::Path::new("/tmp"), inbound_tx, tap)
                .await
                .unwrap();
        *session.child.lock().await = Some(child.clone());
        hub.attach_count(&session, 1, false);
        hub.wake_idle_reaper();
        settle().await;
        Self { hub, clock, log, session, child, _inbound: inbound }
    }

    async fn advance(&self, secs: u64) {
        settle().await;
        self.clock.advance(Duration::from_secs(secs));
        settle().await;
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn stops(&self) -> usize {
        self.log().iter().filter(|e| *e == "stop").count()
    }

    async fn end(self) {
        self.child.kill().await;
    }
}

async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn freeze_a_hidden_idle_harness_stops_once_after_300_s() {
    let f = Fixture::hidden().await;
    f.advance(299).await;
    assert_eq!(f.stops(), 0, "stopped before its quiet period ended: {:?}", f.log());
    f.advance(2).await;
    assert_eq!(f.log(), ["stop"]);
    assert!(f.child.gate.is_frozen());
    assert_eq!(f.hub.session_summary(&f.session)["frozen"], true);
    assert_eq!(f.hub.frozen_count(), 1);
    // A stopped harness needs no timer and is never stopped twice.
    f.advance(3600).await;
    assert_eq!(f.log(), ["stop"]);
    f.end().await;
}

#[tokio::test]
async fn freeze_a_prompt_continues_the_harness_before_its_bytes() {
    let f = Fixture::hidden().await;
    f.advance(301).await;
    assert_eq!(f.log(), ["stop"]);
    f.child.notify("session/prompt", json!({"sessionId": "chat"})).await.unwrap();
    assert_eq!(f.log(), ["stop", "cont", "bytes"]);
    assert!(!f.child.gate.is_frozen());
    assert_eq!(f.hub.session_summary(&f.session)["frozen"], false);
    f.end().await;
}

#[tokio::test]
async fn freeze_showing_the_chat_continues_it_on_the_same_turn() {
    let f = Fixture::hidden().await;
    f.advance(301).await;
    assert_eq!(f.log(), ["stop"]);
    // No await between the show and the check.
    f.hub.set_visible(&f.session, true);
    assert_eq!(f.log(), ["stop", "cont"]);
    assert_eq!(f.hub.frozen_count(), 0);
    // Shown: no timer stops it again.
    f.advance(3600).await;
    assert_eq!(f.stops(), 1);
    f.end().await;
}

#[tokio::test]
async fn freeze_a_running_background_task_blocks_it() {
    let f = Fixture::hidden().await;
    let update = json!({"params": {"update": {
        "toolCallId": "tool-1", "taskId": "task-1", "state": "running", "command": "npm run dev",
    }}});
    let rec = EventRecord {
        seq: 1,
        at: 1,
        dir: "in".into(),
        kind: "background_task_update".into(),
        msg: update,
        host_seq: None,
    };
    f.hub.note_process(&f.session, &rec, None);
    f.advance(301).await;
    f.advance(3600).await;
    assert_eq!(f.stops(), 0, "{:?}", f.log());
    f.end().await;
}

#[tokio::test]
async fn freeze_a_turn_a_queue_or_a_permission_blocks_it() {
    let f = Fixture::hidden().await;
    *f.session.turn.lock().unwrap() =
        Some(TurnInfo::prompted("test", "build it", "t1", "p1", Control::Local));
    f.advance(301).await;
    assert_eq!(f.stops(), 0, "stopped during a turn");
    *f.session.turn.lock().unwrap() = None;
    f.session.queued.store(1, Ordering::SeqCst);
    f.advance(301).await;
    assert_eq!(f.stops(), 0, "stopped with a queued prompt");
    f.session.queued.store(0, Ordering::SeqCst);
    let (reply, _answer) = tokio::sync::oneshot::channel();
    let pending = PendingPermission {
        request: json!({"toolCall": {"title": "rm -rf build"}}),
        reply,
        decision_nonce: "n1".into(),
        ancestor_may_answer: false,
        prev_status: SessionStatus::Ready,
    };
    f.session.permissions.lock().unwrap().pending.insert("r1".into(), pending);
    f.advance(301).await;
    assert_eq!(f.stops(), 0, "stopped with a permission pending");
    f.session.permissions.lock().unwrap().pending.clear();
    // Once nothing holds it, a full quiet period later.
    f.advance(301).await;
    assert_eq!(f.stops(), 1);
    f.end().await;
}

#[tokio::test]
async fn freeze_a_shown_or_detached_chat_is_never_stopped() {
    let f = Fixture::hidden().await;
    f.hub.set_visible(&f.session, true);
    f.advance(3600).await;
    assert_eq!(f.stops(), 0, "a shown chat was stopped");
    f.hub.set_idle_child(None);
    f.hub.set_visible(&f.session, false);
    f.hub.attach_count(&f.session, -1, false);
    f.advance(3600).await;
    assert_eq!(f.stops(), 0, "a chat nobody attaches was stopped (the idle exit owns it)");
    f.end().await;
}

#[tokio::test]
async fn freeze_waits_30_s_under_energy_saver() {
    let f = Fixture::hidden().await;
    f.hub.maintenance.energy().set_saver(true);
    f.advance(29).await;
    assert_eq!(f.stops(), 0);
    f.advance(2).await;
    assert_eq!(f.log(), ["stop"]);
    f.end().await;
}

#[tokio::test]
async fn freeze_is_off_without_the_feature() {
    let f = Fixture::hidden().await;
    f.hub.set_harness_freeze(false);
    f.advance(3600).await;
    assert_eq!(f.stops(), 0);
    f.end().await;
}

#[tokio::test]
async fn freeze_ending_a_stopped_harness_continues_it_first() {
    let f = Fixture::hidden().await;
    f.advance(301).await;
    assert_eq!(f.log(), ["stop"]);
    f.child.terminate(Duration::from_millis(200)).await;
    assert_eq!(f.log(), ["stop", "cont"]);
    f.end().await;
}

#[test]
fn freeze_gate_refuses_while_a_write_runs_or_after_one() {
    let log: Log = Default::default();
    let control: Arc<dyn ProcessControl> = Arc::new(Recorder(log.clone()));
    let gate = HarnessGate::default();
    {
        let _writing = gate.writing();
        assert!(!gate.freeze(control.clone(), 42), "stopped during a write");
    }
    // That write came after the last quiet mark: one refusal marks it quiet.
    assert!(gate.freeze(control.clone(), 42));
    assert!(gate.thaw());
    drop(gate.writing());
    assert!(!gate.freeze(control.clone(), 42), "a write since the mark");
    gate.mark_quiet();
    assert!(gate.freeze(control.clone(), 42));
    assert!(gate.freeze(control, 42), "a second freeze stops nothing more");
    assert_eq!(*log.lock().unwrap(), ["stop", "cont", "stop"]);
}

#[test]
fn freeze_after_follows_the_config_and_the_saver() {
    assert_eq!(freeze_after(None, false), Some(Duration::from_secs(300)));
    assert_eq!(freeze_after(None, true), Some(Duration::from_secs(30)));
    assert_eq!(freeze_after(Some(10), true), Some(Duration::from_secs(10)));
    assert_eq!(freeze_after(Some(0), false), None);
}

#[test]
fn freeze_energy_saver_parks_the_pool_and_starts_no_entry() {
    let pool = crate::config::PoolConfig::default();
    assert!(!pool.parks(false) && pool.starts(false), "plugged in: the defaults");
    assert!(pool.parks(true), "the saver parks every ready entry");
    assert!(!pool.starts(true), "the saver starts no new entry");
    let parked = crate::config::PoolConfig { park: true, ..Default::default() };
    assert!(parked.parks(false));
}
