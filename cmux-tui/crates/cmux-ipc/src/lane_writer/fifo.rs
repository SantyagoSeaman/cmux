//! One FIFO of `LaneWriter` (the control sequence or one stream), its
//! coalescing lookup, and how a long payload is cut into chunks.

use std::collections::VecDeque;

use crate::frame::{Aux, BadMessage, CallFlags, Frame, FrameKind, HEADER_LEN};

use super::CoalesceKey;

/// A freed FIFO keeps no more than this many slots of capacity.
const IDLE_CAPACITY: usize = 16;

/// A queued frame and how much of its payload has been written.
#[derive(Debug)]
pub(super) struct Queued {
    pub(super) frame: Frame,
    pub(super) sent: usize,
    pub(super) key: Option<CoalesceKey>,
}

impl Queued {
    pub(super) fn cost(&self) -> usize {
        HEADER_LEN.saturating_add(self.frame.payload().len().saturating_sub(self.sent))
    }
}

/// The cost of a frame not yet queued.
pub(super) fn frame_cost(frame: &Frame) -> usize {
    HEADER_LEN.saturating_add(frame.payload().len())
}

/// One FIFO: the control sequence or one stream.
#[derive(Debug, Default)]
pub(super) struct Fifo {
    pub(super) items: VecDeque<Queued>,
    pub(super) bytes: usize,
}

/// Makes a payload that `pop` will chunk shareable now: the first slice of a
/// Vec-backed payload allocates, and doing it at push keeps it out of `pop`.
fn prepare_chunks(frame: &Frame, chunk_bytes: usize) {
    if frame.payload().len() > chunk_bytes && chunk_rule(frame.header().kind()).is_some() {
        drop(frame.payload().clone());
    }
}

impl Fifo {
    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(super) fn push(&mut self, frame: Frame, key: Option<CoalesceKey>, chunk_bytes: usize) {
        prepare_chunks(&frame, chunk_bytes);
        let item = Queued { frame, sent: 0, key };
        self.bytes = self.bytes.saturating_add(item.cost());
        self.items.push_back(item);
    }

    /// The index of the unsent item of `stream` with `key` among the keyed
    /// items at the FIFO's tail: the walk back passes keyed items of other
    /// keys or streams and stops at an unkeyed or partly sent item.
    pub(super) fn coalesce_index(&self, key: CoalesceKey, stream: u16) -> Option<usize> {
        for (index, item) in self.items.iter().enumerate().rev() {
            let queued = item.key.filter(|_| item.sent == 0)?;
            if queued == key && item.frame.header().stream() == stream {
                return Some(index);
            }
        }
        None
    }

    /// Replaces the unsent item at `index`; returns false if it is gone.
    pub(super) fn replace(&mut self, index: usize, frame: Frame, chunk_bytes: usize) -> bool {
        let Some(item) = self.items.get_mut(index) else {
            return false;
        };
        prepare_chunks(&frame, chunk_bytes);
        let old = item.cost();
        item.frame = frame;
        let new = item.cost();
        self.bytes = self.bytes.saturating_sub(old).saturating_add(new);
        true
    }

    /// Takes the next frame or chunk of the front item. On a refused chunk
    /// the front item is dropped and the error returned.
    pub(super) fn pop_chunk(&mut self, chunk_bytes: usize) -> Option<Result<Frame, BadMessage>> {
        let front = self.items.front_mut()?;
        let len = front.frame.payload().len();
        let start = front.sent;
        let rule = chunk_rule(front.frame.header().kind());
        let Some(rule) = rule.filter(|_| start > 0 || len > chunk_bytes) else {
            return self.pop_front_item().map(|item| Ok(item.frame));
        };
        let end = len.min(start.saturating_add(chunk_bytes.max(1)));
        let chunk = chunk_frame(&front.frame, rule, start, end);
        front.sent = end;
        self.bytes = self.bytes.saturating_sub(end.saturating_sub(start));
        if chunk.is_err() || end >= len {
            self.pop_front_item();
        }
        Some(chunk)
    }

    fn pop_front_item(&mut self) -> Option<Queued> {
        let item = self.items.pop_front()?;
        self.bytes = self.bytes.saturating_sub(item.cost());
        if self.items.is_empty() {
            self.release_idle();
        }
        Some(item)
    }

    /// Frees the memory of an empty FIFO beyond a small reserve. Only frees.
    fn release_idle(&mut self) {
        if self.items.capacity() > IDLE_CAPACITY {
            self.items = VecDeque::new();
        }
    }

    /// Drops everything; returns the bytes dropped.
    pub(super) fn clear(&mut self) -> usize {
        let dropped = self.bytes;
        self.items.clear();
        self.bytes = 0;
        self.release_idle();
        dropped
    }
}

/// How a kind is cut into chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkRule {
    /// Each chunk carries the PTY offset of its first byte.
    Offset,
    /// Every chunk keeps the frame's kind and aux: the payload is an
    /// envelope whose bytes are not PTY bytes, so the offset stays the cut.
    SameAux,
    /// SNAPSHOT, then SNAPSHOT_MORE with the same offset.
    Snapshot,
    /// The same call aux, with MORE on every chunk but the last and FDS only
    /// on the first: the descriptors ride with the first chunk.
    Call,
}

const fn chunk_rule(kind: FrameKind) -> Option<ChunkRule> {
    match kind {
        FrameKind::Output => Some(ChunkRule::Offset),
        FrameKind::Replay => Some(ChunkRule::SameAux),
        FrameKind::Snapshot | FrameKind::SnapshotMore => Some(ChunkRule::Snapshot),
        FrameKind::Request | FrameKind::Reply | FrameKind::Error | FrameKind::TypedEvent => {
            Some(ChunkRule::Call)
        }
        FrameKind::Grid
        | FrameKind::Event
        | FrameKind::Ring
        | FrameKind::RingAdvance
        | FrameKind::Input
        | FrameKind::SharedBuffer
        | FrameKind::RingAck
        | FrameKind::Cancel => None,
    }
}

/// The frame carrying `frame`'s payload bytes `start..end`.
fn chunk_frame(
    frame: &Frame,
    rule: ChunkRule,
    start: usize,
    end: usize,
) -> Result<Frame, BadMessage> {
    let header = frame.header();
    let last = end >= frame.payload().len();
    let (kind, aux) = match (rule, header.aux()) {
        (ChunkRule::Offset, Aux::Offset(offset)) => {
            (header.kind(), Aux::Offset(offset.saturating_add(start as u64)))
        }
        (ChunkRule::Snapshot, aux) if start > 0 => (FrameKind::SnapshotMore, aux),
        (ChunkRule::Call, Aux::Call(mut call)) => {
            if !last {
                call.flags = call.flags.with(CallFlags::MORE);
            }
            if start > 0 {
                call.flags = call.flags.without(CallFlags::FDS);
            }
            (header.kind(), Aux::Call(call))
        }
        (_, aux) => (header.kind(), aux),
    };
    Frame::new(kind, header.stream(), aux, frame.payload().slice(start..end))
}
