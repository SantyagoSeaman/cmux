use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use super::*;
use crate::Mux;
use crate::journal_ingress::{
    JournalIngressEvent, JournalIngressSender, TERMINAL_OUTPUT_INGRESS_BYTES,
};
use crate::resource::{
    ContentPublicId, PanePublicId, ScreenPublicId, SplitPublicId, TabPublicId, TerminalPublicId,
    WorkspacePublicId,
};
use crate::workspace_registry::{
    RegistryIntent, RegistryLayoutNode, RegistryPane, RegistryScreen, RegistryTab,
    RegistryTerminal, RegistryViewport, RegistryWorkspace, ResourceChange, ResourcePatch,
    TerminalLifecycle, TerminalOnExit, WorkspaceMutation, WorkspaceRegistry,
};

fn terminal_id(value: u128) -> Arc<TerminalPublicId> {
    Arc::new(TerminalPublicId::parse(format!("term_{value:032x}")).unwrap())
}

fn producer_event(key: &str) -> QueuedJournalEvent {
    let ingress = crate::JournalIngress {
        producer_id: "lane_order_probe".into(),
        manifest_version: 1,
        kind: "lane_order.probe".into(),
        schema_version: 1,
        occurred_at_ms: None,
        subjects: Vec::new(),
        sensitivity: None,
        payload: serde_json::json!({"key": key}),
        causation_id: None,
        correlation_id: None,
    };
    QueuedJournalEvent {
        event: JournalIngressEvent::Producer {
            ingress,
            validated: crate::journal_kernel::ValidatedJournalIngress {
                class: crate::JournalClass::Observation,
                replay: crate::JournalReplayPolicy::Advisory,
                sensitivity: crate::JournalSensitivity::Metadata,
            },
            origin: "lane_order_probe".into(),
            idempotency_key: key.into(),
        },
        completion: None,
    }
}

fn queue_durable(sender: &JournalIngressSender, queued: QueuedJournalEvent) {
    sender.enqueue(sender.durable_sender.as_ref().unwrap(), queued).unwrap();
}

#[test]
fn an_intent_queued_behind_256_kib_terminal_batches_commits_first() {
    let (sender, receivers) = JournalIngressSender::new(true);
    let receivers = receivers.unwrap();
    // 1 MiB of one terminal's output: sixteen 64 KiB chunks that fold into
    // four 256 KiB records, all queued before the intent.
    sender.send(JournalIngressEvent::TerminalOutput {
        terminal_id: terminal_id(1),
        generation: Arc::from("lane-order-generation"),
        occurred_at_ms: 1,
        bytes: vec![b'x'; 16 * TERMINAL_OUTPUT_INGRESS_BYTES],
    });
    queue_durable(&sender, producer_event("behind_output"));

    let round = receive_round(&receivers).unwrap();
    assert_eq!(round.len(), 2, "the intent and the output are separate transactions");
    assert!(
        matches!(
            &round[0][..],
            [QueuedJournalEvent { event: JournalIngressEvent::Producer { .. }, .. }]
        ),
        "the intent commits first, alone"
    );
    let output_records = round[1]
        .iter()
        .map(|queued| match &queued.event {
            JournalIngressEvent::TerminalOutput { bytes, .. } => bytes.len(),
            _ => panic!("only terminal output follows the intent"),
        })
        .collect::<Vec<_>>();
    assert_eq!(output_records, [256 * 1024; 4]);
}

#[test]
fn a_small_terminal_batch_shares_the_intent_transaction_before_it() {
    let (sender, receivers) = JournalIngressSender::new(true);
    let receivers = receivers.unwrap();
    sender.send(JournalIngressEvent::TerminalResize {
        terminal_id: terminal_id(2),
        generation: Arc::from("lane-order-resize"),
        occurred_at_ms: 1,
        cols: 80,
        rows: 24,
        cell_width: 8,
        cell_height: 16,
    });
    queue_durable(&sender, producer_event("with_resize"));

    let round = receive_round(&receivers).unwrap();
    assert_eq!(round.len(), 1, "one fsync for the intent and a resize");
    assert!(matches!(
        &round[0][..],
        [
            QueuedJournalEvent { event: JournalIngressEvent::TerminalResize { .. }, .. },
            QueuedJournalEvent { event: JournalIngressEvent::Producer { .. }, .. },
        ]
    ));
}

#[test]
fn a_resize_sharing_a_tab_move_transaction_sequences_with_the_topology_it_names() {
    let mut registry = WorkspaceRegistry::in_memory("lane-order-move").unwrap();
    let topology = MoveTopology::new();
    registry
        .commit_resource_patch(
            &WorkspaceMutation::daemon("lane-order-move-seed", "test").unwrap(),
            "workspace.create",
            &serde_json::json!({"operation":"workspace.create"}),
            None,
            Some(0),
            &topology.seed_patch(),
            &serde_json::json!({"workspace_id":topology.workspace.public_id}),
            &serde_json::json!([{"kind":"workspace.created"}]),
        )
        .unwrap();
    let seeded_records = registry.session_journal_after(0, 256).unwrap().records.len();

    // A tab move intent and the moved PTY's resize land in one writer round.
    let key = "lane-order-move-key";
    let fingerprint = serde_json::json!({"move":"tab"});
    registry
        .prepare_resource_effect(key, "tab.move", &fingerprint, &serde_json::json!({}), None, None)
        .unwrap();
    registry.mark_resource_effect_executing(key, "tab.move", &fingerprint).unwrap();
    let (intent, _finish) = registry
        .prepare_effect_patch_intent(
            key,
            "tab.move",
            &fingerprint,
            &topology.move_patch(),
            &serde_json::json!({"tab_id":topology.tab}),
            &serde_json::json!([{"kind":"upsert","resource":"tab"}]),
            false,
        )
        .unwrap();
    let (sender, receivers) = JournalIngressSender::new(true);
    let receivers = receivers.unwrap();
    queue_durable(
        &sender,
        QueuedJournalEvent {
            event: JournalIngressEvent::Effect(Box::new(RegistryIntent::Effect(intent))),
            completion: None,
        },
    );
    sender.send(JournalIngressEvent::TerminalResize {
        terminal_id: Arc::new(topology.terminal.clone()),
        generation: Arc::from("lane-order-move"),
        occurred_at_ms: 1,
        cols: 100,
        rows: 30,
        cell_width: 8,
        cell_height: 16,
    });
    let round = receive_round(&receivers).unwrap();
    assert_eq!(round.len(), 1, "the resize rides along in the intent's transaction");
    let events = round[0].iter().map(|queued| &queued.event).collect::<Vec<_>>();
    registry.append_journal_ingress_events(&events).unwrap();

    let batch_records =
        registry.session_journal_after(0, 256).unwrap().records.split_off(seeded_records);
    let (resizes, move_records): (Vec<_>, Vec<_>) =
        batch_records.into_iter().partition(|record| record.kind == "terminal.resized");
    let [resize] = &resizes[..] else { panic!("one resize record: {resizes:?}") };
    assert!(!move_records.is_empty(), "the move commits journal records");
    assert!(
        move_records.iter().all(|record| record.sequence > resize.sequence),
        "the resize names the pre-move topology, so it sequences before the move: resize {}, \
         move {:?}",
        resize.sequence,
        move_records.iter().map(|record| (record.sequence, &record.kind)).collect::<Vec<_>>()
    );
    let subject_ids = |kind: &str| {
        resize
            .subjects
            .iter()
            .filter(|subject| subject.kind == kind)
            .map(|subject| subject.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(subject_ids("pane"), [topology.source_pane.to_string()]);
    assert_eq!(subject_ids("tab"), [topology.tab.to_string()]);
    assert_eq!(subject_ids("screen"), [topology.screen.to_string()]);
    assert_eq!(subject_ids("workspace"), [topology.workspace.public_id.to_string()]);
    let moved = registry.resource_topology_snapshot().unwrap();
    assert!(
        moved
            .tabs
            .iter()
            .any(|tab| tab.public_id == topology.tab && tab.pane_id == topology.target_pane),
        "the move applied in the same transaction"
    );
}

/// One workspace whose screen splits two panes; the terminal's tab starts
/// in `source_pane` and moves to the empty `target_pane`.
struct MoveTopology {
    workspace: RegistryWorkspace,
    screen: ScreenPublicId,
    source_pane: PanePublicId,
    target_pane: PanePublicId,
    tab: TabPublicId,
    terminal: TerminalPublicId,
}

impl MoveTopology {
    const LEGACY_TERMINAL_ID: &str = "00000000000040008000000000000001";

    fn new() -> Self {
        Self {
            workspace: RegistryWorkspace {
                id: 1,
                public_id: WorkspacePublicId::parse(format!("ws_{:032x}", 1)).unwrap(),
                key: "one".into(),
                name: "One".into(),
                name_source: Default::default(),
                name_revision: 0,
                group_key: "default".into(),
            },
            screen: ScreenPublicId::parse(format!("screen_{:032x}", 1)).unwrap(),
            source_pane: PanePublicId::parse(format!("pane_{:032x}", 1)).unwrap(),
            target_pane: PanePublicId::parse(format!("pane_{:032x}", 2)).unwrap(),
            tab: TabPublicId::parse(format!("tab_{:032x}", 1)).unwrap(),
            terminal: TerminalPublicId::parse(format!("term_{:032x}", 1)).unwrap(),
        }
    }

    fn pane(&self, public_id: &PanePublicId, active_tab: Option<&TabPublicId>) -> ResourceChange {
        ResourceChange::UpsertPane(RegistryPane {
            public_id: public_id.clone(),
            screen_id: self.screen.clone(),
            name: None,
            active_tab: active_tab.cloned(),
            creation_ordinal: if *public_id == self.source_pane { 1 } else { 2 },
        })
    }

    fn tab_in(&self, pane_id: &PanePublicId) -> ResourceChange {
        ResourceChange::UpsertTab(RegistryTab {
            public_id: self.tab.clone(),
            pane_id: pane_id.clone(),
            position: 0,
            content_id: ContentPublicId::Terminal(self.terminal.clone()),
            name: Some("zsh".into()),
            name_source: Default::default(),
            name_revision: 0,
            browser_url: None,
            terminal_id: Some(Self::LEGACY_TERMINAL_ID.into()),
        })
    }

    fn seed_patch(&self) -> ResourcePatch {
        let workspace_id = self.workspace.public_id.clone();
        ResourcePatch {
            changes: vec![
                ResourceChange::UpsertWorkspace {
                    workspace: self.workspace.clone(),
                    position: 0,
                    active_screen: Some(self.screen.clone()),
                },
                ResourceChange::UpsertScreen(RegistryScreen {
                    public_id: self.screen.clone(),
                    workspace_id: workspace_id.clone(),
                    position: 0,
                    name: None,
                    layout: RegistryLayoutNode::Split {
                        split: SplitPublicId::parse(format!("split_{:032x}", 1)).unwrap(),
                        direction: "right".into(),
                        ratio: 0.5,
                        first: Box::new(RegistryLayoutNode::Leaf {
                            pane: self.source_pane.clone(),
                        }),
                        second: Box::new(RegistryLayoutNode::Leaf {
                            pane: self.target_pane.clone(),
                        }),
                    },
                    active_pane: self.source_pane.clone(),
                    zoomed_pane: None,
                    auto_layout: None,
                    viewport: RegistryViewport::default(),
                }),
                self.pane(&self.source_pane, Some(&self.tab)),
                self.pane(&self.target_pane, None),
                ResourceChange::UpsertTerminal {
                    public_id: self.terminal.clone(),
                    terminal: RegistryTerminal {
                        terminal_id: Self::LEGACY_TERMINAL_ID.into(),
                        workspace_key: "one".into(),
                        incarnation: None,
                        lifecycle: TerminalLifecycle::Launching,
                        launch_spec: serde_json::json!({
                            "command": ["/bin/zsh"], "cwd": "/tmp", "rows": 24, "cols": 80
                        }),
                        exit: None,
                        on_exit: TerminalOnExit::Close,
                    },
                },
                self.tab_in(&self.source_pane),
                ResourceChange::SetWorkspaceOrder { workspace_ids: vec![workspace_id.clone()] },
                ResourceChange::SetScreenOrder {
                    workspace_id: workspace_id.clone(),
                    screen_ids: vec![self.screen.clone()],
                },
                ResourceChange::SetTabOrder {
                    pane_id: self.source_pane.clone(),
                    tab_ids: vec![self.tab.clone()],
                },
                ResourceChange::SetActiveWorkspace { workspace_id: Some(workspace_id) },
            ],
        }
    }

    fn move_patch(&self) -> ResourcePatch {
        ResourcePatch {
            changes: vec![
                self.tab_in(&self.target_pane),
                self.pane(&self.source_pane, None),
                self.pane(&self.target_pane, Some(&self.tab)),
                ResourceChange::SetTabOrder { pane_id: self.source_pane.clone(), tab_ids: vec![] },
                ResourceChange::SetTabOrder {
                    pane_id: self.target_pane.clone(),
                    tab_ids: vec![self.tab.clone()],
                },
            ],
        }
    }
}

#[test]
fn an_idle_writer_takes_an_intent_within_one_millisecond() {
    const SAMPLES: usize = 21;
    let (sender, receivers) = JournalIngressSender::new(true);
    let receivers = receivers.unwrap();
    let (taken, taken_at) = sync_channel(SAMPLES);
    let writer = std::thread::spawn(move || {
        for _ in 0..SAMPLES {
            let round = receive_round(&receivers).unwrap();
            assert_eq!(round.len(), 1);
            taken.send(Instant::now()).unwrap();
        }
    });
    let mut latencies = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        // Let the writer park on its wake channel: the idle case.
        std::thread::sleep(Duration::from_millis(2));
        let sent = Instant::now();
        queue_durable(&sender, producer_event(&format!("idle_{index}")));
        let taken = taken_at.recv_timeout(Duration::from_secs(5)).unwrap();
        latencies.push(taken.saturating_duration_since(sent));
    }
    writer.join().unwrap();
    latencies.sort();
    let median = latencies[SAMPLES / 2];
    assert!(
        median < Duration::from_millis(1),
        "an idle writer must start an intent's commit within 1 ms: median {median:?} of {latencies:?}"
    );
}

#[test]
fn a_writer_paused_in_a_commit_takes_the_intent_before_queued_output() {
    let root = std::env::temp_dir().join(format!(
        "cmux-journal-lane-order-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let mux = Mux::open_persistent("journal-lane-order", crate::SurfaceOptions::default(), &root)
        .unwrap();
    let (entered, entered_receiver) = sync_channel(1);
    let (release, release_receiver) = sync_channel(1);
    mux.install_journal_before_commit_for_test(entered, release_receiver);
    let generation: Arc<str> = Arc::from("lane-order-paused");
    mux.journal_terminal_output(terminal_id(3), generation.clone(), b"pause".to_vec());
    entered_receiver.recv_timeout(Duration::from_secs(10)).unwrap();

    // The writer is inside its first commit. Queue 1 MiB of output (four
    // 256 KiB records), then an intent behind it.
    let backlog = vec![b'y'; 16 * TERMINAL_OUTPUT_INGRESS_BYTES];
    mux.journal_terminal_output(terminal_id(3), generation, backlog.clone());
    let producer_mux = mux.clone();
    let producer = std::thread::spawn(move || {
        let ingress = crate::agent_hook_journal_ingress(
            "codex",
            "SubagentStop",
            None,
            serde_json::json!({
                "session_id": "lane-order-root",
                "root_session_id": "lane-order-root",
                "parent_session_id": "lane-order-root",
                "child_agent_id": "lane-order-child",
                "message": "lane-order-intent",
            }),
        )
        .unwrap();
        producer_mux.append_journal_ingress(&ingress, "client_lane_order", "lane_order").unwrap()
    });
    // The paused writer cannot drain, so the gauge shows the intent queued.
    let queued_by = Instant::now() + Duration::from_secs(10);
    while mux.journal_writer_stats().unwrap().durable_queued == 0 {
        assert!(Instant::now() < queued_by, "the intent was never queued");
        std::thread::sleep(Duration::from_millis(1));
    }
    release.send(()).unwrap();
    let intent = producer.join().unwrap();
    mux.flush_terminal_journal().unwrap();

    let after_intent = mux
        .session_journal_after(0, 1024)
        .unwrap()
        .records
        .into_iter()
        .filter(|record| record.kind == "terminal.output")
        .filter_map(|record| {
            let bytes = record.terminal_output?;
            (bytes.first() == Some(&b'y')).then_some((record.sequence, bytes.len()))
        })
        .collect::<Vec<_>>();
    assert_eq!(after_intent.iter().map(|(_, len)| len).sum::<usize>(), backlog.len());
    assert!(
        after_intent.iter().all(|(sequence, _)| *sequence > intent.sequence),
        "output queued before the intent must commit after it: intent {}, output {after_intent:?}",
        intent.sequence
    );
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}
