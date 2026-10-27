use std::sync::atomic::{AtomicU64, Ordering};

use ghostty_vt::{Callbacks, Cell, CellWidth, RenderState, Terminal};

use super::*;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const GEOMETRY: PageGeometry = PageGeometry { cap_cols: 120, cap_rows: 40 };

/// A worker's parser and the app surface's parser for one terminal.
struct Pair {
    worker: Terminal,
    worker_render: RenderState,
    app: Terminal,
    app_render: RenderState,
    /// Publish straight from the render state (`publish_damage`), not from a `build_frame`.
    damage_only: bool,
}

impl Pair {
    fn new() -> Self {
        Self {
            worker: Terminal::new(COLS, ROWS, 1 << 20, Callbacks::default()).unwrap(),
            worker_render: RenderState::new().unwrap(),
            app: Terminal::new(COLS, ROWS, 1 << 20, Callbacks::default()).unwrap(),
            app_render: RenderState::new().unwrap(),
            damage_only: false,
        }
    }

    /// The worker parses `bytes` and publishes; returns the frame it published.
    fn parse_and_publish(&mut self, writer: &mut PageWriter<'_>, bytes: &[u8]) -> PublishStats {
        self.worker.vt_write(bytes);
        self.worker_render.update(&mut self.worker).unwrap();
        if self.damage_only {
            return writer.publish_damage(&mut self.worker_render).unwrap();
        }
        let frame = self.worker_render.build_frame().unwrap();
        writer.publish(&frame).unwrap()
    }

    /// The app reads the page and draws it from the replay; returns the replay's length.
    fn draw(&mut self, reader: &mut PageReader<'_>) -> usize {
        let Some(update) = reader.read().unwrap() else { return 0 };
        let mut replay = Vec::new();
        encode_replay(&update, &mut replay);
        self.app.vt_write(&replay);
        replay.len()
    }

    fn assert_same_screen(&mut self, context: &str) {
        self.worker_render.update(&mut self.worker).unwrap();
        self.app_render.update(&mut self.app).unwrap();
        let worker = self.worker_render.build_frame().unwrap();
        let app = self.app_render.build_frame().unwrap();
        for (row, (w, a)) in worker.styled_rows().iter().zip(app.styled_rows()).enumerate() {
            for (col, (wc, ac)) in w.iter().zip(a).enumerate() {
                assert_eq!(drawn(wc), drawn(ac), "{context}: row {row} col {col}");
            }
        }
        assert_eq!(worker.cursor, app.cursor, "{context}: cursor");
    }
}

/// What a cell looks like on screen (an end-of-row spacer head draws as a blank).
fn drawn(cell: &Cell) -> (String, CellWidth, ColorSpec, ColorSpec, u16, Option<UnderlineStyle>) {
    let width = if cell.width == CellWidth::SpacerHead { CellWidth::Narrow } else { cell.width };
    let underline = cell.underline_style.or(cell.underline.then_some(UnderlineStyle::Single));
    (cell.text.clone(), width, cell.fg, cell.bg, cell.attrs(), underline)
}

fn corpora() -> Vec<(&'static str, Vec<Vec<u8>>)> {
    let mut typing = vec![b"\x1b[1;32muser@host\x1b[0m:\x1b[34m~/src\x1b[0m$ ".to_vec()];
    typing.extend(b"git status --short && cargo test -p cmux-vt-worker".iter().map(|b| vec![*b]));
    typing.push(b"\r\n".to_vec());

    let mut ls = Vec::new();
    for i in 0..200 {
        ls.extend_from_slice(
            format!(
                "\x1b[0{};3{}mfile-{i:04}.rs\x1b[0m  \x1b[48;2;10;20;{}m dir \x1b[0m\r\n",
                i % 2,
                i % 8,
                i % 256
            )
            .as_bytes(),
        );
    }
    let ls = ls.chunks(4096).map(<[u8]>::to_vec).collect();

    let mut flood = Vec::new();
    for i in 0..3000 {
        flood.extend_from_slice(
            format!("{i:>8} the quick brown fox jumps over the lazy dog\r\n").as_bytes(),
        );
    }
    let flood = flood.chunks(64 << 10).map(<[u8]>::to_vec).collect();

    let mut tui = vec![b"\x1b[?1049h\x1b[2J".to_vec()];
    for frame in 0..50u32 {
        let mut f = Vec::new();
        for row in 0..ROWS as u32 {
            if (row + frame) % 3 == 0 {
                f.extend_from_slice(
                    format!(
                        "\x1b[{};1H\x1b[4:3;38;5;{}m{:<40}\x1b[0m\x1b[K",
                        row + 1,
                        (row * 7 + frame) % 256,
                        format!("line {row} frame {frame}")
                    )
                    .as_bytes(),
                );
            }
        }
        f.extend_from_slice(format!("\x1b[{};5H\x1b[5 q", frame % ROWS as u32 + 1).as_bytes());
        tui.push(f);
    }

    let unicode = vec![
        "日本語のテキスト 🎉 e\u{301} 👩‍👩‍👧 ok\r\n".as_bytes().to_vec(),
        "\x1b[1;3;9mbold italic strike\x1b[0m \x1b[7minverse\x1b[0m \x1b[2mfaint\x1b[0m\r\n"
            .as_bytes()
            .to_vec(),
        format!("{}界\r\n", "x".repeat(COLS as usize - 1)).into_bytes(),
        b"\x1b[?25l hidden cursor".to_vec(),
        // Every column of the last row, ASCII then a grapheme in the last column: the
        // replay must leave the app's surface unscrolled.
        format!("\x1b[{ROWS};1H{}", "y".repeat(usize::from(COLS))).into_bytes(),
        format!("\x1b[{ROWS};1H{}e\u{301}", "z".repeat(usize::from(COLS) - 1)).into_bytes(),
    ];

    vec![("typing", typing), ("ls", ls), ("flood", flood), ("tui", tui), ("unicode", unicode)]
}

#[test]
fn the_app_draws_the_workers_screen_from_the_page() {
    for damage_only in [false, true] {
        for (name, chunks) in corpora() {
            let page = alloc_page(GEOMETRY);
            let mut writer = PageWriter::new(&page, GEOMETRY).unwrap();
            let mut reader = PageReader::new(&page, GEOMETRY).unwrap();
            let mut pair = Pair::new();
            pair.damage_only = damage_only;
            for (i, chunk) in chunks.iter().enumerate() {
                pair.parse_and_publish(&mut writer, chunk);
                pair.draw(&mut reader);
                pair.assert_same_screen(&format!("{name} chunk {i} (damage_only {damage_only})"));
            }
            // Screen mode never scrolls the app's surface: its rows come from the page.
            assert_eq!(pair.app.scrollback_rows(), 0, "{name}");
        }
    }
}

#[test]
fn an_echo_publishes_one_row_and_the_app_parses_only_that_row() {
    let page = alloc_page(GEOMETRY);
    let mut writer = PageWriter::new(&page, GEOMETRY).unwrap();
    let mut reader = PageReader::new(&page, GEOMETRY).unwrap();
    let mut pair = Pair::new();
    pair.damage_only = true;
    let first = pair.parse_and_publish(&mut writer, b"$ ");
    assert_eq!(first.rows_written, usize::from(ROWS), "the first publish is the whole screen");
    pair.draw(&mut reader);
    let echo = pair.parse_and_publish(&mut writer, b"l");
    assert_eq!((echo.rows_written, echo.cells_written), (1, usize::from(COLS)));
    let update = reader.read().unwrap().unwrap();
    assert_eq!(update.changed.len(), 1);
    let mut replay = Vec::new();
    encode_replay(&update, &mut replay);
    assert!(replay.len() < 64, "{} replay bytes for one echo", replay.len());
    pair.app.vt_write(&replay);
    pair.assert_same_screen("echo");
}

#[test]
fn a_reader_that_missed_publishes_gets_every_damaged_row() {
    let page = alloc_page(GEOMETRY);
    let mut writer = PageWriter::new(&page, GEOMETRY).unwrap();
    let mut reader = PageReader::new(&page, GEOMETRY).unwrap();
    let mut pair = Pair::new();
    pair.parse_and_publish(&mut writer, b"first");
    pair.draw(&mut reader);
    pair.parse_and_publish(&mut writer, b"\x1b[5;1Hrow five");
    pair.parse_and_publish(&mut writer, b"\x1b[9;1Hrow nine");
    let update = reader.read().unwrap().unwrap();
    assert!(!update.full);
    let rows: Vec<u16> = update.changed.iter().map(|(row, _)| *row).collect();
    assert!(rows.contains(&4) && rows.contains(&8), "{rows:?}");
    assert!(!rows.contains(&20), "{rows:?}");
    let mut replay = Vec::new();
    encode_replay(&update, &mut replay);
    pair.app.vt_write(&replay);
    pair.assert_same_screen("missed publishes");
    assert_eq!(reader.read().unwrap(), None, "nothing new");
}

#[test]
fn a_resize_damages_every_row() {
    let page = alloc_page(GEOMETRY);
    let mut writer = PageWriter::new(&page, GEOMETRY).unwrap();
    let mut reader = PageReader::new(&page, GEOMETRY).unwrap();
    let mut pair = Pair::new();
    pair.parse_and_publish(&mut writer, b"hello");
    assert!(reader.read().unwrap().unwrap().full);
    pair.worker.resize(100, 30, 8, 16).unwrap();
    pair.parse_and_publish(&mut writer, b"!");
    let update = reader.read().unwrap().unwrap();
    assert!(update.full);
    assert_eq!((update.cols, update.rows, update.changed.len()), (100, 30, 30));
}

#[test]
fn a_screen_larger_than_the_page_is_refused() {
    let small = PageGeometry { cap_cols: 40, cap_rows: 10 };
    let page = alloc_page(small);
    let mut writer = PageWriter::new(&page, small).unwrap();
    let mut pair = Pair::new();
    pair.worker.vt_write(b"x");
    pair.worker_render.update(&mut pair.worker).unwrap();
    let frame = pair.worker_render.build_frame().unwrap();
    assert_eq!(writer.publish(&frame), Err(PageError::TooLarge { cols: COLS, rows: ROWS }));
    assert!(matches!(
        PageReader::new(&page[..10], small),
        Err(PageError::MappingTooSmall { words: 10, .. })
    ));
}

#[test]
fn a_read_while_the_writer_is_inside_a_publish_is_busy() {
    let page = alloc_page(GEOMETRY);
    let mut writer = PageWriter::new(&page, GEOMETRY).unwrap();
    let mut reader = PageReader::new(&page, GEOMETRY).unwrap();
    let mut pair = Pair::new();
    let stats = pair.parse_and_publish(&mut writer, b"x");
    page[1].store(stats.seq + 1, Ordering::Relaxed);
    assert_eq!(reader.read(), Err(PageError::Busy));
    page[1].store(stats.seq, Ordering::Release);
    assert!(reader.read().unwrap().is_some());
}

fn published() -> (Box<[AtomicU64]>, PublishStats) {
    let page = alloc_page(GEOMETRY);
    let stats = {
        let mut writer = PageWriter::new(&page, GEOMETRY).unwrap();
        let mut pair = Pair::new();
        pair.parse_and_publish(&mut writer, "\x1b[31mred\x1b[0m e\u{301}x".as_bytes())
    };
    (page, stats)
}

fn read_fresh(page: &[AtomicU64]) -> Result<Option<ScreenUpdate>, PageError> {
    PageReader::new(page, GEOMETRY).unwrap().read()
}

/// Rewrites one page word.
type Corrupt = Box<dyn Fn(u64) -> u64>;

#[test]
fn every_kind_of_bad_page_is_refused_by_name() {
    let row0 = GEOMETRY.row_offset(0);
    let cases: [(&str, usize, Corrupt, BadPage); 10] = [
        ("magic", 0, Box::new(|w| w ^ 1), BadPage::Magic),
        ("layout", 0, Box::new(|w| w + (1 << 32)), BadPage::Layout),
        ("geometry", 2, Box::new(|w| w + 1), BadPage::Geometry),
        ("cols past capacity", 2, Box::new(|w| (w & !(0xffff << 32)) | (500 << 32)), BadPage::Size),
        ("cursor outside", 3, Box::new(|w| (w & !0xffff) | 900), BadPage::Cursor),
        ("cursor shape", 3, Box::new(|w| w | (9 << 40)), BadPage::CursorShape),
        ("arena", row0, Box::new(|_| 10_000), BadPage::Arena),
        (
            "control in a cell",
            row0 + 1,
            Box::new(|w| (w & !0xffff_ffff) | 0x1b),
            BadPage::Codepoint,
        ),
        ("color tag", row0 + 1, Box::new(|w| w | (0x7f << 56)), BadPage::Color),
        ("row from the future", GEOMETRY.row_seq_offset(), Box::new(|w| w + 100), BadPage::RowSeq),
    ];
    for (name, index, corrupt, reason) in cases {
        let (page, _) = published();
        assert!(read_fresh(&page).unwrap().is_some(), "{name}: the page is good before");
        let word = page[index].load(Ordering::Relaxed);
        page[index].store(corrupt(word), Ordering::Relaxed);
        assert_eq!(read_fresh(&page), Err(PageError::BadPage(reason)), "{name}");
    }
}

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

#[test]
fn hostile_pages_never_panic_and_never_put_a_control_into_the_replay() {
    let mut rng = Rng(0x5eed_cafe_f00d_0001);
    let (good, _) = published();
    let mut accepted = 0;
    for round in 0..2000 {
        let page = alloc_page(GEOMETRY);
        for (dst, src) in page.iter().zip(good.iter()) {
            dst.store(src.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        // Flip random words, more of them in later rounds, keeping the sequence even.
        for _ in 0..1 + round % 64 {
            let index = (rng.next() as usize) % page.len();
            if index == 1 {
                continue;
            }
            let value = match rng.next() % 3 {
                0 => rng.next(),
                1 => page[index].load(Ordering::Relaxed) ^ (1 << (rng.next() % 64)),
                _ => rng.next() & 0xff,
            };
            page[index].store(value, Ordering::Relaxed);
        }
        let Ok(Some(update)) = read_fresh(&page) else { continue };
        accepted += 1;
        assert!(update.cursor.x < update.cols || !update.cursor.visible);
        for (row, cells) in &update.changed {
            assert!(*row < update.rows);
            assert_eq!(cells.len(), update.cols as usize);
            for cell in cells {
                assert!(cell.text.chars().all(|c| !is_control(u32::from(c))), "{:?}", cell.text);
            }
        }
        let mut replay = Vec::new();
        encode_replay(&update, &mut replay);
        let mut app =
            Terminal::new(update.cols, update.rows, 1 << 16, Callbacks::default()).unwrap();
        app.vt_write(&replay);
        assert_eq!(app.title(), None, "round {round}: a replay set a title");
        assert_eq!(app.scrollback_rows(), 0, "round {round}: a replay scrolled");
    }
    assert!(
        accepted > 100,
        "only {accepted} pages read: the corruptions are too blunt to test the reader"
    );
}
