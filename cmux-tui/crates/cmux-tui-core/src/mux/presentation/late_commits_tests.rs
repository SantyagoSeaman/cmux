//! p05-ipc-135: a presentation write whose receipt is indeterminate fails its
//! request, and the writer commits it later. The mux installs what committed:
//! the next presentation writer or state holder reloads the snapshot, and a
//! keyed frontend browser retry resumes as a frontend-rendered tab.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Weak};

use cmux_threads::sync::mpsc::{SyncSender, sync_channel};

use super::super::tests::PresentationTestSession;
use super::*;
use crate::journal_ingress::{EffectSend, PendingRegistryReceipt};
use crate::workspace_registry::{
    RegistryConnection, RegistryIntent, RegistryIntentSink, RegistryReceipt,
};

type HeldIntent = (RegistryIntent, SyncSender<Result<RegistryReceipt, String>>);

/// A journal writer that, once armed, takes one intent, answers
/// indeterminate, and commits it only when the test says so. Unarmed, it
/// takes nothing and the registry commits locally.
struct LateWriter {
    connection: Weak<RegistryConnection>,
    armed: AtomicBool,
    held: Mutex<Option<HeldIntent>>,
}

impl LateWriter {
    fn install(registry: &WorkspaceRegistry) -> Arc<Self> {
        let writer = Arc::new(Self {
            connection: Arc::downgrade(&registry.connection),
            armed: AtomicBool::new(false),
            held: Mutex::new(None),
        });
        registry.connection.install_intent_sink(writer.clone());
        writer
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Commit the held intent in its own transaction, as the writer's
    /// late batch would, and answer its receipt.
    fn commit_late(&self) {
        let (intent, answer) = self.held.lock().unwrap().take().expect("a held intent");
        let connection = self.connection.upgrade().expect("registry connection");
        let receipt = {
            let db = connection.get();
            let tx = db.unchecked_transaction().unwrap();
            let receipt = intent.apply_in_savepoint(&tx).unwrap();
            tx.commit().unwrap();
            receipt
        };
        assert!(receipt.is_ok(), "the late commit applies: {receipt:?}");
        answer.send(receipt).unwrap();
    }
}

impl RegistryIntentSink for LateWriter {
    fn accepts(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    fn send(
        &self,
        intent: RegistryIntent,
        _site: &'static std::panic::Location<'static>,
    ) -> EffectSend {
        self.armed.store(false, Ordering::SeqCst);
        let (answer, receipt) = sync_channel(1);
        *self.held.lock().unwrap() = Some((intent, answer));
        EffectSend::Indeterminate(
            anyhow::anyhow!("the session journal commit is indeterminate"),
            PendingRegistryReceipt::for_test(receipt),
        )
    }
}

fn record(url: &str) -> FrontendBrowserRecord {
    FrontendBrowserRecord {
        engine: "webkit".into(),
        url: url.into(),
        title: Some("Page".into()),
        favicon_url: None,
        profile_id: None,
        owner: None,
    }
}

fn open(label: &'static str) -> (PresentationTestSession, Arc<Mux>, Arc<LateWriter>, PaneId) {
    let session = PresentationTestSession::new(label);
    let (mux, writer) = session.open_with(LateWriter::install);
    let terminal = mux.new_workspace(None, None).unwrap().id;
    let pane = mux.with_state(|state| state.pane_of(terminal)).unwrap();
    (session, mux, writer, pane)
}

fn group_ids(mux: &Mux) -> Vec<String> {
    mux.workspace_groups().into_iter().map(|group| group.id).collect()
}

/// The keyed registration commits after its request failed; the retry
/// resumes under the recorded browser id and the tab is frontend-rendered,
/// never a CDP browser.
#[test]
fn a_keyed_frontend_browser_registered_late_resumes_as_a_frontend_tab() {
    let (_session, mux, writer, pane) = open("late-keyed-frontend-browser");
    let create = || {
        mux.new_frontend_browser_tab_keyed_as(
            &Actor::Daemon,
            Some(pane),
            record("https://late.test/"),
            None,
            "late-key",
            FrontendTabPlacement::default(),
        )
    };
    writer.arm();
    assert!(create().is_err(), "the indeterminate registration fails its request");
    writer.commit_late();
    assert!(mux.has_frontend_browser_key("late-key").unwrap());

    let outcome = create().unwrap();
    assert!(!outcome.replayed);
    assert!(mux.is_frontend_browser_surface(&outcome.surface), "a CDP tab would get a target");
    let stored = mux.workspace_registry.lock().unwrap().presentation_snapshot().unwrap();
    assert_eq!(stored.frontend_browsers.len(), 1, "the retry reused the late browser id");
    assert_eq!(mux.presentation_snapshot().frontend_browsers, stored.frontend_browsers);
}

/// A group created late reaches the snapshot at the next presentation
/// write, with a `TreeChanged` for tree subscribers.
#[test]
fn a_late_workspace_group_reaches_the_snapshot_at_the_next_presentation_write() {
    let (_session, mux, writer, pane) = open("late-group-presentation");
    let browser =
        mux.new_frontend_browser_tab(Some(pane), record("https://a.test/"), None).unwrap();
    writer.arm();
    let created = mux.create_workspace_group(Some("late".into()), "Late".into(), None, false, None);
    assert!(created.is_err());
    writer.commit_late();
    assert!(group_ids(&mux).is_empty(), "nothing installed it yet");

    let events = mux.subscribe();
    mux.set_frontend_browser_history(browser.id, Some("{}".into())).unwrap();
    assert_eq!(group_ids(&mux), ["late"]);
    assert!(events.try_iter().any(|event| matches!(event, MuxEvent::TreeChanged)));
}

/// A group created late reaches the snapshot at the next state holder (a
/// topology commit pins state and settles every receipt).
#[test]
fn a_late_workspace_group_reaches_the_snapshot_at_the_next_topology_commit() {
    let (_session, mux, writer, _pane) = open("late-group-topology");
    writer.arm();
    let created = mux.create_workspace_group(Some("late".into()), "Late".into(), None, false, None);
    assert!(created.is_err());
    writer.commit_late();
    mux.new_workspace(None, None).unwrap();
    assert_eq!(group_ids(&mux), ["late"]);
    let stored = mux.workspace_registry.lock().unwrap().presentation_snapshot().unwrap();
    assert_eq!(mux.presentation_snapshot().groups, stored.groups);
}
