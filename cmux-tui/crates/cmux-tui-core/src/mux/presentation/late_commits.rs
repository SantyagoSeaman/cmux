//! Presentation writes that commit after their request gave up (p05-ipc-135).
//!
//! A presentation write goes to the journal writer
//! (`RegistryIntent::Presentation`). When its receipt is indeterminate the
//! request fails, but the writer can still commit the write. The registry
//! keeps the receipt (`take_settled_presentation`); the mux installs what
//! committed here: it reloads the snapshot and emits `TreeChanged`, so tree
//! readers and `frontend_browser_id` see the late write.

use super::*;

impl Mux {
    /// Install the presentation writes that committed late. A presentation
    /// write path calls it after its own write, which waited for every
    /// earlier receipt (`drain_unsettled`); a state holder gets them from
    /// `settle_registry_receipts`. The caller holds the registry.
    #[track_caller]
    pub(crate) fn settle_presentation_receipts(&self, registry: &WorkspaceRegistry) {
        if registry.connection.take_settled_presentation() > 0 {
            self.install_late_presentation(registry);
        }
    }

    /// Reload the snapshot for presentation writes that committed late and
    /// tell tree subscribers. The caller holds the registry.
    #[track_caller]
    pub(crate) fn install_late_presentation(&self, registry: &WorkspaceRegistry) {
        if let Err(error) = self.reload_presentation(registry) {
            eprintln!("cmux-tui: reloading presentation after a late commit failed: {error:#}");
            return;
        }
        self.emit(MuxEvent::TreeChanged);
    }
}

#[cfg(test)]
#[path = "late_commits_tests.rs"]
mod tests;
