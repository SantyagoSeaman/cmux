//! A hidden view's output in batches (p14-energy-119): the queue is read
//! with `try_recv_at`, so the batch clock is the test's own.

use super::*;
use crate::wake::EnergyMode;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

fn surface(name: &str) -> (Arc<Mux>, Arc<Surface>) {
    let mux = Mux::new_for_test(name, SurfaceOptions::default());
    let surface =
        Surface::spawn_for_test(1, SurfaceOptions::default(), Arc::downgrade(&mux)).unwrap();
    (mux, surface)
}

fn output_of(frame: AttachFrame) -> Vec<u8> {
    frame.output().expect("an output frame").to_vec()
}

fn write_fifty(surface: &Surface) -> Vec<u8> {
    let mut written = Vec::new();
    for index in 0..50 {
        let line = format!("line-{index}\r\n");
        surface.apply_local_pty_output_for_test(line.as_bytes()).unwrap();
        written.extend_from_slice(line.as_bytes());
    }
    written
}

#[test]
fn terminal_stream_hidden_view_gets_fifty_writes_in_one_second_as_one_frame() {
    let (_mux, surface) = surface("hidden-batch-fifty");
    let attach = surface.attach_stream().unwrap();
    attach.stream.hidden_output().hide(&EnergyMode::new(false));
    let start = Instant::now();
    let written = write_fifty(&surface);

    assert!(matches!(attach.stream.try_recv_at(start), Err(TryRecvError::Empty)));
    let before_due = start + HIDDEN_FLUSH_INTERVAL - Duration::from_millis(1);
    assert!(matches!(attach.stream.try_recv_at(before_due), Err(TryRecvError::Empty)));
    let due = Instant::now() + HIDDEN_FLUSH_INTERVAL;
    assert_eq!(output_of(attach.stream.try_recv_at(due).unwrap()), written, "one frame");
    assert!(matches!(attach.stream.try_recv_at(due), Err(TryRecvError::Empty)));

    // The next output opens a new batch.
    surface.apply_local_pty_output_for_test(b"next").unwrap();
    assert!(matches!(attach.stream.try_recv_at(due), Err(TryRecvError::Empty)));
    let next_due = Instant::now() + HIDDEN_FLUSH_INTERVAL;
    assert_eq!(output_of(attach.stream.try_recv_at(next_due).unwrap()), b"next");
}

#[test]
fn terminal_stream_hidden_view_waits_four_seconds_in_energy_saver() {
    let (_mux, surface) = surface("hidden-batch-saver");
    let attach = surface.attach_stream().unwrap();
    attach.stream.hidden_output().hide(&EnergyMode::new(true));
    surface.apply_local_pty_output_for_test(b"saver").unwrap();
    let after = Instant::now();
    assert!(matches!(
        attach.stream.try_recv_at(after + HIDDEN_FLUSH_INTERVAL),
        Err(TryRecvError::Empty)
    ));
    assert_eq!(
        output_of(attach.stream.try_recv_at(after + HIDDEN_SAVER_FLUSH_INTERVAL).unwrap()),
        b"saver"
    );
}

#[test]
fn terminal_stream_hidden_view_sends_at_256_kb() {
    let (_mux, surface) = surface("hidden-batch-bytes");
    let attach = surface.attach_stream().unwrap();
    attach.stream.hidden_output().hide(&EnergyMode::new(false));
    let start = Instant::now();
    let chunk = vec![b'x'; 16 * 1024];
    for _ in 0..(HIDDEN_FLUSH_BYTES / chunk.len()) - 1 {
        surface.apply_local_pty_output_for_test(&chunk).unwrap();
    }
    assert!(matches!(attach.stream.try_recv_at(start), Err(TryRecvError::Empty)));
    surface.apply_local_pty_output_for_test(&chunk).unwrap();
    let frame = attach.stream.try_recv_at(start).expect("sent at 256 KB, before the interval");
    assert_eq!(output_of(frame).len(), HIDDEN_FLUSH_BYTES);
}

#[test]
fn terminal_stream_show_sends_the_held_output_in_the_same_turn() {
    let (_mux, surface) = surface("hidden-batch-show");
    let attach = surface.attach_stream().unwrap();
    let hidden = attach.stream.hidden_output();
    hidden.hide(&EnergyMode::new(false));
    let start = Instant::now();
    let written = write_fifty(&surface);
    assert!(matches!(attach.stream.try_recv_at(start), Err(TryRecvError::Empty)));
    hidden.show();
    assert!(!hidden.is_hidden());
    assert_eq!(output_of(attach.stream.try_recv_at(start).unwrap()), written);
    // Visible again: each write is sent at once.
    surface.apply_local_pty_output_for_test(b"live").unwrap();
    assert_eq!(output_of(attach.stream.try_recv_at(start).unwrap()), b"live");
}

#[test]
fn terminal_stream_visible_view_is_unchanged() {
    let (_mux, surface) = surface("hidden-batch-visible");
    let attach = surface.attach_stream().unwrap();
    let start = Instant::now();
    surface.apply_local_pty_output_for_test(b"one").unwrap();
    assert_eq!(output_of(attach.stream.try_recv_at(start).unwrap()), b"one");
    surface.apply_local_pty_output_for_test(b"two").unwrap();
    assert_eq!(output_of(attach.stream.try_recv_at(start).unwrap()), b"two");
}

#[test]
fn terminal_stream_hiding_sends_what_was_queued_while_visible() {
    let (_mux, surface) = surface("hidden-batch-queued");
    let attach = surface.attach_stream().unwrap();
    let start = Instant::now();
    surface.apply_local_pty_output_for_test(b"before").unwrap();
    attach.stream.hidden_output().hide(&EnergyMode::new(false));
    surface.apply_local_pty_output_for_test(b"-after").unwrap();
    // The queue was not empty at hide: it drains whole, then batches.
    assert_eq!(output_of(attach.stream.try_recv_at(start).unwrap()), b"before-after");
    surface.apply_local_pty_output_for_test(b"held").unwrap();
    assert!(matches!(attach.stream.try_recv_at(start), Err(TryRecvError::Empty)));
}

#[test]
fn terminal_stream_hidden_view_sends_a_grid_change_at_once() {
    let (_mux, surface) = surface("hidden-batch-resize");
    let attach = surface.attach_stream().unwrap();
    attach.stream.hidden_output().hide(&EnergyMode::new(false));
    let start = Instant::now();
    surface.apply_local_pty_output_for_test(b"held").unwrap();
    surface
        .as_pty()
        .unwrap()
        .broadcast_attach_frame(AttachFrame::ColorsChanged(Arc::new(TerminalColors::default())));
    assert_eq!(output_of(attach.stream.try_recv_at(start).unwrap()), b"held");
    assert!(matches!(attach.stream.try_recv_at(start).unwrap(), AttachFrame::ColorsChanged(_)));
}

/// A local terminal whose child is the other end of a socket pair.
fn local_surface(mux: Weak<Mux>, id: SurfaceId) -> (Arc<Surface>, UnixStream, impl FnOnce()) {
    let (reader, child) = UnixStream::pair().unwrap();
    let master = File::from(OwnedFd::from(reader.try_clone().unwrap()));
    let geometry = PtyGeometry { cols: 40, rows: 6, cell_width: 8, cell_height: 16 };
    let (end_tx, end_rx) = std::sync::mpsc::channel::<()>();
    let surface = Surface::spawn_local(
        LocalSpawn {
            id,
            opts: SurfaceOptions { cols: 40, rows: 6, ..SurfaceOptions::default() },
            mux,
            terminal_public_id: None,
            kitty_reservation: None,
            initial_kitty_limits: KittyGraphicsLimits::disabled(),
            resource_identity: None,
            lifetime: PtyLifetime::DaemonOwned,
            cell_pixels: (8, 16),
            initial_geometry: geometry,
            host_identity: None,
        },
        LocalLaunch {
            master: Box::new(FdMasterPty { file: master, size: Mutex::new(geometry.pty_size()) }),
            reader: Box::new(reader),
            writer: Box::new(std::io::sink()),
            killer: Box::new(TestChildKiller),
            wait: Box::new(move || {
                let _ = end_rx.recv();
                TerminalEnd::ProcessEnded(TerminalExit::unknown("test ended"))
            }),
            pid: None,
            command: Vec::new(),
            cwd: None,
            supports_clear_history_key_fallback: false,
            holder: None,
        },
    )
    .unwrap();
    (surface, child, move || {
        let _ = end_tx.send(());
    })
}

/// The output `attach` gets by `at`, a time before its batch is due, within
/// 10 s of real time: only an urgent flush can send it.
fn flushed_before_due(attach: &AttachStream, at: Instant) -> Option<Vec<u8>> {
    attach.stream.recv_at(at, Duration::from_secs(10)).ok().map(output_of)
}

/// Wait until the terminal's stream revision reaches `revision`.
fn wait_for_revision(surface: &Surface, revision: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = surface.terminal_stream_revision().unwrap();
    while seen < revision {
        seen = surface
            .wait_for_terminal_stream_change(seen, Some(deadline))
            .unwrap()
            .expect("the terminal parsed the output");
    }
}

#[test]
fn terminal_stream_hidden_view_gets_a_bell_title_and_exit_at_once() {
    // No mux: a bell's attention notification is a durable write the read
    // loop may not make in a test; the batch is the attach queue's alone.
    let (surface, mut child, end) = local_surface(Weak::new(), 4251);
    let attach = surface.attach_stream().unwrap();
    attach.stream.hidden_output().hide(&EnergyMode::new(false));
    let start = Instant::now();

    let revision = surface.terminal_stream_revision().unwrap();
    child.write_all(b"quiet").unwrap();
    wait_for_revision(&surface, revision + 1);
    assert!(matches!(attach.stream.try_recv_at(start), Err(TryRecvError::Empty)), "held");

    child.write_all(b"\x07").unwrap();
    assert_eq!(flushed_before_due(&attach, start).as_deref(), Some(&b"quiet\x07"[..]), "bell");

    child.write_all(b"\x1b]2;hidden-title\x1b\\").unwrap();
    let title = flushed_before_due(&attach, start).expect("a title change is sent at once");
    assert!(title.windows(12).any(|window| window == b"hidden-title"), "{title:?}");

    let revision = surface.terminal_stream_revision().unwrap();
    child.write_all(b"tail").unwrap();
    wait_for_revision(&surface, revision + 1);
    drop(child);
    end();
    let pty = surface.as_pty().unwrap();
    assert!(pty.reader_completion.wait_until(Instant::now() + Duration::from_secs(10)));
    // The terminal ended: what was held goes out (with the exit's mode
    // reset), then the stream ends.
    let tail = flushed_before_due(&attach, start).expect("exit sends the held output");
    assert!(tail.starts_with(b"tail"), "{tail:?}");
}
