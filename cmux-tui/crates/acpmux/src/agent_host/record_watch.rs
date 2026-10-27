//! The watches on the directories that hold an agent host's record (`host.rs`
//! `watch_record`) and the harness profile sources (`hub/harness_watch.rs`):
//! something runs whenever an entry of one of them changes or one of them
//! goes away.
//!
//! macOS: a kqueue vnode watch on each directory. The kernel delivers it when
//! the directory's own entries change (a record removed, renamed or written
//! by rename) or the directory is deleted. FSEvents (notify's macOS backend)
//! goes through fseventsd, which falls behind a burst of writes anywhere on
//! the volume (a build) and delivered the deletion of a host's home only after
//! many seconds or not at all: hosts outlived their deleted homes on every
//! fleet Mac (cx-ckdxf, host_lifetime.rs).
//!
//! Linux: an inotify watch on each directory with a mask of entry changes
//! only (created, deleted, renamed in or out) and the directory's own
//! deletion or move. notify's inotify backend always asks for open, close and
//! modify events too: the callback's own read of the record then queued an
//! open and a close in the watched directory, which ran the callback again,
//! so every pooled host spun at 100% CPU reading its record. With this mask a
//! read never queues an event.
//!
//! [`DirEvents`] (and [`UntilGone`], the host's record watch) waits for that
//! kernel watch on the caller's tokio runtime, with no thread of its own, so a
//! host runs on its one runtime thread. Chromium waits on its descriptors from
//! the I/O thread's message pump the same way
//! (`base/message_loop/message_pump_kqueue.cc`, `message_pump_epoll.cc`),
//! never with a thread per descriptor.
//!
//! [`DirWatch`] is a callback on a thread of its own, for a caller with no
//! runtime of its own (the harness profile watch): macOS waits in `kevent`
//! with no timeout and dropping it triggers a user event that ends the
//! thread; elsewhere but Linux, notify's recommended watcher, whose access
//! events are ignored for the reason above.

use std::path::PathBuf;

/// Live while held. `on_change` returns true when it needs no more events.
#[cfg(not(target_os = "linux"))]
pub(crate) struct DirWatch {
    #[cfg(target_os = "macos")]
    kq: std::sync::Arc<std::os::fd::OwnedFd>,
    #[cfg(not(target_os = "macos"))]
    _watcher: notify::RecommendedWatcher,
}

#[cfg(target_os = "macos")]
const STOP: usize = 1;

#[cfg(target_os = "macos")]
fn kevent_change(
    kq: std::os::fd::RawFd,
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
) -> std::io::Result<()> {
    let change =
        libc::kevent { ident, filter, flags, fflags, data: 0, udata: std::ptr::null_mut() };
    // SAFETY: one valid change, no event list, no timeout.
    let rc = unsafe { libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if rc < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(target_os = "linux"))]
impl DirWatch {
    /// Watches every directory of `dirs` that can be opened (a missing one
    /// is skipped with a warning). None when no directory is watched.
    #[cfg(target_os = "macos")]
    pub(crate) fn start(
        dirs: &[PathBuf],
        mut on_change: impl FnMut() -> bool + Send + 'static,
    ) -> Option<Self> {
        use std::os::fd::AsRawFd;
        let (kq, files) = open_kqueue(dirs)?;
        if let Err(e) =
            kevent_change(kq.as_raw_fd(), STOP, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0)
        {
            tracing::warn!("the host cannot watch its record: kevent: {e}");
            return None;
        }
        let kq = std::sync::Arc::new(kq);
        let thread_kq = kq.clone();
        let spawned = std::thread::Builder::new()
            .name("acpmux-record-watch".into())
            .spawn(move || wait_events(&thread_kq, files, &mut on_change));
        if let Err(e) = spawned {
            tracing::warn!("the host cannot watch its record: {e}");
            return None;
        }
        Some(Self { kq })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn start(
        dirs: &[PathBuf],
        mut on_change: impl FnMut() -> bool + Send + 'static,
    ) -> Option<Self> {
        use notify::Watcher as _;
        let mut done = false;
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                // An access (the callback's own read of the record among
                // them) never changes whether the record is there.
                let access = matches!(&event, Ok(e) if e.kind.is_access());
                if !done && !access {
                    done = on_change();
                }
            })
            .map_err(|e| tracing::warn!("the host cannot watch its record: {e}"))
            .ok()?;
        let mut watched = 0;
        for dir in dirs {
            match watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
                Ok(()) => watched += 1,
                Err(e) => tracing::warn!("the host cannot watch {}: {e}", dir.display()),
            }
        }
        (watched > 0).then_some(Self { _watcher: watcher })
    }
}

/// Watches `dirs` until `gone` says what they held is gone, then sends `end`
/// on `tx` once. macOS and Linux: a task on the caller's runtime
/// ([`DirEvents`]), aborted when this guard drops. Elsewhere: [`DirWatch`]'s
/// thread, stopped when this guard drops.
pub(crate) struct UntilGone {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    task: tokio::task::JoinHandle<()>,
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    _thread: DirWatch,
}

impl UntilGone {
    /// None when no directory is watched. Call it inside a tokio runtime
    /// with I/O enabled.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn start<E: Send + 'static>(
        dirs: &[PathBuf],
        mut gone: impl FnMut() -> bool + Send + 'static,
        tx: tokio::sync::mpsc::Sender<E>,
        end: E,
    ) -> Option<Self> {
        let mut events = DirEvents::open(dirs)?;
        let task = tokio::spawn(async move {
            loop {
                if let Err(e) = events.changed().await {
                    tracing::warn!("the host's record watch stopped: {e}");
                    return;
                }
                if gone() {
                    // Awaited: a full queue never drops the end.
                    let _ = tx.send(end).await;
                    return;
                }
            }
        });
        Some(Self { task })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn start<E: Send + 'static>(
        dirs: &[PathBuf],
        mut gone: impl FnMut() -> bool + Send + 'static,
        tx: tokio::sync::mpsc::Sender<E>,
        end: E,
    ) -> Option<Self> {
        let mut end = Some(end);
        let thread = DirWatch::start(dirs, move || {
            if !gone() {
                return false;
            }
            // The watch thread is not a runtime thread: a blocking send never
            // drops the end when the queue is full.
            if let Some(end) = end.take() {
                let _ = tx.blocking_send(end);
            }
            true
        })?;
        Some(Self { _thread: thread })
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for UntilGone {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The kernel watch of [`DirWatch`] on the caller's tokio runtime: no thread.
/// Its kqueue or inotify descriptor is registered with the runtime's I/O
/// driver; [`DirEvents::changed`] resolves once per batch of entry changes.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) struct DirEvents {
    fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    /// macOS: the watched directories, open while watched.
    #[cfg(target_os = "macos")]
    _files: Vec<std::fs::File>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl DirEvents {
    /// Watches every directory of `dirs` that can be opened. None when no
    /// directory is watched or the runtime refuses the descriptor. Call it
    /// inside a tokio runtime with I/O enabled.
    pub(crate) fn open(dirs: &[PathBuf]) -> Option<Self> {
        #[cfg(target_os = "macos")]
        let (fd, files) = open_kqueue(dirs)?;
        #[cfg(target_os = "linux")]
        let fd = open_inotify(dirs)?;
        let fd = tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)
            .map_err(|e| tracing::warn!("the host cannot watch its record: {e}"))
            .ok()?;
        Some(Self {
            fd,
            #[cfg(target_os = "macos")]
            _files: files,
        })
    }

    /// Resolves at the next batch of entry changes (every queued event is
    /// taken, so a burst counts once); Err when the watch failed.
    pub(crate) async fn changed(&mut self) -> std::io::Result<()> {
        loop {
            let mut ready = self.fd.readable().await?;
            #[cfg(target_os = "macos")]
            let events = drain_kqueue(ready.get_inner())?;
            #[cfg(target_os = "linux")]
            let events = drain_inotify(ready.get_inner())?;
            // Drained to empty: the next event makes it readable again.
            ready.clear_ready();
            if events {
                return Ok(());
            }
        }
    }
}

/// A kqueue with a vnode watch on each directory of `dirs` that opens (a
/// missing one is skipped with a warning), and the open directories, which
/// stay open while watched. None when no directory is watched.
#[cfg(target_os = "macos")]
fn open_kqueue(dirs: &[PathBuf]) -> Option<(std::os::fd::OwnedFd, Vec<std::fs::File>)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt;
    // SAFETY: kqueue has no preconditions.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        tracing::warn!(
            "the host cannot watch its record: kqueue: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    // SAFETY: kqueue returned a new descriptor that nothing else owns.
    let kq = unsafe { OwnedFd::from_raw_fd(raw) };
    // NOTE_EXTEND and NOTE_ATTRIB: a watched file (the harness profile
    // watch) that is truncated or grows without a NOTE_WRITE.
    let fflags = libc::NOTE_WRITE
        | libc::NOTE_EXTEND
        | libc::NOTE_ATTRIB
        | libc::NOTE_DELETE
        | libc::NOTE_RENAME
        | libc::NOTE_REVOKE;
    let mut files = Vec::new();
    for dir in dirs {
        // O_EVTONLY: the open descriptor never keeps the volume from unmounting.
        #[allow(clippy::disallowed_methods, reason = "inside scoped_blocking_call")]
        let opened = cmux_blocking::scoped_blocking_call(
            "fs::open_with",
            cmux_blocking::BlockingKind::Fs,
            || std::fs::OpenOptions::new().read(true).custom_flags(libc::O_EVTONLY).open(dir),
        )
        .and_then(|file| {
            let ident = file.as_raw_fd() as usize;
            kevent_change(raw, ident, libc::EVFILT_VNODE, libc::EV_ADD | libc::EV_CLEAR, fflags)
                .map(|()| file)
        });
        match opened {
            Ok(file) => files.push(file),
            Err(e) => tracing::warn!("the host cannot watch {}: {e}", dir.display()),
        }
    }
    (!files.is_empty()).then_some((kq, files))
}

/// Takes every queued kqueue event without waiting; true when there was one.
#[cfg(target_os = "macos")]
fn drain_kqueue(kq: &std::os::fd::OwnedFd) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let mut events = false;
    loop {
        // SAFETY: kevent is a plain C struct; all zero is a valid value of it.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: no changes, room for one event, a zero timeout (never waits).
        let count =
            unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, &zero) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Ok(events);
        }
        events = true;
    }
}

/// A non-blocking inotify descriptor with a watch on each directory of
/// `dirs` that can be watched (a missing one is skipped with a warning).
/// None when no directory is watched.
#[cfg(target_os = "linux")]
fn open_inotify(dirs: &[PathBuf]) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: inotify_init1 has no preconditions.
    let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if raw < 0 {
        tracing::warn!(
            "the host cannot watch its record: inotify: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    // SAFETY: inotify_init1 returned a new descriptor that nothing else owns.
    let inotify = unsafe { OwnedFd::from_raw_fd(raw) };
    // Entry changes and the directory's own end only: no IN_OPEN,
    // IN_ACCESS, IN_CLOSE_* or IN_MODIFY, which a read of the record
    // (the callback's own) or a write to an open file would queue.
    let mask = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR;
    let mut watched = 0;
    for dir in dirs {
        let added = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(std::io::Error::other)
            .and_then(|path| {
                // SAFETY: a valid inotify descriptor and a NUL-terminated path.
                let wd =
                    unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), path.as_ptr(), mask) };
                if wd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
            });
        match added {
            Ok(()) => watched += 1,
            Err(e) => tracing::warn!("the host cannot watch {}: {e}", dir.display()),
        }
    }
    (watched > 0).then_some(inotify)
}

/// Reads every queued inotify event; true when there was at least one. It
/// returns at `WouldBlock` (the descriptor is non-blocking).
#[cfg(target_os = "linux")]
fn drain_inotify(inotify: &std::os::fd::OwnedFd) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // Room for at least one event with the longest name.
    let mut buf = [0u8; 4096];
    let mut events = false;
    loop {
        // SAFETY: a valid descriptor and a buffer of `buf.len()` bytes.
        let n = unsafe { libc::read(inotify.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            events = true;
            continue;
        }
        if n == 0 {
            return Ok(events);
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Ok(events),
            _ => return Err(error),
        }
    }
}

/// Runs `on_change` for each vnode event until it returns true or the stop
/// event comes. The directories in `files` stay open (watched) meanwhile.
#[cfg(target_os = "macos")]
fn wait_events(
    kq: &std::os::fd::OwnedFd,
    files: Vec<std::fs::File>,
    on_change: &mut dyn FnMut() -> bool,
) {
    use std::os::fd::AsRawFd;
    let _files = files;
    let mut event = libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    loop {
        // SAFETY: room for one event; a null timeout waits until one arrives.
        let count = unsafe {
            libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, std::ptr::null())
        };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!("the host's record watch stopped: kevent: {error}");
            return;
        }
        if count == 0 {
            continue;
        }
        if event.filter == libc::EVFILT_USER || on_change() {
            return;
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for DirWatch {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // Not joined: the callback may be blocked handing its event to a
        // loop that is ending; it returns once that receiver is gone.
        let _ = kevent_change(self.kq.as_raw_fd(), STOP, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
    }
}
