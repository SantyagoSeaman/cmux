//! `cmux-vt-worker`: the sandboxed VT parser process of
//! `plans/cmux-next/chrome-parity/p01-process.md` (task 133) and its published
//! screen.
//!
//! This first slice is the screen page only ([`screen_page`]): the shared,
//! seqlock-guarded screen a worker writes after it parses, and that every
//! reader (the daemon, and in screen mode the app) copies and validates. The
//! process itself (`main.rs`, `shard.rs`, `sandbox.rs`) lands with task 133.

pub mod screen_page;
