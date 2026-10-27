//! The two threads of one worker link: a reader that turns the worker's
//! frames into broker events, and a writer that drains the broker's
//! bounded queue to the socket. The broker thread itself never reads or
//! writes a socket, so a worker that stops reading (hung, or stopped in a
//! debugger) cannot stall the broker or another shard: its queue fills and
//! the broker replaces it.

use std::io;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

use cmux_threads::ThreadQos;

use super::broker::BrokerMsg;
use super::wire::{Frame, FrameReader};

/// Why a link ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEnd {
    /// The worker closed its end: it exited or was killed.
    Closed,
    /// The worker sent a frame the wire refuses.
    BadFrame(String),
    /// A read or write failed.
    Io(io::ErrorKind),
    /// The worker did not read its queue: the broker ended it.
    Hung,
}

/// What a link's threads tell the broker.
pub(super) enum LinkEvent {
    Frame(Frame),
    Ended(LinkEnd),
}

/// One frame for a worker and the descriptor it carries.
pub(super) struct Outbound {
    pub(super) frame: Frame,
    pub(super) fd: Option<Arc<OwnedFd>>,
}

/// Why [`Link::send`] did not queue a frame.
pub(super) enum SendRefused {
    /// The queue is full: the worker is not reading.
    Full,
    /// The writer ended; its [`LinkEnd`] is on the way.
    Ended,
}

/// The broker's side of one worker link.
pub(super) struct Link {
    outbox: Option<SyncSender<Outbound>>,
    stream: UnixStream,
    threads: Vec<JoinHandle<()>>,
}

impl Link {
    /// Start the reader and writer of `stream`; their events go to
    /// `events` tagged with `shard` and `generation`.
    pub(super) fn start(
        shard: usize,
        generation: u64,
        stream: UnixStream,
        queue: usize,
        events: &Sender<BrokerMsg>,
    ) -> io::Result<Self> {
        let (outbox, inbox) = sync_channel(queue);
        let reader_stream = stream.try_clone()?;
        let writer_stream = stream.try_clone()?;
        let reader_events = events.clone();
        let writer_events = events.clone();
        let mut threads = Vec::with_capacity(2);
        let send = move |events: &Sender<BrokerMsg>, event| {
            let _ = events.send(BrokerMsg::Link { shard, generation, event });
        };
        threads.push(cmux_threads::spawn(
            format!("vt-link-r-{shard}"),
            ThreadQos::UserInitiated,
            move || {
                read_frames(reader_stream, |event| send(&reader_events, event));
            },
        )?);
        let started = cmux_threads::spawn(
            format!("vt-link-w-{shard}"),
            ThreadQos::UserInitiated,
            move || {
                write_frames(&writer_stream, &inbox, |event| send(&writer_events, event));
            },
        );
        let mut link = Self { outbox: Some(outbox), stream, threads };
        // A link whose writer could not start is ended by its drop.
        link.threads.push(started?);
        Ok(link)
    }

    /// Queue `out` without waiting.
    pub(super) fn send(&self, out: Outbound) -> Result<(), SendRefused> {
        let Some(outbox) = &self.outbox else { return Err(SendRefused::Ended) };
        outbox.try_send(out).map_err(|error| match error {
            TrySendError::Full(_) => SendRefused::Full,
            TrySendError::Disconnected(_) => SendRefused::Ended,
        })
    }
}

impl Drop for Link {
    /// Ends both threads: the shutdown wakes a reader in `read` and a writer
    /// blocked in `send` on a worker that does not read.
    fn drop(&mut self) {
        self.outbox = None;
        let _ = self.stream.shutdown(Shutdown::Both);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn read_frames(stream: UnixStream, mut emit: impl FnMut(LinkEvent)) {
    let mut reader = FrameReader::new(stream);
    loop {
        match reader.read_frame() {
            Ok(Some(frame)) => emit(LinkEvent::Frame(frame)),
            Ok(None) => return emit(LinkEvent::Ended(LinkEnd::Closed)),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return emit(LinkEvent::Ended(LinkEnd::BadFrame(error.to_string())));
            }
            Err(error) => return emit(LinkEvent::Ended(LinkEnd::Io(error.kind()))),
        }
    }
}

fn write_frames(stream: &UnixStream, inbox: &Receiver<Outbound>, mut emit: impl FnMut(LinkEvent)) {
    for Outbound { frame, fd } in inbox {
        let fds: Vec<_> = fd.iter().map(|fd| fd.as_raw_fd()).collect();
        if let Err(error) = cmux_ipc::fds::send_all(stream.as_raw_fd(), &frame.encode(), &fds) {
            return emit(LinkEvent::Ended(LinkEnd::Io(error.kind())));
        }
    }
}
