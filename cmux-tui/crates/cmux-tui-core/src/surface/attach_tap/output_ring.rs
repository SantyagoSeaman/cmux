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

use super::super::TerminalColors;

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
pub(crate) use ring::{OutputRingGrant, TerminalOutputRing};

#[cfg(unix)]
mod ring {
    use cmux_ipc::output_ring::{DEFAULT_CAPACITY, LAYOUT, OutputRingWriter, RingError};
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

        /// Writes and publishes `bytes`; the new end. Never waits for a reader.
        pub(crate) fn publish(&mut self, bytes: &[u8]) -> u64 {
            self.writer.push(bytes);
            self.writer.end()
        }

        /// Offset after the last published byte.
        pub(crate) fn end(&self) -> u64 {
            self.writer.end()
        }

        /// What one ring viewer's RING frame announces, with its own
        /// read-only descriptor.
        pub(crate) fn grant(&self) -> Result<OutputRingGrant, RingError> {
            Ok(OutputRingGrant {
                descriptor: self.writer.reader_fd()?,
                layout: LAYOUT,
                capacity: self.writer.capacity(),
                base: self.writer.base(),
                end: self.writer.end(),
            })
        }
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
