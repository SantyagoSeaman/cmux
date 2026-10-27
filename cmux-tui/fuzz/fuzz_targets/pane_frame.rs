//! The pane protocol's stream reader (`cmux_pane_protocol::frame::
//! read_message`, the reader task of `transport::framed` on every unix
//! socket provider) against its buffer decoder (`frame::decode`). A socket
//! hands the reader bytes in whatever pieces the kernel returns, so the
//! input is the size of every read and then the stream: byte 0 is the count
//! N of read sizes, bytes 1..=N the sizes (cycled; 0 reads as 1; with none
//! one read returns the whole stream), the rest the stream. A corpus frame
//! behind a 0x00 byte is a seed as it is. The `pane_protocol_frame` target
//! covers the buffer decoder alone.
//!
//! Properties, beyond "no crash, leak or hang":
//! - however the stream is cut into reads, the reader yields the messages
//!   the buffer decoder yields, in order;
//! - the reader stops where the buffer decoder stops: a clean end after
//!   the last whole frame (or inside a header, which `read_exact` cannot
//!   tell from it), an error on a refused header, on a frame body the
//!   stream cuts short, and on a text body that is not UTF-8.

#![no_main]

use std::future::Future;
use std::io;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

use cmux_pane_protocol::frame::{self, Message};
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncRead, ReadBuf};

/// A socket that is always ready and returns at most the next read size.
struct Chunked<'a> {
    stream: &'a [u8],
    reads: &'a [u8],
    next: usize,
}

impl<'a> Chunked<'a> {
    fn split(data: &'a [u8]) -> Option<Self> {
        let (count, rest) = data.split_first()?;
        let (reads, stream) = rest.split_at_checked(usize::from(*count))?;
        Some(Self { stream, reads, next: 0 })
    }
}

impl AsyncRead for Chunked<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let size = match self.reads {
            [] => usize::MAX,
            reads => usize::from(reads[self.next % reads.len()].max(1)),
        };
        self.next = self.next.wrapping_add(1);
        let count = size.min(buf.remaining()).min(self.stream.len());
        let (now, rest) = self.stream.split_at(count);
        buf.put_slice(now);
        self.stream = rest;
        Poll::Ready(Ok(()))
    }
}

/// How a run of the reader or the decoder ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    Clean,
    Error,
}

/// `future` polled once: the reader never waits, so it is ready.
fn now<T>(future: impl Future<Output = T>) -> T {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("a read of an always-ready stream is pending"),
    }
}

fn decoded(mut rest: &[u8]) -> (Vec<Message>, End) {
    let mut messages = Vec::new();
    loop {
        match frame::decode(rest) {
            Ok(Some((message, consumed))) => {
                messages.push(message);
                rest = rest.get(consumed..).unwrap_or_default();
            }
            // Bytes the decoder still waits for at the end of the stream:
            // part of a header is a clean end to the reader, part of a body
            // is a short read.
            Ok(None) if rest.len() < 4 => return (messages, End::Clean),
            Ok(None) | Err(_) => return (messages, End::Error),
        }
    }
}

fn read(mut reader: Chunked<'_>) -> (Vec<Message>, End) {
    let mut messages = Vec::new();
    loop {
        match now(frame::read_message(&mut reader)) {
            Ok(Some(message)) => messages.push(message),
            Ok(None) => return (messages, End::Clean),
            Err(_) => return (messages, End::Error),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some(reader) = Chunked::split(data) else { return };
    let expected = decoded(reader.stream);
    assert_eq!(read(reader), expected, "the stream reader and the buffer decoder differ");
});
