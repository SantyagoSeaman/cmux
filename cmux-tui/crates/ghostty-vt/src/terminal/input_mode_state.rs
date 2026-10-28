//! The terminal state behind [`Terminal::input_modes`]: the effective mouse
//! tracking and the reader that turns the parser's modes into plain data.

use std::cell::Cell;

use super::*;
use crate::input_modes::{InputModes, InputModesReader, MouseTracking};

pub(super) struct InputModeState {
    /// Ghostty's last-set-wins tracking, refreshed with the mouse modes.
    tracking: MouseTracking,
    /// Taken and put back by `input_modes(&self)`; a Terminal is not Sync.
    reader: Cell<Option<Box<InputModesReader>>>,
}

impl InputModeState {
    pub(super) fn new() -> Result<Self> {
        let reader = Cell::new(Some(Box::new(InputModesReader::new()?)));
        Ok(Self { tracking: MouseTracking::None, reader })
    }
}

impl Terminal {
    /// Re-read the effective tracking; true when it changed. Resetting a
    /// tracking mode that was not set still turns tracking off (Ghostty's
    /// last-set-wins value) with no mode flag changing.
    pub(super) fn refresh_mouse_tracking(&mut self) -> bool {
        let tracking = self.mouse_mode_probe.classify_tracking(self.raw);
        let changed = tracking != self.input_mode_state.tracking;
        self.input_mode_state.tracking = tracking;
        changed
    }

    /// The effective mouse tracking (last set wins, as Ghostty's encoder).
    pub fn active_mouse_tracking(&self) -> MouseTracking {
        self.input_mode_state.tracking
    }

    /// The modes key, mouse, paste and focus encoders read, as plain data
    /// a process without this parser can encode from.
    pub fn input_modes(&self) -> InputModes {
        let mut reader = self.input_mode_state.reader.take();
        let modes = reader.as_mut().map(|reader| reader.read(self)).unwrap_or_default();
        self.input_mode_state.reader.set(reader);
        modes
    }

    /// The Kitty keyboard flags at the top of the active screen's stack.
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.get::<u8>(sys::GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS).unwrap_or(0)
    }
}
