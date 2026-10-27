//! The daemon's broker for sandboxed `cmux-vt-worker` processes (phase B of
//! the VT pool, behind `CMUX_TUI_VT_WORKERS=process`;
//! plans/cmux-next/chrome-parity/p01-process.md, task 134).
//!
//! A worker parses the output of one shard of holder terminals from the
//! terminals' rings, which it maps read-only. The broker owns everything a
//! worker may not touch:
//!
//! - **Launch.** [`WorkerLauncher`] starts a shard's worker through
//!   `cmux_process` (no descriptor but its link socket and the rings it is
//!   given), lazily, when the shard gets its first terminal.
//! - **Shards.** A terminal goes to the shard with the fewest terminals and
//!   stays there; the worker gets its ring as a descriptor ([`wire`]).
//! - **Ring advances.** The broker relays each terminal's ring end to its
//!   worker with at most one advance in flight per terminal, so a slow
//!   worker never makes the link queue grow with the output rate.
//! - **Query answers.** A DA or DSR answer a worker sends is checked against
//!   the fixed answer set ([`QueryAnswer`]) and against the shard's own
//!   terminals before it reaches a PTY: a worker never writes input.
//! - **Restart and reseed.** A worker whose link ends is replaced at once,
//!   and every terminal of its shard is reseeded from the holder's prologue
//!   plus the ring tail after it ([`VtWorkerHost::reseed`]).
//! - **Strike rule.** A shard whose worker died twice within 10 s restores
//!   from the prologue only, a third time from a blank screen, so one byte
//!   stream that crashes the parser cannot crash every restart
//!   ([`StrikeLog`]).
//!
//! Chromium's model: `ServiceProcessHost::Launch` starts a sandboxed utility
//! process with one owner and queues calls until it is ready
//! (content/public/browser/service_process_host.h); the owner restarts it
//! when it dies, and a process that keeps crashing is not restarted the same
//! way for ever: `GpuProcessHost::RecordProcessCrash` counts crashes in a
//! window and falls back to a safer mode
//! (content/browser/gpu/gpu_process_host.cc,
//! `GpuDataManagerImplPrivate::FallBackToNextGpuMode`).
//!
//! Threads: `vt-broker` (the state, one event queue) and per live worker
//! `vt-link-r-<shard>` (reads its frames) and `vt-link-w-<shard>` (writes its
//! queue), all at UserInitiated: an echo's parse waits on them.

#![cfg_attr(not(test), deny(clippy::disallowed_methods, clippy::disallowed_types))]

mod answers;
mod broker;
mod launcher;
mod link;
mod reseed;
mod strikes;
pub mod wire;

#[cfg(test)]
mod tests;

pub use answers::{AnswerRejected, QueryAnswer};
pub use broker::{
    BrokerClock, BrokerConfig, VtTerminalKey, VtTerminalSpec, VtWorkerBroker, VtWorkerHost,
    WorkerLifecycle,
};
pub use launcher::{
    LaunchedWorker, ProcessLauncher, WORKER_FD, WORKER_FD_ENV, WORKER_SHARD_ENV, WorkerLauncher,
    WorkerProcess,
};
pub use link::LinkEnd;
pub use reseed::{Prologue, Reseed, RingSpan, SeedPlan};
pub use strikes::{POISON_STRIKES, ReseedMode, STRIKE_WINDOW, StrikeLog};

/// The env switch whose value `process` makes the daemon parse in worker
/// processes through this broker; anything else keeps the VT pool threads.
pub const VT_WORKERS_ENV: &str = "CMUX_TUI_VT_WORKERS";

/// Whether `value` of [`VT_WORKERS_ENV`] asks for worker processes. Only the
/// exact word `process` does: the broker is off unless asked for.
pub fn process_workers_requested(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim() == "process")
}
