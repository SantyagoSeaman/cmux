//! The daemon's side of the sandboxed `cmux-vt-worker` processes (phase B of
//! plans/cmux-next/chrome-parity/p01-process.md, tasks 134-142).
//!
//! A worker parses untrusted terminal output with unsafe code, so the
//! daemon trusts nothing it sends back. [`WorkerOutputGuard`] checks every
//! event and every screen page read against what the daemon itself told the
//! worker (task 141); the first violation is a [`BadMessage`], and
//! [`drain_frames`] then kills the worker and names the terminals to
//! reseed. This is Chrome's rule for a renderer or utility process
//! (`content/browser/bad_message.h`, `bad_message::ReceivedBadMessage`;
//! `mojo/public/cpp/bindings/message.h`, `ReportBadMessage`): a malformed
//! message ends the sender, it is never repaired or skipped.

mod bad_message;
mod pty_answers;

pub use bad_message::{
    BAD_MESSAGE_REASON, BadMessage, HelloExpectation, PageRead, RingOffsets, WorkerEnd,
    WorkerOutputGuard, WorkerProcess, drain_frames,
};
pub use pty_answers::{AnswerError, AnswerScanner, MAX_ANSWER_BYTES};

/// The worker crate: its wire, page reader and sandbox types.
pub use cmux_vt_worker as worker;
