//! The rank check on plain ranked mutexes (`hub/lock_order_tests.rs` runs it
//! on the hub's and a session's own locks).

use super::{LockRank, RankedMutex};

#[test]
fn locks_taken_in_rank_order_pass() {
    let sessions = RankedMutex::new(LockRank::Sessions, "test.sessions", ());
    let meta = RankedMutex::new(LockRank::SessionMeta, "test.meta", ());
    let clock = RankedMutex::new(LockRank::Clock, "test.clock", ());
    let _sessions = sessions.lock().unwrap();
    let _meta = meta.lock().unwrap();
    let _clock = clock.lock().unwrap();
}

#[test]
#[should_panic(
    expected = "acquiring test.sessions (Sessions) while this thread holds test.meta (SessionMeta)"
)]
fn an_outer_lock_taken_under_an_inner_one_panics_naming_both() {
    let sessions = RankedMutex::new(LockRank::Sessions, "test.sessions", ());
    let meta = RankedMutex::new(LockRank::SessionMeta, "test.meta", ());
    let _meta = meta.lock().unwrap();
    let _sessions = sessions.lock();
}

/// Two locks of one rank have no order between them: two sessions' `meta`,
/// taken by two threads in opposite orders, deadlock.
#[test]
#[should_panic(expected = "acquiring test.b (SessionMeta) while this thread holds test.a")]
fn two_locks_of_one_rank_panic() {
    let a = RankedMutex::new(LockRank::SessionMeta, "test.a", ());
    let b = RankedMutex::new(LockRank::SessionMeta, "test.b", ());
    let _a = a.lock().unwrap();
    let _b = b.lock();
}

#[test]
fn a_released_rank_no_longer_counts() {
    let sessions = RankedMutex::new(LockRank::Sessions, "test.sessions", ());
    let meta = RankedMutex::new(LockRank::SessionMeta, "test.meta", ());
    drop(meta.lock().unwrap());
    let _sessions = sessions.lock().unwrap();
    let _meta = meta.lock().unwrap();
}

#[test]
fn guards_released_out_of_order_keep_the_rest_held() {
    let sessions = RankedMutex::new(LockRank::Sessions, "test.sessions", ());
    let queue = RankedMutex::new(LockRank::SessionQueue, "test.queue", ());
    let meta = RankedMutex::new(LockRank::SessionMeta, "test.meta", ());
    let held_sessions = sessions.lock().unwrap();
    let held_queue = queue.lock().unwrap();
    drop(held_sessions);
    // The queue is still held: meta, after it, is in order.
    let _meta = meta.lock().unwrap();
    drop(held_queue);
}

/// `try_lock` never waits, so it cannot close a cycle: it is recorded, not checked.
#[test]
fn try_lock_out_of_order_is_recorded_not_checked() {
    let sessions = RankedMutex::new(LockRank::Sessions, "test.sessions", ());
    let meta = RankedMutex::new(LockRank::SessionMeta, "test.meta", ());
    let held_meta = meta.lock().unwrap();
    let held_sessions = sessions.try_lock().expect("uncontended");
    drop(held_meta);
    // The try-locked registry counts as held: a second registry may not be waited for.
    let other = RankedMutex::new(LockRank::Sessions, "test.other", ());
    let inverted = std::panic::catch_unwind(|| drop(other.lock()));
    assert!(inverted.is_err(), "the try-locked rank is held");
    drop(held_sessions);
}

#[test]
fn a_poisoned_lock_still_records_its_rank() {
    let meta = std::sync::Arc::new(RankedMutex::new(LockRank::SessionMeta, "test.meta", 0_u32));
    let poisoner = std::sync::Arc::clone(&meta);
    let _ = std::panic::catch_unwind(move || {
        let _guard = poisoner.lock().unwrap();
        // crash-allow: poisons the lock for the test.
        panic!("poison");
    });
    let sessions = RankedMutex::new(LockRank::Sessions, "test.sessions", ());
    let Err(poisoned) = meta.lock() else { unreachable!("the lock is poisoned") };
    let held = poisoned.into_inner();
    let inverted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(sessions.lock())));
    assert!(inverted.is_err(), "the poisoned guard holds its rank");
    drop(held);
}
