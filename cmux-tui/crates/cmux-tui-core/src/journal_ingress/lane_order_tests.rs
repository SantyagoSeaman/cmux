use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use super::*;
use crate::Mux;
use crate::journal_ingress::{
    JournalIngressEvent, JournalIngressSender, TERMINAL_OUTPUT_INGRESS_BYTES,
};
use crate::resource::TerminalPublicId;

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
fn a_small_terminal_batch_shares_the_intent_transaction_after_it() {
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
            QueuedJournalEvent { event: JournalIngressEvent::Producer { .. }, .. },
            QueuedJournalEvent { event: JournalIngressEvent::TerminalResize { .. }, .. },
        ]
    ));
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
