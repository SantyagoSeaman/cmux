//! The rank check on plain ranked mutexes (`session/remote/tests/lock_order.rs`
//! runs it on a remote session's and a surface's own locks).

use super::{LockRank, RankedMutex};

#[test]
fn locks_taken_in_rank_order_pass() {
    let surfaces = RankedMutex::new(LockRank::SessionSurfaces, ());
    let term = RankedMutex::new(LockRank::SurfaceTerm, ());
    let leaf = RankedMutex::new(LockRank::Leaf, ());
    let _surfaces = surfaces.lock().unwrap();
    let _term = term.lock().unwrap();
    let _leaf = leaf.lock().unwrap();
}

#[test]
#[should_panic(expected = "acquiring SessionSurfaces while this thread holds SurfaceTerm")]
fn an_outer_lock_taken_under_an_inner_one_panics_naming_both() {
    let surfaces = RankedMutex::new(LockRank::SessionSurfaces, ());
    let term = RankedMutex::new(LockRank::SurfaceTerm, ());
    let _term = term.lock().unwrap();
    let _surfaces = surfaces.lock();
}

/// Two locks of one rank have no order between them: two surfaces' `term`,
/// taken by two threads in opposite orders, deadlock.
#[test]
#[should_panic(expected = "acquiring SurfaceTerm while this thread holds SurfaceTerm")]
fn two_locks_of_one_rank_panic() {
    let a = RankedMutex::new(LockRank::SurfaceTerm, ());
    let b = RankedMutex::new(LockRank::SurfaceTerm, ());
    let _a = a.lock().unwrap();
    let _b = b.lock();
}

#[test]
fn a_released_rank_no_longer_counts() {
    let surfaces = RankedMutex::new(LockRank::SessionSurfaces, ());
    let term = RankedMutex::new(LockRank::SurfaceTerm, ());
    drop(term.lock().unwrap());
    let _surfaces = surfaces.lock().unwrap();
    let _term = term.lock().unwrap();
}

#[test]
fn guards_released_out_of_order_keep_the_rest_held() {
    let surfaces = RankedMutex::new(LockRank::SessionSurfaces, ());
    let tree = RankedMutex::new(LockRank::SessionTree, ());
    let term = RankedMutex::new(LockRank::SurfaceTerm, ());
    let held_surfaces = surfaces.lock().unwrap();
    let held_tree = tree.lock().unwrap();
    drop(held_surfaces);
    // The tree is still held: the terminal, after it, is in order.
    let _term = term.lock().unwrap();
    drop(held_tree);
}

/// `try_lock` never waits, so it cannot close a cycle: it is recorded, not checked.
#[test]
fn try_lock_out_of_order_is_recorded_not_checked() {
    let surfaces = RankedMutex::new(LockRank::SessionSurfaces, ());
    let term = RankedMutex::new(LockRank::SurfaceTerm, ());
    let held_term = term.lock().unwrap();
    let held_surfaces = surfaces.try_lock().expect("uncontended");
    drop(held_term);
    // The try-locked map counts as held: a second map may not be waited for.
    let other = RankedMutex::new(LockRank::SessionSurfaces, ());
    let inverted = std::panic::catch_unwind(|| drop(other.lock()));
    assert!(inverted.is_err(), "the try-locked rank is held");
    drop(held_surfaces);
}

#[test]
fn a_poisoned_lock_still_records_its_rank() {
    let term = std::sync::Arc::new(RankedMutex::new(LockRank::SurfaceTerm, 0_u32));
    let poisoner = std::sync::Arc::clone(&term);
    let _ = std::panic::catch_unwind(move || {
        let _guard = poisoner.lock().unwrap();
        // crash-allow: poisons the lock for the test.
        panic!("poison");
    });
    let surfaces = RankedMutex::new(LockRank::SessionSurfaces, ());
    let Err(poisoned) = term.lock() else { unreachable!("the lock is poisoned") };
    let held = poisoned.into_inner();
    let inverted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(surfaces.lock())));
    assert!(inverted.is_err(), "the poisoned guard holds its rank");
    drop(held);
}
