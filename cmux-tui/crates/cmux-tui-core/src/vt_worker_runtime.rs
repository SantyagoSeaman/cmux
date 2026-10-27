//! The daemon's side of sandboxed `cmux-vt-worker` processes (phase B of the
//! VT pool, behind `CMUX_TUI_VT_WORKERS=process`;
//! plans/cmux-next/chrome-parity/p01-process.md).
//!
//! Screen reads (screen_reads.rs): `read-screen`, the snapshot of `terminal.screen.read`,
//! agent waits and `copy` read a worker terminal's screen from its seqlock
//! page with no request to the worker; scrollback, which is not in the page,
//! is requested from the worker and answered asynchronously (task 135).

#![cfg_attr(not(test), deny(clippy::disallowed_methods, clippy::disallowed_types))]

#[cfg(test)]
pub(crate) mod fake_screen_source;
mod screen_reads;

pub use screen_reads::{
    HistoryError, HistoryPage, HistoryRange, HistoryReply, PAGE_READ_ATTEMPTS, PageReadError,
    ScreenReadError, WorkerScreen, WorkerScreenSource, WorkerScreens, read_worker_screen,
    request_worker_history, screen_text,
};
