//! `cmux-vt-worker`: a sandboxed process that parses terminal output.
//!
//! Terminal output is untrusted input and ghostty-vt (with Kitty graphics)
//! is unsafe code, so by Chrome's Rule of 2 (`docs/security/rule-of-2.md`)
//! the parse leaves the daemon for a process that holds no privilege, as
//! Chrome's Data Decoder Service does for untrusted formats. The daemon
//! starts the worker with one Unix socket at fd 3, sends the sandbox policy
//! it chose ([`sandbox`]: a compiled Seatbelt profile on macOS, seccomp-bpf
//! and landlock on Linux) as the first frame, and only after the
//! worker applied it (its HELLO says how) passes rings and pages as
//! descriptors. The worker maps each terminal's output ring read-only
//! ([`ring_view`]), runs the daemon's own per-chunk output rewrites on it
//! (`cmux_kitty_media`, `cmux_vt_stream`), parses its shard ([`shard`]),
//! publishes each screen and its terminal modes into a seqlock page the
//! daemon maps read-only ([`screen_page`]), and sends events (parsed
//! offsets, DA/DSR and OSC 7501 answers, titles, program status) on the
//! socket ([`wire`]). A Kitty image sent as a file path is the one read the
//! sandbox cannot do: the worker asks the daemon for it and that terminal's
//! parse waits for the answer.
//!
//! The daemon broker that spawns workers and relays ring advances is
//! p01-process-134 (`CMUX_TUI_VT_WORKERS=process`); this crate is the
//! process, its wire and the page reader the daemon uses. The app bundles
//! the binary as `bin/cmux-vt-worker` beside the daemon, signed with the
//! hardened runtime and no entitlements; the daemon runs it from a
//! content-addressed copy, as it runs `cmux-holder` (p01-process-211).

// The crash ratchet keeps this crate at zero production panics
// (plans/cmux-next/crash-elimination.md section 6).
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::exit
    )
)]

pub mod ring_view;
pub mod sandbox;
pub mod screen_page;
pub mod serve;
pub mod shard;
mod shm;
pub mod wire;

/// The lock file beside a content-addressed copy of the worker (macOS,
/// `cmux_tui_platform::host_exe`): every process that runs the copy holds it
/// shared, and a daemon deletes a copy only when it can take it exclusively.
pub const IN_USE_LOCK: &str = "in-use.lock";

/// When this worker runs from a content-addressed copy (an [`IN_USE_LOCK`]
/// beside its executable), hold that lock shared for the worker's life, so
/// an update that collects old copies never deletes the file a running
/// worker came from. Called before the sandbox, which denies the open.
/// Best effort; the descriptor is close-on-exec.
pub fn hold_copy_in_use() {
    use std::os::unix::ffi::OsStrExt;
    let Some(lock) =
        std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join(IN_USE_LOCK)))
    else {
        return;
    };
    let Ok(path) = std::ffi::CString::new(lock.as_os_str().as_bytes()) else { return };
    // SAFETY: opens a NUL-terminated path; the descriptor stays open for the
    // life of the process on purpose (the lock lives with it).
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW);
        if fd >= 0 && libc::flock(fd, libc::LOCK_SH) != 0 {
            libc::close(fd);
        }
    }
}
