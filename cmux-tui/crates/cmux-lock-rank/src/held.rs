//! The ranks this thread holds, debug and test builds only. One stack holds
//! every family; a check compares only entries of its own family.

use super::Rank;
use std::any::TypeId;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

struct Held {
    family: TypeId,
    ordinal: usize,
    name: &'static str,
    id: u64,
}

thread_local! {
    static HELD: RefCell<Vec<Held>> = const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The innermost rank of family `R` this thread holds, with its lock name.
fn innermost<R: Rank>() -> Option<(R, &'static str)> {
    let family = TypeId::of::<R>();
    HELD.with(|held| {
        held.borrow()
            .iter()
            .filter(|entry| entry.family == family)
            .max_by_key(|entry| entry.ordinal)
            .and_then(|entry| R::ALL.get(entry.ordinal).map(|rank| (*rank, entry.name)))
    })
}

#[track_caller]
pub(super) fn check<R: Rank>(rank: R, name: &'static str) {
    // Asserted outside the thread-local closure so the location is the acquisition site.
    if let Some((held_rank, held_name)) = innermost::<R>() {
        let order = R::ORDER;
        debug_assert!(
            held_rank < rank,
            "lock order violation: acquiring {name} ({rank:?}) while this thread holds \
             {held_name} ({held_rank:?}); the order is {order}"
        );
    }
}

pub(super) fn push<R: Rank>(rank: R, name: &'static str) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let entry = Held { family: TypeId::of::<R>(), ordinal: rank.ordinal(), name, id };
    HELD.with(|held| held.borrow_mut().push(entry));
    id
}

/// The family said its thread may yield (`free`): then it may hold no rank of
/// the family at all, or the thread would yield while holding a lock.
#[track_caller]
pub(super) fn assert_none_held_if<R: Rank>(free: bool) {
    if !free {
        return;
    }
    let family = TypeId::of::<R>();
    let held_name = HELD.with(|held| {
        held.borrow().iter().find(|entry| entry.family == family).map(|entry| entry.name)
    });
    debug_assert!(
        held_name.is_none(),
        "this thread would yield while it holds {held_name:?}: nothing waits for other \
         work while it holds a ranked lock"
    );
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
