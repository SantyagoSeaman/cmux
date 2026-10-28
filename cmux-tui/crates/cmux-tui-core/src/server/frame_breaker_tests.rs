//! The crash-loop breaker's count (p05-ipc-191): three closes within 60 s
//! trip it once; closes spread wider do not.

use std::time::{Duration, Instant};

use super::{BreakerVerdict, FrameBreaker, TRIP_WINDOW};

#[test]
fn bad_message_breaker_trips_on_the_third_close_within_the_window() {
    let start = Instant::now();
    let breaker = FrameBreaker::new(start);
    assert_eq!(breaker.record(start), BreakerVerdict::Counted);
    assert_eq!(breaker.record(start + Duration::from_secs(30)), BreakerVerdict::Counted);
    assert!(!breaker.is_tripped());
    assert_eq!(breaker.record(start + TRIP_WINDOW), BreakerVerdict::Tripped);
    assert!(breaker.is_tripped());
    assert_eq!(breaker.record(start + TRIP_WINDOW * 3), BreakerVerdict::AlreadyOff);
}

#[test]
fn bad_message_breaker_ignores_closes_spread_wider_than_the_window() {
    let start = Instant::now();
    let breaker = FrameBreaker::new(start);
    let step = TRIP_WINDOW / 2 + Duration::from_millis(1);
    for close in 0..10 {
        assert_eq!(breaker.record(start + step * close), BreakerVerdict::Counted, "close {close}");
    }
    assert!(!breaker.is_tripped());
}

#[test]
fn bad_message_breaker_counts_the_last_three_closes() {
    let start = Instant::now();
    let breaker = FrameBreaker::new(start);
    assert_eq!(breaker.record(start), BreakerVerdict::Counted);
    assert_eq!(breaker.record(start + Duration::from_secs(50)), BreakerVerdict::Counted);
    // 70 s after the first: the first falls out, two remain.
    assert_eq!(breaker.record(start + Duration::from_secs(70)), BreakerVerdict::Counted);
    assert_eq!(breaker.record(start + Duration::from_secs(80)), BreakerVerdict::Tripped);
}
