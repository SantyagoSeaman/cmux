//! `cmux-vt-worker`: started by the daemon with its socket at fd 3 and
//! nothing else ([`cmux_vt_worker::serve`]). `--version` prints
//! [`cmux_vt_worker::wire::version_line`], which the daemon checks before
//! it uses the executable.

use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::ExitCode;

/// The descriptor the daemon passes the worker's socket at.
const SOCKET_FD: libc::c_int = 3;

fn main() -> ExitCode {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--version") {
        let mut stdout = std::io::stdout().lock();
        let printed = writeln!(stdout, "{}", cmux_vt_worker::wire::version_line())
            .and_then(|()| stdout.flush());
        return if printed.is_ok() { ExitCode::SUCCESS } else { ExitCode::from(2) };
    }
    // SAFETY: F_GETFD only reads the descriptor's flags.
    if unsafe { libc::fcntl(SOCKET_FD, libc::F_GETFD) } < 0 {
        eprintln!("cmux-vt-worker: no socket at fd {SOCKET_FD}");
        return ExitCode::from(2);
    }
    // Before the sandbox, which denies every open.
    cmux_vt_worker::hold_copy_in_use();
    // SAFETY: the daemon passed this descriptor for the worker to own, and
    // nothing else in the process holds it.
    let socket = unsafe { OwnedFd::from_raw_fd(SOCKET_FD) };
    match cmux_vt_worker::serve::run(socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cmux-vt-worker: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
