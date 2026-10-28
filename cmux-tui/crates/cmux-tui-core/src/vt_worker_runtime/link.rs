//! The two threads of one worker link: a reader that decodes the worker's
//! events (`cmux_vt_worker::wire`) into broker messages, and a writer that
//! drains the broker's bounded command queue to the socket. The broker
//! thread itself never reads or writes a socket, so a worker that stops
//! reading (hung, or stopped in a debugger) cannot stall the broker or
//! another shard: its queue fills and the broker replaces it.

use std::io;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

use cmux_threads::ThreadQos;
use cmux_vt_worker::wire::{Command, Event, FrameReader, decode_event, send_command};

use super::broker::BrokerMsg;

/// Why a link ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEnd {
    /// The worker closed its end: it exited or was killed.
    Closed,
    /// The worker sent a frame the wire refuses, or an event out of turn.
    BadFrame(String),
    /// A read or write failed.
    Io(io::ErrorKind),
    /// The worker did not read its queue: the broker ended it.
    Hung,
    /// The worker sent no HELLO in time: the broker ended it.
    NoHello,
}

/// What a link's threads tell the broker.
pub(super) enum LinkEvent {
    Event(Event),
    Ended(LinkEnd),
}

/// Why [`Link::send`] did not queue a command.
pub(super) enum SendRefused {
    /// The queue is full: the worker is not reading.
    Full,
    /// The writer ended; its [`LinkEnd`] is on the way.
    Ended,
}

/// The broker's side of one worker link.
pub(super) struct Link {
    outbox: Option<SyncSender<Command>>,
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
        let send = move |events: &Sender<BrokerMsg>, event| {
            let _ = events.send(BrokerMsg::Link { shard, generation, event });
        };
        // A link whose threads did not all start is ended by its drop: the
        // shutdown ends a reader that started.
        let mut link = Self { outbox: Some(outbox), stream, threads: Vec::with_capacity(2) };
        link.threads.push(cmux_threads::spawn(
            format!("vt-link-r-{shard}"),
            ThreadQos::UserInitiated,
            move || read_events(&reader_stream, |event| send(&reader_events, event)),
        )?);
        link.threads.push(cmux_threads::spawn(
            format!("vt-link-w-{shard}"),
            ThreadQos::UserInitiated,
            move || write_commands(&writer_stream, &inbox, |event| send(&writer_events, event)),
        )?);
        Ok(link)
    }

    /// Queue `command` without waiting.
    pub(super) fn send(&self, command: Command) -> Result<(), SendRefused> {
        let Some(outbox) = &self.outbox else { return Err(SendRefused::Ended) };
        outbox.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => SendRefused::Full,
            TrySendError::Disconnected(_) => SendRefused::Ended,
        })
    }
}

impl Drop for Link {
    /// Ends both threads: the shutdown wakes a reader in `recvmsg` and a
    /// writer blocked in `sendmsg` on a worker that does not read.
    fn drop(&mut self) {
        self.outbox = None;
        let _ = self.stream.shutdown(Shutdown::Both);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn read_events(stream: &UnixStream, mut emit: impl FnMut(LinkEvent)) {
    let fd = stream.as_raw_fd();
    let mut reader = FrameReader::default();
    loop {
        loop {
            match reader.next_frame().and_then(|frame| frame.map(decode_event).transpose()) {
                Ok(Some(event)) => emit(LinkEvent::Event(event)),
                Ok(None) => break,
                Err(error) => return emit(LinkEvent::Ended(LinkEnd::BadFrame(error.to_string()))),
            }
        }
        match reader.fill(fd) {
            Ok(0) => return emit(LinkEvent::Ended(LinkEnd::Closed)),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return emit(LinkEvent::Ended(LinkEnd::Io(error.kind()))),
        }
    }
}

fn write_commands(stream: &UnixStream, inbox: &Receiver<Command>, mut emit: impl FnMut(LinkEvent)) {
    for command in inbox {
        if let Err(error) = send_command(stream.as_raw_fd(), &command) {
            return emit(LinkEvent::Ended(LinkEnd::Io(error.kind())));
        }
    }
}
