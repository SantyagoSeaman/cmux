//! Debug-build lock ranks (lockdep style) for the mux and surface locks: the
//! daemon's rank family for the shared `cmux_lock_rank` crate, which holds
//! [`RankedMutex`], the per-thread rank stack and the order check
//! (Chromium's base/task/common/checked_lock.h).
//!
//! Order, outer first: workspace registry -> journal writer -> registry
//! connection -> `Mux::state` -> Kitty limits request -> PTY geometry ->
//! terminal -> PTY runtime -> Kitty limits -> attach taps -> leaf. A thread may acquire a ranked lock only when its rank is
//! after every rank the thread already holds. A blocking acquisition at a rank
//! equal to or before a held rank panics in debug and test builds and names
//! both locks, so every test, Testbox gate and debug dogfood build checks the
//! order on every path it runs. `try_lock` cannot deadlock: it is recorded but
//! not checked. In release builds the rank, the name and the thread-local
//! stack are compiled out; [`RankedMutex`] is then a plain `Mutex` ranked
//! `Leaf`. Holds of the ranks up to `Mux::state` are counted in every build:
//! lock admission never makes a thread wait while it holds one
//! ([`HeldRank::may_yield`]).
//!
//! Debug and test builds also time a contended [`RankedMutex::lock`]: the
//! lock is tried first and only a failed try reads the clock, and a wait of
//! `cmux_threads::LOCK_WAIT_BUDGET` (2 ms) or more on a thread that
//! disallows blocking records a `lock-wait` site at the caller
//! (`cmux_threads::lock_wait::note_timed_lock_wait`), read with
//! `cmux_threads::blocking::snapshot`. Chromium times only the contended
//! path the same way: base/synchronization/lock_impl.h (`LockImpl::Lock`
//! tries first) and base/synchronization/lock_metrics_recorder.h.

use std::cell::Cell;
use std::panic::Location;
use std::time::Duration;

/// One rank this thread holds; dropping it releases the rank.
pub(crate) type HeldRank = cmux_lock_rank::HeldRank<LockRank>;
/// A checked rank this thread is about to block on: the boost is taken
/// before the wait, so the lock is never handed to a lowered waiter.
pub(crate) type RankWait = cmux_lock_rank::RankWait<LockRank>;
/// A `Mutex` with a lock rank. The API mirrors `std::sync::Mutex`.
pub(crate) type RankedMutex<T> = cmux_lock_rank::RankedMutex<LockRank, T>;
/// A held [`RankedMutex`]. The lock is released before its rank.
pub(crate) type RankedGuard<'a, T> = cmux_lock_rank::RankedGuard<'a, LockRank, T>;

/// Rank of a ranked lock. A thread acquires ranks in ascending order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LockRank {
    /// `Mux::workspace_registry` (the registry `SignaledMutex`).
    WorkspaceRegistry,
    /// Not a lock: the journal writer thread holds this rank for each loop
    /// iteration (`JournalWriterCommitScope`). It ranks after the registry,
    /// so the writer can never take the registry lock: a request thread
    /// holds the registry while it waits for a writer receipt.
    JournalWriter,
    /// `RegistryConnection`, the SQLite connection lock. It is reentrant for
    /// its holder; only the first acquisition on a thread is ranked.
    RegistryConnection,
    /// `Mux::state`.
    MuxState,
    /// `PtyTerminalRuntime::kitty_limits_request`, held for a whole Kitty
    /// limits request, including the runtime, terminal and taps it takes.
    KittyLimitsRequest,
    /// `PtyTerminalRuntime::geometry`.
    Geometry,
    /// `PtyTerminalRuntime::term`.
    Terminal,
    /// `PtyTerminalRuntime::runtime`.
    Runtime,
    /// `PtyTerminalRuntime::kitty_graphics_limits`; a commit resynchronizes
    /// the attach taps while it holds it.
    KittyLimits,
    /// `PtyTerminalRuntime::taps` (byte and snapshot attach taps); a tap
    /// update reads the last attach colors.
    AttachTaps,
    /// Locks that never take another ranked lock while held (render hub,
    /// title, mouse encoders, attach colors, `Mux::default_colors`).
    Leaf,
}

/// The last rank whose holds are counted in every build: lock admission
/// (`server::priority_gate`) gates the registry, connection and state locks,
/// and the lock order allows a thread to hold only these ranks while it
/// acquires one of them.
const LAST_ADMISSION_RANK: LockRank = LockRank::MuxState;

thread_local! {
    /// Holds of ranks up to [`LAST_ADMISSION_RANK`] on this thread, counted
    /// in release builds too.
    static ADMISSION_RANK_HOLDS: Cell<u32> = const { Cell::new(0) };
}

impl LockRank {
    /// Every lock: a thread lowered to UTILITY for an automation request is
    /// raised before it waits for one and while it holds it
    /// (p06-scheduler-035, `cmux_tui_platform::qos::LockBoost`), so a
    /// frontend request or the PTY echo path never waits behind a UTILITY
    /// thread. Release builds rank every `RankedMutex` as `Leaf`, so the rule
    /// cannot depend on the rank. The journal writer rank is not a lock.
    const fn raises_holder(self) -> bool {
        !matches!(self, Self::JournalWriter)
    }
}

impl cmux_lock_rank::Rank for LockRank {
    const ALL: &'static [Self] = &[
        Self::WorkspaceRegistry,
        Self::JournalWriter,
        Self::RegistryConnection,
        Self::MuxState,
        Self::KittyLimitsRequest,
        Self::Geometry,
        Self::Terminal,
        Self::Runtime,
        Self::KittyLimits,
        Self::AttachTaps,
        Self::Leaf,
    ];
    const ORDER: &'static str = "workspace.registry > journal.writer > registry.connection > \
         mux.state > pty.kitty_limits_request > pty.geometry > pty.term > pty.runtime > \
         pty.kitty_graphics_limits > pty.taps > leaf (see PtyTerminalRuntime)";
    const UNRANKED: Self = Self::Leaf;
    type Wait = cmux_tui_platform::qos::LockBoost;
    type Hold = AdmissionHold;

    #[inline]
    fn ordinal(self) -> usize {
        self as usize
    }

    /// Raise a lowered thread before it blocks on the lock.
    #[inline]
    fn before_wait(self) -> cmux_tui_platform::qos::LockBoost {
        if self.raises_holder() {
            cmux_tui_platform::qos::LockBoost::hold()
        } else {
            cmux_tui_platform::qos::LockBoost::none()
        }
    }

    #[inline]
    fn on_held(self, boost: cmux_tui_platform::qos::LockBoost) -> AdmissionHold {
        let counted = self <= LAST_ADMISSION_RANK;
        if counted {
            ADMISSION_RANK_HOLDS.with(|holds| holds.set(holds.get() + 1));
        }
        AdmissionHold { counted, _boost: boost }
    }

    /// True when this thread holds no ranked lock, so a lock admission may
    /// make it wait (`server::priority_gate`): a thread that yields while it
    /// holds a lock could hold up the very request it yields to.
    #[inline]
    fn may_yield() -> bool {
        ADMISSION_RANK_HOLDS.with(Cell::get) == 0
    }

    fn note_contended_wait(waited: Duration, site: &'static Location<'static>) {
        cmux_threads::lock_wait::note_timed_lock_wait(waited, site);
    }
}

/// What a held rank keeps in every build: the holder's QoS boost and the
/// count of the ranks lock admission reads.
pub(crate) struct AdmissionHold {
    counted: bool,
    /// Dropped after the count, as the rank's last part.
    _boost: cmux_tui_platform::qos::LockBoost,
}

impl Drop for AdmissionHold {
    #[inline]
    fn drop(&mut self) {
        if self.counted {
            // Guards may drop during thread teardown.
            let _ = ADMISSION_RANK_HOLDS.try_with(|holds| holds.set(holds.get().saturating_sub(1)));
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::{LockRank, RankedMutex};
    use cmux_threads::{BlockingKind, BlockingMode, LOCK_WAIT_BUDGET, ThreadQos};
    use std::panic::Location;
    use std::sync::Arc;
    use std::time::Duration;

    /// The `file:line` of a lock call; `lock` returns it from the line it
    /// locks on.
    type Site = (&'static str, u32);

    /// A contended lock: the `lock-wait` records at its site and the wait
    /// the lock measured.
    struct Contended {
        count: u64,
        waited: Duration,
    }

    fn lock_wait_count((file, line): Site) -> u64 {
        cmux_threads::blocking::snapshot()
            .site(file, line)
            .filter(|site| site.kinds.contains(BlockingKind::LockWait))
            .map_or(0, |site| site.count)
    }

    /// Starts `body` on a thread that disallows blocking, in panic mode.
    fn spawn_disallowed<R: Send + 'static>(
        body: impl FnOnce() -> R + Send + 'static,
    ) -> std::thread::JoinHandle<R> {
        cmux_threads::spawn("lock-rank-waiter", ThreadQos::UserInitiated, move || {
            cmux_threads::with_blocking_disallowed(|| {
                cmux_threads::blocking::with_mode(BlockingMode::Panic, body)
            })
        })
        .expect("spawn waiter")
    }

    fn on_disallowed_thread<R: Send + 'static>(body: impl FnOnce() -> R + Send + 'static) -> R {
        cmux_threads::checked::sync::join(spawn_disallowed(body)).expect("waiter")
    }

    /// Holds a ranked mutex while a thread that disallows blocking, in panic
    /// mode, locks it with `lock`. The hold starts only once the waiter is
    /// past its failed try (as `contend` in cmux-threads' lock_wait tests),
    /// so the wait the lock measures lasts at least `hold` under any load.
    fn contend(hold: Duration, lock: fn(&RankedMutex<u32>) -> Site) -> Contended {
        let mutex = Arc::new(RankedMutex::new(LockRank::Leaf, "test.leaf", 0_u32));
        let guard = mutex.lock().expect("holder lock");
        let waiter = spawn_disallowed({
            let mutex = Arc::clone(&mutex);
            move || lock(&mutex)
        });
        while mutex.contention().waiting == 0 {
            std::thread::yield_now();
        }
        cmux_threads::checked::sync::sleep(hold);
        drop(guard);
        let site = cmux_threads::checked::sync::join(waiter).expect("waiter");
        let waited = mutex.contention().last_wait;
        Contended { count: lock_wait_count(site), waited }
    }

    #[test]
    fn five_ms_holder_records_one_lock_wait_site_without_panic() {
        let contended = contend(Duration::from_millis(5), |mutex| {
            let (site, guard) = ((file!(), line!()), mutex.lock().expect("waiter lock"));
            drop(guard);
            site
        });
        assert!(contended.waited >= Duration::from_millis(5), "waited {:?}", contended.waited);
        assert_eq!(contended.count, 1);
    }

    /// A 0.1 ms hold bounds the measured wait from below only, so the
    /// record follows the wait the lock measured; a 0.1 ms wait given
    /// directly records nothing.
    #[test]
    fn tenth_ms_holder_records_no_lock_wait_site() {
        let contended = contend(Duration::from_micros(100), |mutex| {
            let (site, guard) = ((file!(), line!()), mutex.lock().expect("waiter lock"));
            drop(guard);
            site
        });
        assert!(contended.waited >= Duration::from_micros(100), "waited {:?}", contended.waited);
        assert_eq!(contended.count, u64::from(contended.waited >= LOCK_WAIT_BUDGET));

        let site = on_disallowed_thread(|| {
            let site = Location::caller();
            cmux_threads::lock_wait::note_timed_lock_wait(Duration::from_micros(100), site);
            (site.file(), site.line())
        });
        assert_eq!(lock_wait_count(site), 0);
    }

    /// The shared crate maps a held rank back through `ALL` by ordinal.
    #[test]
    fn every_rank_is_listed_in_order() {
        use cmux_lock_rank::Rank;
        for (index, rank) in LockRank::ALL.iter().enumerate() {
            assert_eq!(rank.ordinal(), index, "{rank:?}");
        }
        assert!(LockRank::ALL.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(LockRank::ALL.last(), Some(&LockRank::UNRANKED));
    }

    #[test]
    #[should_panic(
        expected = "lock order violation: acquiring test.state (MuxState) while this thread holds \
                    test.term (Terminal); the order is workspace.registry > journal.writer > \
                    registry.connection > mux.state > pty.kitty_limits_request > pty.geometry > \
                    pty.term > pty.runtime > pty.kitty_graphics_limits > pty.taps > leaf (see \
                    PtyTerminalRuntime)"
    )]
    fn an_inversion_names_both_locks_and_the_order() {
        let term = RankedMutex::new(LockRank::Terminal, "test.term", ());
        let state = RankedMutex::new(LockRank::MuxState, "test.state", ());
        let _term = term.lock().expect("term");
        let _state = state.lock();
    }

    /// Lock admission may yield only once the counted ranks are released:
    /// a state hold counts from when it is held, not while it waits.
    #[test]
    fn admission_ranks_count_from_held_to_drop() {
        use super::HeldRank;
        assert!(HeldRank::may_yield());
        let wait = HeldRank::acquire(LockRank::MuxState, "test.state");
        assert!(HeldRank::may_yield(), "a waiting thread holds nothing yet");
        let held = wait.held();
        assert!(!HeldRank::may_yield());
        let leaf = HeldRank::record(LockRank::Leaf, "test.leaf");
        drop(held);
        drop(leaf);
        assert!(HeldRank::may_yield());
    }

    #[test]
    fn an_uncontended_lock_records_no_lock_wait_site() {
        let mutex = Arc::new(RankedMutex::new(LockRank::Leaf, "test.leaf", 0_u32));
        let site = on_disallowed_thread({
            let mutex = Arc::clone(&mutex);
            move || {
                let (site, guard) = ((file!(), line!()), mutex.lock().expect("lock"));
                drop(guard);
                site
            }
        });
        assert_eq!(mutex.contention().last_wait, Duration::ZERO);
        assert_eq!(lock_wait_count(site), 0);
    }
}
