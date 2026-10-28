//! The bench row that gates SHARED_BUFFER (p05-ipc-115): one large request
//! body from a sender thread to a receiver thread over a Unix socket pair
//! with the daemon's 512 KiB buffers, three ways:
//!
//! - `inline`: one REQUEST frame (what a framed request is today);
//! - `chunked`: 64 KiB frames the receiver joins (BULK_FRAME_MAX chunks);
//! - `shared`: one SHARED_BUFFER frame through the pool, copied out by the
//!   receiver (`cmux_ipc::shared_buffer`).
//!
//! The receiver answers each body with one byte, as a reply frees the
//! region; the sender times send-to-answer. Each row prints one JSON line:
//! p50/p95/p99/max microseconds, MB/s and process CPU microseconds per body.
//!
//! ```text
//! cargo run --release -p cmux-ipc --example shared_buffer_bench -- [SIZE ...]
//! ```
//!
//! Run it on a fleet Mac for evidence; a Linux VM row is a direction only.

#[cfg(unix)]
fn main() {
    bench::main();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("shared_buffer_bench needs a Unix socket");
}

#[cfg(unix)]
#[allow(
    unsafe_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod bench {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    use cmux_ipc::fds::{self, FdReceiver};
    use cmux_ipc::shared_buffer::{SharedBufferDescriptor, SharedBufferMappings, SharedBufferPool};
    use cmux_ipc::{
        Aux, CallAux, CallFlags, FrameHeader, FrameKind, FrameReader, Message, PriorityHint,
        ReaderLimits,
    };

    const SOCKET_BUFFER: libc::c_int = 512 * 1024;
    const CHUNK: usize = 64 * 1024;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Way {
        Inline,
        Chunked,
        Shared,
    }

    fn sized(socket: &UnixStream) {
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: setsockopt reads one c_int.
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&SOCKET_BUFFER as *const libc::c_int).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
        }
    }

    fn cpu_seconds() -> f64 {
        // SAFETY: a zeroed rusage is a valid buffer for getrusage.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: getrusage fills `usage`.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
        seconds(usage.ru_utime) + seconds(usage.ru_stime)
    }

    fn call(request_id: u32, flags: CallFlags) -> CallAux {
        CallAux { request_id, method_id: 0x0101, priority: PriorityHint::Control, flags }
    }

    fn header(len: usize, call: CallAux) -> [u8; 16] {
        FrameHeader::new(FrameKind::Request, 0, len as u32, Aux::Call(call)).unwrap().encode()
    }

    /// What the sender writes for one body inline or chunked.
    fn frames(way: Way, request_id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 16 * (body.len() / CHUNK + 1));
        let chunks: Vec<&[u8]> =
            if way == Way::Chunked { body.chunks(CHUNK).collect() } else { vec![body] };
        let last = chunks.len() - 1;
        for (index, chunk) in chunks.into_iter().enumerate() {
            let flags = if index < last { CallFlags::MORE } else { CallFlags::empty() };
            out.extend_from_slice(&header(chunk.len(), call(request_id, flags)));
            out.extend_from_slice(chunk);
        }
        out
    }

    /// The receiver: cuts frames as the daemon's reader does, gets each
    /// body as owned or shared bytes, answers one byte per body.
    fn serve(mut socket: UnixStream, way: Way, count: usize, len: usize) {
        let mut reader = FrameReader::new(ReaderLimits::default());
        reader.negotiate_frames();
        let mut fds = FdReceiver::default();
        let mut mappings = SharedBufferMappings::default();
        let mut joined: Vec<u8> = Vec::with_capacity(len);
        let mut done = 0;
        while done < count {
            let Some(message) = reader.next_message().unwrap() else {
                let read = reader
                    .read_with(|room| fds.recv_into(socket.as_raw_fd(), room, |_| {}))
                    .unwrap();
                assert_ne!(read, 0);
                continue;
            };
            let Message::Frame(frame) = message else { panic!("a line") };
            let complete = match way {
                Way::Inline => {
                    std::hint::black_box(frame.payload());
                    true
                }
                Way::Chunked => {
                    joined.extend_from_slice(frame.payload());
                    let more = frame.header().call().unwrap().flags.contains(CallFlags::MORE);
                    if !more {
                        std::hint::black_box(&joined);
                        joined.clear();
                    }
                    !more
                }
                Way::Shared => {
                    let Aux::SharedBufferSize(bytes) = frame.header().aux() else { panic!("aux") };
                    let descriptor = SharedBufferDescriptor::decode(frame.payload()).unwrap();
                    let fd = if descriptor.attached { fds.take(1).unwrap().pop() } else { None };
                    let body = mappings.receive(bytes, &descriptor, fd).unwrap();
                    assert_eq!(body.len(), len);
                    std::hint::black_box(body);
                    true
                }
            };
            if complete {
                socket.write_all(&[1]).unwrap();
                done += 1;
            }
        }
    }

    fn row(way: Way, len: usize) -> String {
        let count = (2_000_000_000 / len).clamp(30, 2000);
        let body: Vec<u8> = (0..len).map(|at| (at as u8).wrapping_mul(31)).collect();
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sized(&sender);
        sized(&receiver);
        let served = std::thread::spawn(move || serve(receiver, way, count, len));
        let mut pool = SharedBufferPool::default();
        let mut samples = Vec::with_capacity(count);
        let cpu = cpu_seconds();
        let started = Instant::now();
        let mut answer = [0u8; 1];
        for request in 0..count as u32 {
            let sent = Instant::now();
            match way {
                Way::Inline | Way::Chunked => {
                    sender.write_all(&frames(way, request, &body)).unwrap();
                }
                Way::Shared => {
                    let staged = pool
                        .stage(0, call(request, CallFlags::empty()), &body)
                        .unwrap()
                        .expect("a free region");
                    let passed: Vec<_> = staged.fd.iter().map(AsRawFd::as_raw_fd).collect();
                    fds::send_all(sender.as_raw_fd(), &staged.frame, &passed).unwrap();
                }
            }
            sender.read_exact(&mut answer).unwrap();
            pool.release(request);
            samples.push(sent.elapsed().as_secs_f64() * 1e6);
        }
        let wall = started.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu;
        served.join().unwrap();
        samples.sort_by(f64::total_cmp);
        let pick = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
        format!(
            "{{\"way\":\"{way:?}\",\"bytes\":{len},\"n\":{count},\"p50_us\":{:.1},\"p95_us\":{:.1},\"p99_us\":{:.1},\"max_us\":{:.1},\"mb_per_s\":{:.0},\"cpu_us_per_body\":{:.1}}}",
            pick(0.5),
            pick(0.95),
            pick(0.99),
            samples[samples.len() - 1],
            (len * count) as f64 / wall / 1e6,
            cpu / count as f64 * 1e6,
        )
    }

    pub(super) fn main() {
        let sizes: Vec<usize> =
            std::env::args().skip(1).map(|size| size.parse().expect("a size in bytes")).collect();
        let sizes =
            if sizes.is_empty() { vec![128 * 1024, 1024 * 1024, 10 * 1024 * 1024] } else { sizes };
        for len in sizes {
            for way in [Way::Inline, Way::Chunked, Way::Shared] {
                println!("{}", row(way, len));
            }
        }
    }
}
