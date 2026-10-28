//! The connection loop shared by the local socket and the remote entry.
//!
//! A [`FrameReader`] (cmux-ipc) cuts the bytes into JSON lines, exactly as
//! `read_line` did, and into binary frames (plans/cmux-next/chrome-parity/
//! p05-ipc.md, section 3.2). Every line is seen by the connection's
//! admission before anything parses or dispatches it. A frame skips that, so
//! it passes only on a local Unix connection whose admission admits frames
//! and whose client holds the [`IPC_FRAMES_CAPABILITY`] grant when the frame
//! arrives; any other frame is a bad message.
//!
//! A local socket is read with one `recvmsg` per read, bytes and descriptors
//! together, straight into the reader's buffer, as Mojo's
//! `ChannelPosix::OnFileCanReadWithoutBlocking` reads into the buffer
//! `Channel::GetReadBuffer` lends it (chromium mojo/core/channel_posix.cc).
//! Descriptors queue in arrival order, each with the stream bytes of the
//! read that brought it ([`ArrivedFds`]), and only an ATTACHED SHARED_BUFFER
//! frame whose first byte is in that read takes one (p05-ipc-115), as Mojo's
//! `ChannelPosix` hands each message the handles its header counts. They may
//! arrive only while the client holds the shared-buffer grant, and at most
//! [`FrameLane::descriptors_allowed`] may wait for their frame. A descriptor
//! no message took once every message that starts in its read was routed
//! (one sent with a REQUEST, a CANCEL, a line or a reuse frame) is a bad
//! message, as is any descriptor without the grant: each is refused where the
//! kernel would otherwise drop it unseen or a later frame would map it.
//!
//! A bad message closes the connection, as Chrome's `ReportBadMessage`
//! (mojo/public/cpp/bindings/message.h) ends the sender: nothing after the
//! bad byte is interpreted.
//!
//! REQUEST and CANCEL frames go to [`frame_dispatch`] (p05-ipc-071), and so
//! do INPUT frames, which queue on the stream they name until its opener is
//! answered ([`pending_streams`], p05-ipc-087).

use std::fmt;

use cmux_ipc::{BadMessage, Frame, FrameKind, FrameReader, Message, ReaderLimits};

use crate::diagnostics::{IpcTransport, JsonParseScope};
use crate::supervised::{self, Fatal, ShutdownBehavior};

use super::connection_scheduler::connection_qos::ReaderClass;
use super::*;

/// The grant that lets a connection send frames. p05-ipc-060 decides who
/// gets it (a local Unix peer at this daemon's api_revision); this loop only
/// reads it from the client's record.
pub(super) const IPC_FRAMES_CAPABILITY: &str = "ipc-frames-v2";

/// The grant that lets a connection send SHARED_BUFFER frames and the
/// descriptors they carry (capabilities.rs `IpcCapability::SharedBuffers`).
pub(super) const SHARED_BUFFERS_CAPABILITY: &str = "shared-buffers-v1";

/// A JSON line's payload may be `MAX_JSON_LINE_BYTES` long, its `\n`
/// excluded, as with `read_line`; frames keep the wire's 32 MiB bound.
fn line_limits() -> ReaderLimits {
    ReaderLimits { max_line: MAX_JSON_LINE_BYTES, ..ReaderLimits::default() }
}

pub(super) fn handle_connection_with_permit(
    mux: Arc<Mux>,
    stream: Box<dyn transport::Stream>,
    peer: origin_gate::AcceptedPeer,
    render_service: Arc<RenderService>,
    connection_permit: Option<ConnectionPermit>,
) {
    serve_line_connection(
        mux,
        stream,
        peer,
        render_service,
        connection_permit,
        ClientTransport::Unix,
        &admission::LocalAdmission,
    );
}

/// The class of a connection's reader thread (mux-conn, mux-remote-conn)
/// until its client class is known: it serves the app and automation alike.
/// Once classified it rests at the class's (connection_qos.rs, task 224).
pub(super) const CONNECTION_READER_QOS: cmux_threads::ThreadQos =
    crate::thread_qos::ThreadType::UnclassifiedClient.qos();

/// Starts connection reader thread `name` at [`CONNECTION_READER_QOS`]. It
/// runs `setup` first, which may block: the connection's own setup before
/// its first message (the peer's identity, [`origin_gate::AcceptedPeer`]),
/// as a terminal-host reader registers its descriptor before it serves
/// (`echo_thread::spawn_host_stream_reader`). Then it disallows blocking and
/// serves with `setup`'s value (each blocking request site is recorded, and
/// a panic in Debug, MQ-13).
pub(super) fn spawn_connection_reader<S: 'static>(
    name: &str,
    setup: impl FnOnce() -> S + Send + 'static,
    serve: impl FnOnce(S) + Send + 'static,
) -> std::io::Result<supervised::SupervisedThread<()>> {
    supervised::spawn_at(
        name,
        CONNECTION_READER_QOS,
        Fatal,
        ShutdownBehavior::Continue,
        move || {
            let accepted = setup();
            // Each frame it reads is one hang-watch scope (connection_frame.rs);
            // the read itself is outside every scope.
            let thread = std::thread::current();
            let _hang_watch = cmux_hang_watch::register_thread(
                cmux_hang_watch::ThreadKind::Io,
                thread.name().unwrap_or("mux-conn"),
            );
            cmux_threads::disallow_blocking();
            serve(accepted);
        },
    )
}

/// One JSON line per message, and frames once granted. `admission` sees
/// every line before anything parses or dispatches it, and may answer it
/// instead (the remote entry).
pub(super) fn serve_line_connection(
    mux: Arc<Mux>,
    stream: Box<dyn transport::Stream>,
    peer: origin_gate::AcceptedPeer,
    render_service: Arc<RenderService>,
    connection_permit: Option<ConnectionPermit>,
    transport: ClientTransport,
    admission: &dyn admission::LineAdmission,
) {
    let _ = serve_connection(
        mux,
        stream,
        peer,
        render_service,
        connection_permit,
        transport,
        admission,
        &mut frame_dispatch::RequestFrames::default(),
    );
}

/// What a connection does with frames: whether it may send them yet, and
/// where each one goes.
pub(super) trait FrameLane {
    /// Whether `client` holds the frames grant now. Asked again for every
    /// frame, before it is cut: a later `set-client-info` or `client-hello`
    /// can end the grant (p05-ipc-060), and frame payloads are not
    /// revision-translated. Never asked on a connection that cannot carry
    /// frames (not a local Unix socket, or an admission that sees only lines).
    fn granted(&self, mux: &Mux, client: u64) -> bool;

    /// How many received descriptors may wait for the frame that takes
    /// them; asked only when one arrived. More is a bad message.
    fn descriptors_allowed(&self, _mux: &Mux, _client: u64) -> usize {
        0
    }

    /// Takes one frame. `Ok(false)` closes the connection as a line's
    /// handler does; an error closes it as a bad message.
    fn route(
        &mut self,
        frame: Frame,
        connection: &mut FrameConnection<'_>,
    ) -> Result<bool, ConnectionBadMessage>;
}

/// The connection a frame arrived on: what a line's dispatch gets.
pub(super) struct FrameConnection<'a> {
    pub(super) mux: &'a Arc<Mux>,
    pub(super) client: u64,
    pub(super) transport: ClientTransport,
    pub(super) writer: &'a MessageWriter,
    pub(super) scheduler: &'a Arc<ConnectionSurfaceScheduler>,
    /// When the reader cut the frame, for its request span.
    pub(super) arrived: Instant,
    /// The stream offsets of the frame's first byte and of the byte after it.
    #[cfg(unix)]
    pub(super) bytes: std::ops::Range<u64>,
    /// The descriptors that arrived and no frame took yet.
    #[cfg(unix)]
    pub(super) fds: &'a mut ArrivedFds,
}

impl FrameConnection<'_> {
    /// The descriptor sent with the frame's first byte: the oldest queued
    /// one, if the read that brought it also brought that byte. `None`
    /// leaves the queue as it is; a descriptor a later byte brought waits for
    /// its own frame.
    #[cfg(unix)]
    pub(super) fn take_fd(&mut self) -> Option<std::os::fd::OwnedFd> {
        self.fds.take_for(self.bytes.start)
    }

    /// Refuses, before it runs, a frame that takes no descriptor when one
    /// came with it: a descriptor whose read ended inside the frame belongs
    /// to no later message.
    pub(super) fn takes_no_fd(&self) -> Result<(), ConnectionBadMessage> {
        #[cfg(unix)]
        if self.fds.stray_before(self.bytes.end) {
            return Err(ConnectionBadMessage::UnexpectedFds);
        }
        Ok(())
    }
}

/// A received descriptor and the stream bytes of the read that brought it:
/// the sender passed it with the first byte of a message in that range.
#[cfg(unix)]
struct ArrivedFd {
    fd: std::os::fd::OwnedFd,
    read: std::ops::Range<u64>,
}

/// The descriptors that arrived and no message took yet, oldest first.
#[cfg(unix)]
#[derive(Default)]
pub(super) struct ArrivedFds {
    queue: VecDeque<ArrivedFd>,
}

#[cfg(unix)]
impl ArrivedFds {
    /// Moves the descriptors one read queued in `receiver` here, tagged with
    /// that read's bytes.
    fn arrived(&mut self, receiver: &mut cmux_ipc::fds::FdReceiver, read: std::ops::Range<u64>) {
        while let Ok(taken) = receiver.take(cmux_ipc::fds::MAX_FDS_PER_FRAME) {
            if taken.is_empty() {
                break;
            }
            self.queue.extend(taken.into_iter().map(|fd| ArrivedFd { fd, read: read.clone() }));
        }
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    /// The oldest descriptor, if its read brought the byte at `start`.
    fn take_for(&mut self, start: u64) -> Option<std::os::fd::OwnedFd> {
        if !self.queue.front()?.read.contains(&start) {
            return None;
        }
        self.queue.pop_front().map(|arrived| arrived.fd)
    }

    /// Whether a descriptor's read ended at or before `end`, the end of the
    /// message just routed: every message that could take it was routed, so
    /// it came with one that takes none.
    fn stray_before(&self, end: u64) -> bool {
        self.queue.front().is_some_and(|arrived| arrived.read.end <= end)
    }
}

/// Why a connection was closed as a bad message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConnectionBadMessage {
    /// The bytes broke the wire's rules (cmux-ipc reader).
    Wire(BadMessage),
    /// A valid frame of a kind this daemon does not route.
    UnroutedKind { kind: FrameKind },
    /// A stream frame (INPUT) naming a stream this connection never
    /// announced (server/pending_streams.rs).
    UnknownStream { kind: FrameKind, stream: u16 },
    /// A REQUEST or CANCEL frame that its method or the requests in flight
    /// refuse.
    Call(frame_dispatch::CallRefusal),
    /// A descriptor arrived without the shared-buffer grant, or more than
    /// may wait for their frames.
    UnexpectedFds,
    /// A SHARED_BUFFER frame its region or descriptor refuses.
    #[cfg(unix)]
    SharedBuffer(cmux_ipc::shared_buffer::SharedBufferRefusal),
}

impl ConnectionBadMessage {
    /// The `bad_message` reason, stable for logs and counters.
    pub(super) fn reason(&self) -> &'static str {
        match self {
            ConnectionBadMessage::Wire(bad) => bad.reason(),
            ConnectionBadMessage::UnroutedKind { .. } => "unrouted_kind",
            ConnectionBadMessage::UnknownStream { .. } => "unknown_stream",
            ConnectionBadMessage::Call(refusal) => refusal.reason(),
            ConnectionBadMessage::UnexpectedFds => "unexpected_fds",
            #[cfg(unix)]
            ConnectionBadMessage::SharedBuffer(refusal) => refusal.reason(),
        }
    }
}

impl fmt::Display for ConnectionBadMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionBadMessage::Wire(bad) => write!(f, "{bad}"),
            ConnectionBadMessage::UnroutedKind { kind } => {
                write!(f, "no route for {} frames", kind.name())
            }
            ConnectionBadMessage::UnknownStream { kind, stream } => {
                write!(f, "{} frame on never-announced stream {stream}", kind.name())
            }
            ConnectionBadMessage::Call(refusal) => write!(f, "{refusal}"),
            ConnectionBadMessage::UnexpectedFds => write!(f, "a file descriptor arrived"),
            #[cfg(unix)]
            ConnectionBadMessage::SharedBuffer(refusal) => write!(f, "{refusal}"),
        }
    }
}

/// How a connection ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ConnectionEnd {
    /// The peer closed after whole messages: accepted work drains.
    PeerClosed,
    /// A read failed, setup failed, admission refused, or a handler closed
    /// the connection.
    Dropped,
    /// The peer sent a bad message.
    BadMessage(ConnectionBadMessage),
}

pub(super) fn serve_connection(
    mux: Arc<Mux>,
    stream: Box<dyn transport::Stream>,
    peer: origin_gate::AcceptedPeer,
    render_service: Arc<RenderService>,
    connection_permit: Option<ConnectionPermit>,
    transport: ClientTransport,
    admission: &dyn admission::LineAdmission,
    frames: &mut dyn FrameLane,
) -> ConnectionEnd {
    let Ok(mut write_half) = stream.try_clone_box() else { return ConnectionEnd::Dropped };
    let Ok(control) = write_half.try_clone_box() else { return ConnectionEnd::Dropped };
    let Ok(writer_socket) = write_half.try_clone_box() else { return ConnectionEnd::Dropped };
    if write_half.set_write_timeout(Some(STREAM_WRITE_TIMEOUT)).is_err() {
        return ConnectionEnd::Dropped;
    }
    let outbound = Arc::new(BoundedOutbound::with_chunk_bytes(mux.bulk_frame_max()));
    let writer = MessageWriter::new_with_render_service(
        QueuedSink { outbound: outbound.clone(), control: Some(SinkControl::Unix(control)) },
        render_service,
    );
    let writer_outbound = outbound;
    let writer_close = writer.clone();
    let writer_ipc = mux.ipc_stats().clone();
    let writer_hangs = mux.echo_watch();
    // The writer holds the connection's slot until it has written its last
    // item, so the reader never joins it: a join is a wait for another
    // thread, which a reader that disallows blocking may not make.
    let writer_permit = connection_permit.clone();
    let Ok(writer_thread) = crate::echo_thread::spawn("mux-line-out", move || {
        let _permit = writer_permit;
        // Ends the connection however the writer ends, a panic included: a
        // dead writer with an open socket left its client waiting forever.
        let _end = WriterEnd { writer: writer_close, socket: writer_socket };
        let _hang_watch =
            cmux_hang_watch::register_thread(cmux_hang_watch::ThreadKind::Io, "mux-line-out");
        // Every ready item up to 256 KiB leaves in one writev (p05-ipc-088).
        let mut batch = Vec::new();
        // mq14-allow: the writer's idle wait for its own queue (BoundedOutbound::recv_batch).
        while writer_outbound.recv_batch(&mut batch) {
            // Each batch's write is a 100 ms hang scope; the wait above is not.
            let _batch = writer_hangs.item(WatchKind::LineOut);
            // One scope per write, as long as the socket's write timeout:
            // the queue wait above is outside it.
            let write = cmux_hang_watch::watch(STREAM_WRITE_TIMEOUT);
            let written = write_counted_batch(&mut *write_half, &writer_ipc, &batch);
            drop(write);
            // The written texts go now, not when the next item arrives.
            batch.clear();
            if written.is_err() {
                writer_outbound.close();
                break;
            }
        }
    }) else {
        writer.close();
        return ConnectionEnd::Dropped;
    };
    let client = mux.control_clients.register(transport, writer.clone());
    // The connection's stream 0, open until the connection ends
    // (`server-stats` `ipc`).
    let ipc = mux.ipc_stats().clone();
    let _stream = ipc.stream_opened();
    origin_gate::set_peer_identity_at_accept(&mux, client, peer);
    if !admission.registered(&mux, client) {
        // Refused before its first frame (the remote entry's revocation
        // limits): nothing is read or dispatched. The writer ends on its own
        // once the disconnect closes its queue.
        disconnect_client(&mux, client, false);
        drop(writer_thread);
        return ConnectionEnd::Dropped;
    }
    let mut hello = client_hello::HelloGate::new(transport);
    let class = mux.control_clients.connection_class(client);
    let mut reader_class = ReaderClass::new(class.clone(), mux.request_qos());
    let surface_scheduler = Arc::new(
        ConnectionSurfaceScheduler::new_inner(
            mux.surface_operation_admission.clone(),
            connection_permit.clone(),
        )
        .classed(class),
    );
    let mut input = ConnectionInput::new(stream);
    let carries_frames = matches!(transport, ClientTransport::Unix) && admission.admits_frames();
    // A panic while serving (a handler's, or a blocking call's in the
    // detector's panic mode) still ends the connection below, then goes on
    // as the thread's panic. Before, the teardown never ran: the client
    // stayed registered with its socket open and waited forever.
    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            if input.messages.next_is_frame() {
                if !(carries_frames && frames.granted(&mux, client)) {
                    break ConnectionEnd::BadMessage(ConnectionBadMessage::Wire(
                        BadMessage::FramesNotNegotiated,
                    ));
                }
                input.messages.negotiate_frames();
            }
            let buffered = input.messages.buffered_len();
            // The stream offset of the next message's first byte.
            let start = input.received.saturating_sub(buffered as u64);
            let mut line = match input.messages.next_message() {
                Ok(Some(Message::Json(line))) => {
                    // Dispatch sees the line with its `\n`, as `read_line` gave
                    // it: serde's error positions in `bad request` replies name
                    // it. Only a last line cut at end of stream has none.
                    let mut text = String::with_capacity(line.len() + 1);
                    text.push_str(line);
                    if line.len() < buffered {
                        text.push('\n');
                    }
                    ipc.line_received(text.len());
                    text
                }
                Ok(Some(Message::Frame(frame))) => {
                    ipc.frame_received(frame.header().kind(), frame.header().frame_len());
                    let end = start.saturating_add(frame.header().frame_len() as u64);
                    let mut connection = FrameConnection {
                        mux: &mux,
                        client,
                        transport,
                        writer: &writer,
                        scheduler: &surface_scheduler,
                        arrived: Instant::now(),
                        #[cfg(unix)]
                        bytes: start..end,
                        #[cfg(unix)]
                        fds: &mut input.arrived,
                    };
                    // Whatever the lane, no descriptor outlives its frame.
                    match frames.route(frame, &mut connection) {
                        Ok(true) if input.stray_descriptor(end) => {
                            break ConnectionEnd::BadMessage(ConnectionBadMessage::UnexpectedFds);
                        }
                        Ok(true) => continue,
                        Ok(false) => break ConnectionEnd::Dropped,
                        Err(bad) => break ConnectionEnd::BadMessage(bad),
                    }
                }
                Ok(None) if input.ended => break ConnectionEnd::PeerClosed,
                Ok(None) => match input.fill(|| {
                    if carries_frames { frames.descriptors_allowed(&mux, client) } else { 0 }
                }) {
                    Ok(()) => continue,
                    Err(end) => break end,
                },
                Err(bad) => break ConnectionEnd::BadMessage(ConnectionBadMessage::Wire(bad)),
            };
            // A line takes no descriptor.
            if input.stray_descriptor(start.saturating_add(line.len() as u64)) {
                zeroize_string(&mut line);
                break ConnectionEnd::BadMessage(ConnectionBadMessage::UnexpectedFds);
            }
            // The line's arrival, for its request span (server/request_spans.rs).
            let arrived = Instant::now();
            if line.trim().is_empty() {
                zeroize_string(&mut line);
                continue;
            }
            // Every line from here is one request of `server-stats` `ipc`: the
            // parses of its admission, hello gate and route all count toward it.
            let parses = JsonParseScope::enter(&ipc, IpcTransport::Lines);
            let keep_open = match admission.refusal(&line) {
                Some(refusal) => {
                    // A refused line still counts as a line: the window closes.
                    hello.close();
                    writer.send_control(&refusal).is_ok()
                }
                None => {
                    let peer = || client_hello::Peer {
                        key: input.stream.peer_process_key(),
                        token: input.stream.peer_token(),
                    };
                    match hello.observe(&mux, client, &line, peer) {
                        Some(reply) => writer.send_control(&reply).is_ok(),
                        None => {
                            reader_class.before_line(&mux, client, hello.is_closed());
                            handle_connection_frame_at(
                                &mux,
                                client,
                                transport,
                                &line,
                                &writer,
                                &surface_scheduler,
                                arrived,
                            )
                        }
                    }
                }
            };
            drop(parses);
            zeroize_string(&mut line);
            if !keep_open {
                break ConnectionEnd::Dropped;
            }
        }
    }));
    let (end, panic) = match served {
        Ok(end) => (end, None),
        Err(panic) => {
            // Close the queue first: the writer ends and shuts the socket
            // down even if the disconnect below finds a poisoned lock.
            writer.close();
            (ConnectionEnd::Dropped, Some(panic))
        }
    };
    input.release();
    if let ConnectionEnd::BadMessage(bad) = &end {
        ipc.bad_message();
        eprintln!("cmux-tui: client {client} closed: bad_message {}: {bad}", bad.reason());
    }
    if end == ConnectionEnd::PeerClosed {
        surface_scheduler.finish_and_wait();
    } else {
        let _ = surface_scheduler.close_and_wait(CONNECTION_SURFACE_SHUTDOWN_TIMEOUT);
    }
    disconnect_client(&mux, client, false);
    // The disconnect closed the writer's queue: the writer writes what is
    // left, shuts the socket down and releases its hold on the slot.
    drop(writer_thread);
    drop(connection_permit);
    if let Some(panic) = panic {
        std::panic::resume_unwind(panic);
    }
    end
}

/// The end of a connection's writer, however the writer ends: its queue
/// closes and its socket shuts down, so the client reads the end of the
/// stream and the reader's next read fails. It takes only locks that
/// recover from poison, so it runs while the writer unwinds too.
struct WriterEnd {
    writer: MessageWriter,
    socket: Box<dyn transport::Stream>,
}

impl Drop for WriterEnd {
    fn drop(&mut self) {
        self.writer.close();
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

/// The read side of one connection: the socket, the reader that cuts its
/// bytes, and on a local socket the descriptor FIFO `recvmsg` fills.
struct ConnectionInput {
    stream: Box<dyn transport::Stream>,
    messages: FrameReader,
    #[cfg(unix)]
    fds: cmux_ipc::fds::FdReceiver,
    /// The descriptors `fds` received, tagged with the bytes of their read.
    #[cfg(unix)]
    arrived: ArrivedFds,
    /// Bytes read from the stream so far: the offset of the next read.
    received: u64,
    /// The peer closed its side.
    ended: bool,
}

impl ConnectionInput {
    fn new(stream: Box<dyn transport::Stream>) -> Self {
        ConnectionInput {
            stream,
            messages: FrameReader::new(line_limits()),
            #[cfg(unix)]
            fds: cmux_ipc::fds::FdReceiver::default(),
            #[cfg(unix)]
            arrived: ArrivedFds::default(),
            received: 0,
            ended: false,
        }
    }

    /// One read into the reader; at end of stream marks it ended. When
    /// descriptors are queued, `allowed` says how many may wait.
    fn fill(&mut self, allowed: impl FnOnce() -> usize) -> Result<(), ConnectionEnd> {
        let read = loop {
            match self.read() {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => break result,
            }
        };
        match read {
            Ok(_) if self.descriptors_queued() > 0 && self.descriptors_queued() > allowed() => {
                Err(ConnectionEnd::BadMessage(ConnectionBadMessage::UnexpectedFds))
            }
            Ok(0) => {
                self.ended = true;
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(error) if is_descriptor_refusal(&error) => {
                Err(ConnectionEnd::BadMessage(ConnectionBadMessage::UnexpectedFds))
            }
            Err(_) => Err(ConnectionEnd::Dropped),
        }
    }

    /// One read straight into the reader's buffer; the descriptors it
    /// brings are tagged with its bytes.
    #[cfg(unix)]
    fn read(&mut self) -> std::io::Result<usize> {
        let ConnectionInput { stream, messages, fds, arrived, received, .. } = self;
        let read = match stream.socket_fd() {
            Some(fd) => messages.read_with(|room| fds.recv_into(fd, room, |_| {})),
            None => messages.read_from(stream),
        }?;
        let start = *received;
        *received = start.saturating_add(read as u64);
        arrived.arrived(fds, start..*received);
        Ok(read)
    }

    #[cfg(not(unix))]
    fn read(&mut self) -> std::io::Result<usize> {
        let read = self.messages.read_from(&mut self.stream)?;
        self.received = self.received.saturating_add(read as u64);
        Ok(read)
    }

    #[cfg(unix)]
    fn descriptors_queued(&self) -> usize {
        self.arrived.len()
    }

    /// Whether a descriptor came with the bytes up to `end`, the end of the
    /// message just routed, and no message took it.
    #[cfg(unix)]
    fn stray_descriptor(&self, end: u64) -> bool {
        self.arrived.stray_before(end)
    }

    #[cfg(not(unix))]
    fn stray_descriptor(&self, _end: u64) -> bool {
        false
    }

    #[cfg(not(unix))]
    fn descriptors_queued(&self) -> usize {
        0
    }

    /// Zeroes the line the reader still lends (it does so on its next call).
    fn release(&mut self) {
        self.messages.push(&[]);
    }
}

#[cfg(unix)]
fn is_descriptor_refusal(error: &std::io::Error) -> bool {
    error.get_ref().is_some_and(|source| source.is::<cmux_ipc::fds::FdBadMessage>())
}

#[cfg(not(unix))]
fn is_descriptor_refusal(_error: &std::io::Error) -> bool {
    false
}

#[cfg(all(test, unix))]
#[path = "line_connection_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "echo_thread_rows_tests.rs"]
mod echo_thread_rows_tests;

#[cfg(all(test, unix))]
#[path = "line_connection_panic_mode_tests.rs"]
mod panic_mode_tests;

#[cfg(all(test, unix))]
#[path = "request_work_panic_mode_tests.rs"]
mod request_work_panic_mode_tests;
