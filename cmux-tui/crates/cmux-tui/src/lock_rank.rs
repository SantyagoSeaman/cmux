//! Debug-build lock ranks (lockdep style) for the remote session's locks.
//!
//! Chromium's `base/task/common/checked_lock.h` (CheckedLock) checks, in
//! DCHECK builds, that a thread takes locks only in their declared order;
//! cmux-tui-core's `lock_rank.rs` does the same for the mux and surface
//! locks. This module ranks the long-lived std locks of `RemoteSession`, its
//! `RemoteSidebarDoc` and each mirrored `RemoteSurface`: the client side of
//! every attached TUI, taken by the UI thread, the session reader and the
//! session workers at once.
//!
//! The order, outer first, is [`LockRank`]'s; each rank is one lock. A
//! thread may block on a ranked lock only when its rank is after every rank
//! the thread already holds. A blocking acquisition at a rank equal to or
//! before a held rank panics in debug and test builds and names both ranks,
//! so every test and debug build checks the order on each path it runs.
//! `try_lock` cannot deadlock: it is recorded but not checked. In release
//! builds the rank and the thread-local stack are compiled out;
//! [`RankedMutex`] is then a plain [`Mutex`] (`cmux_threads::sync`, whose
//! contended waits are timed in debug builds).
//!
//! The interactive write queue (`InteractiveWriter`) waits on a `Condvar`
//! with its lock and stays a plain `Mutex`; it takes no other lock while
//! held. The stack is this crate's: an inversion between these locks and
//! cmux-tui-core's ranked locks is not seen here. p10-stability-187 moves the
//! check into the shared `cmux-lock-rank` crate.

#![cfg_attr(not(test), deny(clippy::disallowed_types))]

use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, MutexGuard, PoisonError, TryLockError, TryLockResult};

use cmux_threads::sync::Mutex;

/// Rank of a ranked lock. A thread acquires ranks in ascending order.
///
/// The order comes from the nesting the suite takes (a recording run over
/// all of `nextest -p cmux-tui`) and a scan of every guard held across
/// another lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LockRank {
    /// `RemoteSession::tree_refresh`, held for a whole tree refresh: its
    /// requests (`pending`), the retired set, the tree cache and the
    /// browser sources.
    SessionTreeRefresh,
    /// `RemoteSession::cell_pixel_lifecycle`: a cell metric change and an
    /// attach commit or retirement take the surface map, the recovery and
    /// lease maps and each mirror's geometry under it.
    SessionCellPixelLifecycle,
    /// `RemoteSession::frame_logs`: the frame dump on drop walks the
    /// surfaces and their terminals under it.
    SessionFrameLogs,
    /// `RemoteSession::retired_surfaces`: the tree cache drops retired
    /// agents under it.
    SessionRetiredSurfaces,
    /// `RemoteSession::exited_surfaces`: a retirement clears the surface's
    /// lease and overflow recovery under it.
    SessionExitedSurfaces,
    /// `RemoteSession::browser_sources`: an attach falls back to the tree
    /// cache under it.
    SessionBrowserSources,
    /// `RemoteSession::surfaces`: a walk of the map reads each mirror's
    /// locks.
    SessionSurfaces,
    /// `RemoteSession::tree`.
    SessionTree,
    /// `RemoteSession::surface_leases`.
    SessionSurfaceLeases,
    /// `RemoteSession::surface_overflow_recovery`.
    SessionSurfaceOverflowRecovery,
    /// `RemoteSession::subscription_recovery`: events are emitted under it.
    SessionSubscriptionRecovery,
    /// `RemoteSession::primed_subscription`.
    SessionPrimedSubscription,
    /// `RemoteSession::pending`, the requests waiting for a response.
    SessionPending,
    /// `RemoteSession::capabilities`.
    SessionCapabilities,
    /// `RemoteSession::size_states`.
    SessionSizeStates,
    /// `RemoteSession::cell_pixels`.
    SessionCellPixels,
    /// `RemoteSidebarDoc::stream`: a check that the stream is open reads
    /// the model in the same condition.
    SidebarStream,
    /// `RemoteSidebarDoc::model`.
    SidebarModel,
    /// `RemoteSidebarDoc::cursor`.
    SidebarCursor,
    /// `RemoteSidebarDoc::pending`.
    SidebarPending,
    /// `RemoteSidebarDoc::fallback_read_at`.
    SidebarFallbackReadAt,
    /// `RemoteSurface::geometry_lifecycle`, held for a stream resize or a
    /// cell metric change: the terminal, encoders and cell size under it.
    SurfaceGeometryLifecycle,
    /// `RemoteSurface::term`: a resize or a terminal callback updates the
    /// mouse encoders, the cursor provenance and the cell size under it.
    SurfaceTerm,
    /// `RemoteSurface::mouse_encoders`.
    SurfaceMouseEncoders,
    /// `RemoteSurface::cursor_provenance`.
    SurfaceCursorProvenance,
    /// `RemoteSurface::cell_pixels`.
    SurfaceCellPixels,
    /// `RemoteSurface::reported_size`.
    SurfaceReportedSize,
    /// `RemoteSurface::browser`.
    SurfaceBrowser,
    /// The test-only hooks and markers: they take no ranked lock while held.
    #[cfg(test)]
    Leaf,
}

#[cfg(debug_assertions)]
mod held {
    use super::LockRank;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    thread_local! {
        /// The ranks this thread holds, with the id of each hold.
        static HELD: RefCell<Vec<(LockRank, u64)>> = const { RefCell::new(Vec::new()) };
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    #[track_caller]
    pub(super) fn check(rank: LockRank) {
        let violation = HELD.with(|held| {
            held.borrow().iter().map(|&(held, _)| held).max().filter(|&held| held >= rank)
        });
        // Panic outside the closure so the location is the acquisition site.
        if let Some(held) = violation {
            // crash-allow: the debug and test lock-order assertion; release builds compile it out.
            panic!(
                "lock order violation: acquiring {rank:?} while this thread holds {held:?}; \
                 the order is cmux_tui::lock_rank::LockRank's"
            );
        }
    }

    pub(super) fn push(rank: LockRank) -> u64 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        HELD.with(|held| held.borrow_mut().push((rank, id)));
        id
    }

    pub(super) fn pop(id: u64) {
        // Guards may drop out of order, and during thread teardown.
        let _ = HELD.try_with(|held| {
            let mut held = held.borrow_mut();
            if let Some(index) = held.iter().rposition(|&(_, held_id)| held_id == id) {
                held.remove(index);
            }
        });
    }
}

/// One rank this thread holds; dropping it releases the rank.
struct HeldRank {
    #[cfg(debug_assertions)]
    id: u64,
}

impl Drop for HeldRank {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        held::pop(self.id);
    }
}

/// A [`Mutex`] with a lock rank. The API mirrors `std::sync::Mutex`; the
/// guard holds a std `MutexGuard`.
pub(crate) struct RankedMutex<T> {
    inner: Mutex<T>,
    #[cfg(debug_assertions)]
    rank: LockRank,
}

impl<T> RankedMutex<T> {
    pub(crate) const fn new(rank: LockRank, value: T) -> Self {
        #[cfg(not(debug_assertions))]
        let _ = rank;
        Self {
            inner: Mutex::new(value),
            #[cfg(debug_assertions)]
            rank,
        }
    }

    /// Check that this lock's rank may be waited for now.
    #[track_caller]
    #[inline]
    fn check(&self) {
        #[cfg(debug_assertions)]
        held::check(self.rank);
    }

    /// Record that this thread now holds this lock's rank.
    #[inline]
    fn record(&self) -> HeldRank {
        #[cfg(debug_assertions)]
        {
            HeldRank { id: held::push(self.rank) }
        }
        #[cfg(not(debug_assertions))]
        {
            HeldRank {}
        }
    }

    #[track_caller]
    #[inline]
    pub(crate) fn lock(&self) -> LockResult<RankedGuard<'_, T>> {
        self.check();
        match self.inner.lock() {
            Ok(guard) => Ok(RankedGuard { guard, _held: self.record() }),
            Err(poison) => Err(PoisonError::new(RankedGuard {
                guard: poison.into_inner(),
                _held: self.record(),
            })),
        }
    }

    #[inline]
    pub(crate) fn try_lock(&self) -> TryLockResult<RankedGuard<'_, T>> {
        match self.inner.try_lock() {
            Ok(guard) => Ok(RankedGuard { guard, _held: self.record() }),
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
            Err(TryLockError::Poisoned(poison)) => {
                Err(TryLockError::Poisoned(PoisonError::new(RankedGuard {
                    guard: poison.into_inner(),
                    _held: self.record(),
                })))
            }
        }
    }
}

/// A held [`RankedMutex`]. The lock is released before its rank.
#[must_use = "the lock is released when the guard drops"]
pub(crate) struct RankedGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    _held: HeldRank,
}

impl<T> Deref for RankedGuard<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for RankedGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(all(test, debug_assertions))]
#[path = "lock_rank_tests.rs"]
mod tests;
