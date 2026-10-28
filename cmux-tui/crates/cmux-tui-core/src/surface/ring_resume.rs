//! Resume by offset on reattach (p05-ipc-114;
//! plans/cmux-next/app-daemon-connection-DESIGN.md 4.3, "Reconnect and resume
//! by offset").
//!
//! A ring viewer that reattaches (a new stream after a lost one, or after a
//! queue overflow that recovered) names the PTY offset it parsed and the grid
//! generation of its screen. When the terminal's ring still holds every byte
//! from that offset and the grid was not replaced since, the viewer's screen
//! plus those ring bytes is the terminal now: it gets no READY, and its first
//! event is one ring advance from the offset to the ring's end. Otherwise it
//! is an ordinary ring attach whose first event is a READY. Chromium resumes
//! an interrupted download the same way: from the bytes it has when the
//! server's validators still match, else from the start
//! (components/download/internal/common/download_item_impl.cc,
//! `ResumeInterruptedDownload`; the generation is the validator here).

use super::*;

/// Where a reattaching ring viewer's screen is (`attach-surface`
/// `resume_offset` and `resume_generation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), expect(dead_code, reason = "only a ring stream reads it; rings are unix"))]
pub(crate) struct RingResume {
    /// The PTY offset after the last byte the viewer parsed.
    pub(crate) offset: u64,
    /// The grid generation of the viewer's screen (its last READY's).
    pub(crate) generation: u64,
}

/// A ring attach that named a [`RingResume`].
#[cfg(unix)]
#[cfg_attr(not(test), expect(dead_code, reason = "attached by attach frames once bound"))]
pub(crate) enum RingResumeAttach {
    /// No READY: the viewer's first event is the advance from `from` to the
    /// ring's end, which carries the terminal's colors now (a colors change
    /// while the viewer was away is in no ring byte).
    Resumed { stream: SnapshotAttachStream, ring: OutputRingGrant, from: u64 },
    /// The resume could not be served (`why`): an ordinary ring attach,
    /// whose first event is a READY.
    Ready { attach: RingAttachStream, why: ResumeRefused },
}

/// Why a resume gets a READY instead.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) enum ResumeRefused {
    /// The terminal has no ring (no ring viewer made one), or it ended.
    NoRing,
    /// The grid was replaced (a resize or a host replay) after the viewer's
    /// screen: ring bytes alone do not give the new grid.
    Generation { resumed: u64, current: u64 },
    /// The ring does not hold every byte from `offset`: older than its
    /// oldest byte, or past its end.
    OutsideRing { offset: u64, oldest: u64, end: u64 },
    /// The ring's descriptor could not be copied for the viewer.
    Ring(cmux_ipc::output_ring::RingError),
}

/// The reason a ring attach logs when it sends a READY instead.
#[cfg(unix)]
impl std::fmt::Display for ResumeRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRing => f.write_str("the terminal has no ring"),
            Self::Generation { resumed, current } => {
                write!(f, "the grid is at generation {current}, the screen at {resumed}")
            }
            Self::OutsideRing { offset, oldest, end } => {
                write!(f, "offset {offset} is outside the ring ({oldest}..={end})")
            }
            Self::Ring(error) => error.fmt(f),
        }
    }
}

#[cfg(unix)]
impl SnapshotRequestGate {
    /// A resumed viewer's gate: unlike a new viewer's, no snapshot is pending
    /// at attach, so its first `snapshot-request` is admitted.
    fn resumed(handle: SnapshotRequestHandle) -> Self {
        Self { handle, state: Arc::new(Mutex::new(GateState::default())) }
    }
}

#[cfg(unix)]
impl Surface {
    /// A ring attach that resumes at `resume` when it can
    /// ([`RingResumeAttach::Resumed`]), else an ordinary ring attach
    /// ([`Self::attach_snapshot_ring_stream`]). Grant it only to a connection
    /// whose peer may map the ring (server/capabilities.rs,
    /// `grants_output_ring`).
    #[cfg_attr(not(test), expect(dead_code, reason = "attached by attach frames once bound"))]
    pub(crate) fn resume_snapshot_ring_stream(
        &self,
        lifecycle: AttachLifecycle,
        backlog_bytes: usize,
        local_history: bool,
        resume: RingResume,
    ) -> ghostty_vt::Result<RingResumeAttach> {
        match self.register_resumed_ring_viewer(&lifecycle, backlog_bytes, local_history, resume)? {
            Ok((stream, ring)) => {
                Ok(RingResumeAttach::Resumed { stream, ring, from: resume.offset })
            }
            Err(why) => self
                .attach_snapshot_ring_stream(lifecycle, backlog_bytes, local_history)
                .map(|attach| RingResumeAttach::Ready { attach, why }),
        }
    }

    /// Under one terminal lock: the resume check, the viewer's queue with its
    /// first advance, and its registration, so no output byte lands between
    /// the ring's end in the advance and the viewer's first queued advance
    /// after it.
    fn register_resumed_ring_viewer(
        &self,
        lifecycle: &AttachLifecycle,
        backlog_bytes: usize,
        local_history: bool,
        resume: RingResume,
    ) -> ghostty_vt::Result<Result<(SnapshotAttachStream, OutputRingGrant), ResumeRefused>> {
        let Some(pty) = self.as_pty() else {
            return Err(ghostty_vt::Error::InvalidValue);
        };
        let term = pty.term.lock().unwrap();
        if pty.dead.load(Ordering::Acquire) {
            return Ok(Err(ResumeRefused::NoRing));
        }
        let (generation, offset) = pty.snapshot_position.load();
        if generation != resume.generation {
            return Ok(Err(ResumeRefused::Generation {
                resumed: resume.generation,
                current: generation,
            }));
        }
        let mut taps = pty.taps.lock().unwrap();
        let ring = match taps.ring_resume_grant(resume.offset) {
            Ok(ring) => ring,
            Err(why) => return Ok(Err(why)),
        };
        debug_assert_eq!(ring.end, offset, "the ring's end is the snapshot offset");
        let defaults = pty.mux.upgrade().map(|mux| mux.default_colors()).unwrap_or_default();
        let colors = pty.terminal_colors_locked(&term, defaults);
        let advance = RingAdvance::new(ring.end, 0, Some(Box::new(colors)));
        let (tap, receiver) = AttachTap::ring_resume_pair(
            lifecycle.clone(),
            ATTACH_STREAM_CAPACITY,
            backlog_bytes,
            advance,
        );
        receiver.set_local_history(local_history);
        taps.push(tap);
        drop(taps);
        drop(term);
        let requests = SnapshotRequestGate::resumed(receiver.snapshot_request_handle());
        Ok(Ok((SnapshotAttachStream { receiver, lifecycle: lifecycle.clone(), requests }, ring)))
    }
}
