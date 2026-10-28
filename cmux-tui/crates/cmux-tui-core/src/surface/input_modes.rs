//! A terminal's input modes for its encoders, published without a lock.
//!
//! Key, mouse, paste and focus encoders read the modes from one atomic word
//! and never take the terminal lock or ask a parser (p01-process-209). The
//! parser that owns the terminal publishes after every change: the daemon's
//! own parser under its terminal lock ([`Surface`]'s `sync_input_modes`), or,
//! with `CMUX_TUI_VT_WORKERS=process`, the screen page a VT worker writes
//! ([`TerminalInputModes::publish_from_page`], read with seqlock retry).
//! Chromium keeps the same split: the browser routes and encodes input from
//! state it already has and never waits on a renderer
//! (`content/browser/renderer_host/input/input_router_impl.cc`).

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use cmux_vt_worker::screen_page::{PageError, PageReader};
use ghostty_vt::{InputModes, KeyEncoder, MouseEncoders, Terminal};

use super::*;

/// The published modes of one terminal.
#[derive(Debug)]
pub(crate) struct TerminalInputModes {
    /// `InputModes::to_word`.
    word: AtomicU64,
}

impl TerminalInputModes {
    pub(crate) fn new(modes: InputModes) -> Self {
        Self { word: AtomicU64::new(modes.to_word()) }
    }

    /// The modes last published.
    pub(crate) fn load(&self) -> InputModes {
        // Only `publish` stores, and only words `to_word` made.
        InputModes::from_word(self.word.load(Ordering::Acquire)).unwrap_or_default()
    }

    pub(crate) fn publish(&self, modes: InputModes) {
        self.word.store(modes.to_word(), Ordering::Release);
    }

    /// Publish the modes a VT worker last wrote into `page`. A page with no
    /// screen yet, or a writer still busy after the reader's retries, leaves
    /// the last published modes; a corrupt page is the caller's bad message.
    #[cfg(unix)]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the VT worker broker calls it once it maps pages (p01-process-135)"
        )
    )]
    pub(crate) fn publish_from_page(&self, page: &PageReader) -> Result<InputModes, PageError> {
        let modes = page.read_modes()?;
        self.publish(modes);
        Ok(modes)
    }
}

impl PtySurface {
    /// Publish `term`'s input modes and set the mouse encoders from them.
    /// The caller holds the terminal lock, so a reader that holds it too
    /// sees modes that match the terminal.
    pub(super) fn sync_input_modes(&self, term: &Terminal) {
        let modes = term.input_modes();
        self.input_modes.publish(modes);
        self.mouse_encoders.lock().recover_poison().sync_from_modes(modes);
    }
}

impl Surface {
    /// The terminal's published input modes; `None` for a browser surface.
    pub(crate) fn input_modes(&self) -> Option<InputModes> {
        Some(self.as_pty()?.input_modes.load())
    }

    /// A key encoder set from the published modes (no terminal lock).
    pub(crate) fn key_encoder(&self) -> anyhow::Result<KeyEncoder> {
        let modes =
            self.input_modes().ok_or_else(|| anyhow::anyhow!("surface is not a terminal"))?;
        let mut encoder = KeyEncoder::new()?;
        encoder.sync_from_modes(modes);
        Ok(encoder)
    }
}

/// New mouse encoders and the published modes of a terminal being built.
pub(super) fn new_input_encoders(
    term: &Terminal,
) -> ghostty_vt::Result<(MouseEncoders, TerminalInputModes)> {
    let modes = term.input_modes();
    let mut mouse_encoders = MouseEncoders::new()?;
    mouse_encoders.sync_from_modes(modes);
    Ok((mouse_encoders, TerminalInputModes::new(modes)))
}

#[cfg(test)]
#[path = "input_modes_tests.rs"]
mod tests;
