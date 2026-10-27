//! The screen page: one terminal's parsed screen as a VT worker publishes it.
//!
//! A worker parses a terminal's bytes once and writes the result here: every
//! viewport cell (text, width, authored colors, attributes), the cursor, the
//! default colors and the visible Kitty placements by reference. Readers never
//! see a byte of terminal output. This is Chrome's split: the renderer
//! rasterizes, the browser only composites what it is handed
//! (`components/viz/common/quads/compositor_frame.h`, a frame plus its damage).
//!
//! # Memory and the seqlock
//!
//! The page is a slice of [`AtomicU64`] words, in a worker process a shared
//! mapping that readers map read-only (Chrome:
//! `base/memory/read_only_shared_memory_region.h`). One writer, any number of
//! readers, no lock: word 1 is a sequence that is odd while the writer is
//! inside a publish. A reader copies what it needs with relaxed word loads and
//! keeps the copy only if the sequence was even and unchanged across the copy.
//! This is Chromium's `device/base/synchronization/one_writer_seqlock.h`
//! (`WriteBegin` / `WriteEnd`, `ReadBegin` / `ReadRetry`, word-wise atomic
//! copies), which shares gamepad and sensor state across processes.
//!
//! # Damage
//!
//! Each row carries the sequence of the publish that last wrote it, and the
//! header the sequence of the last full invalidation (first publish, resize).
//! A reader asks for "everything changed since the sequence I last drew", so a
//! reader that missed publishes never misses damage: damage accumulates for it
//! like a compositor's damage since its last drawn frame.
//!
//! # The page is untrusted
//!
//! The worker parses hostile output, so a reader treats the page as a
//! renderer's IPC message (`mojo::ReportBadMessage`): [`PageReader::read`]
//! bounds-checks every count against the mapping it owns and checks every
//! field's encoding, and a page that fails is a [`PageError::BadPage`], on
//! which the owner ends that worker. A validated [`ScreenUpdate`] holds no C0,
//! DEL or C1 control in any cell text, so [`encode_replay`], the canonical VT
//! a Ghostty surface can take today, carries only the escapes it writes itself.
//!
//! Layout (little-endian, every section 8-byte aligned):
//!
//! ```text
//! word 0   magic "CMSP" u32 | layout u32
//! word 1   seq u64 (odd while writing)
//! word 2   cap_cols u16 | cap_rows u16 | cols u16 | rows u16
//! word 3   cursor_x u16 | cursor_y u16 | cursor flags u8 | cursor shape u8 | 0 u16
//! word 4   default fg u32 | default bg u32 (0x00RRGGBB)
//! word 5   cursor color u32 (tagged color) | image count u32
//! word 6   full_seq u64 (every row changed at this publish)
//! word 7   dropped grapheme codepoints u32 | 0 u32
//! row_seq  cap_rows words
//! rows     cap_rows x (1 header word + 2 words per cell + arena words)
//! images   MAX_PAGE_IMAGES x 4 words
//! ```

use std::sync::atomic::{AtomicU64, Ordering, fence};

use ghostty_vt::{
    Cell, CellWidth, ColorSpec, CursorInfo, CursorShape, KittyGraphicsSnapshot, RenderFrame,
    RenderState, Rgb, UnderlineStyle,
};

/// `"CMSP"` read as a little-endian u32.
pub const PAGE_MAGIC: u32 = u32::from_le_bytes(*b"CMSP");
/// This layout.
pub const PAGE_LAYOUT: u32 = 1;
/// Visible Kitty placements a page carries by reference; more are dropped (counted in
/// [`PublishStats::images_dropped`]).
pub const MAX_PAGE_IMAGES: usize = 64;
/// Reads that met a writer mid-publish before [`PageReader::read`] gives up with
/// [`PageError::Busy`]. A publish writes at most one screen, so a retry is rare; the
/// caller tries again at its next frame, it never spins.
pub const READ_ATTEMPTS: u32 = 4;

const HEADER_WORDS: usize = 8;
const IMAGE_WORDS: usize = 4;
const CELL_WORDS: usize = 2;

const CURSOR_VISIBLE: u8 = 1;
const CURSOR_BLINKING: u8 = 2;
const CURSOR_FLAGS: u8 = CURSOR_VISIBLE | CURSOR_BLINKING;

const COLOR_TAG_DEFAULT: u32 = 0;
const COLOR_TAG_PALETTE: u32 = 1;
const COLOR_TAG_RGB: u32 = 2;
/// The tagged form of "no color" for the cursor color word.
const COLOR_NONE: u32 = 0;

/// The `Cell::attrs` bits a page may carry.
const ATTR_MASK: u16 = ghostty_vt::ATTR_BOLD
    | ghostty_vt::ATTR_ITALIC
    | ghostty_vt::ATTR_STRIKETHROUGH
    | ghostty_vt::ATTR_INVERSE
    | ghostty_vt::ATTR_FAINT
    | ghostty_vt::ATTR_INVISIBLE
    | ghostty_vt::ATTR_BLINK;

/// A page's capacity in cells. The worker picks it when it maps the page; a terminal
/// that grows past it gets a new, larger page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageGeometry {
    pub cap_cols: u16,
    pub cap_rows: u16,
}

impl PageGeometry {
    /// Codepoint slots per row for the second and later codepoints of grapheme clusters.
    fn arena_slots(self) -> usize {
        self.cap_cols as usize
    }

    fn arena_words(self) -> usize {
        self.arena_slots().div_ceil(2)
    }

    fn row_words(self) -> usize {
        1 + self.cap_cols as usize * CELL_WORDS + self.arena_words()
    }

    fn row_seq_offset(self) -> usize {
        HEADER_WORDS
    }

    fn row_offset(self, row: usize) -> usize {
        HEADER_WORDS + self.cap_rows as usize + row * self.row_words()
    }

    fn images_offset(self) -> usize {
        self.row_offset(self.cap_rows as usize)
    }

    /// Words the page needs.
    pub fn words(self) -> usize {
        self.images_offset() + MAX_PAGE_IMAGES * IMAGE_WORDS
    }

    /// Bytes the page needs (the size of its shared mapping).
    pub fn bytes(self) -> usize {
        self.words() * 8
    }
}

/// An in-process page (tests, the spike, and pool-thread workers). A worker process
/// points [`PageWriter`] and [`PageReader`] at its shared mapping instead.
pub fn alloc_page(geometry: PageGeometry) -> Box<[AtomicU64]> {
    (0..geometry.words()).map(|_| AtomicU64::new(0)).collect()
}

/// Why a page could not be written or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    /// The mapping is smaller than its geometry needs.
    MappingTooSmall { words: usize, needed: usize },
    /// The screen does not fit the page: the worker maps a larger one.
    TooLarge { cols: u16, rows: u16 },
    /// The writer was inside a publish on every attempt; read again next frame.
    Busy,
    /// The page breaks its layout. The owner ends the worker (Chrome's bad_message rule).
    BadPage(BadPage),
    /// The render state could not be read; the publish ended with the rows read so far.
    Render(ghostty_vt::Error),
}

/// What a bad page got wrong (the reason the owner logs when it ends the worker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadPage {
    Magic,
    Layout,
    Geometry,
    Size,
    Cursor,
    CursorShape,
    Color,
    Attrs,
    CellMeta,
    Codepoint,
    Arena,
    ImageCount,
    RowSeq,
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::MappingTooSmall { words, needed } => {
                write!(f, "screen page mapping has {words} words, needs {needed}")
            }
            PageError::TooLarge { cols, rows } => {
                write!(f, "a {cols}x{rows} screen does not fit the page")
            }
            PageError::Busy => write!(f, "screen page writer busy"),
            PageError::BadPage(reason) => write!(f, "bad screen page: {reason:?}"),
            PageError::Render(error) => write!(f, "screen page render state: {error}"),
        }
    }
}

impl std::error::Error for PageError {}

/// One cell as a reader gets it, validated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageCell {
    /// The grapheme; empty for a blank cell. Never holds a control character.
    pub text: String,
    pub width: CellWidth,
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    /// `Cell::attrs` bits.
    pub attrs: u16,
    pub underline: Option<UnderlineStyle>,
}

impl PageCell {
    /// A cell that draws as nothing on the default background.
    fn is_plain_blank(&self) -> bool {
        self.text.is_empty()
            && self.bg == ColorSpec::Default
            && self.attrs & ghostty_vt::ATTR_INVERSE == 0
            && self.underline.is_none()
            && matches!(self.width, CellWidth::Narrow | CellWidth::SpacerHead)
    }

    fn same_style(&self, other: &PageCell) -> bool {
        self.fg == other.fg
            && self.bg == other.bg
            && self.attrs == other.attrs
            && self.underline == other.underline
    }
}

/// The cursor as published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCursor {
    pub x: u16,
    pub y: u16,
    /// Visible and inside the viewport.
    pub visible: bool,
    pub blinking: bool,
    pub shape: CursorShape,
    pub color: Option<Rgb>,
}

/// A visible Kitty placement, by reference: the reader fetches the pixels of
/// `(image_id, generation)` on the trusted image path when it has not got them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRef {
    pub image_id: u32,
    pub placement_id: u32,
    pub generation: u64,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub grid_cols: u16,
    pub grid_rows: u16,
}

/// Everything that changed since the reader's last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenUpdate {
    /// The publish this update comes from.
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    /// Every row is in [`ScreenUpdate::changed`] (first read, or the screen was resized).
    pub full: bool,
    /// Changed rows, top to bottom, each with exactly `cols` cells.
    pub changed: Vec<(u16, Vec<PageCell>)>,
    pub cursor: PageCursor,
    /// Default (bg, fg).
    pub default_colors: (Rgb, Rgb),
    pub images: Vec<ImageRef>,
}

/// What one publish wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublishStats {
    pub seq: u64,
    pub rows_written: usize,
    pub cells_written: usize,
    /// Grapheme codepoints past a row's arena, dropped (the cell keeps its first codepoint).
    pub codepoints_dropped: usize,
    pub images_dropped: usize,
}

/// What a publish writes into the header, from a frame or straight from a render state.
struct FrameMeta {
    size: (u16, u16),
    cursor: Option<CursorInfo>,
    cursor_visual: (CursorShape, bool),
    cursor_color: Option<Rgb>,
    default_colors: (Rgb, Rgb),
}

/// One publish between WriteBegin and WriteEnd.
struct Publish {
    full: bool,
    cols: u16,
    rows: u16,
    stats: PublishStats,
}

/// The page's one writer (the worker that parses the terminal).
pub struct PageWriter<'a> {
    page: &'a [AtomicU64],
    geometry: PageGeometry,
    seq: u64,
    size: Option<(u16, u16)>,
    scratch: Vec<u64>,
}

impl<'a> PageWriter<'a> {
    /// Takes `page` as an empty page of `geometry`. There must be no other writer.
    pub fn new(page: &'a [AtomicU64], geometry: PageGeometry) -> Result<Self, PageError> {
        let needed = geometry.words();
        if page.len() < needed {
            return Err(PageError::MappingTooSmall { words: page.len(), needed });
        }
        if geometry.cap_cols == 0 || geometry.cap_rows == 0 {
            return Err(PageError::TooLarge { cols: 0, rows: 0 });
        }
        page[0].store(u64::from(PAGE_MAGIC) | (u64::from(PAGE_LAYOUT) << 32), Ordering::Relaxed);
        page[2].store(pack16(geometry.cap_cols, geometry.cap_rows, 0, 0), Ordering::Relaxed);
        page[1].store(0, Ordering::Release);
        Ok(Self { page, geometry, seq: 0, size: None, scratch: Vec::new() })
    }

    /// The last published sequence.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Publishes `frame`: its damaged rows (every row after a resize), the cursor, the
    /// default colors and the visible placements.
    pub fn publish(&mut self, frame: &RenderFrame) -> Result<PublishStats, PageError> {
        let meta = FrameMeta {
            size: frame.size,
            cursor: frame.cursor,
            cursor_visual: frame.cursor_visual,
            cursor_color: frame.cursor_color,
            default_colors: frame.default_colors,
        };
        let mut publish = self.begin(&meta)?;
        let rows: Vec<u16> =
            if publish.full { (0..meta.size.1).collect() } else { frame.dirty_rows.clone() };
        for row in rows {
            if let Some(cells) = frame.styled_row(row) {
                self.write_damaged_row(&mut publish, usize::from(row), cells);
            }
        }
        Ok(self.finish(publish, &frame.kitty_graphics))
    }

    /// Publishes the damage of `render` (updated from the terminal just before) and
    /// clears it. Only damaged rows are read out of the render state, so an echo costs
    /// one row; [`PageWriter::publish`] of a `build_frame` copies the whole screen.
    // perf-hot: 500us vt-screen-page-publish
    pub fn publish_damage(&mut self, render: &mut RenderState) -> Result<PublishStats, PageError> {
        let cursor_visual = render.cursor_visual().unwrap_or((CursorShape::Block, false));
        let meta = FrameMeta {
            size: render.size(),
            cursor: render.cursor(),
            cursor_visual,
            cursor_color: render.cursor_color(),
            default_colors: render.default_colors(),
        };
        let kitty = render.kitty_graphics().clone();
        let mut publish = self.begin(&meta)?;
        let full = publish.full;
        let mut write =
            |row: usize, cells: &[Cell]| self.write_damaged_row(&mut publish, row, cells);
        let walked = if full {
            render.walk_rows(|row, _, cells| write(row, cells)).map(|()| render.set_clean())
        } else {
            render.walk_damage(write)
        };
        // The publish ends either way, so readers never see a writer stuck mid-publish;
        // the rows written before a failed walk are whole.
        let stats = self.finish(publish, &kitty);
        walked.map_err(PageError::Render)?;
        Ok(stats)
    }

    /// WriteBegin and the header: the sequence goes odd, then a release fence so no
    /// data store is seen before it.
    fn begin(&mut self, meta: &FrameMeta) -> Result<Publish, PageError> {
        let (cols, rows) = meta.size;
        if cols == 0 || rows == 0 || cols > self.geometry.cap_cols || rows > self.geometry.cap_rows
        {
            return Err(PageError::TooLarge { cols, rows });
        }
        let publish = Publish {
            full: self.size != Some((cols, rows)),
            cols,
            rows,
            stats: PublishStats { seq: self.seq + 2, ..PublishStats::default() },
        };
        self.page[1].store(self.seq + 1, Ordering::Relaxed);
        fence(Ordering::Release);

        self.page[2].store(
            pack16(self.geometry.cap_cols, self.geometry.cap_rows, cols, rows),
            Ordering::Relaxed,
        );
        let cursor = meta.cursor;
        let (shape, blinking) = cursor.map_or(meta.cursor_visual, |c| (c.shape, c.blinking));
        let flags = if cursor.is_some() { CURSOR_VISIBLE } else { 0 }
            | if blinking { CURSOR_BLINKING } else { 0 };
        let (cx, cy) = cursor.map_or((0, 0), |c| (c.x, c.y));
        self.page[3].store(
            u64::from(cx)
                | (u64::from(cy) << 16)
                | (u64::from(flags) << 32)
                | (u64::from(shape_code(shape)) << 40),
            Ordering::Relaxed,
        );
        let (bg, fg) = meta.default_colors;
        self.page[4]
            .store(u64::from(rgb_word(fg)) | (u64::from(rgb_word(bg)) << 32), Ordering::Relaxed);
        self.page[5].store(
            u64::from(meta.cursor_color.map_or(COLOR_NONE, |c| color_word(ColorSpec::Rgb(c)))),
            Ordering::Relaxed,
        );
        if publish.full {
            self.page[6].store(publish.stats.seq, Ordering::Relaxed);
        }
        Ok(publish)
    }

    fn write_damaged_row(&mut self, publish: &mut Publish, row: usize, cells: &[Cell]) {
        if row >= usize::from(publish.rows) {
            return;
        }
        let cols = usize::from(publish.cols);
        publish.stats.codepoints_dropped += self.write_row(row, cols, cells);
        self.page[self.geometry.row_seq_offset() + row].store(publish.stats.seq, Ordering::Relaxed);
        publish.stats.rows_written += 1;
        publish.stats.cells_written += cols;
    }

    /// The placements, then WriteEnd: even again, with release, so a reader that sees
    /// it sees every store of the publish.
    fn finish(&mut self, publish: Publish, kitty: &KittyGraphicsSnapshot) -> PublishStats {
        let mut stats = publish.stats;
        let mut count = 0usize;
        let images = self.geometry.images_offset();
        for placement in kitty.placements.iter().filter(|p| p.viewport_visible) {
            if count == MAX_PAGE_IMAGES {
                stats.images_dropped += 1;
                continue;
            }
            let generation = kitty
                .images
                .iter()
                .find(|image| image.id == placement.image_id)
                .map_or(0, |image| image.generation);
            let base = images + count * IMAGE_WORDS;
            self.page[base].store(
                u64::from(placement.image_id) | (u64::from(placement.placement_id) << 32),
                Ordering::Relaxed,
            );
            self.page[base + 1].store(generation, Ordering::Relaxed);
            self.page[base + 2].store(
                u64::from(placement.viewport_col as u32)
                    | (u64::from(placement.viewport_row as u32) << 32),
                Ordering::Relaxed,
            );
            self.page[base + 3].store(
                u64::from(clamp16(placement.grid_cols))
                    | (u64::from(clamp16(placement.grid_rows)) << 16),
                Ordering::Relaxed,
            );
            count += 1;
        }
        let color = self.page[5].load(Ordering::Relaxed) & 0xffff_ffff;
        self.page[5].store(color | ((count as u64) << 32), Ordering::Relaxed);
        let dropped_total = (self.page[7].load(Ordering::Relaxed) as u32)
            .saturating_add(u32::try_from(stats.codepoints_dropped).unwrap_or(u32::MAX));
        self.page[7].store(u64::from(dropped_total), Ordering::Relaxed);

        self.page[1].store(stats.seq, Ordering::Release);
        self.seq = stats.seq;
        self.size = Some((publish.cols, publish.rows));
        stats
    }

    /// Writes one row's cells and its grapheme arena; returns the codepoints dropped.
    fn write_row(&mut self, row: usize, cols: usize, cells: &[Cell]) -> usize {
        let base = self.geometry.row_offset(row);
        let slots = self.geometry.arena_slots();
        let mut arena: Vec<u32> = Vec::new();
        let mut dropped = 0usize;
        self.scratch.clear();
        for col in 0..cols {
            let blank = Cell::default();
            let cell = cells.get(col).unwrap_or(&blank);
            let mut chars = cell.text.chars().map(sanitize);
            let first = chars.next().map_or(0, u32::from);
            let mut extra = 0u8;
            for c in chars {
                if arena.len() < slots && extra < u8::MAX {
                    arena.push(u32::from(c));
                    extra += 1;
                } else {
                    dropped += 1;
                }
            }
            let underline = match (cell.underline, cell.underline_style) {
                (_, Some(style)) => underline_code(style),
                (true, None) => underline_code(UnderlineStyle::Single),
                (false, None) => 0,
            };
            let meta = width_code(cell.width) | (underline << 2);
            self.scratch.push(u64::from(first) | (u64::from(color_word(cell.fg)) << 32));
            self.scratch.push(
                u64::from(color_word(cell.bg))
                    | (u64::from(cell.attrs() & ATTR_MASK) << 32)
                    | (u64::from(meta) << 48)
                    | (u64::from(extra) << 56),
            );
        }
        self.page[base].store(arena.len() as u64, Ordering::Relaxed);
        for (i, word) in self.scratch.iter().enumerate() {
            self.page[base + 1 + i].store(*word, Ordering::Relaxed);
        }
        let arena_base = base + 1 + self.geometry.cap_cols as usize * CELL_WORDS;
        for (i, pair) in arena.chunks(2).enumerate() {
            let lo = u64::from(pair[0]);
            let hi = pair.get(1).map_or(0, |&cp| u64::from(cp));
            self.page[arena_base + i].store(lo | (hi << 32), Ordering::Relaxed);
        }
        dropped
    }
}

/// One reader of a page. It remembers the publish it last read, so each read returns
/// only the damage since.
pub struct PageReader<'a> {
    page: &'a [AtomicU64],
    geometry: PageGeometry,
    last_seq: u64,
}

impl<'a> PageReader<'a> {
    /// `geometry` is the reader's own record of the mapping (from the worker's
    /// handshake, checked against the mapping's size), never the page's header.
    pub fn new(page: &'a [AtomicU64], geometry: PageGeometry) -> Result<Self, PageError> {
        let needed = geometry.words();
        if page.len() < needed {
            return Err(PageError::MappingTooSmall { words: page.len(), needed });
        }
        Ok(Self { page, geometry, last_seq: 0 })
    }

    /// Forgets what this reader drew: the next read returns the whole screen.
    pub fn invalidate(&mut self) {
        self.last_seq = 0;
    }

    /// The damage since the last read, or `None` when nothing was published since.
    // perf-hot: 250us vt-screen-page-read
    pub fn read(&mut self) -> Result<Option<ScreenUpdate>, PageError> {
        for _ in 0..READ_ATTEMPTS {
            let begin = self.page[1].load(Ordering::Acquire);
            if begin & 1 == 1 {
                continue;
            }
            if begin == 0 || begin == self.last_seq {
                return Ok(None);
            }
            let copied = self.copy_since(self.last_seq);
            // ReadRetry: an acquire fence orders the relaxed copy before the second load.
            fence(Ordering::Acquire);
            if self.page[1].load(Ordering::Relaxed) != begin {
                continue;
            }
            let update = validate(copied?, begin)?;
            self.last_seq = begin;
            return Ok(Some(update));
        }
        Err(PageError::Busy)
    }

    /// Copies the header and every row changed after `since`. Anything it decodes is
    /// still unvalidated (and may be torn until the caller's second sequence load).
    fn copy_since(&self, since: u64) -> Result<RawUpdate, PageError> {
        let g = self.geometry;
        let w0 = self.load(0);
        if w0 as u32 != PAGE_MAGIC {
            return Err(PageError::BadPage(BadPage::Magic));
        }
        if (w0 >> 32) as u32 != PAGE_LAYOUT {
            return Err(PageError::BadPage(BadPage::Layout));
        }
        let [cap_cols, cap_rows, cols, rows] = unpack16(self.load(2));
        if cap_cols != g.cap_cols || cap_rows != g.cap_rows {
            return Err(PageError::BadPage(BadPage::Geometry));
        }
        if cols == 0 || rows == 0 || cols > cap_cols || rows > cap_rows {
            return Err(PageError::BadPage(BadPage::Size));
        }
        let full_seq = self.load(6);
        let full = since == 0 || full_seq > since;
        let mut changed = Vec::new();
        for row in 0..rows as usize {
            let row_seq = self.load(g.row_seq_offset() + row);
            if !full && row_seq <= since {
                continue;
            }
            let base = g.row_offset(row);
            let arena_used = self.load(base);
            if arena_used > g.arena_slots() as u64 {
                return Err(PageError::BadPage(BadPage::Arena));
            }
            let cells: Vec<u64> =
                (0..cols as usize * CELL_WORDS).map(|i| self.load(base + 1 + i)).collect();
            let arena_base = base + 1 + g.cap_cols as usize * CELL_WORDS;
            let arena: Vec<u32> = (0..(arena_used as usize).div_ceil(2))
                .flat_map(|i| {
                    let word = self.load(arena_base + i);
                    [word as u32, (word >> 32) as u32]
                })
                .take(arena_used as usize)
                .collect();
            changed.push(RawRow { row: row as u16, row_seq, cells, arena });
        }
        let images_count = (self.load(5) >> 32) as usize;
        if images_count > MAX_PAGE_IMAGES {
            return Err(PageError::BadPage(BadPage::ImageCount));
        }
        let images_base = g.images_offset();
        let images = (0..images_count)
            .map(|i| {
                let base = images_base + i * IMAGE_WORDS;
                [self.load(base), self.load(base + 1), self.load(base + 2), self.load(base + 3)]
            })
            .collect();
        Ok(RawUpdate {
            cols,
            rows,
            full,
            full_seq,
            header: [self.load(3), self.load(4), self.load(5)],
            changed,
            images,
        })
    }

    fn load(&self, index: usize) -> u64 {
        self.page[index].load(Ordering::Relaxed)
    }
}

struct RawRow {
    row: u16,
    row_seq: u64,
    cells: Vec<u64>,
    arena: Vec<u32>,
}

struct RawUpdate {
    cols: u16,
    rows: u16,
    full: bool,
    full_seq: u64,
    header: [u64; 3],
    changed: Vec<RawRow>,
    images: Vec<[u64; 4]>,
}

/// Turns a consistent copy into a [`ScreenUpdate`], or names what is wrong with it.
fn validate(raw: RawUpdate, seq: u64) -> Result<ScreenUpdate, PageError> {
    let bad = PageError::BadPage;
    let [w3, w4, w5] = raw.header;
    let cx = w3 as u16;
    let cy = (w3 >> 16) as u16;
    let flags = (w3 >> 32) as u8;
    if flags & !CURSOR_FLAGS != 0 || (w3 >> 48) != 0 {
        return Err(bad(BadPage::Cursor));
    }
    let visible = flags & CURSOR_VISIBLE != 0;
    if visible && (cx >= raw.cols || cy >= raw.rows) {
        return Err(bad(BadPage::Cursor));
    }
    let shape = shape_from((w3 >> 40) as u8).ok_or(bad(BadPage::CursorShape))?;
    let fg = rgb_from(w4 as u32).ok_or(bad(BadPage::Color))?;
    let bg = rgb_from((w4 >> 32) as u32).ok_or(bad(BadPage::Color))?;
    let cursor_color = match w5 as u32 {
        COLOR_NONE => None,
        word => match color_from(word) {
            Some(ColorSpec::Rgb(rgb)) => Some(rgb),
            _ => return Err(bad(BadPage::Color)),
        },
    };
    if raw.full_seq & 1 == 1 || raw.full_seq == 0 || raw.full_seq > seq {
        return Err(bad(BadPage::RowSeq));
    }
    let mut changed = Vec::with_capacity(raw.changed.len());
    for row in raw.changed {
        // Every publish is even and at most `seq`; the full publish wrote every row.
        if row.row_seq & 1 == 1 || row.row_seq > seq || row.row_seq < raw.full_seq {
            return Err(bad(BadPage::RowSeq));
        }
        changed.push((row.row, validate_row(&row)?));
    }
    let images = raw
        .images
        .iter()
        .map(|w| ImageRef {
            image_id: w[0] as u32,
            placement_id: (w[0] >> 32) as u32,
            generation: w[1],
            viewport_col: w[2] as u32 as i32,
            viewport_row: (w[2] >> 32) as u32 as i32,
            grid_cols: w[3] as u16,
            grid_rows: (w[3] >> 16) as u16,
        })
        .collect();
    Ok(ScreenUpdate {
        seq,
        cols: raw.cols,
        rows: raw.rows,
        full: raw.full,
        changed,
        cursor: PageCursor {
            x: cx,
            y: cy,
            visible,
            blinking: flags & CURSOR_BLINKING != 0,
            shape,
            color: cursor_color,
        },
        default_colors: (bg, fg),
        images,
    })
}

fn validate_row(row: &RawRow) -> Result<Vec<PageCell>, PageError> {
    let bad = PageError::BadPage;
    let mut cells = Vec::with_capacity(row.cells.len() / CELL_WORDS);
    let mut arena = row.arena.iter();
    for pair in row.cells.chunks_exact(CELL_WORDS) {
        let (a, b) = (pair[0], pair[1]);
        let first = a as u32;
        let fg = color_from((a >> 32) as u32).ok_or(bad(BadPage::Color))?;
        let bg = color_from(b as u32).ok_or(bad(BadPage::Color))?;
        let attrs = (b >> 32) as u16;
        if attrs & !ATTR_MASK != 0 {
            return Err(bad(BadPage::Attrs));
        }
        let meta = (b >> 48) as u8;
        let width = width_from(meta & 3);
        let underline = underline_from((meta >> 2) & 7).ok_or(bad(BadPage::CellMeta))?;
        if meta >> 5 != 0 {
            return Err(bad(BadPage::CellMeta));
        }
        let extra = (b >> 56) as u8;
        let mut text = String::new();
        if first != 0 {
            text.push(drawable(first).ok_or(bad(BadPage::Codepoint))?);
        } else if extra != 0 {
            return Err(bad(BadPage::Codepoint));
        }
        for _ in 0..extra {
            let cp = *arena.next().ok_or(bad(BadPage::Arena))?;
            text.push(drawable(cp).ok_or(bad(BadPage::Codepoint))?);
        }
        cells.push(PageCell { text, width, fg, bg, attrs, underline });
    }
    Ok(cells)
}

/// The canonical VT that draws `update` on a surface of the same grid: cursor moves,
/// SGR, erase-in-line, text and the cursor's state, nothing else. A surface whose grid
/// differs from `update.cols` x `update.rows` must wait for the page of its own size.
///
/// It never scrolls the surface. Every row starts with an absolute move, so a row that
/// fills the last column only leaves a pending wrap that the next move clears. The
/// column after every non-ASCII cell is set again (CHA), so a surface whose width
/// table differs from the worker's still puts each cell in its own column, and a
/// non-ASCII cell in the last column is written with autowrap off. Autowrap stays on
/// otherwise: with `?7l` over the whole replay, Ghostty parsed it 2.5x slower in the
/// spike (its print fast path needs autowrap).
// perf-hot: 100us vt-screen-page-replay
pub fn encode_replay(update: &ScreenUpdate, out: &mut Vec<u8>) {
    for (row, cells) in &update.changed {
        push_csi(out, &[u32::from(*row) + 1, 1], b'H');
        out.extend_from_slice(b"\x1b[0m");
        let mut style = PageCell::default();
        let end = cells.iter().rposition(|c| !c.is_plain_blank()).map_or(0, |i| i + 1);
        let last = cells.len().saturating_sub(1);
        let mut col = 0usize;
        while col < end {
            let cell = &cells[col];
            if !cell.same_style(&style) {
                push_sgr(out, cell);
                style.clone_from(cell);
            }
            let ascii = cell.text.is_ascii();
            if cell.text.is_empty()
                || cell.width == CellWidth::SpacerTail
                || (col == last && cell.width == CellWidth::Wide)
            {
                // A wide lead cannot start in the last column; only a bad page has one.
                out.push(b' ');
            } else if col == last && !ascii {
                out.extend_from_slice(b"\x1b[?7l");
                out.extend_from_slice(cell.text.as_bytes());
                out.extend_from_slice(b"\x1b[?7h");
            } else {
                out.extend_from_slice(cell.text.as_bytes());
            }
            col += 1;
            if cell.width == CellWidth::Wide
                && matches!(cells.get(col), Some(next) if next.width == CellWidth::SpacerTail)
            {
                col += 1;
            }
            if !ascii && col < end {
                push_csi(out, &[col as u32 + 1], b'G');
            }
        }
        if end < cells.len() {
            out.extend_from_slice(b"\x1b[0m\x1b[K");
        }
    }
    out.extend_from_slice(b"\x1b[0m");
    let cursor = update.cursor;
    push_csi(out, &[u32::from(cursor.y) + 1, u32::from(cursor.x) + 1], b'H');
    push_csi(out, &[decscusr(cursor.shape, cursor.blinking)], b' ');
    out.push(b'q');
    out.extend_from_slice(if cursor.visible { b"\x1b[?25h" } else { b"\x1b[?25l" });
}

fn push_csi(out: &mut Vec<u8>, params: &[u32], final_byte: u8) {
    out.extend_from_slice(b"\x1b[");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        push_decimal(out, *p);
    }
    out.push(final_byte);
}

fn push_decimal(out: &mut Vec<u8>, mut value: u32) {
    let mut digits = [0u8; 10];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        n += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend(digits[..n].iter().rev());
}

fn push_sgr(out: &mut Vec<u8>, cell: &PageCell) {
    out.extend_from_slice(b"\x1b[0");
    for (bit, code) in [
        (ghostty_vt::ATTR_BOLD, &b";1"[..]),
        (ghostty_vt::ATTR_FAINT, b";2"),
        (ghostty_vt::ATTR_ITALIC, b";3"),
        (ghostty_vt::ATTR_BLINK, b";5"),
        (ghostty_vt::ATTR_INVERSE, b";7"),
        (ghostty_vt::ATTR_INVISIBLE, b";8"),
        (ghostty_vt::ATTR_STRIKETHROUGH, b";9"),
    ] {
        if cell.attrs & bit != 0 {
            out.extend_from_slice(code);
        }
    }
    if let Some(style) = cell.underline {
        out.extend_from_slice(match style {
            UnderlineStyle::Single => b";4",
            UnderlineStyle::Double => b";4:2",
            UnderlineStyle::Curly => b";4:3",
            UnderlineStyle::Dotted => b";4:4",
            UnderlineStyle::Dashed => b";4:5",
        });
    }
    push_sgr_color(out, cell.fg, 38);
    push_sgr_color(out, cell.bg, 48);
    out.push(b'm');
}

fn push_sgr_color(out: &mut Vec<u8>, color: ColorSpec, base: u32) {
    match color {
        ColorSpec::Default => {}
        ColorSpec::Palette(index) => {
            out.push(b';');
            push_decimal(out, base);
            out.extend_from_slice(b";5;");
            push_decimal(out, u32::from(index));
        }
        ColorSpec::Rgb(rgb) => {
            out.push(b';');
            push_decimal(out, base);
            out.extend_from_slice(b";2;");
            push_decimal(out, u32::from(rgb.r));
            out.push(b';');
            push_decimal(out, u32::from(rgb.g));
            out.push(b';');
            push_decimal(out, u32::from(rgb.b));
        }
    }
}

fn decscusr(shape: CursorShape, blinking: bool) -> u32 {
    let steady = match shape {
        // DECSCUSR has no hollow block; a surface draws its unfocused cursor hollow itself.
        CursorShape::Block | CursorShape::BlockHollow => 2,
        CursorShape::Underline => 4,
        CursorShape::Bar => 6,
    };
    if blinking { steady - 1 } else { steady }
}

fn pack16(a: u16, b: u16, c: u16, d: u16) -> u64 {
    u64::from(a) | (u64::from(b) << 16) | (u64::from(c) << 32) | (u64::from(d) << 48)
}

fn unpack16(word: u64) -> [u16; 4] {
    [word as u16, (word >> 16) as u16, (word >> 32) as u16, (word >> 48) as u16]
}

fn clamp16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn is_control(cp: u32) -> bool {
    cp < 0x20 || (0x7f..=0x9f).contains(&cp)
}

/// The writer never publishes a control: Ghostty does not keep one in a cell, and a
/// worker that tried would only get its page refused.
fn sanitize(c: char) -> char {
    if is_control(u32::from(c)) { '\u{FFFD}' } else { c }
}

fn drawable(cp: u32) -> Option<char> {
    if is_control(cp) { None } else { char::from_u32(cp) }
}

fn rgb_word(rgb: Rgb) -> u32 {
    (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
}

fn rgb_from(word: u32) -> Option<Rgb> {
    (word >> 24 == 0).then_some(Rgb { r: (word >> 16) as u8, g: (word >> 8) as u8, b: word as u8 })
}

fn color_word(color: ColorSpec) -> u32 {
    match color {
        ColorSpec::Default => COLOR_TAG_DEFAULT << 24,
        ColorSpec::Palette(index) => (COLOR_TAG_PALETTE << 24) | u32::from(index),
        ColorSpec::Rgb(rgb) => (COLOR_TAG_RGB << 24) | rgb_word(rgb),
    }
}

fn color_from(word: u32) -> Option<ColorSpec> {
    match word >> 24 {
        COLOR_TAG_DEFAULT if word == 0 => Some(ColorSpec::Default),
        COLOR_TAG_PALETTE if word & 0x00ff_ff00 == 0 => Some(ColorSpec::Palette(word as u8)),
        COLOR_TAG_RGB => rgb_from(word & 0x00ff_ffff).map(ColorSpec::Rgb),
        _ => None,
    }
}

fn width_code(width: CellWidth) -> u8 {
    match width {
        CellWidth::Narrow => 0,
        CellWidth::Wide => 1,
        CellWidth::SpacerTail => 2,
        CellWidth::SpacerHead => 3,
    }
}

fn width_from(code: u8) -> CellWidth {
    match code {
        1 => CellWidth::Wide,
        2 => CellWidth::SpacerTail,
        3 => CellWidth::SpacerHead,
        _ => CellWidth::Narrow,
    }
}

fn underline_code(style: UnderlineStyle) -> u8 {
    match style {
        UnderlineStyle::Single => 1,
        UnderlineStyle::Double => 2,
        UnderlineStyle::Curly => 3,
        UnderlineStyle::Dotted => 4,
        UnderlineStyle::Dashed => 5,
    }
}

fn underline_from(code: u8) -> Option<Option<UnderlineStyle>> {
    Some(match code {
        0 => None,
        1 => Some(UnderlineStyle::Single),
        2 => Some(UnderlineStyle::Double),
        3 => Some(UnderlineStyle::Curly),
        4 => Some(UnderlineStyle::Dotted),
        5 => Some(UnderlineStyle::Dashed),
        _ => return None,
    })
}

fn shape_code(shape: CursorShape) -> u8 {
    match shape {
        CursorShape::Block => 0,
        CursorShape::Bar => 1,
        CursorShape::Underline => 2,
        CursorShape::BlockHollow => 3,
    }
}

fn shape_from(code: u8) -> Option<CursorShape> {
    Some(match code {
        0 => CursorShape::Block,
        1 => CursorShape::Bar,
        2 => CursorShape::Underline,
        3 => CursorShape::BlockHollow,
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
