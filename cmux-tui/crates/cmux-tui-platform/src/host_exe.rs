//! The executable terminal hosts run from (cx-0tgl LF).
//!
//! A terminal host is a long-lived process; the app bundle it came from is
//! not. Tooling that owns a bundle (reload scripts, cleanup and age reapers,
//! `pkill -f "<app>.app"`, an agent's `pgrep -f <tag>`) used to match every
//! host by its executable path, and a rebuild that overwrote the bundle's
//! binary in place got hosts killed by code signing on the next page-in. On
//! macOS a host therefore runs from a content-addressed copy of the daemon's
//! executable in a per-user directory that names no tag and no bundle:
//! `~/Library/Application Support/cmux-tui/host-exe/<sha256>/cmux-tui`
//! (`CMUX_TUI_HOST_EXE_DIR` overrides the root). Linux hosts already exec
//! `/proc/self/exe` (no path in their argv, and a running binary cannot be
//! written), so this module only returns that there.
//!
//! The slim host: when the daemon called [`use_sibling_host`] and a regular
//! file `cmux-tui-host` sits beside its (canonical) executable, the copy is
//! of that small binary instead, as `<sha256>/cmux-tui-host`: an idle host
//! re-exec'd from the monolith mapped ~390 dylib images and cost ~3.7 MB.
//! The sibling is never looked up on `PATH`. After it is copied and verified,
//! the copy runs once with `--version` and must print `cmux-tui-host
//! <identity>`, the daemon's own build identity; a missing sibling, a failed
//! copy or another answer leaves this daemon on the copy of its own
//! executable as before. Both binaries take the same arguments
//! (`__terminal-host --bootstrap-stdio ...`).
//!
//! Safety of the copy: the root and each `<sha256>` directory are `0700` and
//! owned by the user, or the copy is not used; the copy is written to a
//! temporary file (an APFS `clonefile`, else a plain copy, never a hard
//! link), made read-only, hashed, then renamed into place. Before every exec
//! the copy's identity (device, inode, size, times, mode, owner, link count)
//! must equal the one whose SHA-256 was verified; any change re-hashes it,
//! and a mismatch refuses the copy (the daemon's own executable is used and
//! the copy reinstalled).
//!
//! The standalone holder (`cmux-holder`) and the sandboxed VT worker
//! (`cmux-vt-worker`) beside the daemon's executable get copies the same
//! way, each in the directory named by its own file's hash
//! ([`holder_executable`], [`vt_worker_executable`]).
//!
//! Lifetime: every daemon and host holds a shared `flock` on its copy's
//! `in-use.lock` for its whole life. A daemon that installs its copy deletes
//! the other copies whose lock it can take exclusively (no daemon or host
//! uses them), except the two newest. Deleting a file never stops a process
//! that runs it; a later daemon of that build installs it again.

use std::io;
use std::path::{Path, PathBuf};

/// The standalone terminal holder beside the daemon's executable
/// (crate `cmux-holder`, std and libc only), and its copy's file name.
pub const HOLDER_NAME: &str = "cmux-holder";
/// Names the holder executable instead (tests: an older holder build).
pub const HOLDER_ENV: &str = "CMUX_TUI_HOLDER_EXE";

/// The executable for a new terminal holder: the standalone `cmux-holder`
/// beside the daemon's (canonical) executable, never looked up on `PATH`,
/// or the file [`HOLDER_ENV`] names. On macOS it runs from a verified
/// content-addressed copy under the host-exe root, as terminal hosts do, so
/// an update or a rebuild of the bundle never overwrites the file a running
/// holder came from; the copy's directory is named by the holder file's own
/// hash, so a build whose holder file did not change reuses it (signed
/// builds strip and re-sign the holder with a timestamp, so each signed
/// build's holder has a new hash and a new copy). The candidate
/// must answer `--version` with a line `accepts` takes (the wire range the
/// daemon speaks). `None` when there is no such file or it answers
/// otherwise: the daemon then runs the holder built into its host
/// executable (`__holder`). A file [`HOLDER_ENV`] names is used as it is.
pub fn holder_executable(accepts: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let source = match std::env::var_os(HOLDER_ENV).filter(|value| !value.is_empty()) {
        // An explicit file runs as named, with no copy and no `--version`:
        // a test's stand-in for a holder an older cmux build started.
        Some(path) => return Some(PathBuf::from(path)),
        None => sibling(HOLDER_NAME)?,
    };
    imp::holder_copy(&source, accepts)
}

/// The sandboxed VT parser beside the daemon's executable (crate
/// `cmux-vt-worker`), and its copy's file name.
pub const VT_WORKER_NAME: &str = "cmux-vt-worker";
/// Names the VT worker executable instead (tests: a worker of another build).
pub const VT_WORKER_ENV: &str = "CMUX_TUI_VT_WORKER_EXE";

/// The executable for a new VT worker process, shipped and checked as
/// [`holder_executable`] ships and checks the holder: the `cmux-vt-worker`
/// beside the daemon's canonical executable (never looked up on `PATH`),
/// run on macOS from a verified content-addressed copy named by its own
/// hash, so an update that swaps the bundle never touches the file running
/// workers came from and a worker the daemon restarts after the swap is
/// still the daemon's own build. The candidate must answer `--version` with
/// a line `accepts` takes (the worker wire the daemon speaks). `None` when
/// there is no such file or it answers otherwise: the daemon then parses
/// with its VT pool threads. A file [`VT_WORKER_ENV`] names is used as it
/// is; its HELLO still has to name the daemon's wire.
pub fn vt_worker_executable(accepts: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let source = match std::env::var_os(VT_WORKER_ENV).filter(|value| !value.is_empty()) {
        Some(path) => return Some(PathBuf::from(path)),
        None => sibling(VT_WORKER_NAME)?,
    };
    imp::vt_worker_copy(&source, accepts)
}

/// `name` beside the daemon's canonical executable, a regular file.
fn sibling(name: &str) -> Option<PathBuf> {
    let own = std::fs::canonicalize(std::env::current_exe().ok()?).ok()?;
    let sibling = own.parent()?.join(name);
    std::fs::symlink_metadata(&sibling)
        .is_ok_and(|meta| meta.file_type().is_file())
        .then_some(sibling)
}

/// Run `path --version`; true when it succeeds with an answer `accepts`
/// takes. The first exec of a new file also pays its code-signature check
/// here, before a terminal waits on the holder or worker.
fn answers(path: &Path, accepts: &dyn Fn(&str) -> bool) -> bool {
    cmux_process::Command::new(path)
        .arg("--version")
        .stdin(cmux_process::Stdio::null())
        .stderr(cmux_process::Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success() && accepts(String::from_utf8_lossy(&output.stdout).trim_end())
        })
}

/// The executable for a new terminal host: the verified copy on macOS, else
/// (or when the copy cannot be used) the daemon's own executable.
pub fn terminal_host_executable() -> io::Result<PathBuf> {
    imp::terminal_host_executable()
}

/// Make the executable for terminal hosts ready on a background thread, once
/// per process: on macOS hash, copy and verify it, then run the copy once
/// (`--version`), so its first exec has paid its code-signature check. A
/// daemon calls it when it starts serving; without it the first terminal of
/// the daemon paid both (the SHA-256 of the daemon's executable and of its
/// copy, the clone, and the first exec of a new copy: 0.75-0.97 s on macOS,
/// later terminals 55-110 ms; cx-b4sjc). A terminal that starts meanwhile
/// waits only for what is left of the install. Elsewhere it does nothing.
pub fn prepare_in_background() {
    imp::prepare_in_background();
}

/// Let terminal hosts run from the slim `cmux-tui-host` beside this
/// executable when that binary reports `identity` (macOS; elsewhere it does
/// nothing). The cmux-tui daemon calls it once at start with its build
/// identity, before any terminal starts.
pub fn use_sibling_host(identity: &'static str) {
    imp::use_sibling_host(identity);
}

/// Hold the shared in-use lock of the copy this process runs from, if it
/// runs from one. A terminal host calls it once at start.
pub fn hold_in_use_lock() {
    imp::hold_in_use_lock();
}

/// Spawn a terminal host or holder from [`terminal_host_executable`]
/// through `cmux_process`: the child gets descriptors 0-2 and the ones it is
/// given, and no other (reviews 1 and 2 of pl-holder-11). Its command line is
/// `cmux-tui __terminal-host ...` with no path, also when the daemon's own
/// executable or the slim host is used. A spawn that fails from the copy is
/// retried once from the daemon's own executable, and the copy is not used
/// again.
#[track_caller]
pub fn spawn_host_isolated(
    configure: impl Fn(&mut cmux_process::Command) -> io::Result<()>,
) -> io::Result<cmux_process::Child> {
    crate::request_path::note(crate::request_path::BlockingKind::ProcessSpawn);
    let binary = terminal_host_executable()?;
    let spawn = |binary: &Path| {
        let mut command = cmux_process::Command::new(binary);
        command.arg0("cmux-tui");
        configure(&mut command)?;
        command.spawn()
    };
    match spawn(&binary) {
        Err(error) if binary != crate::platform::self_exe_for_spawn()? => {
            imp::copy_failed(&binary);
            eprintln!("cmux-tui: a terminal host did not start from {}: {error}", binary.display());
            spawn(&crate::platform::self_exe_for_spawn()?)
        }
        result => result,
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::io;
    use std::path::{Path, PathBuf};

    pub(super) fn terminal_host_executable() -> io::Result<PathBuf> {
        crate::platform::self_exe_for_spawn()
    }

    /// Elsewhere the holder runs as it is: a running binary cannot be
    /// written on Linux.
    pub(super) fn holder_copy(source: &Path, accepts: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
        super::answers(source, accepts).then(|| source.to_path_buf())
    }

    /// The VT worker too: a package update replaces the file by rename, and
    /// running workers keep the old one.
    pub(super) fn vt_worker_copy(source: &Path, accepts: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
        super::answers(source, accepts).then(|| source.to_path_buf())
    }

    pub(super) fn hold_in_use_lock() {}

    pub(super) fn use_sibling_host(_identity: &'static str) {}

    pub(super) fn prepare_in_background() {}

    pub(super) fn copy_failed(_path: &Path) {}
}

#[cfg(target_os = "macos")]
#[path = "host_exe/macos.rs"]
mod imp;
