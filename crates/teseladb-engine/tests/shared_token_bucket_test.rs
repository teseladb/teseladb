//! Tests of `SharedTokenBucket` on a manual clock: tokens replenish at the configured rate,
//! with and without a cap on the tokens taken but not yet released.

#![cfg(target_os = "linux")]

use std::time::Duration;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::manual_clock::ManualClock;
use teseladb::engine::util::shared_token_bucket::internal::{
    Ratio1, RoversCapped, RoversUncapped, SharedTokenBucket, TokenBucketClock,
};
use teseladb::engine_test_case;

/// The manual clock's current time, as the token bucket reads it.
fn now() -> i64 {
    <ManualClock as TokenBucketClock>::now()
}

engine_test_case!(test_basic_non_capped_loop, {
    let tb = SharedTokenBucket::<Ratio1, RoversUncapped, ManualClock>::new(1, 1, 0, false);

    // Grab one token and make sure it's only available in 1s
    let mut th = tb.grab(1);
    assert!(tb.deficiency(th) > 0);
    ManualClock::advance(Duration::from_secs(1));
    tb.replenish(now());
    assert!(tb.deficiency(th) == 0);

    // Grab one more token and check the same
    th = tb.grab(1);
    assert!(tb.deficiency(th) > 0);
    ManualClock::advance(Duration::from_secs(1));
    tb.replenish(now());
    assert!(tb.deficiency(th) == 0);

    make_ready_future(())
});

engine_test_case!(test_basic_capped_loop, {
    let tb = SharedTokenBucket::<Ratio1, RoversCapped, ManualClock>::new(1, 1, 0, false);

    // Grab on token and make sure it's only available in 1s
    let mut th = tb.grab(1);
    assert!(tb.deficiency(th) > 0);
    ManualClock::advance(Duration::from_secs(1));
    tb.replenish(now());
    assert!(tb.deficiency(th) == 0);

    // The 2nd time this trick only works after the 1st token is explicitly released
    th = tb.grab(1);
    assert!(tb.deficiency(th) > 0);
    ManualClock::advance(Duration::from_secs(1));
    tb.replenish(now());
    assert!(tb.deficiency(th) > 0);
    ManualClock::advance(Duration::from_secs(1));
    tb.release(1);
    tb.replenish(now());
    assert!(tb.deficiency(th) == 0);

    make_ready_future(())
});
