//! The daemon's broker for sandboxed `cmux-vt-worker` processes (phase B of
//! the VT pool, behind `CMUX_TUI_VT_WORKERS=process`;
//! plans/cmux-next/chrome-parity/p01-process.md, tasks 134 and 141).
//!
//! A worker parses the output of one shard of holder terminals from the
//! terminals' rings, which it maps read-only, and publishes each screen in
//! a page the daemon maps. The broker speaks the worker's own wire
//! (`cmux_vt_worker::wire`) and owns everything a worker may not touch:
//!
//! - **Launch and sandbox.** [`WorkerLauncher`] starts a shard's worker
//!   through `cmux_process` (no descriptor but its link socket), lazily,
//!   when the shard gets its first terminal. Its first frame is BOOTSTRAP
//!   with the Seatbelt policy the daemon compiled ([`WorkerSandbox`]); the
//!   worker gets no ring before its HELLO reports this wire version and the
//!   platform's sandbox. A worker that reports another is handed back.
//! - **Shards.** A terminal goes to the live shard with the fewest
//!   terminals and stays there. Its ATTACH carries its ring, its screen
//!   page ([`ScreenPage`]) and the screen to start from.
//! - **Ring advances.** The broker relays each terminal's ring end to its
//!   worker with at most one advance in flight per terminal, so a slow
//!   worker never makes the link queue grow with the output rate.
//! - **Untrusted output** ([`WorkerOutputGuard`], task 141). Every event a
//!   worker sends and every page the daemon reads is checked against what
//!   the broker told that worker: a terminal of its shard, offsets inside
//!   the announced ring, newer frames, text without controls, page needs
//!   bounded by the terminal's grids. The first violation is a
//!   [`BadMessage`]: the worker is killed and its shard reseeded, with the
//!   link's end [`LinkEnd::BadMessage`] (reason `bad_message`). This is
//!   Chrome's rule for a renderer or utility process
//!   (content/browser/bad_message.h, `bad_message::ReceivedBadMessage`): a
//!   malformed message ends the sender, it is never repaired or skipped.
//! - **Query answers.** A worker never writes input: the broker parses its
//!   PTY answers into the fixed answer set ([`QueryAnswer`]) and writes
//!   their own encoding. An answer to output an earlier worker already
//!   answered is not written again after a reseed.
//! - **Restart and reseed.** A worker whose link ends is replaced, and each
//!   terminal of its shard is reseeded from the holder's prologue plus the
//!   ring tail after it ([`VtWorkerHost::reseed`]).
//! - **Strike rule** ([`strikes`](StrikeLog)). A terminal whose parse was in
//!   flight at a second death within 10 s restores from its prologue only,
//!   at a third from a blank screen, at a fourth it is handed back. A shard
//!   restarts at once three times, then backs off, and at ten deaths it is
//!   handed back for good.
//! - **Hand back.** Terminals the broker cannot parse go back to the daemon
//!   with their keys ([`WorkerLifecycle::HandedBack`]), so it can parse them
//!   with pool threads.
//!
//! Chromium's model: `ServiceProcessHost::Launch` starts a sandboxed utility
//! process with one owner and queues calls until it is ready
//! (content/public/browser/service_process_host.h); the parent compiles the
//! Seatbelt policy and the child applies it before untrusted input
//! (sandbox/mac/seatbelt_exec.cc); a process that keeps crashing is not
//! restarted the same way for ever: `GpuProcessHost::RecordProcessCrash`
//! counts crashes in a window and falls back, in the end for good
//! (content/browser/gpu/gpu_process_host.cc).
//!
//! Not wired into the daemon's parser selection yet: the VT pool (task
//! 120) owns that choice and its holder sink adapter; task 242 implements
//! [`VtWorkerHost`] over it and starts the broker when
//! [`process_workers_requested`].
//!
//! Threads: `vt-broker` (the state, one event queue) and per live worker
//! `vt-link-r-<shard>` (reads its events) and `vt-link-w-<shard>` (writes
//! its queue), all at UserInitiated: an echo's parse waits on them.

#![cfg_attr(not(test), deny(clippy::disallowed_methods, clippy::disallowed_types))]

mod answers;
mod bad_message;
mod broker;
mod host;
mod launcher;
mod link;
mod reseed;
mod strikes;

#[cfg(test)]
mod tests;

pub use answers::{
    AnswerError, AnswerGrid, AnswerRejected, AnswerScanner, ColorTarget, KittyStatus, MAX_ANSWER,
    QueryAnswer, Scanned, StringEnd,
};
pub use bad_message::{
    BAD_MESSAGE_REASON, BadMessage, Checked, HelloExpectation, MAX_PENDING_ANSWER_BYTES, PageRead,
    RingOffsets, WorkerOutputGuard,
};
pub use broker::{MediaReply, PageChecker, VtWorkerBroker};
pub use cmux_kitty_media::{FileMedium, MediaRequest};
/// The worker crate: its wire, page reader and sandbox types.
pub use cmux_vt_worker as worker;
pub use cmux_vt_worker::sandbox::{SandboxMechanism, SandboxPolicy};
pub use cmux_vt_worker::wire::{
    AttachFailure, Colors, Event as WorkerEvent, Geometry, MediaAnswer,
};
pub use host::{
    BrokerClock, BrokerConfig, HandBack, ScreenPage, VtTerminalKey, VtTerminalSpec, VtWorkerHost,
    WorkerLifecycle, WorkerSandbox,
};
pub use launcher::{LaunchedWorker, ProcessLauncher, WORKER_FD, WorkerLauncher, WorkerProcess};
pub use link::LinkEnd;
pub use reseed::{Prologue, Reseed, RingSpan, SeedPlan};
pub use strikes::{
    IMMEDIATE_RESTARTS, POISON_STRIKES, ReseedMode, Restart, SHARD_GIVE_UP, STRIKE_WINDOW,
    StrikeLog, TERMINAL_GIVE_UP,
};

use std::io;
use std::path::PathBuf;
use std::time::Instant;

use cmux_vt_worker::sandbox::SandboxError;

/// The env switch whose value `process` makes the daemon parse in worker
/// processes through this broker; anything else keeps the VT pool threads.
pub const VT_WORKERS_ENV: &str = "CMUX_TUI_VT_WORKERS";

/// Whether `value` of [`VT_WORKERS_ENV`] asks for worker processes. Only the
/// exact word `process` does: the broker is off unless asked for.
pub fn process_workers_requested(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim() == "process")
}

/// Why the worker broker did not start.
#[derive(Debug)]
pub enum StartError {
    /// The platform's worker sandbox did not compile: no worker may run.
    Sandbox(SandboxError),
    /// The broker thread did not start.
    Io(io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandbox(error) => write!(f, "VT worker sandbox: {error}"),
            Self::Io(error) => write!(f, "VT worker broker: {error}"),
        }
    }
}

impl std::error::Error for StartError {}

/// The daemon's start of the broker: `None` unless `value` of
/// [`VT_WORKERS_ENV`] asks for worker processes, else a broker that
/// launches `worker_exe` per shard, sized for this machine, under this
/// platform's sandbox. The caller reads the env (`std::env::var`) and picks
/// the worker executable (task 211 ships it like `cmux-holder`).
pub fn start_process_workers(
    value: Option<&str>,
    worker_exe: impl Into<PathBuf>,
    host: Box<dyn VtWorkerHost>,
) -> Result<Option<VtWorkerBroker>, StartError> {
    if !process_workers_requested(value) {
        return Ok(None);
    }
    let sandbox = WorkerSandbox::for_platform().map_err(StartError::Sandbox)?;
    VtWorkerBroker::start(
        BrokerConfig::for_machine(),
        sandbox,
        Box::new(ProcessLauncher::new(worker_exe)),
        host,
        Box::new(Instant::now),
    )
    .map(Some)
    .map_err(StartError::Io)
}
