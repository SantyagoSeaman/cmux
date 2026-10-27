//! An app child's output pipe, read on the daemon's one read loop
//! (`mux-terminal-io`, mux/terminal_work/read_loop.rs), not on a thread of
//! its own: its readiness queues a run on a `mux-terminal-read` worker, as
//! Chrome's IO thread routes a child's pipe (content/browser/
//! child_process_launcher_helper.cc reads nothing on the UI thread).

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::mux::Mux;
use crate::mux::terminal_work::read_loop::{READS_PER_RUN, ReadFlow, ReadKick, ReadSource};

/// Bytes one read takes.
const CHUNK: usize = 64 * 1024;

/// Where a pipe's bytes go. Both calls run on a read-loop worker and must
/// not block.
pub(super) trait OutputSink: Send + 'static {
    fn bytes(&mut self, bytes: &[u8]);
    /// The output ended: end of file, a read error, or [`Watch::give_up`].
    /// Called once.
    fn ended(self: Box<Self>);
}

/// A pipe, both ends close-on-exec, the read end non-blocking: the read end
/// for the daemon, the write end for the child's stdout and stderr.
pub(super) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // Linux makes both ends close-on-exec in the same call. macOS has no
    // `pipe2`; its window before `F_SETFD` is harmless here because every
    // daemon child starts through `cmux_process` with
    // `POSIX_SPAWN_CLOEXEC_DEFAULT`, so no other child inherits these.
    #[cfg(target_os = "linux")]
    // SAFETY: pipe2 writes two descriptors into the local array.
    let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: pipe writes two descriptors into the local array.
    let made = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both were just returned by the call above and nothing else owns them.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(not(target_os = "linux"))]
    for fd in [&read, &write] {
        set_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
    }
    set_flag(&read, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)?;
    Ok((read, write))
}

fn set_flag(fd: &OwnedFd, get: libc::c_int, set: libc::c_int, flag: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor this function borrows open; no pointers.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), get) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), set, flags | flag) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A watched pipe. Dropping it keeps the watch: the pipe is read until its
/// end of file.
pub(super) struct Watch {
    kick: ReadKick,
    give_up: Arc<AtomicBool>,
}

impl Watch {
    /// Stops reading (a background child holds the pipe open after the
    /// child exited): the next run ends the source and the sink hears
    /// [`OutputSink::ended`].
    pub(super) fn give_up(&self) {
        self.give_up.store(true, Ordering::Release);
        self.kick.kick();
    }
}

struct PipeReader {
    file: File,
    buffer: Vec<u8>,
    sink: Option<Box<dyn OutputSink>>,
    give_up: Arc<AtomicBool>,
}

impl ReadSource for PipeReader {
    fn run(&mut self) -> ReadFlow {
        for _ in 0..READS_PER_RUN {
            if self.give_up.load(Ordering::Acquire) {
                return ReadFlow::Finished;
            }
            match self.file.read(&mut self.buffer) {
                Ok(0) => return ReadFlow::Finished,
                Ok(read) => {
                    if let Some(sink) = self.sink.as_mut() {
                        sink.bytes(&self.buffer[..read]);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return ReadFlow::Continue;
                }
                Err(_) => return ReadFlow::Finished,
            }
        }
        ReadFlow::Continue
    }

    fn finish(self: Box<Self>) {
        let PipeReader { file, sink, .. } = *self;
        drop(file);
        if let Some(sink) = sink {
            sink.ended();
        }
    }
}

/// Reads `read` on `mux`'s read loop into `sink` until its end. When the
/// pipe cannot be watched, `sink` hears [`OutputSink::ended`] at once and
/// the error is returned.
pub(super) fn watch(mux: &Mux, read: OwnedFd, sink: Box<dyn OutputSink>) -> io::Result<Watch> {
    let fd = read.as_raw_fd();
    let give_up = Arc::new(AtomicBool::new(false));
    let source = PipeReader {
        file: File::from(read),
        buffer: vec![0; CHUNK],
        sink: Some(sink),
        give_up: give_up.clone(),
    };
    match mux.terminal_reads().register(fd, Box::new(source)) {
        Ok(kick) => Ok(Watch { kick, give_up }),
        Err(source) => {
            source.finish();
            Err(io::Error::other("the output pipe cannot be watched on the read loop"))
        }
    }
}
