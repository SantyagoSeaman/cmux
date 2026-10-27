//! Rank order, hooks and contention timing, with two test families.

use super::{HeldRank, Rank, RankWait, RankedMutex};
use std::cell::Cell;
use std::panic::Location;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Outer {
    Registry,
    State,
    Leaf,
}

thread_local! {
    /// Holds of `Outer::Registry` on this thread (the counted rank).
    static COUNTED: Cell<u32> = const { Cell::new(0) };
    /// `before_wait` calls minus `Wait` drops on this thread.
    static WAITING: Cell<u32> = const { Cell::new(0) };
    /// When set, `may_yield` lies, to exercise the debug check.
    static LIE: Cell<bool> = const { Cell::new(false) };
}

/// Contended waits reported, with their site line.
static WAITS: Mutex<Vec<(Duration, u32)>> = Mutex::new(Vec::new());

struct TestWait;

impl Drop for TestWait {
    fn drop(&mut self) {
        WAITING.with(|waiting| waiting.set(waiting.get() - 1));
    }
}

struct TestHold {
    counted: bool,
    _wait: TestWait,
}

impl Drop for TestHold {
    fn drop(&mut self) {
        if self.counted {
            COUNTED.with(|counted| counted.set(counted.get() - 1));
        }
    }
}

impl Rank for Outer {
    const ALL: &'static [Self] = &[Self::Registry, Self::State, Self::Leaf];
    const ORDER: &'static str = "registry > state > leaf";
    const UNRANKED: Self = Self::Leaf;
    type Wait = TestWait;
    type Hold = TestHold;

    fn ordinal(self) -> usize {
        self as usize
    }

    fn before_wait(self) -> TestWait {
        WAITING.with(|waiting| waiting.set(waiting.get() + 1));
        TestWait
    }

    fn on_held(self, wait: TestWait) -> TestHold {
        let counted = self == Self::Registry;
        if counted {
            COUNTED.with(|counted| counted.set(counted.get() + 1));
        }
        TestHold { counted, _wait: wait }
    }

    fn may_yield() -> bool {
        LIE.with(Cell::get) || COUNTED.with(Cell::get) == 0
    }

    fn note_contended_wait(waited: Duration, site: &'static Location<'static>) {
        WAITS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((waited, site.line()));
    }
}

/// A second family with no hooks, as a crate that only checks order declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Other {
    Hub,
    Session,
}

impl Rank for Other {
    const ALL: &'static [Self] = &[Self::Hub, Self::Session];
    const ORDER: &'static str = "hub > session";
    const UNRANKED: Self = Self::Session;
    type Wait = ();
    type Hold = ();

    fn ordinal(self) -> usize {
        self as usize
    }

    fn before_wait(self) {}

    fn on_held(self, (): ()) {}
}

fn families_are_consistent<R: Rank>() {
    for (index, rank) in R::ALL.iter().enumerate() {
        assert_eq!(rank.ordinal(), index, "{rank:?}");
    }
    assert!(R::ALL.windows(2).all(|pair| pair[0] < pair[1]), "ALL is in order");
}

#[test]
fn test_families_list_every_rank_in_order() {
    families_are_consistent::<Outer>();
    families_are_consistent::<Other>();
}

#[test]
fn ascending_acquisitions_pass() {
    let registry = RankedMutex::new(Outer::Registry, "test.registry", ());
    let state = RankedMutex::new(Outer::State, "test.state", ());
    let leaf = RankedMutex::new(Outer::Leaf, "test.leaf", ());
    let _registry = registry.lock().expect("registry");
    let _state = state.lock().expect("state");
    let _leaf = leaf.lock().expect("leaf");
}

#[test]
#[should_panic(
    expected = "lock order violation: acquiring test.registry (Registry) while this thread holds \
                test.state (State); the order is registry > state > leaf"
)]
fn an_inversion_panics_and_names_both_locks() {
    let registry = RankedMutex::new(Outer::Registry, "test.registry", ());
    let state = RankedMutex::new(Outer::State, "test.state", ());
    let _state = state.lock().expect("state");
    let _registry = registry.lock();
}

#[test]
#[should_panic(
    expected = "acquiring test.second (State) while this thread holds test.first (State)"
)]
fn a_second_lock_of_the_same_rank_panics() {
    let first = RankedMutex::new(Outer::State, "test.first", ());
    let second = RankedMutex::new(Outer::State, "test.second", ());
    let _first = first.lock().expect("first");
    let _second = second.lock();
}

#[test]
fn try_lock_out_of_order_is_recorded_but_not_checked() {
    let registry = RankedMutex::new(Outer::Registry, "test.registry", ());
    let state = RankedMutex::new(Outer::State, "test.state", ());
    let _state = state.lock().expect("state");
    let registry_guard = registry.try_lock().expect("try registry");
    assert!(!HeldRank::<Outer>::may_yield());
    drop(registry_guard);
}

#[test]
fn a_rank_dropped_out_of_order_is_released() {
    let registry = RankedMutex::new(Outer::Registry, "test.registry", ());
    let state = RankedMutex::new(Outer::State, "test.state", ());
    let registry_guard = registry.lock().expect("registry");
    let state_guard = state.lock().expect("state");
    drop(registry_guard);
    drop(state_guard);
    // Nothing is held: the outermost rank may be taken again.
    let _registry = registry.lock().expect("registry again");
}

#[test]
fn families_are_not_compared() {
    let state = RankedMutex::new(Outer::Leaf, "test.leaf", ());
    let hub = RankedMutex::new(Other::Hub, "test.hub", ());
    let _leaf = state.lock().expect("leaf");
    let _hub = hub.lock().expect("hub after another family's leaf");
}

#[test]
fn hooks_run_before_the_wait_and_while_the_lock_is_held() {
    let wait = HeldRank::acquire(Outer::Registry, "test.registry");
    assert_eq!(WAITING.with(Cell::get), 1, "before_wait runs at acquire");
    assert!(HeldRank::<Outer>::may_yield(), "the rank counts only once it is held");
    let held = wait.held();
    assert!(!HeldRank::<Outer>::may_yield());
    drop(held);
    assert!(HeldRank::<Outer>::may_yield());
    assert_eq!(WAITING.with(Cell::get), 0, "the wait hook drops with the rank");

    let reentry = RankWait::reentry(Outer::Registry, "test.registry").held();
    let record = HeldRank::record(Outer::Registry, "test.registry");
    assert_eq!(COUNTED.with(Cell::get), 2);
    drop((reentry, record));
    assert_eq!(COUNTED.with(Cell::get), 0);
}

#[test]
#[should_panic(expected = "would yield while it holds Some(\"test.state\")")]
fn a_family_that_may_yield_while_it_holds_a_rank_is_caught() {
    let state = RankedMutex::new(Outer::State, "test.state", ());
    let _state = state.lock().expect("state");
    LIE.with(|lie| lie.set(true));
    let _ = HeldRank::<Outer>::may_yield();
}

#[test]
fn a_poisoned_lock_still_records_its_rank() {
    let registry = Arc::new(RankedMutex::new(Outer::Registry, "test.registry", ()));
    let poisoner = Arc::clone(&registry);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock().expect("lock");
        std::panic::resume_unwind(Box::new("poison the lock"));
    })
    .join();
    let locked = registry.lock();
    assert!(locked.is_err(), "poisoned");
    assert!(!HeldRank::<Outer>::may_yield());
    drop(locked);
    let tried = registry.try_lock();
    assert!(matches!(tried, Err(std::sync::TryLockError::Poisoned(_))));
    assert!(!HeldRank::<Outer>::may_yield());
    drop(tried);
    assert!(HeldRank::<Outer>::may_yield());
}

#[test]
fn a_contended_lock_reports_its_wait_at_the_caller() {
    let mutex = Arc::new(RankedMutex::new(Outer::Leaf, "test.contended", 0_u32));
    let guard = mutex.lock().expect("holder");
    let waiter = std::thread::spawn({
        let mutex = Arc::clone(&mutex);
        move || {
            let (line, guard) = (line!(), mutex.lock().expect("waiter"));
            drop(guard);
            line
        }
    });
    while mutex.contention().waiting == 0 {
        std::thread::yield_now();
    }
    let hold = Duration::from_millis(5);
    std::thread::sleep(hold);
    drop(guard);
    let line = waiter.join().expect("waiter");
    let contention = mutex.contention();
    assert_eq!(contention.waiting, 0);
    assert!(contention.last_wait >= hold, "waited {:?}", contention.last_wait);
    let waits = WAITS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        waits.iter().any(|(waited, at)| *at == line && *waited == contention.last_wait),
        "{waits:?}"
    );
}

#[test]
fn an_uncontended_lock_reports_nothing() {
    let mutex = RankedMutex::new(Outer::Leaf, "test.uncontended", 0_u32);
    let (line, guard) = (line!(), mutex.lock().expect("lock"));
    drop(guard);
    assert_eq!(mutex.contention(), super::Contention::default());
    let waits = WAITS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(waits.iter().all(|(_, at)| *at != line), "{waits:?}");
}
