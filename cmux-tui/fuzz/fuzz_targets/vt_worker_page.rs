//! The screen page a sandboxed VT worker writes and the daemon reads
//! (`cmux_vt_worker::screen_page`, p01-process-141). A compromised worker
//! can store any bytes in its page, so the input is the page image: it goes
//! through `decode` directly and through a real shared mapping (the worker's
//! `PageWriter`, the daemon's `PageReader` and `WorkerOutputGuard`).
//!
//! Properties, beyond "no crash, out-of-bounds read, leak or hang":
//! - a page that decodes re-encodes to an image that decodes to the same
//!   screen;
//! - the mapped read decodes the same as `decode` on the stored image;
//! - a screen never has more rows than it says, or a row wider than its
//!   columns.

#![no_main]

use std::sync::{Mutex, OnceLock};

use cmux_tui_core::vt_worker_runtime::{HelloExpectation, PageRead, WorkerOutputGuard};
use cmux_vt_worker::sandbox::SandboxMechanism;
use cmux_vt_worker::screen_page::{
    self, PageEncoder, PageMeta, PageReader, PageWriter, Screen, decode,
};
use cmux_vt_worker::wire::Geometry;
use libfuzzer_sys::fuzz_target;

/// One page for every input: 64 KiB, above libFuzzer's default -max_len.
const PAGE_BYTES: usize = 64 << 10;

struct Page {
    writer: PageWriter,
    reader: PageReader,
    guard: WorkerOutputGuard,
}

fn page() -> &'static Mutex<Page> {
    static PAGE: OnceLock<Mutex<Page>> = OnceLock::new();
    PAGE.get_or_init(|| {
        let fd = screen_page::create_page(PAGE_BYTES).expect("page");
        let reader = PageReader::open(&fd, PAGE_BYTES).expect("reader");
        let writer = PageWriter::open(fd, PAGE_BYTES).expect("writer");
        let mut guard =
            WorkerOutputGuard::new(HelloExpectation { sandbox: SandboxMechanism::None, pid: None });
        let grid = screen_page::MAX_GRID;
        guard.attached(1, 0, Geometry { cols: grid, rows: grid, cell_width: 1, cell_height: 1 });
        guard.advanced(1, u64::MAX);
        Mutex::new(Page { writer, reader, guard })
    })
}

fn reencode(screen: &Screen) -> Vec<u8> {
    let mut encoder = PageEncoder::default();
    encoder.begin();
    for row in &screen.cells {
        encoder.push_row(row);
    }
    let meta = PageMeta {
        parsed: screen.parsed,
        frame: screen.frame,
        cols: screen.cols,
        cursor: screen.cursor,
        cursor_visual: screen.cursor_visual,
        modes: screen.modes,
        mouse_format: screen.mouse_format,
    };
    encoder.finish(&meta).to_vec()
}

fn check_shape(screen: &Screen) {
    assert!(screen.cells.len() <= usize::from(screen.rows), "more rows than the header says");
    for row in &screen.cells {
        assert!(row.len() <= usize::from(screen.cols), "a row wider than the grid");
    }
}

fuzz_target!(|data: &[u8]| {
    let direct = decode(data);
    if let Ok(screen) = &direct {
        check_shape(screen);
        let again = decode(&reencode(screen)).expect("a decoded screen re-encodes");
        assert_eq!(&again, screen, "a screen changes when re-encoded");
    }

    // The writer stores whole words and owns the sequence word.
    let mut image = data.to_vec();
    image.resize(image.len().next_multiple_of(8), 0);
    if let Some(seq) = image.get_mut(8..16) {
        seq.fill(0);
    }
    let mut page = page().lock().expect("page");
    let Page { writer, reader, guard } = &mut *page;
    writer.publish(&image);
    match guard.read_page(1, reader) {
        Ok(PageRead::Screen(screen)) => {
            check_shape(&screen);
            assert_eq!(decode(&image).as_ref(), Ok(&screen), "the mapped read differs");
        }
        Ok(PageRead::NotPublished | PageRead::Busy | PageRead::TooSmall { .. }) | Err(_) => {}
    }
});
