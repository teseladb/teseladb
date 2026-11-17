//! The fair queue's group checks and accessors (the original's fair_queue.cc): a group whose
//! minimum grab exceeds its replenish threshold is refused; capacities convert to tokens and
//! durations; a request larger than the tokens at hand leaves a pending reservation whose
//! expected time `next_pending_aio` reports, and is dispatched once the bucket refills; an
//! unplugged class is skipped until it is plugged again; the waiting and executing ticket counters
//! stay zero, as in the original.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::fair_queue::{
    FairGroup, FairGroupConfig, FairQueue, FairQueueConfig, FairQueueEntry,
};
use teseladb::engine::util::shared_token_bucket::internal::{SteadyClock, TokenBucketClock};

fn nanos(d: Duration) -> i64 {
    d.as_nanos() as i64
}

fn group() -> Box<FairGroup> {
    let cfg = FairGroupConfig {
        rate_limit_duration: Duration::from_micros(1000),
        ..FairGroupConfig::default()
    };
    let fg = Box::new(FairGroup::new(cfg, 1).unwrap());
    // No refill but the ones the test makes.
    fg.replenish_capacity(
        <SteadyClock as TokenBucketClock>::now() + nanos(Duration::from_secs(86400)),
    );
    fg
}

#[test]
fn a_group_whose_minimum_exceeds_its_threshold_is_refused() {
    let cfg = FairGroupConfig {
        min_tokens: 1e12,
        rate_limit_duration: Duration::from_micros(1),
        ..FairGroupConfig::default()
    };
    let e = FairGroup::new(cfg, 1).err().expect("refused");
    assert_eq!(
        e.get().unwrap().what(),
        "Fair-group replenisher limit is lower than threshold"
    );
}

#[test]
fn capacities_convert_to_tokens_and_durations() {
    let fg = group();
    let max = fg.maximum_capacity();
    assert_eq!(fg.token_bucket().limit(), max);
    assert_eq!(FairGroup::capacity_tokens(0), 0.0);
    let one = FairGroup::capacity_tokens(FairGroup::tokens_capacity(1.0));
    let two = FairGroup::capacity_tokens(FairGroup::tokens_capacity(2.0));
    assert!(one > 0.0);
    assert!((two / one - 2.0).abs() < 1e-6, "{one} {two}");
    // The duration of a capacity grows with it.
    let d0 = fg.capacity_duration(0);
    let d1 = fg.capacity_duration(max);
    assert!(d1 > d0);
}

#[test]
fn a_request_beyond_the_tokens_at_hand_waits_for_its_reservation() {
    let fg = group();
    // SAFETY: the group lives in its box, dropped after the queue.
    let mut fq = unsafe { FairQueue::new(&*fg, FairQueueConfig::default()) };
    assert_eq!(fq.maximum_capacity(), fg.maximum_capacity());
    fq.register_priority_class(0, 100);
    let dispatched: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
    let cap = fq.maximum_capacity();
    let ent = Box::into_raw(Box::new(FairQueueEntry::new(cap)));
    let second = Box::into_raw(Box::new(FairQueueEntry::new(cap)));
    // SAFETY: the entries live until they are dispatched and freed below.
    unsafe {
        fq.queue(0, ent);
        fq.queue(0, second);
    }
    // As in the original, the ticket counters are no longer kept since the queue counts
    // capacity: both stay zero.
    assert!(!fq.resources_currently_waiting().is_non_zero());
    assert!(!fq.resources_currently_executing().is_non_zero());
    assert_eq!(fq.next_pending_aio(), i64::MAX, "no reservation yet");

    // The bucket starts full: the first request takes all of it, the second must wait.
    let d = dispatched.clone();
    // SAFETY: the queue is live; the callback does not touch it.
    unsafe { FairQueue::dispatch_requests(&mut fq, move |e| d.borrow_mut().push(e as usize)) };
    assert_eq!(
        *dispatched.borrow(),
        vec![ent as usize],
        "one bucket's worth"
    );
    let now = <SteadyClock as TokenBucketClock>::now();
    let when = fq.next_pending_aio();
    assert!(
        when != i64::MAX && when >= now,
        "a reservation is pending ({when} vs {now})"
    );

    // Refill the bucket and dispatch again.
    fg.replenish_capacity(fg.replenished_ts() + nanos(Duration::from_secs(1)));
    let d = dispatched.clone();
    // SAFETY: as above.
    unsafe { FairQueue::dispatch_requests(&mut fq, move |e| d.borrow_mut().push(e as usize)) };
    assert_eq!(*dispatched.borrow(), vec![ent as usize, second as usize]);
    fq.notify_request_finished(cap);
    fq.notify_request_finished(cap);
    // SAFETY: the dispatched entries are ours again.
    unsafe {
        drop(Box::from_raw(ent));
        drop(Box::from_raw(second));
    }
    fq.unregister_priority_class(0);
}

#[test]
fn an_unplugged_class_is_skipped_until_plugged() {
    let fg = group();
    fg.replenish_capacity(fg.replenished_ts() + nanos(Duration::from_secs(10)));
    // SAFETY: the group lives in its box, dropped after the queue.
    let mut fq = unsafe { FairQueue::new(&*fg, FairQueueConfig::default()) };
    fq.register_priority_class(0, 100);
    fq.register_priority_class(1, 100);
    let small = fq.tokens_capacity(0.000_001).max(1);
    let a = Box::into_raw(Box::new(FairQueueEntry::new(small)));
    let b = Box::into_raw(Box::new(FairQueueEntry::new(small)));
    // SAFETY: the entries live until they are dispatched and freed below.
    unsafe {
        fq.queue(0, a);
        fq.queue(1, b);
    }
    fq.unplug_class(0);
    let dispatched: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
    let d = dispatched.clone();
    // SAFETY: the queue is live; the callback does not touch it.
    unsafe { FairQueue::dispatch_requests(&mut fq, move |e| d.borrow_mut().push(e as usize)) };
    assert_eq!(
        *dispatched.borrow(),
        vec![b as usize],
        "class 0 is unplugged"
    );
    fq.plug_class(0);
    let d = dispatched.clone();
    // SAFETY: as above.
    unsafe { FairQueue::dispatch_requests(&mut fq, move |e| d.borrow_mut().push(e as usize)) };
    assert_eq!(*dispatched.borrow(), vec![b as usize, a as usize]);
    fq.notify_request_finished(small);
    fq.notify_request_finished(small);
    // SAFETY: both entries are ours again.
    unsafe {
        drop(Box::from_raw(a));
        drop(Box::from_raw(b));
    }
    fq.unregister_priority_class(0);
    fq.unregister_priority_class(1);
}
