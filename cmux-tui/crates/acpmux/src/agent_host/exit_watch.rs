//! The host's wait for its harness leader's exit on the host's own tokio
//! runtime, with no thread: a pidfd (Linux) or a kqueue `EVFILT_PROC`
//! `NOTE_EXIT` (macOS) registered with the runtime's I/O driver. The leader
//! stays unreaped (`WNOWAIT`), so its process group id stays this host's until
//! the host kills the group and reaps it (`host.rs`). Chromium waits for a
//! child the same way, on a kqueue or a pidfd instead of a blocked thread
//! (`base/process/kill_mac.cc` `WaitForChildToDie`,
//! `base/process/process_posix.cc` `WaitForExitWithTimeout`).
//!
//! Elsewhere the wait is one blocking `waitid` off the runtime, as before.

/// What a `WNOHANG` `waitid` says about the leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Peek {
    /// It exited and is still unreaped (a zombie of this process).
    Exited,
    /// It is still running.
    Running,
    /// It is no waitable child of this process (another reaper took it): its
    /// group id is not proven ours.
    Lost,
}

/// One `waitid(P_PID, pid, WEXITED | WNOWAIT | WNOHANG)`: never waits, never reaps.
fn peek(pid: i32) -> Peek {
    loop {
        // SAFETY: a zeroed siginfo_t is a valid out-parameter for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waits on this process's own child without blocking; WNOWAIT
        // leaves it waitable, so the reaper still gets its status.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if rc == 0 {
            // WNOHANG with no exit leaves the info zeroed.
            return if signal_pid(&info) == 0 { Peek::Running } else { Peek::Exited };
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Peek::Lost;
        }
    }
}

#[cfg(target_os = "linux")]
fn signal_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: waitid filled (or zeroed) the child fields of this siginfo_t.
    unsafe { info.si_pid() }
}

#[cfg(not(target_os = "linux"))]
fn signal_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

/// Resolves once child `pid` has exited, leaving it unreaped: true then;
/// false when that cannot be proven (another reaper took it), so the caller
/// must not signal its group.
pub(super) async fn exited_unreaped(pid: i32) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(watch) = ExitEvent::open(pid) {
        // Armed before the peek: an exit between the two still wakes the watch.
        match peek(pid) {
            Peek::Exited => return true,
            Peek::Lost => return false,
            Peek::Running => {}
        }
        if let Err(e) = watch.fired().await {
            tracing::warn!("the host's exit watch failed, waiting on a blocking thread: {e}");
            return blocking_wait(pid).await;
        }
        return match peek(pid) {
            Peek::Exited => true,
            Peek::Lost => false,
            // The kernel posts the exit event while it ends the process,
            // before the zombie is waitable: wait out that last step.
            Peek::Running => blocking_wait(pid).await,
        };
    }
    blocking_wait(pid).await
}

/// The wait of earlier builds: one blocking `waitid` off the runtime.
async fn blocking_wait(pid: i32) -> bool {
    tokio::task::spawn_blocking(move || wait_exit_unreaped(pid)).await.unwrap_or(false)
}

/// Blocks until child `pid` has exited, without reaping it. False when the
/// wait failed (another reaper took the child).
fn wait_exit_unreaped(pid: i32) -> bool {
    loop {
        // SAFETY: a zeroed siginfo_t is a valid out-parameter for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waits on this process's own child; WNOWAIT leaves it
        // waitable, so the reaper below still gets its status.
        let rc = unsafe {
            libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if rc == 0 {
            return true;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

/// A descriptor that turns readable when the process exits, registered with
/// the runtime: a pidfd on Linux, a kqueue with `NOTE_EXIT` on macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ExitEvent {
    fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ExitEvent {
    /// None when the kernel or the runtime refuses the watch (the caller
    /// falls back to the blocking wait).
    fn open(pid: i32) -> Option<Self> {
        let fd = match exit_descriptor(pid) {
            Ok(fd) => fd,
            Err(e) => {
                tracing::debug!("no exit descriptor for {pid}: {e}");
                return None;
            }
        };
        let fd = tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)
            .map_err(|e| tracing::debug!("the runtime refused the exit descriptor: {e}"))
            .ok()?;
        Some(Self { fd })
    }

    /// Resolves when the process exited. Linux: a pidfd turns readable at
    /// the exit and stays so.
    #[cfg(target_os = "linux")]
    async fn fired(&self) -> std::io::Result<()> {
        self.fd.readable().await.map(drop)
    }

    /// Resolves when the process exited: the kqueue's `NOTE_EXIT` is taken.
    #[cfg(target_os = "macos")]
    async fn fired(&self) -> std::io::Result<()> {
        loop {
            let mut ready = self.fd.readable().await?;
            if take_exit(ready.get_inner())? {
                return Ok(());
            }
            ready.clear_ready();
        }
    }
}

/// Linux: `pidfd_open(pid, 0)`, readable once the process exits.
#[cfg(target_os = "linux")]
fn exit_descriptor(pid: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open(pid, 0) returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = libc::c_int::try_from(fd).map_err(std::io::Error::other)?;
    // SAFETY: pidfd_open returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// macOS: a kqueue with one `EVFILT_PROC` `NOTE_EXIT` event for `pid`; the
/// kqueue itself is readable while that event is pending.
#[cfg(target_os = "macos")]
fn exit_descriptor(pid: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: kqueue has no preconditions.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: kqueue returned a new descriptor that nothing else owns.
    let kq = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: kevent is a plain C struct; all zero is a valid value of it.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = pid as libc::uintptr_t;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    // SAFETY: one change, no event list, no timeout (a registration never waits).
    let rc = unsafe { libc::kevent(raw, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(kq)
}

/// macOS: takes the pending events without waiting; true when `NOTE_EXIT` came.
#[cfg(target_os = "macos")]
fn take_exit(kq: &std::os::fd::OwnedFd) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    loop {
        // SAFETY: kevent is a plain C struct; all zero is a valid value of it.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: no changes, room for one event, a zero timeout (never waits).
        let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, &zero) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Ok(n > 0 && event.fflags & libc::NOTE_EXIT != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit is seen on a one-thread runtime through a descriptor of its
    /// own (no thread waits for it), with the child left unreaped.
    #[tokio::test(flavor = "current_thread")]
    async fn the_exit_wakes_the_runtime_and_leaves_the_child_unreaped() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read line; exit 3"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let pid = i32::try_from(child.id()).expect("pid");
        assert_eq!(peek(pid), Peek::Running);
        assert!(ExitEvent::open(pid).is_some(), "no exit descriptor: the wait needs a thread");
        drop(child.stdin.take());
        let exited = tokio::time::timeout(std::time::Duration::from_secs(20), exited_unreaped(pid));
        assert!(exited.await.expect("the exit in time"), "the exit was not proven");
        assert_eq!(peek(pid), Peek::Exited, "seen exited, still unreaped");
        assert_eq!(
            child.wait().expect("reap").code(),
            Some(3),
            "the status is kept for the reaper"
        );
        assert_eq!(peek(pid), Peek::Lost, "reaped: no longer this process's child");
    }
}
