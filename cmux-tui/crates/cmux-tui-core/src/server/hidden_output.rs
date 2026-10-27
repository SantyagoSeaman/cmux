//! A hidden view's byte stream sends its output in batches (p14-energy-119,
//! feature `terminalHiddenOutputBatch`, default off).
//!
//! The cmux app hides a view in place with `set-size-counts {lease,
//! counts:false}` and shows it with `counts:null`
//! (`TerminalAttachment.sendHidden`). That lease's attach queue then holds
//! its output and sends it at most once per second (4 s in Energy Saver);
//! the queue's own rules are in surface/attach_tap/hidden_batch.rs. Only the
//! view's own lease hides it: a participant or client selector (another
//! device's "Counts toward size" menu) changes sizing alone, since that view
//! may still be on screen.

use std::sync::atomic::{AtomicBool, Ordering};

use super::ClientRegistry;
use crate::Mux;
use crate::SurfaceId;
use crate::panic_free::RecoverPoison;
use crate::surface::HiddenOutput;

/// The registry name of the feature.
pub(super) const FEATURE: &str = "terminalHiddenOutputBatch";

/// Whether hidden views batch their output: read from `CMUX_ENABLE_FEATURES`
/// and `CMUX_DISABLE_FEATURES` once, at daemon start.
#[derive(Debug)]
pub(super) struct HiddenOutputGate(AtomicBool);

impl HiddenOutputGate {
    pub(super) fn from_env() -> Self {
        Self(AtomicBool::new(cmux_features::listed_in_env(FEATURE)))
    }

    fn enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn set_for_test(&self, enabled: bool) {
        self.0.store(enabled, Ordering::Relaxed);
    }
}

impl ClientRegistry {
    /// Keep `output`, the queue of `client`'s view `lease` of `surface`, so a
    /// later `set-size-counts` of that lease can hide it. The entry goes with
    /// the view's stream (`detach_surface`).
    pub(super) fn register_hidden_output(
        &self,
        client: u64,
        surface: SurfaceId,
        lease: &str,
        output: HiddenOutput,
    ) {
        if !self.hidden_output.enabled() {
            return;
        }
        let mut state = self.state.lock().recover_poison();
        if let Some(attached) =
            state.clients.get_mut(&client).and_then(|record| record.attached.get_mut(&surface))
        {
            attached.hidden_outputs.insert(lease.to_owned(), output);
        }
    }

    pub(super) fn hidden_output(
        &self,
        client: u64,
        surface: SurfaceId,
        lease: &str,
    ) -> Option<HiddenOutput> {
        let state = self.state.lock().recover_poison();
        state.clients.get(&client)?.attached.get(&surface)?.hidden_outputs.get(lease).cloned()
    }
}

/// `set-size-counts {lease, counts}` of the caller's own view: `false` holds
/// its output, anything else sends what is held in this turn and stops.
pub(super) fn apply(mux: &Mux, client: u64, surface: SurfaceId, lease: &str, counts: Option<bool>) {
    let Some(output) = mux.control_clients.hidden_output(client, surface, lease) else { return };
    if counts == Some(false) {
        output.hide(mux.energy.mode());
    } else {
        output.show();
    }
}
