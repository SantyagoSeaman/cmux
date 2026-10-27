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
    let (cursor_col, cursor_row) = screen.cursor.unwrap_or((0, 0));
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
