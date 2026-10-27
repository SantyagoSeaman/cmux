use super::*;

fn cell(text: &str) -> Cell {
    Cell { text: text.to_owned(), ..Cell::default() }
}

fn meta(cols: u16) -> PageMeta {
    PageMeta {
        parsed: 42,
        frame: 7,
        cols,
        cursor: Some(CursorInfo { x: 1, y: 0, shape: CursorShape::Bar, blinking: true }),
        cursor_visual: (CursorShape::Bar, true),
        modes: Modes(Modes::CURSOR_KEYS | Modes::BRACKETED_PASTE),
        mouse_format: MouseWireFormat::Sgr,
    }
}

fn rows() -> Vec<Vec<Cell>> {
    let styled = Cell {
        text: "é".to_owned(),
        width: CellWidth::Narrow,
        fg: ColorSpec::Palette(196),
        bg: ColorSpec::Rgb(Rgb { r: 1, g: 2, b: 3 }),
        resolved_fg: Some(Rgb { r: 255, g: 0, b: 0 }),
        resolved_bg: Some(Rgb { r: 1, g: 2, b: 3 }),
        bold: true,
        italic: true,
        underline: true,
        underline_style: Some(UnderlineStyle::Curly),
        inverse: true,
        ..Cell::default()
    };
    let wide = Cell { text: "漢".to_owned(), width: CellWidth::Wide, ..Cell::default() };
    let tail = Cell { width: CellWidth::SpacerTail, ..Cell::default() };
    vec![vec![cell("a"), styled, wide, tail], vec![cell(""), cell("👍🏽"), cell(" "), cell("z")]]
}

fn image(encoder: &mut PageEncoder, rows: &[Vec<Cell>], cols: u16) -> Vec<u8> {
    encoder.begin();
    for row in rows {
        encoder.push_row(row);
    }
    encoder.finish(&meta(cols)).to_vec()
}

fn page(len: usize) -> (PageWriter, PageReader) {
    let fd = create_page(len).expect("page");
    let reader = PageReader::open(&fd, len).expect("reader");
    (PageWriter::open(fd, len).expect("writer"), reader)
}

#[test]
fn a_published_screen_reads_back_equal() {
    let (mut writer, mut reader) = page(page_len_for(4, 2));
    assert_eq!(reader.read(), Err(PageError::NotPublished));
    let rows = rows();
    let image = image(&mut PageEncoder::default(), &rows, 4);
    assert_eq!(writer.publish(&image), Published::Screen);
    let screen = reader.read().expect("screen");
    assert_eq!(screen.cells, rows);
    assert_eq!((screen.cols, screen.rows, screen.parsed, screen.frame), (4, 2, 42, 7));
    assert_eq!(screen.cursor, meta(4).cursor);
    assert_eq!(screen.cursor_visual, (CursorShape::Bar, true));
    assert!(screen.modes.contains(Modes::CURSOR_KEYS));
    assert!(!screen.modes.contains(Modes::ALT_SCREEN));
    assert_eq!(screen.mouse_format, MouseWireFormat::Sgr);
}

#[test]
fn a_later_publish_replaces_the_screen() {
    let (mut writer, mut reader) = page(page_len_for(4, 2));
    let mut encoder = PageEncoder::default();
    let first = image(&mut encoder, &rows(), 4);
    writer.publish(&first);
    let second = image(&mut encoder, &[vec![cell("q")]], 4);
    writer.publish(&second);
    assert_eq!(reader.read().expect("screen").cells, vec![vec![cell("q")]]);
}

#[test]
fn a_page_too_small_says_how_many_bytes_it_needs() {
    let (mut writer, mut reader) = page(PAGE_HEADER + 64);
    let image = image(&mut PageEncoder::default(), &rows(), 4);
    let needed = image.len();
    assert_eq!(writer.publish(&image), Published::TooSmall { needed });
    assert_eq!(reader.read(), Err(PageError::TooSmall { needed: needed as u64 }));
}

#[test]
fn a_reopened_writer_continues_the_sequence() {
    let len = page_len_for(4, 2);
    let fd = create_page(len).expect("page");
    let dup = fd.try_clone().expect("dup");
    let mut reader = PageReader::open(&fd, len).expect("reader");
    let mut first = PageWriter::open(fd, len).expect("writer");
    let image = image(&mut PageEncoder::default(), &rows(), 4);
    first.publish(&image);
    let mut second = PageWriter::open(dup, len).expect("second writer");
    assert_eq!(second.seq, first.seq);
    let mut encoder = PageEncoder::default();
    encoder.begin();
    let later = PageMeta { parsed: 99, cursor: None, ..meta(4) };
    second.publish(encoder.finish(&later));
    assert_eq!(reader.read().expect("screen").parsed, 99);
}

#[test]
fn corrupt_headers_and_cells_are_errors() {
    let good = image(&mut PageEncoder::default(), &rows(), 4);
    assert!(decode(&good).is_ok());
    let cases: [(usize, &[u8], &str); 6] = [
        (0, b"XXXX", "magic"),
        (4, &9u32.to_le_bytes(), "layout"),
        (16, &[9], "status"),
        (40, &5000u16.to_le_bytes(), "grid size"),
        (48, &u32::MAX.to_le_bytes(), "cell count"),
        // The first cell's text offset, far outside the text.
        (PAGE_HEADER + 8, &0xffff_u32.to_le_bytes(), "cell text outside the text"),
    ];
    for (at, bytes, what) in cases {
        let mut bad = good.clone();
        bad[at..at + bytes.len()].copy_from_slice(bytes);
        assert!(matches!(decode(&bad), Err(PageError::Corrupt(_))), "{what}: {:?}", decode(&bad));
    }
    // A row longer than the grid is wide.
    let mut bad = good.clone();
    bad[40..42].copy_from_slice(&3u16.to_le_bytes());
    assert_eq!(decode(&bad), Err(PageError::Corrupt("row length")));
    // A cursor outside the grid.
    let mut bad = good;
    bad[44..46].copy_from_slice(&9u16.to_le_bytes());
    assert_eq!(decode(&bad), Err(PageError::Corrupt("cursor outside the grid")));
}

#[test]
fn mutated_pages_never_read_out_of_bounds() {
    let good = image(&mut PageEncoder::default(), &rows(), 4);
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let mut bad = good.clone();
        for _ in 0..1 + next() % 4 {
            let at = (next() as usize) % bad.len();
            bad[at] = next() as u8;
        }
        let cut = (next() as usize) % (bad.len() + 1);
        let _ = decode(&bad);
        let _ = decode(&bad[..cut]);
    }
}

/// Byte offset of cell `index`'s record in an image of `rows` rows.
fn cell_at(rows: usize, index: usize) -> usize {
    PAGE_HEADER + pad8(rows * 2) + index * CELL_BYTES
}

#[test]
fn cell_texts_must_follow_each_other() {
    let good = image(&mut PageEncoder::default(), &rows(), 4);
    // Every cell pointing at the first cell's text would decode to more
    // text than the page holds (an allocation amplifier): refused.
    let mut overlap = good.clone();
    overlap[cell_at(2, 1)..cell_at(2, 1) + 4].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(decode(&overlap), Err(PageError::Corrupt("cell text out of order")));
    // Text bytes no cell names.
    let mut extra = good.clone();
    let text_len = u32::from_le_bytes(extra[52..56].try_into().expect("word"));
    extra[52..56].copy_from_slice(&(text_len - 1).to_le_bytes());
    assert!(matches!(decode(&extra), Err(PageError::Corrupt(_))), "{:?}", decode(&extra));
    let mut longer = good;
    let last = cell_at(2, 7);
    let len = u32::from_le_bytes(longer[last + 4..last + 8].try_into().expect("word"));
    longer[last + 4..last + 8].copy_from_slice(&(len - 1).to_le_bytes());
    assert_eq!(decode(&longer), Err(PageError::Corrupt("text outside the cells")));
}

#[test]
fn reserved_bytes_and_padding_must_be_zero() {
    let good = image(&mut PageEncoder::default(), &rows(), 4);
    let cases: [(usize, &str); 3] = [
        (OFF_RESERVED + 9, "reserved header bytes"),
        (cell_at(2, 0) + 28, "reserved cell bytes"),
        (PAGE_HEADER + 4, "padding"),
    ];
    for (at, what) in cases {
        let mut bad = good.clone();
        bad[at] = 1;
        assert_eq!(decode(&bad), Err(PageError::Corrupt(what)));
    }
}

#[test]
fn a_need_no_larger_than_the_header_is_corrupt() {
    let good = image(&mut PageEncoder::default(), &rows(), 4);
    let mut tiny = too_small_image(&good, 0);
    assert_eq!(decode(&tiny), Err(PageError::Corrupt("needed bytes")));
    tiny[56..64].copy_from_slice(&4096u64.to_le_bytes());
    assert_eq!(decode(&tiny), Err(PageError::TooSmall { needed: 4096 }));
}

#[test]
fn a_long_grapheme_is_cut_to_the_cell_limit() {
    let long = "e\u{301}".repeat(MAX_CELL_TEXT);
    let rows = vec![vec![cell(&long), cell("x")]];
    let screen = decode(&image(&mut PageEncoder::default(), &rows, 2)).expect("screen");
    let text = &screen.cells[0][0].text;
    assert!(text.len() <= MAX_CELL_TEXT && long.starts_with(text.as_str()), "{}", text.len());
    assert_eq!(screen.cells[0][1].text, "x");
}

#[test]
fn a_full_grid_of_the_longest_cells_fits_its_bound() {
    let widest = "\u{10ffff}".repeat(MAX_CELL_TEXT);
    let row = vec![cell(&widest); 3];
    let rows = vec![row.clone(), row];
    let image = image(&mut PageEncoder::default(), &rows, 3);
    assert!(image.len() as u64 <= max_image_len(3, 2), "{} > {}", image.len(), max_image_len(3, 2));
    assert!(decode(&image).is_ok());
}

#[test]
fn a_reader_refuses_a_segment_shorter_than_its_length() {
    let fd = create_page(4096).expect("page");
    let error = PageReader::open(&fd, 8192).expect_err("short segment");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[cfg(target_os = "linux")]
#[test]
fn a_page_segment_cannot_be_resized() {
    use std::os::fd::AsRawFd;
    let fd = create_page(8192).expect("page");
    for len in [4096, 16384] {
        // SAFETY: resizes a descriptor the test owns; the call must fail.
        let result = unsafe { libc::ftruncate(fd.as_raw_fd(), len) };
        assert_eq!(result, -1, "ftruncate to {len} succeeded");
    }
    assert_eq!(shm::segment_len(fd.as_fd()).expect("len"), 8192);
}

#[test]
fn modes_follow_the_terminal() {
    let mut terminal = Terminal::new(10, 3, 0, ghostty_vt::Callbacks::default()).expect("terminal");
    assert!(!Modes::of(&terminal).contains(Modes::CURSOR_KEYS));
    terminal.vt_write(b"\x1b[?1h\x1b[?2004h\x1b[?1004h\x1b[?1049h\x1b[?1002h\x1b=");
    let modes = Modes::of(&terminal);
    for bit in [
        Modes::CURSOR_KEYS,
        Modes::BRACKETED_PASTE,
        Modes::FOCUS_EVENTS,
        Modes::ALT_SCREEN,
        Modes::MOUSE_BUTTON,
        Modes::KEYPAD_APPLICATION,
    ] {
        assert!(modes.contains(bit), "{bit:#x} in {modes:?}");
    }
    terminal.vt_write(b"\x1b[?1l\x1b[?1049l\x1b>");
    let modes = Modes::of(&terminal);
    assert!(!modes.contains(Modes::CURSOR_KEYS));
    assert!(!modes.contains(Modes::ALT_SCREEN));
    assert!(!modes.contains(Modes::KEYPAD_APPLICATION));
}
