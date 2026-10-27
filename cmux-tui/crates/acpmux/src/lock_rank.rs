//! Debug-build lock ranks (lockdep style) for the hub and session locks.
//!
//! Chromium's `base/task/common/checked_lock.h` (CheckedLock) checks, in
//! DCHECK builds, that a thread takes locks only in their declared order;
//! cmux-tui-core's `lock_rank.rs` does the same for the mux and surface
//! locks. This module ranks the long-lived std locks of
//! [`Hub`](crate::hub::Hub) and [`Session`](crate::hub::Session), and of the
//! parts they own (`hub/`'s services, the pool, the watchers, the floor).
//!
//! The order, outer first, is [`LockRank`]'s. A thread may block on a ranked
//! lock only when its rank is after every rank the thread already holds. A
//! blocking acquisition at a rank equal to or before a held rank panics in
//! debug and test builds and names both locks, so every test and debug
//! build checks the order on each path it runs. `try_lock` cannot deadlock:
//! it is recorded but not checked. In release builds the rank, the name and
//! the thread-local stack are compiled out; [`RankedMutex`] is then a plain
//! [`Mutex`].
//!
//! A std guard is not `Send`, so a spawned task cannot hold one across an
//! `.await` and the thread that took a lock is the one that releases it. The
//! tokio locks (`Session::child`, `spawn_lock`, `turn_lock`) wait
//! asynchronously and are not ranked.
//!
//! Also not ranked: the meta and config writers' queues (`hub/meta_writer.rs`,
//! `hub/config_writer.rs`), which a `Condvar` waits on and so need a std guard;
//! the process-wide worktree care state (`hub/worktree_care.rs`, a static, not
//! the hub's); and the locks of the layers below the hub (the mailbox, the
//! store's log writer, a peer link, pairing, the catalog, harness health, the
//! chat index), whose guards stay inside their own modules and call no hub
//! code, so they are always taken innermost.

use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError, TryLockError, TryLockResult};

/// Rank of a ranked lock. A thread acquires ranks in ascending order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockRank {
    /// A pooled entry's tap slot (`hub/pool/held.rs`): its held lines replay
    /// through the session tap under it, and the tap logs and takes any
    /// session lock.
    PooledTap,
    /// `Hub::sessions`, the session registry: a lookup reads a session's
    /// `meta` under it, a create makes the session (`clock`).
    Sessions,
    /// `Hub::peer_leases`: a lease change starts or removes the peer
    /// (`peers`) and purges its rows (`remote_sessions`).
    PeerLeases,
    /// `Hub::remote_sessions`, the peers' rows: a row update checks the
    /// peer's generation (`peers`).
    RemoteSessions,
    /// `Hub::peers`.
    Peers,
    /// `Session::moving`: a bookmark change logs and saves under it.
    SessionMoving,
    /// `Session::composers`: its record reads the running turn and the queue.
    SessionComposers,
    /// `Session::queue`: takes the prompt ledger and the restored prompts.
    SessionQueue,
    /// `Session::prompts`, the prompt ledger: a settled prompt is logged under it.
    SessionPrompts,
    /// `Session::permissions`: an ask or a decision is logged and the
    /// status set under it.
    SessionPermissions,
    /// `Session::silent_turn`: `turn_silent` is logged under it.
    SessionSilentTurn,
    /// `Session::processes`: a fold reads the session's `meta`.
    SessionProcesses,
    /// `Session::append_lock`, held from a record's sequence until it is
    /// stored and sent: it updates `meta` and the idle clock.
    SessionAppend,
    /// `Session::meta`: a Web control verdict reads `Hub::web_modes`.
    SessionMeta,
    /// `Hub::known_models`: a probe pick reads `Hub::probe_only`.
    KnownModels,
    /// `Hub::auto_resumes`: a turn's end takes `Session::turn` under it, in one
    /// step with Stop's check (`hub/auto_resume.rs`).
    AutoResumes,
    /// Every other lock of the hub and a session itself: it takes no ranked
    /// lock but sub-state and the clock while held.
    Leaf,
    /// A sub-state lock that takes one [`Self::SubState`] lock of its own part
    /// while held: the pool's entries (a cap pass marks the oversize keys), a
    /// profile or settings watcher's folder list (its notify watcher or kernel
    /// watch changes), the outbox's deliveries (a save reads the file path).
    SubStateOuter,
    /// The sub-state the hub and a session own through their parts: the
    /// services' maps (secrets, offers, outbox, the subsession index and runs,
    /// trust waits, agent pairs, briefs, the workflow index, the person gate),
    /// the pool's other tables, the watchers' handles, the floor marks and a
    /// queued prompt's join slot. It takes no ranked lock but the clock while
    /// held, so any other rank may be held when it is taken.
    SubState,
    /// `Hub::clock`: read under any other lock (`Hub::clock_now`).
    Clock,
}

#[cfg(debug_assertions)]
mod held {
    use super::LockRank;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Held {
        rank: LockRank,
        name: &'static str,
        id: u64,
    }

    thread_local! {
        static HELD: RefCell<Vec<Held>> = const { RefCell::new(Vec::new()) };
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    #[track_caller]
    pub(super) fn check(rank: LockRank, name: &'static str) {
        let violation = HELD.with(|held| {
            held.borrow()
                .iter()
                .max_by_key(|entry| entry.rank)
                .filter(|inner| inner.rank >= rank)
                .map(|inner| (inner.name, inner.rank))
        });
        // Panic outside the closure so the location is the acquisition site.
        if let Some((held_name, held_rank)) = violation {
            // crash-allow: the debug and test lock-order assertion; release builds compile it out.
            panic!(
                "lock order violation: acquiring {name} ({rank:?}) while this thread holds \
                 {held_name} ({held_rank:?}); the order is acpmux::lock_rank::LockRank's"
            );
        }
    }

    pub(super) fn push(rank: LockRank, name: &'static str) -> u64 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        HELD.with(|held| held.borrow_mut().push(Held { rank, name, id }));
        id
    }

    pub(super) fn pop(id: u64) {
        // Guards may drop out of order, and during thread teardown.
        let _ = HELD.try_with(|held| {
            let mut held = held.borrow_mut();
            if let Some(index) = held.iter().rposition(|entry| entry.id == id) {
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

impl HeldRank {
    /// Check that `rank` may be acquired now. Call before blocking on the lock.
    #[track_caller]
    #[inline]
    fn check(rank: LockRank, name: &'static str) {
        #[cfg(debug_assertions)]
        held::check(rank, name);
        #[cfg(not(debug_assertions))]
        let _ = (rank, name);
    }

    /// Record that this thread now holds `rank`.
    #[inline]
    fn record(rank: LockRank, name: &'static str) -> Self {
        #[cfg(debug_assertions)]
        {
            Self { id: held::push(rank, name) }
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = (rank, name);
            Self {}
        }
    }
}

impl Drop for HeldRank {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        held::pop(self.id);
    }
}

/// A [`Mutex`] with a lock rank. The API mirrors `std::sync::Mutex`.
pub struct RankedMutex<T> {
    inner: Mutex<T>,
    #[cfg(debug_assertions)]
    rank: LockRank,
    #[cfg(debug_assertions)]
    name: &'static str,
}

impl<T> RankedMutex<T> {
    pub const fn new(rank: LockRank, name: &'static str, value: T) -> Self {
        #[cfg(not(debug_assertions))]
        let _ = (rank, name);
        Self {
            inner: Mutex::new(value),
            #[cfg(debug_assertions)]
            rank,
            #[cfg(debug_assertions)]
            name,
        }
    }

    #[cfg(debug_assertions)]
    #[inline]
    fn rank(&self) -> (LockRank, &'static str) {
        (self.rank, self.name)
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    fn rank(&self) -> (LockRank, &'static str) {
        (LockRank::Leaf, "")
    }

    /// The value through an exclusive borrow: no other thread can hold the lock, so no rank is
    /// taken.
    #[inline]
    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.inner.get_mut()
    }

    #[track_caller]
    #[inline]
    pub fn lock(&self) -> LockResult<RankedGuard<'_, T>> {
        let (rank, name) = self.rank();
        HeldRank::check(rank, name);
        match self.inner.lock() {
            Ok(guard) => Ok(RankedGuard { guard, _held: HeldRank::record(rank, name) }),
            Err(poison) => Err(PoisonError::new(RankedGuard {
                guard: poison.into_inner(),
                _held: HeldRank::record(rank, name),
            })),
        }
    }

    #[inline]
    pub fn try_lock(&self) -> TryLockResult<RankedGuard<'_, T>> {
        let (rank, name) = self.rank();
        match self.inner.try_lock() {
            Ok(guard) => Ok(RankedGuard { guard, _held: HeldRank::record(rank, name) }),
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
            Err(TryLockError::Poisoned(poison)) => {
                Err(TryLockError::Poisoned(PoisonError::new(RankedGuard {
                    guard: poison.into_inner(),
                    _held: HeldRank::record(rank, name),
                })))
            }
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RankedMutex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

/// A held [`RankedMutex`]. The lock is released before its rank.
#[must_use = "the lock is released when the guard drops"]
pub struct RankedGuard<'a, T> {
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
