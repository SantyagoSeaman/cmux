//! The terminal modes that decide how input is encoded, as plain data.
//!
//! A key, mouse or paste encoder needs only these bits of parser state, so a
//! process that does not own the parser (the daemon, when a sandboxed VT
//! worker parses: p01-process-209) can encode from a published copy instead
//! of asking the parser. Chromium splits input the same way: the browser
//! keeps the state that routes and encodes an event and never asks a
//! renderer synchronously (`content/browser/renderer_host/input/`).
//! [`KeyEncoder::sync_from_modes`] and [`MouseEncoders::sync_from_modes`]
//! configure an encoder exactly as `sync_from_terminal` would.
//!
//! [`KeyEncoder::sync_from_modes`]: crate::KeyEncoder::sync_from_modes
//! [`MouseEncoders::sync_from_modes`]: crate::MouseEncoders::sync_from_modes

use crate::key::{KeyEncoder, KeyInput, Mods};
use crate::mouse::MouseWireFormat;
use crate::sys;
use crate::terminal::{Screen, Terminal};

/// The mouse events an application asked for (DEC 9, 1000, 1002, 1003).
/// Ghostty keeps one last-set-wins value: resetting any of the four modes
/// turns tracking off even while another one's flag is still set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MouseTracking {
    #[default]
    None,
    X10,
    Normal,
    Button,
    Any,
}

impl MouseTracking {
    pub(crate) fn raw(self) -> sys::GhosttyMouseTrackingMode {
        match self {
            Self::None => sys::GHOSTTY_MOUSE_TRACKING_NONE,
            Self::X10 => sys::GHOSTTY_MOUSE_TRACKING_X10,
            Self::Normal => sys::GHOSTTY_MOUSE_TRACKING_NORMAL,
            Self::Button => sys::GHOSTTY_MOUSE_TRACKING_BUTTON,
            Self::Any => sys::GHOSTTY_MOUSE_TRACKING_ANY,
        }
    }
}

impl MouseWireFormat {
    pub(crate) fn raw(self) -> sys::GhosttyMouseFormat {
        match self {
            Self::X10 => sys::GHOSTTY_MOUSE_FORMAT_X10,
            Self::Utf8 => sys::GHOSTTY_MOUSE_FORMAT_UTF8,
            Self::Sgr => sys::GHOSTTY_MOUSE_FORMAT_SGR,
            Self::Urxvt => sys::GHOSTTY_MOUSE_FORMAT_URXVT,
            Self::SgrPixels => sys::GHOSTTY_MOUSE_FORMAT_SGR_PIXELS,
        }
    }

    /// The format's wire number (the order of Ghostty's `GhosttyMouseFormat`).
    pub fn wire_value(self) -> u8 {
        match self {
            Self::X10 => 0,
            Self::Utf8 => 1,
            Self::Sgr => 2,
            Self::Urxvt => 3,
            Self::SgrPixels => 4,
        }
    }

    /// The format with wire number `value`, if there is one.
    pub fn from_wire_value(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::X10,
            1 => Self::Utf8,
            2 => Self::Sgr,
            3 => Self::Urxvt,
            4 => Self::SgrPixels,
            _ => return None,
        })
    }
}

/// A terminal's input modes: what [`KeyEncoder`], the mouse encoders,
/// bracketed paste and focus reports read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputModes {
    flags: u32,
    mouse_format: MouseWireFormat,
}

impl InputModes {
    /// DECCKM (DEC 1): cursor keys send application sequences.
    pub const CURSOR_KEYS: u32 = 1 << 0;
    /// DECKPAM (DEC 66): keypad application mode.
    pub const KEYPAD_APPLICATION: u32 = 1 << 1;
    /// Bracketed paste (DEC 2004).
    pub const BRACKETED_PASTE: u32 = 1 << 2;
    /// Focus reporting (DEC 1004).
    pub const FOCUS_EVENTS: u32 = 1 << 3;
    /// The alternate screen is active.
    pub const ALT_SCREEN: u32 = 1 << 4;
    /// Effective mouse tracking X10 (DEC 9). At most one tracking bit is set.
    pub const MOUSE_X10: u32 = 1 << 5;
    /// Effective mouse tracking normal (DEC 1000).
    pub const MOUSE_NORMAL: u32 = 1 << 6;
    /// Effective mouse tracking button events (DEC 1002).
    pub const MOUSE_BUTTON: u32 = 1 << 7;
    /// Effective mouse tracking any event (DEC 1003).
    pub const MOUSE_ANY: u32 = 1 << 8;
    /// DECTCEM (DEC 25): the cursor is shown.
    pub const CURSOR_VISIBLE: u32 = 1 << 9;
    /// DECBKM (DEC 67): backspace sends BS instead of DEL.
    pub const BACKARROW_SENDS_BS: u32 = 1 << 10;
    /// DEC 1035: the keypad ignores Num Lock.
    pub const IGNORE_KEYPAD_WITH_NUMLOCK: u32 = 1 << 11;
    /// DEC 1036: Alt sends an ESC prefix.
    pub const ALT_ESC_PREFIX: u32 = 1 << 12;
    /// xterm modifyOtherKeys state 2 (`CSI > 4 ; 2 m`).
    pub const MODIFY_OTHER_KEYS_2: u32 = 1 << 13;
    const KITTY_SHIFT: u32 = 16;
    const KITTY_MASK: u32 = 0x1f << Self::KITTY_SHIFT;
    const MOUSE_MASK: u32 =
        Self::MOUSE_X10 | Self::MOUSE_NORMAL | Self::MOUSE_BUTTON | Self::MOUSE_ANY;
    const KNOWN: u32 = ((Self::MODIFY_OTHER_KEYS_2 << 1) - 1) | Self::KITTY_MASK;

    /// DEC private modes stored as one flag each.
    const DEC: [(u16, u32); 8] = [
        (1, Self::CURSOR_KEYS),
        (66, Self::KEYPAD_APPLICATION),
        (2004, Self::BRACKETED_PASTE),
        (1004, Self::FOCUS_EVENTS),
        (25, Self::CURSOR_VISIBLE),
        (67, Self::BACKARROW_SENDS_BS),
        (1035, Self::IGNORE_KEYPAD_WITH_NUMLOCK),
        (1036, Self::ALT_ESC_PREFIX),
    ];

    /// The modes from their stored form, or `None` when `flags` has an
    /// unknown bit or more than one tracking bit (a bad page, never a mode a
    /// terminal can be in).
    pub fn from_parts(flags: u32, mouse_format: MouseWireFormat) -> Option<Self> {
        if flags & !Self::KNOWN != 0 || (flags & Self::MOUSE_MASK).count_ones() > 1 {
            return None;
        }
        Some(Self { flags, mouse_format })
    }

    /// The flag word ([`Self::from_parts`] takes it back).
    pub fn flags(self) -> u32 {
        self.flags
    }

    /// The mouse coordinate wire format (last set wins, as xterm).
    pub fn mouse_format(self) -> MouseWireFormat {
        self.mouse_format
    }

    /// Whether every flag of `bits` is set.
    pub fn contains(self, bits: u32) -> bool {
        self.flags & bits == bits
    }

    /// The Kitty keyboard flags at the top of the active screen's stack.
    pub fn kitty_flags(self) -> u8 {
        ((self.flags & Self::KITTY_MASK) >> Self::KITTY_SHIFT) as u8
    }

    /// The effective mouse tracking.
    pub fn mouse_tracking(self) -> MouseTracking {
        match self.flags & Self::MOUSE_MASK {
            Self::MOUSE_X10 => MouseTracking::X10,
            Self::MOUSE_NORMAL => MouseTracking::Normal,
            Self::MOUSE_BUTTON => MouseTracking::Button,
            Self::MOUSE_ANY => MouseTracking::Any,
            _ => MouseTracking::None,
        }
    }

    /// One word for a lock-free cell: the flags in the low half, the mouse
    /// format above them. [`Self::from_word`] takes it back.
    pub fn to_word(self) -> u64 {
        u64::from(self.flags) | (u64::from(self.mouse_format.wire_value()) << 32)
    }

    /// The modes [`Self::to_word`] stored, or `None` for a word it never makes.
    pub fn from_word(word: u64) -> Option<Self> {
        if word >> 40 != 0 {
            return None;
        }
        let format = MouseWireFormat::from_wire_value((word >> 32) as u8)?;
        Self::from_parts(word as u32, format)
    }

    fn tracking_flag(tracking: MouseTracking) -> u32 {
        match tracking {
            MouseTracking::None => 0,
            MouseTracking::X10 => Self::MOUSE_X10,
            MouseTracking::Normal => Self::MOUSE_NORMAL,
            MouseTracking::Button => Self::MOUSE_BUTTON,
            MouseTracking::Any => Self::MOUSE_ANY,
        }
    }
}

/// Reads [`InputModes`] from a terminal ([`Terminal::input_modes`]). It
/// owns the key encoder that observes modifyOtherKeys state 2, for which
/// libghostty-vt has no getter.
pub(crate) struct InputModesReader {
    probe: KeyEncoder,
    ctrl_a: KeyInput,
    out: Vec<u8>,
}

impl InputModesReader {
    pub(crate) fn new() -> crate::Result<Self> {
        let ctrl_a = KeyInput {
            key: sys::GHOSTTY_KEY_A,
            mods: Mods::CTRL,
            utf8: "a".to_owned(),
            unshifted_codepoint: u32::from('a'),
            ..KeyInput::default()
        };
        Ok(Self { probe: KeyEncoder::new()?, ctrl_a, out: Vec::with_capacity(16) })
    }

    /// `terminal`'s modes now.
    pub(crate) fn read(&mut self, terminal: &Terminal) -> InputModes {
        let mut flags = InputModes::DEC
            .iter()
            .filter(|(mode, _)| terminal.mode(*mode, false))
            .fold(0, |flags, (_, bit)| flags | bit);
        if terminal.active_screen() == Screen::Alternate {
            flags |= InputModes::ALT_SCREEN;
        }
        flags |= InputModes::tracking_flag(terminal.active_mouse_tracking());
        flags |= u32::from(terminal.kitty_keyboard_flags() & 0x1f) << InputModes::KITTY_SHIFT;
        if self.modify_other_keys_2(terminal) {
            flags |= InputModes::MODIFY_OTHER_KEYS_2;
        }
        InputModes { flags, mouse_format: terminal.active_mouse_format() }
    }

    /// Under state 2 the legacy encoder sends Ctrl+A as `CSI 27;5;97~`;
    /// otherwise as 0x01.
    fn modify_other_keys_2(&mut self, terminal: &Terminal) -> bool {
        self.probe.sync_from_terminal(terminal);
        self.probe.set_kitty_flags(0);
        self.out.clear();
        self.probe.encode(&self.ctrl_a, &mut self.out).is_ok() && self.out.starts_with(b"\x1b[27;")
    }
}

#[cfg(test)]
#[path = "input_modes_tests.rs"]
mod tests;
