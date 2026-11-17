//! Small utilities: the single-producer queue of the syscall thread filled to capacity and
//! drained in order, and the allocation sampler with sampling off (it never samples, even
//! when its countdown runs out) and its pause guard.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use teseladb::engine::core::syscall_work_queue::SpscQueue;
use teseladb::engine::util::sampler::{DisableSamplingTemporarily, RandomDevice, Sampler};

#[test]
fn spsc_queue_full_and_drained() {
    let q: SpscQueue<u32, 4> = SpscQueue::default();
    assert!(q.empty());
    assert_eq!(q.pop(), None);
    let mut pushed = 0;
    while q.push(pushed) {
        pushed += 1;
    }
    assert_eq!(pushed, 4, "the capacity is the queue's size");
    assert!(!q.empty());
    for i in 0..4 {
        assert_eq!(q.pop(), Some(i));
    }
    assert!(q.empty());
    // Around the end of the slots.
    for round in 0..10u32 {
        assert!(q.push(round));
        assert_eq!(q.pop(), Some(round));
    }
}

#[test]
fn sampler_with_sampling_off() {
    let mut s = Sampler::default();
    assert_eq!(s.sampling_interval(), 0);
    // The countdown starts at the largest value: it runs out only after that many bytes.
    assert!(!s.maybe_sample(i64::MAX as usize));
    assert!(s.maybe_sample(1));
    // Sampling is off: no sample, and the countdown starts over.
    assert!(!s.definitely_sample(1));
    assert!(!s.maybe_sample(1 << 20));
    assert_eq!(s.sample_size(10), 10);
    // A paused sampler resumes with its interval.
    s.set_sampling_interval(4096);
    let p: *mut Sampler = &mut s;
    // SAFETY: the sampler stays in place and is not referenced while the guard lives.
    let g = unsafe { Sampler::pause_sampling(p) };
    // SAFETY: as above, read through the same pointer.
    assert_eq!(unsafe { (*p).sampling_interval() }, 0);
    drop(g);
    assert_eq!(s.sampling_interval(), 4096);
    // The default guard pauses nothing.
    drop(DisableSamplingTemporarily::default());
    let mut rd = RandomDevice;
    let _ = rd.call();
}
