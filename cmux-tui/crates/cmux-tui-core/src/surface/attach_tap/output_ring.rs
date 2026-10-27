//! A terminal's shared-memory output ring (`terminal-output-ring-v1`,
//! plans/cmux-next/app-daemon-connection-DESIGN.md section 4, p05-ipc-111).
//!
//! The daemon makes a terminal's ring on its first ring attach and from then
//! on writes every output byte the parser accepted into it, in output order:
//! the attach broadcast publishes the bytes under the `pty.taps` lock right
//! after it counted them in the snapshot offset, so a ring offset is a
//! snapshot offset and the ring holds exactly the parser's bytes. The ring
//! lives until the terminal ends; a later attach, or a reattach by offset,
//! maps the same one.
//!
//! A ring viewer's queue holds no bytes, only the newest published end (a
//! [`RingAdvance`]). Consecutive advances merge and the newest end wins, so a
//! viewer that is not reading has one advance waiting however much output
//! arrives: the reader learns the state "readable up to here", not each
//! write, as a Mojo data pipe's consumer is signalled readable once for any
//! number of writes (mojo/core/data_pipe_consumer_dispatcher.cc).
//!
//! Each ring viewer has a consumer cursor in the ring (p05-ipc-119): its app
//! acks the offset below which it holds no span (RING_ACK), and the ring
//! never writes a byte the app may still parse. Output that would skips the
//! ring instead and gaps the viewers ([`RingPublish::gapped`]); a gapped
//! viewer's resync waits until the ring releases its cursor
//! ([`RingPublish::released`]): when every gapped viewer acked its gap (the
//! stall is over) or its own hold lapsed. So the READY it gets is taken
//! after its app let go of every old span, and after the last skip of the
//! stall: one stall is one READY per viewer (attach_tap.rs, `ring_gap`).

use super::super::TerminalColors;

/// What publishing one chunk in the ring did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RingPublish {
    /// The ring's end after the chunk: what the viewers' advance names.
    pub(crate) end: u64,
    /// Cursors the chunk gapped (the ring skipped it for one of them).
    pub(crate) gapped: Vec<u64>,
    /// Cursors the chunk released: their viewers' READY goes now.
    pub(crate) released: Vec<u64>,
}

/// The end of the published ring bytes a ring viewer may read next: one
/// RING_ADVANCE on its stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RingAdvance {
    /// Offset after the last published byte.
    pub(crate) end: u64,
    /// The key press the bytes up to `end` echo (0: none).
    pub(crate) flow: u64,
    /// The colors one parser transition replaced, applied after the bytes up
    /// to `end` (the `OutputWithColors` of a byte viewer).
    pub(crate) colors: Option<Box<TerminalColors>>,
}

impl RingAdvance {
    pub(crate) fn new(end: u64, flow: u64, colors: Option<Box<TerminalColors>>) -> Self {
        Self { end, flow, colors }
    }

    /// Takes `next` into this advance: the newest end wins. Refused (handed
    /// back) when the merge would move what this advance says: its colors
    /// apply after the bytes up to its own end, and of two echoes one would
    /// be lost.
    pub(super) fn merge(&mut self, next: Self) -> Result<(), Self> {
        if self.colors.is_some() || (self.flow != 0 && next.flow != 0) || next.end < self.end {
            return Err(next);
        }
        self.end = next.end;
        if next.flow != 0 {
            self.flow = next.flow;
        }
        self.colors = next.colors;
        Ok(())
    }

    /// What it holds in a viewer's backlog.
    pub(super) fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.colors.as_ref().map_or(0, |_| size_of::<TerminalColors>())
    }
}

#[cfg(unix)]
pub(crate) use ring::{OutputRingGrant, RingCursor, TerminalOutputRing};

#[cfg(unix)]
mod ring {
    use super::RingPublish;
    use cmux_ipc::output_ring::{
        DEFAULT_CAPACITY, LAYOUT, OutputRingWriter, RingAcked, RingError, RingReader,
    };
    use std::os::fd::OwnedFd;

    /// One terminal's ring and its only writer.
    pub(crate) struct TerminalOutputRing {
        writer: OutputRingWriter,
    }

    impl TerminalOutputRing {
        /// An empty ring whose next byte has offset `base` (the terminal's
        /// snapshot offset now).
        pub(crate) fn create(base: u64) -> Result<Self, RingError> {
            OutputRingWriter::create(base, DEFAULT_CAPACITY).map(|writer| Self { writer })
        }

        /// Writes and publishes `bytes`, or skips them when a viewer's app
        /// may still hold a byte they would overwrite. Never waits.
        pub(crate) fn publish(&mut self, bytes: &[u8]) -> RingPublish {
            let publish = self.writer.push(bytes);
            RingPublish { end: publish.end, gapped: publish.gapped, released: publish.released }
        }

        /// A viewer's RING_ACK.
        pub(crate) fn ack(&mut self, cursor: &RingReader, offset: u64) -> RingAcked {
            self.writer.ack(cursor, offset)
        }

        /// Chunks skipped for a cursor since the ring was made.
        #[cfg(test)]
        pub(crate) fn skips(&self) -> u64 {
            self.writer.skips()
        }

        /// Offset after the last published byte.
        pub(crate) fn end(&self) -> u64 {
            self.writer.end()
        }

        /// What one ring viewer's RING frame announces, with its own
        /// read-only descriptor, and the viewer's consumer cursor, acked at
        /// the ring's end now. The viewer's tap keeps the cursor: it closes
        /// when the viewer goes.
        pub(crate) fn grant(&mut self) -> Result<(OutputRingGrant, RingCursor), RingError> {
            let grant = OutputRingGrant {
                descriptor: self.writer.reader_fd()?,
                layout: LAYOUT,
                capacity: self.writer.capacity(),
                base: self.writer.base(),
                end: self.writer.end(),
            };
            let waits = self.writer.stalled();
            Ok((grant, RingCursor { reader: self.writer.open_reader(), waits }))
        }
    }

    /// A ring viewer's consumer cursor, as its tap gets it.
    #[derive(Debug)]
    pub(crate) struct RingCursor {
        pub(crate) reader: RingReader,
        /// It opened during a stall: the viewer's first READY waits for the
        /// ring to release it.
        pub(crate) waits: bool,
    }

    /// The ring one viewer may map: the RING frame's meta and the descriptor
    /// that rides with it (SCM_RIGHTS).
    #[derive(Debug)]
    pub(crate) struct OutputRingGrant {
        /// Read-only (`cmux_ipc::shm::create_read_only`): a reader cannot
        /// change the ring, and maps it only `PROT_READ`.
        pub(crate) descriptor: OwnedFd,
        pub(crate) layout: u32,
        pub(crate) capacity: u64,
        /// Offset of the first byte the ring held.
        pub(crate) base: u64,
        /// The ring's end when the viewer attached: the RING frame's offset.
        /// The viewer's own output starts at its first READY's cut, at or
        /// after it.
        pub(crate) end: u64,
    }
}
