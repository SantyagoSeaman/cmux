use super::*;
use ghostty_vt::{MouseAction, MouseButton, MouseInput, key_input_from_chord};

const CHORDS: &[&str] = &["up", "home", "f1", "backspace", "ctrl+a", "alt+a", "enter", "a"];

/// A program's mode changes: application cursor keys and keypad, DEL as BS,
/// modifyOtherKeys, Kitty keys, button tracking in SGR, bracketed paste.
const MODES: &[u8] =
    b"\x1b[?1h\x1b=\x1b[?67h\x1b[>4;2m\x1b[>1u\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1004h";

fn encode_keys(encoder: &mut KeyEncoder) -> Vec<u8> {
    let mut out = Vec::new();
    for chord in CHORDS {
        encoder.encode(&key_input_from_chord(chord).expect(chord), &mut out).unwrap();
    }
    out
}

fn reference_keys(bytes: &[u8]) -> Vec<u8> {
    let mut terminal = Terminal::new(80, 24, 0, Default::default()).unwrap();
    terminal.vt_write(bytes);
    let mut encoder = KeyEncoder::new().unwrap();
    encoder.sync_from_terminal(&terminal);
    encode_keys(&mut encoder)
}

fn press() -> MouseInput {
    MouseInput {
        action: MouseAction::Press,
        button: Some(MouseButton::Left),
        mods: ghostty_vt::Mods::default(),
        position: (20.5, 40.5),
        screen_size: (640, 384),
        cell_size: (8, 16),
        any_button_pressed: true,
    }
}

/// The daemon's parser publishes its modes; a key encoder made from them
/// sends what one synced from the terminal sends, while another thread
/// holds the terminal lock (as the parser does through a large write).
#[test]
fn key_encoding_reads_published_modes_without_the_terminal_lock() {
    let mux = Mux::new_for_test("input-modes-published", SurfaceOptions::default());
    let surface =
        Surface::spawn_for_test(1, SurfaceOptions::default(), Arc::downgrade(&mux)).unwrap();
    surface.with_terminal(|term| term.vt_write(MODES));
    let modes = surface.input_modes().unwrap();
    assert!(modes.contains(InputModes::CURSOR_KEYS | InputModes::BRACKETED_PASTE));
    assert_eq!(modes.kitty_flags(), 1);

    let pty = surface.as_pty().unwrap();
    let held = pty.term.lock().recover_poison();
    let mut encoder = surface.key_encoder().unwrap();
    assert_eq!(encode_keys(&mut encoder), reference_keys(MODES));
    let mut out = Vec::new();
    assert!(surface.encode_mouse(press(), &mut out).unwrap().is_ok());
    assert_eq!(out, b"\x1b[<0;3;3M");
    drop(held);
}

/// Bracketed paste follows the published mode.
#[test]
fn paste_brackets_from_published_modes() {
    let mux = Mux::new_for_test("input-modes-paste", SurfaceOptions::default());
    let surface =
        Surface::spawn_for_test(1, SurfaceOptions::default(), Arc::downgrade(&mux)).unwrap();
    let written = surface.capture_pty_input_for_test();
    surface.write_paste(b"one").unwrap();
    surface.with_terminal(|term| term.vt_write(b"\x1b[?2004h"));
    surface.write_paste(b"two").unwrap();
    assert_eq!(written.written(), b"one\x1b[200~two\x1b[201~");
}

/// With a VT worker the modes come from its screen page (seqlock read):
/// the encoders send exactly what encoders synced from the parser send.
#[cfg(unix)]
#[test]
fn page_backed_modes_encode_like_the_parser() {
    use cmux_vt_worker::screen_page::{
        PageEncoder, PageMeta, PageWriter, create_page, page_len_for,
    };

    let mut terminal = Terminal::new(80, 24, 0, Default::default()).unwrap();
    terminal.vt_write(MODES);
    let len = page_len_for(80, 24);
    let fd = create_page(len).unwrap();
    let reader = PageReader::open(&fd, len).unwrap();
    let mut writer = PageWriter::open(fd, len).unwrap();

    let published = TerminalInputModes::new(InputModes::default());
    assert_eq!(published.publish_from_page(&reader), Err(PageError::NotPublished));
    assert_eq!(published.load(), InputModes::default());

    let mut page = PageEncoder::default();
    page.begin();
    let image = page.finish(&PageMeta {
        parsed: MODES.len() as u64,
        frame: 1,
        cols: 80,
        cursor: None,
        cursor_visual: (CursorShape::Block, false),
        modes: terminal.input_modes(),
    });
    writer.publish(image);
    assert_eq!(published.publish_from_page(&reader), Ok(terminal.input_modes()));

    let mut from_page = KeyEncoder::new().unwrap();
    from_page.sync_from_modes(published.load());
    assert_eq!(encode_keys(&mut from_page), reference_keys(MODES));

    let mut mouse_from_page = MouseEncoders::new().unwrap();
    mouse_from_page.sync_from_modes(published.load());
    let mut mouse_from_terminal = MouseEncoders::new().unwrap();
    mouse_from_terminal.sync_from_terminal(&terminal);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    mouse_from_page.encode(press(), &mut a).unwrap();
    mouse_from_terminal.encode(press(), &mut b).unwrap();
    assert_eq!(a, b);
    assert!(!a.is_empty());
}
