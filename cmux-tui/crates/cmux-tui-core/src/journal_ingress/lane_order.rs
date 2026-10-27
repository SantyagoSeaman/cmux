//! The order in which the journal writer commits its two lanes
//! (p05-ipc-127): the durable lane (registry intents, producer and frontend
//! events, receipts a request waits for) before the terminal lane.
//!
//! The writer is one sequence with two queues, like a Chromium sequence
//! whose task queues have priorities: `TaskQueueSelector` takes work from
//! the highest-priority non-empty queue first
//! (base/task/sequence_manager/task_queue_selector.cc). Each round drains
//! both lanes once and commits the durable batch before the terminal batch,
//! so an intent queued behind a large terminal backlog waits for at most the
//! one transaction already in progress, never for terminal batches queued
//! with it. The terminal lane is not starved: every round that drained
//! terminal events commits them before the writer drains again.
//!
//! A small terminal batch (the barriers, resizes and gaps shipped builds
//! send) commits in the durable batch's transaction, before its events: the
//! two share one fsync, and the intent's commit grows by only a few rows.
//! Terminal records go first because the writer computes their topology
//! subjects at transaction start (`writer_batch.rs`): a registry intent
//! after them can move or create the terminal without making those
//! subjects stale for their sequence position.
//!
//! At idle the writer waits on the wake channel with no timeout, and no hold
//! applies to a round that drained durable events, so an intent starts its
//! commit as soon as the writer thread runs (`writer_qos.rs` sets its class).

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};

use cmux_threads::checked::sync as waits;

#[cfg(test)]
use super::coalesce;
use super::{
    JOURNAL_DURABLE_BATCH_BYTES, JOURNAL_DURABLE_QUEUE_CAPACITY, JOURNAL_TERMINAL_BATCH_CHUNKS,
    JournalIngressReceivers, JournalLane, QueuedJournalEvent,
};

/// Terminal-lane bytes that may share a durable batch's transaction. A full
/// batch of resizes or gaps (64 events of at most 128 bytes) fits; terminal
/// output past it commits in its own transaction after the durable batch.
const TERMINAL_RIDE_ALONG_BYTES: usize = 16 * 1024;

/// The batches of one writer round, in commit order: `None` once the writer
/// is closed or every sender is gone with both lanes empty.
pub(super) fn receive_round(
    receivers: &JournalIngressReceivers,
) -> Option<VecDeque<Vec<QueuedJournalEvent>>> {
    loop {
        // Any event queued after this drain posts a new wake.
        while receivers.wake.try_recv().is_ok() {}
        // Durable first: an intent queued while terminal events wait is in
        // this round. Each transaction stays bounded: at most 8 MiB of
        // durable payload and 4 MiB of unmerged terminal input.
        let mut durable = Vec::new();
        let durable_drained = drain_lane(
            &receivers.durable,
            &mut durable,
            JOURNAL_DURABLE_QUEUE_CAPACITY,
            JOURNAL_DURABLE_BATCH_BYTES,
        );
        let mut terminal = Vec::with_capacity(JOURNAL_TERMINAL_BATCH_CHUNKS);
        let terminal_drained = drain_lane(
            &receivers.terminal,
            &mut terminal,
            JOURNAL_TERMINAL_BATCH_CHUNKS,
            usize::MAX,
        );
        receivers.state.stats.drained(JournalLane::Durable, durable_drained);
        receivers.state.stats.drained(JournalLane::Terminal, terminal_drained);
        if terminal_drained + durable_drained != 0 {
            receivers.state.publish_queue_space();
        }
        // Test builds: plain terminal output waits a bounded window for more
        // output, and ends it for any durable event (coalesce.rs). Shipped
        // builds journal no output.
        #[cfg(test)]
        coalesce::hold_terminal_output(receivers, &mut terminal, &mut durable, terminal_drained);
        let round = order_round(durable, terminal);
        if !round.is_empty() {
            return Some(round);
        }
        if receivers.state.closed.load(Ordering::Acquire) {
            return None;
        }
        if waits::recv(&receivers.wake).is_err() {
            return None;
        }
    }
}

/// The durable batch first, with a small terminal batch in its transaction
/// ahead of its events; a larger terminal batch is its own transaction
/// after it.
fn order_round(
    durable: Vec<QueuedJournalEvent>,
    mut terminal: Vec<QueuedJournalEvent>,
) -> VecDeque<Vec<QueuedJournalEvent>> {
    if durable.is_empty() || terminal.is_empty() {
        return [durable, terminal].into_iter().filter(|batch| !batch.is_empty()).collect();
    }
    let terminal_bytes = terminal
        .iter()
        .fold(0_usize, |bytes, queued| bytes.saturating_add(queued.event.estimated_bytes()));
    if terminal_bytes <= TERMINAL_RIDE_ALONG_BYTES {
        terminal.extend(durable);
        return VecDeque::from([terminal]);
    }
    VecDeque::from([durable, terminal])
}

/// Moves up to `limit` events, and events of up to about `byte_limit`
/// bytes, from `receiver` into `batch`; returns how many it moved.
pub(super) fn drain_lane(
    receiver: &Receiver<QueuedJournalEvent>,
    batch: &mut Vec<QueuedJournalEvent>,
    limit: usize,
    byte_limit: usize,
) -> usize {
    let mut drained = 0;
    let mut drained_bytes = 0_usize;
    while drained < limit && drained_bytes < byte_limit {
        let next = match receiver.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        };
        drained += 1;
        drained_bytes = drained_bytes.saturating_add(next.event.estimated_bytes());
        #[cfg(test)]
        coalesce::push_coalesced(batch, next);
        #[cfg(not(test))]
        batch.push(next);
    }
    drained
}

#[cfg(test)]
#[path = "lane_order_tests.rs"]
mod tests;
