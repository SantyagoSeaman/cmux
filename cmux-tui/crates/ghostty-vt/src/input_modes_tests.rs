use super::*;
use crate::key::{KeyAction, key_input_from_chord};
use crate::mouse::{MouseAction, MouseButton, MouseEncoders, MouseInput};
use crate::terminal::Callbacks;

fn terminal() -> Terminal {
    Terminal::new(200, 40, 0, Callbacks::default()).unwrap()
}

/// Mode changes a program makes, applied one after another.
const SEQUENCES: &[&[u8]] = &[
    b"",
    b"\x1b[?1h",
    b"\x1b=",
    b"\x1b[?67h",
    b"\x1b[?1035h",
    b"\x1b[?1036h",
    b"\x1b[>4;2m",
    b"\x1b[>1u",
    b"\x1b[>31u",
    b"\x1b[<u",
    b"\x1b[=5;1u",
    b"\x1b[?1049h",
    b"\x1b[>7u",
    b"\x1b[?1049l",
    b"\x1b[?9h",
    b"\x1b[?1000h",
    b"\x1b[?1006h",
    b"\x1b[?1002h",
    b"\x1b[?1003h\x1b[?1005h",
    b"\x1b[?1016h",
    b"\x1b[?1015h",
    // Tracking off with the 1000 and 1002 flags still set.
    b"\x1b[?1003l",
    b"\x1b[?1000h\x1b[?1002l",
    b"\x1b[?2004h\x1b[?1004h\x1b[?25l",
    b"\x1b[>4;0m\x1b[?1l\x1b>",
    b"\x1b[<99u\x1b[?67l\x1b[?1036l",
    b"\x1bc",
];

fn keys() -> Vec<KeyInput> {
    let mut keys: Vec<KeyInput> = [
        "up",
        "left",
        "home",
        "end",
        "f1",
        "f5",
        "backspace",
        "enter",
        "tab",
        "backtab",
        "escape",
        "a",
        "shift+a",
        "ctrl+a",
        "alt+a",
        "ctrl+1",
        "ctrl+space",
        "delete",
        "pageup",
    ]
    .iter()
    .map(|chord| key_input_from_chord(chord).unwrap())
    .collect();
    for key in [sys::GHOSTTY_KEY_NUMPAD_5, sys::GHOSTTY_KEY_NUMPAD_ENTER, sys::GHOSTTY_KEY_NUMPAD_1]
    {
        keys.push(KeyInput { key, action: Some(KeyAction::Press), ..KeyInput::default() });
        keys.push(KeyInput { key, mods: Mods::NUM_LOCK, ..KeyInput::default() });
    }
    keys.push(KeyInput {
        key: sys::GHOSTTY_KEY_A,
        mods: Mods::CTRL,
        utf8: "a".to_owned(),
        unshifted_codepoint: u32::from('a'),
        ..KeyInput::default()
    });
    keys.push(KeyInput {
        key: sys::GHOSTTY_KEY_A,
        utf8: "a".to_owned(),
        unshifted_codepoint: u32::from('a'),
        action: Some(KeyAction::Release),
        ..KeyInput::default()
    });
    keys
}

fn mice() -> Vec<MouseInput> {
    let at = |action, button, x: f32, any_button_pressed| MouseInput {
        action,
        button,
        mods: Mods::default(),
        position: (x, 100.5),
        screen_size: (1600, 640),
        cell_size: (8, 16),
        any_button_pressed,
    };
    vec![
        at(MouseAction::Press, Some(MouseButton::Left), 20.5, true),
        at(MouseAction::Motion, Some(MouseButton::Left), 60.5, true),
        at(MouseAction::Release, Some(MouseButton::Left), 60.5, false),
        at(MouseAction::Motion, None, 1300.5, false),
        at(MouseAction::Press, Some(MouseButton::WheelUp), 1300.5, false),
        at(MouseAction::Press, Some(MouseButton::Right), 1300.5, true),
        at(MouseAction::Press, Some(MouseButton::Middle), 900.5, true),
    ]
}

fn key_bytes(encoder: &mut KeyEncoder, keys: &[KeyInput]) -> Vec<Vec<u8>> {
    keys.iter()
        .map(|key| {
            let mut out = Vec::new();
            encoder.encode(key, &mut out).unwrap();
            out
        })
        .collect()
}

fn mouse_bytes(encoders: &mut MouseEncoders, mice: &[MouseInput]) -> Vec<Vec<u8>> {
    mice.iter()
        .map(|input| {
            let mut out = Vec::new();
            encoders.encode(*input, &mut out).unwrap();
            encoders.encode_release(*input, &mut out).unwrap();
            out
        })
        .collect()
}

/// Encoders configured from published modes send exactly the bytes of
/// encoders configured from the live terminal, after every mode change.
#[test]
fn encoders_synced_from_modes_match_encoders_synced_from_the_terminal() {
    let keys = keys();
    let mice = mice();
    let mut terminal = terminal();
    let mut reader = InputModesReader::new().unwrap();
    let mut from_terminal = KeyEncoder::new().unwrap();
    let mut from_modes = KeyEncoder::new().unwrap();
    let mut mouse_from_terminal = MouseEncoders::new().unwrap();
    let mut mouse_from_modes = MouseEncoders::new().unwrap();
    for sequence in SEQUENCES {
        terminal.vt_write(sequence);
        let modes = reader.read(&terminal);
        assert_eq!(InputModes::from_word(modes.to_word()), Some(modes));
        from_terminal.sync_from_terminal(&terminal);
        from_modes.sync_from_modes(modes);
        let label = String::from_utf8_lossy(sequence);
        assert_eq!(
            key_bytes(&mut from_terminal, &keys),
            key_bytes(&mut from_modes, &keys),
            "keys after {label:?} ({modes:?})"
        );
        mouse_from_terminal.sync_from_terminal(&terminal);
        mouse_from_modes.sync_from_modes(modes);
        assert_eq!(
            mouse_bytes(&mut mouse_from_terminal, &mice),
            mouse_bytes(&mut mouse_from_modes, &mice),
            "mouse after {label:?} ({modes:?})"
        );
    }
}

#[test]
fn modes_name_each_input_state() {
    let mut terminal = terminal();
    let mut reader = InputModesReader::new().unwrap();
    // Ghostty's defaults: a shown cursor, the keypad ignoring Num Lock, Alt as ESC.
    assert_eq!(
        reader.read(&terminal).flags(),
        InputModes::CURSOR_VISIBLE
            | InputModes::IGNORE_KEYPAD_WITH_NUMLOCK
            | InputModes::ALT_ESC_PREFIX
    );
    terminal
        .vt_write(b"\x1b[?1h\x1b=\x1b[?2004h\x1b[?1004h\x1b[>4;2m\x1b[>5u\x1b[?1002h\x1b[?1006h");
    let modes = reader.read(&terminal);
    for bit in [
        InputModes::CURSOR_KEYS,
        InputModes::KEYPAD_APPLICATION,
        InputModes::BRACKETED_PASTE,
        InputModes::FOCUS_EVENTS,
        InputModes::MODIFY_OTHER_KEYS_2,
        InputModes::MOUSE_BUTTON,
    ] {
        assert!(modes.contains(bit), "{bit:#x} in {modes:?}");
    }
    assert_eq!(modes.kitty_flags(), 5);
    assert_eq!(modes.mouse_tracking(), MouseTracking::Button);
    assert_eq!(modes.mouse_format(), MouseWireFormat::Sgr);
    terminal.vt_write(b"\x1b[?1049h");
    let alt = reader.read(&terminal);
    assert!(alt.contains(InputModes::ALT_SCREEN));
    // Each screen has its own Kitty stack.
    assert_eq!(alt.kitty_flags(), 0);
    terminal.vt_write(b"\x1b[>4;0m\x1b[?1000l");
    let modes = reader.read(&terminal);
    assert!(!modes.contains(InputModes::MODIFY_OTHER_KEYS_2));
    assert_eq!(modes.mouse_tracking(), MouseTracking::None);
}

/// Resetting a tracking mode that is not on turns tracking off while
/// another mode's flag stays set: the published tracking follows Ghostty.
#[test]
fn tracking_is_the_last_set_value_not_the_flags() {
    let mut terminal = terminal();
    let mut reader = InputModesReader::new().unwrap();
    terminal.vt_write(b"\x1b[?1000h");
    assert_eq!(reader.read(&terminal).mouse_tracking(), MouseTracking::Normal);
    let before = terminal.pointer_semantic_snapshot();
    terminal.vt_write(b"\x1b[?1002l");
    assert!(terminal.mode(1000, false));
    assert_eq!(reader.read(&terminal).mouse_tracking(), MouseTracking::None);
    assert_ne!(terminal.pointer_semantic_snapshot(), before, "the revision moved");
}

#[test]
fn stored_forms_reject_states_a_terminal_cannot_be_in() {
    let format = MouseWireFormat::Sgr;
    assert!(
        InputModes::from_parts(InputModes::MOUSE_X10 | InputModes::MOUSE_ANY, format).is_none()
    );
    assert!(InputModes::from_parts(1 << 14, format).is_none());
    assert!(InputModes::from_parts(1 << 31, format).is_none());
    let modes = InputModes::from_parts(InputModes::MOUSE_ANY | (31 << 16), format).unwrap();
    assert_eq!(modes.kitty_flags(), 31);
    assert_eq!(InputModes::from_word(modes.to_word()), Some(modes));
    assert!(InputModes::from_word(5 << 32).is_none());
    assert!(InputModes::from_word(1 << 40).is_none());
}
