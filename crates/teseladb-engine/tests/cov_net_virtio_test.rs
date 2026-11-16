#![cfg(target_os = "linux")]
//! Tests of the virtio parts that run without a vhost-net device: the split virtqueue
//! (descriptor allocation and chaining, the available ring, host notification with and
//! without event index, completions read from the used ring by its poller), the feature
//! negotiation of the device from its options, the options themselves and the small owned
//! buffer and notifier types.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::alloc::{Layout, alloc_zeroed};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::internal::pollable_fd::WriteableEventfd;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::net::native_stack::NativeStackOptions;
use teseladb::engine::net::net::Device;
use teseladb::engine::net::virtio::{
    Buffer, BufferChain, Completion, MallocBuf, Notifier, NotifierVhost, RingConfig, VirtioDevice,
    VirtioOptions, Vring, create_virtio_net_device, virt_to_phys,
};
use teseladb::engine::net::virtio_interface::{
    VIRTIO_NET_F_CSUM, VIRTIO_NET_F_GUEST_CSUM, VIRTIO_NET_F_GUEST_TSO4, VIRTIO_NET_F_GUEST_UFO,
    VIRTIO_NET_F_HOST_TSO4, VIRTIO_NET_F_HOST_UFO, VIRTIO_NET_F_MAC, VIRTIO_NET_F_MRG_RXBUF,
    VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
    VRING_USED_F_NO_NOTIFY,
};
use teseladb::engine::util::program_options::Value;
use teseladb::engine_thread_test_case;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(p: *mut c_void);
}

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

/// Records the completed chains (number of buffers, bytes written) and the bunch sizes.
struct Done {
    completed: Rc<RefCell<Vec<(usize, usize)>>>,
    bunches: Rc<Cell<u32>>,
}

impl Completion<Chain> for Done {
    unsafe fn complete(this: *mut Self, bc: Chain, len: usize) {
        // SAFETY: the ring calls this with its functor's root pointer and nothing else live.
        unsafe { (*this).completed.borrow_mut().push((bc.0.len(), len)) };
    }
    unsafe fn bunch(this: *mut Self, _c: u64) {
        // SAFETY: as above.
        unsafe { (*this).bunches.set((*this).bunches.get() + 1) };
    }
}

/// Counts host notifications and wakes.
struct Counter {
    notified: Rc<Cell<u32>>,
    woken: Rc<Cell<u32>>,
}

impl Notifier for Counter {
    fn notify(&mut self) {
        self.notified.set(self.notified.get() + 1);
    }
    fn wake_wait(&mut self) {
        self.woken.set(self.woken.get() + 1);
    }
}

/// Ring memory laid out as the virtio queue pair lays it out: the descriptor table, the
/// available ring after it, and the used ring on the next page boundary.
struct RingMem {
    cfg: RingConfig,
}

impl RingMem {
    fn new(size: u32, event_index: bool) -> RingMem {
        let bytes = 3 * 4096 + size as usize * (16 + 2 + 8);
        // SAFETY: a non-zero size with a valid power-of-two alignment; the block is leaked,
        // so it outlives the ring.
        let base = unsafe { alloc_zeroed(Layout::from_size_align(bytes, 4096).expect("layout")) };
        assert!(!base.is_null());
        // SAFETY: both offsets stay inside the block.
        let (avail, used) = unsafe {
            let avail = base.add(16 * size as usize);
            let end = avail.add(2 * size as usize + 6) as usize;
            (avail, base.add((end - base as usize).div_ceil(4096) * 4096))
        };
        RingMem {
            cfg: RingConfig {
                descs: base,
                avail,
                used,
                size,
                event_index,
                indirect: false,
                mergable_buffers: false,
            },
        }
    }
    fn u16_at(p: *mut u8) -> u16 {
        // SAFETY: inside the ring block.
        unsafe { (p as *const u16).read_unaligned() }
    }
    fn set_u16(p: *mut u8, v: u16) {
        // SAFETY: inside the ring block; the ring reads it atomically on this same thread.
        unsafe { (p as *mut u16).write_unaligned(v) };
    }
    /// Descriptor `i`: (address, length, flags, next).
    fn desc(&self, i: usize) -> (u64, u32, u16, u16) {
        // SAFETY: inside the descriptor table.
        unsafe {
            let d = self.cfg.descs.add(16 * i);
            (
                (d as *const u64).read_unaligned(),
                (d.add(8) as *const u32).read_unaligned(),
                (d.add(12) as *const u16).read_unaligned(),
                (d.add(14) as *const u16).read_unaligned(),
            )
        }
    }
    fn avail_idx(&self) -> u16 {
        // SAFETY: the index follows the flags.
        Self::u16_at(unsafe { self.cfg.avail.add(2) })
    }
    fn avail_entry(&self, j: usize) -> u16 {
        // SAFETY: inside the available ring.
        Self::u16_at(unsafe { self.cfg.avail.add(4 + 2 * j) })
    }
    /// The device uses chain `id`, writing `len` bytes, as element `k` of the used ring.
    fn use_chain(&self, k: usize, id: u32, len: u32) {
        // SAFETY: inside the used ring.
        unsafe {
            let e = self.cfg.used.add(4 + 8 * k);
            (e as *mut u32).write_unaligned(id);
            (e.add(4) as *mut u32).write_unaligned(len);
        }
    }
    fn set_used_idx(&self, v: u16) {
        // SAFETY: the index follows the flags.
        Self::set_u16(unsafe { self.cfg.used.add(2) }, v);
    }
    fn set_used_flags(&self, v: u16) {
        Self::set_u16(self.cfg.used, v);
    }
    /// The event index the device writes after the used ring.
    fn set_avail_event(&self, v: u16) {
        // SAFETY: just past the used ring elements, inside the block.
        Self::set_u16(unsafe { self.cfg.used.add(4 + 8 * self.cfg.size as usize) }, v);
    }
}

fn buf(addr: u64, len: u32, writeable: bool) -> Buffer {
    Buffer { addr, len, writeable }
}

engine_thread_test_case!(vring_posts_chains_and_completes_them, {
    let mem = RingMem::new(8, false);
    let completed = Rc::new(RefCell::new(Vec::new()));
    let bunches = Rc::new(Cell::new(0));
    let ring: *mut Vring<Chain, Done> = Box::into_raw(Box::new(Vring::new(
        mem.cfg,
        Done { completed: completed.clone(), bunches: bunches.clone() },
    )));
    let notified = Rc::new(Cell::new(0));
    let woken = Rc::new(Cell::new(0));
    // SAFETY: the ring is leaked and only touched from this thread, between reactor tasks.
    let r = unsafe { &mut *ring };
    assert_eq!(r.size(), 8);
    assert_eq!(r.getconfig().size, 8);
    assert_eq!(r.available_descriptors().available_units(), 8);
    r.set_notifier(Box::new(Counter { notified: notified.clone(), woken: woken.clone() }));
    r.wake_notifier_wait();
    assert_eq!(woken.get(), 1);
    let _ = r.completion_mut();

    // Two chains: two buffers (the second one writeable), then one.
    r.post(
        vec![
            Chain(vec![buf(0x1000, 100, false), buf(0x2000, 50, true)]),
            Chain(vec![buf(0x3000, 10, false)]),
        ]
        .into_iter(),
    );
    assert_eq!(mem.desc(0), (0x1000, 100, VRING_DESC_F_NEXT, 1));
    let (a, l, f, _) = mem.desc(1);
    assert_eq!((a, l, f), (0x2000, 50, VRING_DESC_F_WRITE));
    let (a, l, f, _) = mem.desc(2);
    assert_eq!((a, l, f), (0x3000, 10, 0));
    assert_eq!(mem.avail_idx(), 2);
    assert_eq!((mem.avail_entry(0), mem.avail_entry(1)), (0, 2));
    assert_eq!(notified.get(), 1);

    // The device uses both; the poller completes them and frees their descriptors.
    mem.use_chain(0, 0, 50);
    mem.use_chain(1, 2, 0);
    mem.set_used_idx(2);
    // SAFETY: the ring's root pointer; it is never moved nor freed.
    unsafe { Vring::start_poller(ring) };
    let mut tries = 0;
    while completed.borrow().len() < 2 && tries < 1000 {
        sleep_steady(Duration::from_millis(1)).get().expect("sleep");
        tries += 1;
    }
    assert_eq!(*completed.borrow(), vec![(2, 50), (1, 0)]);
    assert!(bunches.get() >= 1);

    // With notifications disabled by the device, posting does not notify.
    mem.set_used_flags(VRING_USED_F_NO_NOTIFY);
    // SAFETY: as above.
    let r = unsafe { &mut *ring };
    r.post((0..8).map(|i| Chain(vec![buf(0x10000 + i, 1, true)])));
    assert_eq!(mem.avail_idx(), 10);
    assert_eq!(notified.get(), 1);
});

engine_thread_test_case!(vring_event_index_suppresses_kicks, {
    let mem = RingMem::new(4, true);
    let ring: *mut Vring<Chain, Done> = Box::into_raw(Box::new(Vring::new(
        mem.cfg,
        Done { completed: Rc::new(RefCell::new(Vec::new())), bunches: Rc::new(Cell::new(0)) },
    )));
    let notified = Rc::new(Cell::new(0));
    // SAFETY: the ring is leaked and only touched from this thread.
    let r = unsafe { &mut *ring };
    r.set_notifier(Box::new(Counter { notified: notified.clone(), woken: Rc::new(Cell::new(0)) }));
    // The device asks to be kicked only once the available index passes 5.
    mem.set_avail_event(5);
    r.post(std::iter::once(Chain(vec![buf(1, 1, false)])));
    assert_eq!(notified.get(), 0);
    // Now it asks for a kick at index 1, which the next post passes.
    mem.set_avail_event(1);
    r.post(std::iter::once(Chain(vec![buf(2, 1, false)])));
    assert_eq!(notified.get(), 1);
});

engine_thread_test_case!(device_features_follow_the_options, {
    let mut opts = VirtioOptions::default();
    let lro = Value::new("lro", Some("on".to_string()), "LRO");
    let dev = VirtioDevice::new(&opts, &lro);
    let all = VIRTIO_RING_F_INDIRECT_DESC
        | VIRTIO_NET_F_MRG_RXBUF
        | VIRTIO_RING_F_EVENT_IDX
        | VIRTIO_NET_F_CSUM
        | VIRTIO_NET_F_GUEST_CSUM
        | VIRTIO_NET_F_HOST_TSO4
        | VIRTIO_NET_F_GUEST_TSO4
        | VIRTIO_NET_F_HOST_UFO
        | VIRTIO_NET_F_GUEST_UFO
        | VIRTIO_NET_F_MAC;
    assert_eq!(dev.features(), all);
    let hw = dev.hw_features();
    assert!(hw.tx_csum_l4_offload && hw.rx_csum_offload && hw.tx_tso && hw.rx_lro && hw.tx_ufo);
    assert_eq!(dev.hw_address().mac, [0x12, 0x23, 0x34, 0x56, 0x67, 0x78]);

    for v in [&mut opts.event_index, &mut opts.csum_offload, &mut opts.tso, &mut opts.ufo] {
        v.set_value("off".to_string());
    }
    let mut lro_off = Value::new("lro", Some("on".to_string()), "LRO");
    lro_off.set_value("off".to_string());
    let dev = VirtioDevice::new(&opts, &lro_off);
    assert_eq!(dev.features(), VIRTIO_RING_F_INDIRECT_DESC | VIRTIO_NET_F_MRG_RXBUF | VIRTIO_NET_F_MAC);
    let hw = dev.hw_features();
    assert!(!hw.tx_csum_l4_offload && !hw.rx_csum_offload && !hw.tx_tso && !hw.rx_lro && !hw.tx_ufo);

    let boxed = create_virtio_net_device(&opts, &lro_off).expect("a device");
    assert!(!boxed.hw_features().tx_tso);

    // Without a vhost-net device (and a tap device) the queue cannot be created; with one,
    // it is created and released.
    let net_opts = NativeStackOptions::new();
    let _ = boxed.init_local_queue(&net_opts, 0).map(|q| drop(q));
});

#[test]
fn virtio_options_defaults_and_linking() {
    let o = VirtioOptions::new();
    assert_eq!(o.event_index.get_value(), "on");
    assert_eq!(o.csum_offload.get_value(), "on");
    assert_eq!(o.tso.get_value(), "on");
    assert_eq!(o.ufo.get_value(), "on");
    assert_eq!(*o.virtio_ring_size.get_value(), 256);
    let mut linked = Box::new(VirtioOptions::new());
    // SAFETY: the options stay in their box, which is not moved afterwards.
    unsafe { Pin::new_unchecked(&mut *linked) }.link(None);
    let d = RingConfig::default();
    assert!(d.descs.is_null() && d.size == 0 && !d.event_index && !d.indirect && !d.mergable_buffers);
    let b = Buffer::default();
    assert_eq!((b.addr, b.len, b.writeable), (0, 0, false));
}

#[test]
fn malloc_buffers_free_what_they_own() {
    assert!(MallocBuf::default().get().is_null());
    // SAFETY: plain malloc; the buffer takes ownership.
    let owned = MallocBuf::from_raw(unsafe { malloc(32) } as *mut u8);
    assert!(!owned.get().is_null());
    drop(owned);
    // SAFETY: as above.
    let released = MallocBuf::from_raw(unsafe { malloc(16) } as *mut u8).release();
    assert!(!released.is_null());
    // SAFETY: the buffer gave up ownership of this malloc'ed block.
    unsafe { free(released as *mut c_void) };
    let mut x = 5u8;
    assert_eq!(virt_to_phys(&mut x), &mut x as *mut u8 as usize as u64);
}

engine_thread_test_case!(vhost_notifier_signals_its_eventfd, {
    let kick = WriteableEventfd::new(0).expect("an eventfd");
    let fd = kick.get_read_fd();
    let mut n = NotifierVhost::new(kick);
    n.notify();
    let mut v = 0u64;
    // SAFETY: reading 8 bytes into `v` from the eventfd the notifier signals.
    let r = unsafe { libc_read(fd, &mut v as *mut u64 as *mut c_void, 8) };
    assert_eq!(r, 8);
    assert_eq!(v, 1);
});

unsafe extern "C" {
    #[link_name = "read"]
    fn libc_read(fd: i32, buf: *mut c_void, n: usize) -> isize;
}
