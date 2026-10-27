//! Ranked locks shared by acpmux and the cmux-tui daemon (lockdep style).
//!
//! A crate declares a family of ranks (an enum implementing [`Rank`]). A
//! thread may acquire a ranked lock only when its rank is after every rank of
//! the same family the thread already holds. A blocking acquisition at a rank
//! equal to or before a held rank panics in debug and test builds and names
//! both locks, so every test and debug dogfood build checks the order on
//! every path it runs. `try_lock` cannot deadlock: it is recorded but not
//! checked. Ranks of different families have no declared order and are not
//! compared. In release builds the rank, the name and the thread-local stack
//! are compiled out; [`RankedMutex`] is then a plain [`Mutex`] that records
//! [`Rank::UNRANKED`] for the family's hooks.
//!
//! A family hooks every acquisition: [`Rank::before_wait`] runs before a
//! blocking wait (cmux-tui-core raises a lowered thread's QoS there),
//! [`Rank::on_held`] when the lock is held, and [`Rank::may_yield`] answers
//! [`HeldRank::may_yield`] in every build.
//!
//! Debug and test builds also time a contended [`RankedMutex::lock`]: the
//! lock is tried first and only a failed try reads the clock; the wait goes
//! to [`Rank::note_contended_wait`].
//!
//! Chromium's model: base/task/common/checked_lock.h and checked_lock_impl.cc
//! (the acquisition order validated per thread in DCHECK builds), and
//! base/synchronization/lock_impl.h (`LockImpl::Lock` tries first, so only the
//! contended path is timed).

#![forbid(unsafe_code)]

use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError, TryLockError, TryLockResult};
use std::time::Duration;
#[cfg(debug_assertions)]
use std::time::Instant;

#[cfg(debug_assertions)]
mod held;
#[cfg(all(test, debug_assertions))]
mod tests;

/// A family of lock ranks. A thread acquires ranks of one family in
/// ascending order.
pub trait Rank: Copy + Ord + fmt::Debug + 'static {
    /// Every rank of the family in order, outer first: `ALL[r.ordinal()] == r`.
    const ALL: &'static [Self];
    /// The order, outer first, as a violation panic names it.
    const ORDER: &'static str;
    /// The rank a [`RankedMutex`] passes to the hooks in release builds,
    /// which store no rank.
    const UNRANKED: Self;

    /// Kept from before a blocking wait until the lock is held.
    type Wait;
    /// Kept while the lock is held; dropped after the lock is released.
    type Hold;

    /// The index of this rank in [`Self::ALL`].
    fn ordinal(self) -> usize;

    /// Runs before the thread blocks on a lock of this rank, and before a
    /// lock taken without a wait is recorded.
    fn before_wait(self) -> Self::Wait;

    /// Runs when the thread holds a lock of this rank.
    fn on_held(self, wait: Self::Wait) -> Self::Hold;

    /// True when no rank of this family that this thread holds forbids it
    /// from yielding to other work. Read in every build.
    fn may_yield() -> bool {
        true
    }

    /// A contended [`RankedMutex::lock`] waited `waited` at `site`. Debug and
    /// test builds only; it must not panic, since it runs with the lock held.
    fn note_contended_wait(waited: Duration, site: &'static Location<'static>) {
        let _ = (waited, site);
    }
}

/// One rank this thread holds; dropping it releases the rank. In release
/// builds only the family's [`Rank::Hold`] is kept.
pub struct HeldRank<R: Rank> {
    #[cfg(debug_assertions)]
    id: u64,
    /// Dropped with the rank, after the lock the guard releases first.
    _hold: R::Hold,
}

/// A checked rank this thread is about to block on: [`Rank::before_wait`]
/// ran before the wait.
#[must_use = "the wait hook holds only until the lock is recorded with `held`"]
pub struct RankWait<R: Rank> {
    rank: R,
    name: &'static str,
    wait: R::Wait,
}

impl<R: Rank> RankWait<R> {
    /// A re-entry of a reentrant lock this thread holds: no order check and
    /// no wait.
    #[inline]
    pub fn reentry(rank: R, name: &'static str) -> Self {
        Self { rank, name, wait: rank.before_wait() }
    }

    /// Record that this thread now holds the rank it waited for.
    #[inline]
    pub fn held(self) -> HeldRank<R> {
        HeldRank::with_wait(self.rank, self.name, self.wait)
    }
}

impl<R: Rank> HeldRank<R> {
    /// Check that `rank` may be acquired now. Call before blocking on the lock.
    #[track_caller]
    #[inline]
    pub fn check(rank: R, name: &'static str) {
        #[cfg(debug_assertions)]
        held::check(rank, name);
        #[cfg(not(debug_assertions))]
        let _ = (rank, name);
    }

    /// [`Self::check`], then [`Rank::before_wait`]; [`RankWait::held`]
    /// records the acquisition.
    #[track_caller]
    #[inline]
    pub fn acquire(rank: R, name: &'static str) -> RankWait<R> {
        Self::check(rank, name);
        RankWait { rank, name, wait: rank.before_wait() }
    }

    /// Record that this thread now holds `rank`, taken without a wait (a
    /// `try_lock`, a re-entry).
    #[inline]
    pub fn record(rank: R, name: &'static str) -> Self {
        Self::with_wait(rank, name, rank.before_wait())
    }

    #[inline]
    fn with_wait(rank: R, name: &'static str, wait: R::Wait) -> Self {
        let hold = rank.on_held(wait);
        #[cfg(debug_assertions)]
        {
            Self { id: held::push(rank, name), _hold: hold }
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = name;
            Self { _hold: hold }
        }
    }

    /// [`Rank::may_yield`]. Debug and test builds check that a thread that
    /// may yield holds no rank of the family at all.
    #[inline]
    pub fn may_yield() -> bool {
        let free = R::may_yield();
        #[cfg(debug_assertions)]
        held::assert_none_held_if::<R>(free);
        free
    }
}

impl<R: Rank> Drop for HeldRank<R> {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        held::pop(self.id);
    }
}

/// A [`Mutex`] with a lock rank. The API mirrors `std::sync::Mutex`.
pub struct RankedMutex<R: Rank, T> {
    inner: Mutex<T>,
    #[cfg(debug_assertions)]
    rank: R,
    #[cfg(debug_assertions)]
    name: &'static str,
    #[cfg(debug_assertions)]
    probe: ContentionProbe,
    _family: PhantomData<fn() -> R>,
}

/// What debug builds measure of a [`RankedMutex`]'s contended waits.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contention {
    /// Threads past a failed try, waiting for the lock now.
    pub waiting: u32,
    /// The last contended wait, zero before the first one.
    pub last_wait: Duration,
}

#[cfg(debug_assertions)]
#[derive(Default)]
struct ContentionProbe {
    waiting: std::sync::atomic::AtomicU32,
    last_wait_nanos: std::sync::atomic::AtomicU64,
}

impl<R: Rank, T> RankedMutex<R, T> {
    pub fn new(rank: R, name: &'static str, value: T) -> Self {
        #[cfg(not(debug_assertions))]
        let _ = (rank, name);
        Self {
            inner: Mutex::new(value),
            #[cfg(debug_assertions)]
            rank,
            #[cfg(debug_assertions)]
            name,
            #[cfg(debug_assertions)]
            probe: ContentionProbe::default(),
            _family: PhantomData,
        }
    }

    #[cfg(debug_assertions)]
    fn rank(&self) -> (R, &'static str) {
        (self.rank, self.name)
    }

    #[cfg(not(debug_assertions))]
    fn rank(&self) -> (R, &'static str) {
        (R::UNRANKED, "")
    }

    /// The contended waits measured so far, so a test starts a hold only
    /// once its waiter is contended.
    #[cfg(debug_assertions)]
    pub fn contention(&self) -> Contention {
        use std::sync::atomic::Ordering;
        Contention {
            waiting: self.probe.waiting.load(Ordering::Acquire),
            last_wait: Duration::from_nanos(self.probe.last_wait_nanos.load(Ordering::Acquire)),
        }
    }

    #[track_caller]
    #[inline]
    pub fn lock(&self) -> LockResult<RankedGuard<'_, R, T>> {
        let (rank, name) = self.rank();
        let wait = HeldRank::acquire(rank, name);
        match self.lock_timed() {
            Ok(guard) => Ok(RankedGuard { guard, _held: wait.held() }),
            Err(poison) => Err(PoisonError::new(RankedGuard {
                guard: poison.into_inner(),
                _held: wait.held(),
            })),
        }
    }

    /// The inner lock, tried first; a contended wait is timed
    /// ([`Self::wait_timed`]).
    #[cfg(debug_assertions)]
    #[track_caller]
    #[inline]
    fn lock_timed(&self) -> LockResult<MutexGuard<'_, T>> {
        match self.inner.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(poison)) => Err(poison),
            Err(TryLockError::WouldBlock) => self.wait_timed(Location::caller()),
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    fn lock_timed(&self) -> LockResult<MutexGuard<'_, T>> {
        self.inner.lock()
    }

    /// Waits for the inner lock after a failed try and reports the wait for
    /// `site`.
    #[cfg(debug_assertions)]
    #[cold]
    #[inline(never)]
    fn wait_timed(&self, site: &'static Location<'static>) -> LockResult<MutexGuard<'_, T>> {
        use std::sync::atomic::Ordering;
        let since = Instant::now();
        self.probe.waiting.fetch_add(1, Ordering::AcqRel);
        let locked = self.inner.lock();
        let waited = since.elapsed();
        let nanos = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        self.probe.last_wait_nanos.store(nanos, Ordering::Release);
        self.probe.waiting.fetch_sub(1, Ordering::AcqRel);
        R::note_contended_wait(waited, site);
        locked
    }

    #[inline]
    pub fn try_lock(&self) -> TryLockResult<RankedGuard<'_, R, T>> {
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

/// A held [`RankedMutex`]. The lock is released before its rank.
pub struct RankedGuard<'a, R: Rank, T> {
    guard: MutexGuard<'a, T>,
    _held: HeldRank<R>,
}

impl<R: Rank, T> Deref for RankedGuard<'_, R, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<R: Rank, T> DerefMut for RankedGuard<'_, R, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}
