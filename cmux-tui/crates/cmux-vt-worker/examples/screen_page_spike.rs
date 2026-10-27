//! Task p01-process-241 spike: what the app would parse and pay if it drew a terminal
//! from the worker's screen page instead of parsing the terminal's bytes.
//!
//! `cargo run --release -p cmux-vt-worker --example screen_page_spike`
//!
//! For each corpus the "worker" parses the bytes chunk by chunk and publishes a screen
//! page after each chunk (a chunk stands for one frame's worth of output); the "app"
//! reads the page and parses only the canonical replay. Byte mode is today's app: it
//! parses every output byte with the same parser. Numbers go to
//! plans/cmux-next/terminal-host-model-DESIGN.md section 8.

use std::time::{Duration, Instant};

use cmux_vt_worker::screen_page::{
    PageGeometry, PageReader, PageWriter, alloc_page, encode_replay,
};
use ghostty_vt::{Callbacks, RenderState, Terminal};

const COLS: u16 = 200;
const ROWS: u16 = 50;
const GEOMETRY: PageGeometry = PageGeometry { cap_cols: 256, cap_rows: 64 };

fn corpora() -> Vec<(&'static str, Vec<Vec<u8>>)> {
    let mut typing = vec![b"\x1b[1;32muser@host\x1b[0m:\x1b[34m~/src/cmux\x1b[0m$ ".to_vec()];
    let command = b"cargo test --release -p cmux-vt-worker -- --nocapture screen_page ".repeat(2);
    typing.extend(command.iter().map(|b| vec![*b]));

    let mut ls = Vec::new();
    for i in 0..5000 {
        ls.extend_from_slice(
            format!(
                "\x1b[0{};3{}m-rw-r--r--  1 user staff {:>8} Oct 27 12:00 file-{i:05}.rs\x1b[0m\r\n",
                i % 2,
                i % 8,
                i * 37
            )
            .as_bytes(),
        );
    }

    let mut flood = Vec::new();
    for i in 0..100_000 {
        flood.extend_from_slice(
            format!("{i:>8} the quick brown fox jumps over the lazy dog 0123456789\r\n").as_bytes(),
        );
    }

    let mut tui = vec![b"\x1b[?1049h\x1b[2J".to_vec()];
    for frame in 0..500u32 {
        let mut f = Vec::new();
        for row in 0..u32::from(ROWS) {
            f.extend_from_slice(
                format!(
                    "\x1b[{};1H\x1b[38;5;{}m{:>6}\x1b[0m \x1b[48;2;20;20;{}m{:<120}\x1b[0m\x1b[K",
                    row + 1,
                    (row + frame) % 256,
                    row * frame,
                    (row * 3) % 256,
                    format!("pid {row} cpu {}.{} mem {}", frame % 100, row % 10, row * frame)
                )
                .as_bytes(),
            );
        }
        tui.push(f);
    }

    let mut unicode = Vec::new();
    for i in 0..2000 {
        unicode.extend_from_slice(
            format!("{i:>5} 日本語のテキスト 🎉 e\u{301} 👩\u{200d}👩\u{200d}👧 中文 한국어\r\n")
                .as_bytes(),
        );
    }

    vec![
        ("typing (1 B echo per frame)", typing),
        ("ls -l colored, 4 KiB frames", ls.chunks(4096).map(<[u8]>::to_vec).collect()),
        ("flood 100k lines, 64 KiB frames", flood.chunks(64 << 10).map(<[u8]>::to_vec).collect()),
        ("full-screen TUI, 500 redraws", tui),
        ("CJK + emoji, 4 KiB frames", unicode.chunks(4096).map(<[u8]>::to_vec).collect()),
    ]
}

#[derive(Default)]
struct Row {
    raw_bytes: usize,
    frames: usize,
    replay_bytes: usize,
    app_parse_raw: Duration,
    app_parse_replay: Duration,
    publish: Vec<Duration>,
    read: Vec<Duration>,
    encode: Vec<Duration>,
    app_replay_per_frame: Vec<Duration>,
    app_raw_per_frame: Vec<Duration>,
}

fn percentile(values: &mut [Duration], p: f64) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * p).round() as usize;
    values[index]
}

fn us(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e6)
}

/// One pass over `chunks`; `whole_frames` publishes from `build_frame` (every row
/// copied), else straight from the render state's damage.
fn run(chunks: &[Vec<u8>], whole_frames: bool) -> Row {
    let mut row = Row::default();
    let page = alloc_page(GEOMETRY);
    let mut writer = PageWriter::new(&page, GEOMETRY).expect("page");
    let mut reader = PageReader::new(&page, GEOMETRY).expect("page");
    let mut worker = Terminal::new(COLS, ROWS, 10 << 20, Callbacks::default()).expect("terminal");
    let mut render = RenderState::new().expect("render state");
    let mut app_screen =
        Terminal::new(COLS, ROWS, 10 << 20, Callbacks::default()).expect("terminal");
    let mut app_bytes =
        Terminal::new(COLS, ROWS, 10 << 20, Callbacks::default()).expect("terminal");
    let mut replay = Vec::with_capacity(1 << 20);

    for chunk in chunks {
        row.raw_bytes += chunk.len();
        let t = Instant::now();
        app_bytes.vt_write(chunk);
        let raw = t.elapsed();
        row.app_parse_raw += raw;
        row.app_raw_per_frame.push(raw);

        worker.vt_write(chunk);
        let t = Instant::now();
        render.update(&mut worker).expect("update");
        if whole_frames {
            let frame = render.build_frame().expect("frame");
            writer.publish(&frame).expect("publish");
        } else {
            writer.publish_damage(&mut render).expect("publish");
        }
        row.publish.push(t.elapsed());

        let t = Instant::now();
        let update = reader.read().expect("read");
        row.read.push(t.elapsed());
        let Some(update) = update else { continue };
        replay.clear();
        let t = Instant::now();
        encode_replay(&update, &mut replay);
        row.encode.push(t.elapsed());
        row.frames += 1;
        row.replay_bytes += replay.len();
        let t = Instant::now();
        app_screen.vt_write(&replay);
        let parse = t.elapsed();
        row.app_parse_replay += parse;
        row.app_replay_per_frame.push(parse);
    }
    row
}

fn main() {
    println!(
        "| corpus | raw B | frames | replay B | app parse raw ms | app parse replay ms | worker update+build_frame+publish p50/p99 us | worker update+publish_damage p50/p99 us | read p50/p99 us | encode p50/p99 us | app raw p50 us | app replay p50/p99 us |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    for (name, chunks) in corpora() {
        // A warm-up pass of each so the numbers are the steady state, not first-touch faults.
        run(&chunks, true);
        let mut frames = run(&chunks, true);
        run(&chunks, false);
        let mut r = run(&chunks, false);
        assert_eq!(
            frames.replay_bytes, r.replay_bytes,
            "{name}: both writer paths publish the same damage"
        );
        println!(
            "| {name} | {} | {} | {} | {:.2} | {:.2} | {}/{} | {}/{} | {}/{} | {}/{} | {} | {}/{} |",
            r.raw_bytes,
            r.frames,
            r.replay_bytes,
            r.app_parse_raw.as_secs_f64() * 1e3,
            r.app_parse_replay.as_secs_f64() * 1e3,
            us(percentile(&mut frames.publish, 0.5)),
            us(percentile(&mut frames.publish, 0.99)),
            us(percentile(&mut r.publish, 0.5)),
            us(percentile(&mut r.publish, 0.99)),
            us(percentile(&mut r.read, 0.5)),
            us(percentile(&mut r.read, 0.99)),
            us(percentile(&mut r.encode, 0.5)),
            us(percentile(&mut r.encode, 0.99)),
            us(percentile(&mut r.app_raw_per_frame, 0.5)),
            us(percentile(&mut r.app_replay_per_frame, 0.5)),
            us(percentile(&mut r.app_replay_per_frame, 0.99)),
        );
    }
    println!(
        "page: {COLS}x{ROWS} in a {}x{} page of {} bytes",
        GEOMETRY.cap_cols,
        GEOMETRY.cap_rows,
        GEOMETRY.bytes()
    );
}
