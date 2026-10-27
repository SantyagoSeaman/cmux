//! `OutboundStream`: one backpressured stream of a connection, the handle a
//! producer queues its items through (server/message_writer.rs).

use super::BudgetedText;
use super::ClientStreamId;
use crate::panic_free::RecoverPoison;
use crate::stream_interrupt::InterruptSet;
use crate::stream_interrupt::StreamInterrupt;
use cmux_ipc::StreamLane;
use cmux_threads::sync::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[derive(Clone)]
pub(in crate::server) struct OutboundStream {
    pub(in crate::server) id: u64,
    pub(in crate::server) open: Arc<AtomicBool>,
    /// The stream rides the connection's control sequence (stream 0): its
    /// items share the reply FIFO. False for every other stream.
    pub(in crate::server) control_sequence: bool,
    /// The outbound lane of a stream outside the control sequence.
    pub(in crate::server) lane: StreamLane,
    pub(in crate::server) terminal_enqueued: Arc<AtomicBool>,
    pub(in crate::server) overflow_text: Arc<Mutex<Arc<BudgetedText>>>,
    /// Fired by `close`, so stream loops block instead of polling `is_open`.
    pub(in crate::server) closed: InterruptSet,
    /// The id the client minted for this stream (attach-streams-v1): every
    /// item carries it as `"stream"` and the terminal is `stream-end`.
    pub(in crate::server) client_stream: Option<ClientStreamId>,
    /// Run when the writer takes an item of this stream while it held its
    /// backpressure cap: the producer's drain task (p05-ipc-079), which
    /// stopped at the cap instead of waiting for room.
    pub(in crate::server) space_hook: Arc<crate::stream_interrupt::ReadyHook>,
}

impl OutboundStream {
    #[cfg(test)]
    pub(in crate::server) fn new(id: u64, overflow_text: Arc<BudgetedText>) -> Self {
        Self::with_sequence(id, overflow_text, false, StreamLane::Bulk)
    }

    pub(in crate::server) fn with_sequence(
        id: u64,
        overflow_text: Arc<BudgetedText>,
        control_sequence: bool,
        lane: StreamLane,
    ) -> Self {
        Self {
            id,
            open: Arc::new(AtomicBool::new(true)),
            control_sequence,
            lane,
            terminal_enqueued: Arc::new(AtomicBool::new(false)),
            overflow_text: Arc::new(Mutex::new(overflow_text)),
            closed: InterruptSet::default(),
            client_stream: None,
            space_hook: Arc::default(),
        }
    }

    /// Runs `hook` each time the writer takes an item of this stream while
    /// it was at its backpressure cap. False when a hook is set already.
    pub(in crate::server) fn on_space(&self, hook: impl Fn() + Send + Sync + 'static) -> bool {
        self.space_hook.set(hook)
    }

    pub(in crate::server) fn register_interrupt(&self, interrupt: &Arc<StreamInterrupt>) {
        self.closed.register(interrupt);
    }

    pub(in crate::server) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    pub(in crate::server) fn close(&self) {
        self.open.store(false, Ordering::Release);
        self.closed.fire();
    }

    pub(in crate::server) fn update_overflow(&self, text: Arc<BudgetedText>) {
        *self.overflow_text.lock().recover_poison() = text;
    }
}
