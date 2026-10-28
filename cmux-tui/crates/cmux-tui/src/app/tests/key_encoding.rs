//! Tests: the in-process TUI encodes keys and pastes from the surface's
//! published input modes, never under the core terminal lock
//! (p01-process-209).

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ghostty_vt::{InputModes, KeyInput};

use super::*;

fn up_arrow() -> KeyInput {
    crate::keys::key_input_from(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)).unwrap()
}

#[test]
fn a_key_encodes_while_another_thread_holds_the_terminal_lock() {
    let mux = Mux::new("key-encode-lock-free-test", SurfaceOptions::default());
    let surface = mux.new_workspace(None, Some((80, 24))).unwrap();
    // DECCKM (application cursor keys) and bracketed paste.
    surface.with_terminal(|terminal| terminal.vt_write(b"\x1b[?1h\x1b[?2004h"));
    let handle = SurfaceHandle::Local(surface.clone(), mux.clone());
    let mut app = test_app(Session::Local(mux.clone()));

    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = thread::spawn({
        let surface = surface.clone();
        move || {
            surface
                .with_terminal(|_| {
                    locked_tx.send(()).unwrap();
                    // The test releases it; the timeout ends the hold when
                    // the encode waited for this lock instead.
                    release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
                })
                .unwrap()
        }
    });
    locked_rx.recv().unwrap();

    app.encode_buf.clear();
    assert!(app.encode_terminal_key(&handle, &up_arrow()));
    let modes = handle.input_modes().unwrap();
    release_tx.send(()).unwrap();

    assert!(holder.join().unwrap(), "the key encode waited for the terminal lock");
    assert_eq!(app.encode_buf, b"\x1bOA", "DECCKM comes from the published modes");
    assert!(modes.contains(InputModes::BRACKETED_PASTE), "paste reads the published modes");
    mux.close_surface(surface.id).unwrap();
}

/// DECBKM (mode 67), which no shell's line editor touches: the published
/// modes follow each change, set and reset.
#[test]
fn a_key_encodes_with_the_modes_the_parser_last_set() {
    let mux = Mux::new("key-encode-mode-change-test", SurfaceOptions::default());
    let surface = mux.new_workspace(None, Some((80, 24))).unwrap();
    let handle = SurfaceHandle::Local(surface.clone(), mux.clone());
    let mut app = test_app(Session::Local(mux.clone()));
    let backspace =
        crate::keys::key_input_from(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .unwrap();

    surface.with_terminal(|terminal| terminal.vt_write(b"\x1b[?67h"));
    app.encode_buf.clear();
    assert!(app.encode_terminal_key(&handle, &backspace));
    assert_eq!(app.encode_buf, b"\x08");

    surface.with_terminal(|terminal| terminal.vt_write(b"\x1b[?67l"));
    app.encode_buf.clear();
    assert!(app.encode_terminal_key(&handle, &backspace));
    assert_eq!(app.encode_buf, b"\x7f");
    mux.close_surface(surface.id).unwrap();
}
