#![cfg(target_os = "linux")]
//! The completion poller of a virtio ring reports, before completing them, how many used
//! buffers it found: the device's used index minus the ones already completed. The receive
//! queue adds that number to its packet statistics, so it must be the number of buffers.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::alloc::{Layout, alloc_zeroed};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::net::virtio::{Buffer, BufferChain, Completion, RingConfig, Vring};
use teseladb::engine_thread_test_case;

/// A chain of buffers.
#[derive(Default)]
struct Chain(Vec<Buffer>);

impl BufferChain for Chain {
    fn for_each_buffer<F: FnMut(Buffer)>(&self, mut f: F) {
        for b in &self.0 {
            f(*b);
        }
    }
}

/// Counts the completed chains and adds up the counts the ring reports.
struct Done {
    completed: Rc<RefCell<u64>>,
    reported: Rc<Cell<u64>>,
}

impl Completion<Chain> for Done {
    unsafe fn complete(this: *mut Self, _bc: Chain, _len: usize) {
        // SAFETY: the ring calls this with its functor's root pointer and nothing else live.
        unsafe { *(*this).completed.borrow_mut() += 1 };
    }
    unsafe fn bunch(this: *mut Self, c: u64) {
        // SAFETY: as above.
        unsafe { (*this).reported.set((*this).reported.get().wrapping_add(c)) };
    }
}

/// Ring memory as the virtio queue pair lays it out: descriptors, the available ring after
/// them, and the used ring on the next page boundary. Leaked, so it outlives the ring.
fn ring_memory(size: u32) -> RingConfig {
    let bytes = 3 * 4096 + size as usize * (16 + 2 + 8);
    // SAFETY: a non-zero size with a valid power-of-two alignment.
    let base = unsafe { alloc_zeroed(Layout::from_size_align(bytes, 4096).expect("layout")) };
    assert!(!base.is_null());
    // SAFETY: both offsets stay inside the block.
    let (avail, used) = unsafe {
        let avail = base.add(16 * size as usize);
        let end = avail.add(2 * size as usize + 6) as usize;
        (avail, base.add((end - base as usize).div_ceil(4096) * 4096))
    };
    RingConfig {
        descs: base,
        avail,
        used,
        size,
        event_index: false,
        indirect: false,
        mergable_buffers: false,
    }
}

/// The device uses chain `id` as element `k` of the used ring.
fn use_chain(cfg: &RingConfig, k: usize, id: u32) {
    // SAFETY: inside the used ring.
    unsafe {
        let e = cfg.used.add(4 + 8 * k);
        (e as *mut u32).write_unaligned(id);
        (e.add(4) as *mut u32).write_unaligned(0);
    }
}

/// The device publishes its used index.
fn set_used_idx(cfg: &RingConfig, v: u16) {
    // SAFETY: the index follows the flags, inside the used ring.
    unsafe { (cfg.used.add(2) as *mut u16).write_unaligned(v) };
}

engine_thread_test_case!(the_poller_reports_the_number_of_used_buffers, {
    let cfg = ring_memory(8);
    let completed = Rc::new(RefCell::new(0u64));
    let reported = Rc::new(Cell::new(0u64));
    let ring: *mut Vring<Chain, Done> = Box::into_raw(Box::new(Vring::new(
        cfg,
        Done { completed: completed.clone(), reported: reported.clone() },
    )));
    // SAFETY: the ring is leaked and only touched from this thread, between reactor tasks.
    let r = unsafe { &mut *ring };
    let buf = |addr| Buffer { addr, len: 64, writeable: true };
    let chains = vec![Chain(vec![buf(0x1000)]), Chain(vec![buf(0x2000)]), Chain(vec![buf(0x3000)])];
    r.post(chains.into_iter());

    // The device uses all three.
    use_chain(&cfg, 0, 0);
    use_chain(&cfg, 1, 1);
    use_chain(&cfg, 2, 2);
    set_used_idx(&cfg, 3);
    // SAFETY: the ring's root pointer; it is never moved nor freed.
    unsafe { Vring::start_poller(ring) };
    let mut tries = 0;
    while *completed.borrow() < 3 && tries < 1000 {
        sleep_steady(Duration::from_millis(1)).get().expect("sleep");
        tries += 1;
    }
    assert_eq!(*completed.borrow(), 3);
    // Idle polls report nothing more.
    sleep_steady(Duration::from_millis(5)).get().expect("sleep");
    assert_eq!(reported.get(), 3, "the poller reported a wrong number of used buffers");
});
