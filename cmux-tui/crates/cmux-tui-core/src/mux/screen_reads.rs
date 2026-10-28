//! Screen reads of a terminal, wherever it is parsed: from its VT worker's
//! page when a worker process parses it (vt_worker_runtime/screen_reads.rs,
//! p01-process-135), else from the daemon's own parser. `read-screen`,
//! `wait-for`, `copy`, `terminal.screen.read`, `terminal.wait` and the
//! resource waits all read through here, so none of them hops to a worker.

use super::*;
use crate::surface::TerminalScreenSnapshot;

impl Mux {
    /// The terminals whose screens are read from VT worker pages; the broker
    /// attaches and detaches them.
    #[cfg(unix)]
    pub fn worker_screens(&self) -> &crate::vt_worker_runtime::WorkerScreens {
        &self.worker_screens
    }

    /// `surface`'s viewport as plain text (`Terminal::viewport_text`).
    pub(crate) fn screen_text(&self, surface: &Surface) -> anyhow::Result<String> {
        #[cfg(unix)]
        if let Some(read) = self.worker_screens.read(surface.id) {
            return Ok(crate::vt_worker_runtime::screen_text(&read?.cells));
        }
        Ok(surface.try_with_terminal(|terminal| terminal.viewport_text())??)
    }

    /// `surface`'s viewport text for a wait: `None` while its worker page
    /// cannot be read for a reason that passes by itself (not published
    /// yet, mid-publish, being resized; `ScreenReadError::is_transient`).
    /// The publish that ends it advances the stream revision the wait
    /// sleeps on, so the wait reads again then; its last read at the
    /// deadline uses [`Self::screen_text`], which reports the error.
    pub(crate) fn screen_text_for_wait(&self, surface: &Surface) -> anyhow::Result<Option<String>> {
        #[cfg(unix)]
        if let Some(read) = self.worker_screens.read(surface.id) {
            return match read {
                Ok(screen) => Ok(Some(crate::vt_worker_runtime::screen_text(&screen.cells))),
                Err(error) if error.is_transient() => Ok(None),
                Err(error) => Err(error.into()),
            };
        }
        self.screen_text(surface).map(Some)
    }

    /// Whether a VT worker process parses `surface` (its screen is read
    /// from the worker's page).
    pub(crate) fn is_worker_terminal(&self, surface: &Surface) -> bool {
        #[cfg(unix)]
        return self.worker_screens.source(surface.id).is_some();
        #[cfg(not(unix))]
        {
            let _ = surface;
            false
        }
    }

    /// `surface`'s viewport text, size, cursor, stream revision and OSC
    /// progress at one cut.
    pub(crate) fn screen_snapshot(
        &self,
        surface: &Surface,
    ) -> anyhow::Result<TerminalScreenSnapshot> {
        #[cfg(unix)]
        if let Some(source) = self.worker_screens.source(surface.id) {
            return worker_screen_snapshot(surface, source.as_ref());
        }
        surface.terminal_screen_snapshot()
    }
}

/// The snapshot of a worker terminal. The revision is read before the page:
/// the broker advances it only after the worker published the parse it
/// reports, so the page is at that revision or newer, and a waiter that
/// waits past this revision misses no change.
#[cfg(unix)]
fn worker_screen_snapshot(
    surface: &Surface,
    source: &dyn crate::vt_worker_runtime::WorkerScreenSource,
) -> anyhow::Result<TerminalScreenSnapshot> {
    let revision = surface.terminal_stream_revision()?;
    let screen = crate::vt_worker_runtime::read_worker_screen(source)?;
    // The position whether the cursor is shown or not, as the daemon's
    // `terminal_screen_snapshot` reads `Terminal::cursor_position`.
    let (cursor_col, cursor_row) = screen.cursor;
    Ok(TerminalScreenSnapshot {
        text: crate::vt_worker_runtime::screen_text(&screen.cells),
        cols: screen.cols,
        rows: screen.rows,
        cursor_col,
        cursor_row,
        cursor_visible: screen.cursor_visible,
        revision,
        osc_progress: surface.terminal_osc_progress()?,
    })
}

/// A read that needs the history of a terminal a VT worker process parses,
/// on a path that does not ask the worker for it: the worker holds the
/// history and the daemon's own parser of the terminal is stale, so the read
/// fails instead of answering from it. `read-scrollback` and
/// `terminal.history.read` over a client connection ask the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorkerHistoryUnavailable {
    pub(crate) operation: &'static str,
}

impl fmt::Display for WorkerHistoryUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} cannot read the history of a terminal a VT worker parses: the worker holds it",
            self.operation
        )
    }
}

impl std::error::Error for WorkerHistoryUnavailable {}

impl WorkerHistoryUnavailable {
    /// The resource protocol's `operation.failed` for this refusal.
    pub(crate) fn resource_error(self) -> ResourceError {
        ResourceError::operation_failed(
            self.operation,
            self.to_string(),
            serde_json::json!({"worker_terminal":true}),
        )
    }
}
