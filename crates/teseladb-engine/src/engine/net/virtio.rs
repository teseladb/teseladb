//! The virtio-net device of the native stack, driven through the Linux vhost-net kernel
//! backend and a tap interface.
//!
//! [`VirtioDevice`] describes the device and its offloads (checksum, TSO, UFO, merged
//! receive buffers, event index) from [`VirtioOptions`]; [`QpVhost`] is its single queue
//! pair: it opens `/dev/vhost-net` and `/dev/net/tun`, sets up the split virtqueues (see
//! [`crate::engine::net::virtio_interface`]) in memory shared with the kernel, and wires
//! the kick and call eventfds. The transmit queue posts packets as descriptor chains
//! without copying; the receive queue keeps the ring filled with buffers and hands each
//! completed buffer up the stack. Both are driven by reactor pollers.
//!
//! The rings live in memory shared with the kernel; the queue objects point into it and the
//! completions and pollers keep the address of their queue. The queue pair is therefore
//! built in a stable allocation and its back-pointers are set once it is in place.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU16, Ordering, fence};

use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::deleter::{Deleter, make_free_deleter, make_object_deleter};
use crate::engine::core::future::{ExceptionPtr, Future};
use crate::engine::core::internal::poll::Poller;
use crate::engine::core::internal::pollable_fd::{ReadableEventfd, WriteableEventfd};
use crate::engine::core::loop_::keep_doing;
use crate::engine::core::posix::FileDesc;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::net::const_::IpProtocolNum;
use crate::engine::net::ethernet::{EthHdr, EthernetAddress};
use crate::engine::net::native_stack::NativeStackOptions;
use crate::engine::net::net::{
    Device, DeviceBase, HwFeatures, Qp, QpBase, QpPtr, QpStats, qp_init,
};
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::net::packet::{Fragment, Packet};
use crate::engine::net::virtio_interface::*;
use crate::engine::util::program_options::{OptionGroup, Value};
use std::pin::Pin;

/// Virtio configuration.
#[repr(C)]
pub struct VirtioOptions {
    /// The option group these options belong to.
    pub group: OptionGroup,
    /// Enable the event-index feature (`on` / `off`).
    ///
    /// Default: `on`.
    pub event_index: Value<String>,
    /// Enable the checksum offload feature (`on` / `off`).
    ///
    /// Default: `on`.
    pub csum_offload: Value<String>,
    /// Enable the TCP segmentation offload feature (`on` / `off`).
    ///
    /// Default: `on`.
    pub tso: Value<String>,
    /// Enable the UDP fragmentation offload feature (`on` / `off`).
    ///
    /// Default: `on`.
    pub ufo: Value<String>,
    /// Virtio ring size (must be a power of two).
    ///
    /// Default: 256.
    pub virtio_ring_size: Value<u32>,
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(p: *mut c_void);
    fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> i32;
}

/// An owned buffer allocated with `malloc`, freed with `free` on drop.
pub struct MallocBuf(*mut u8);

impl MallocBuf {
    /// Takes ownership of a malloc'ed buffer.
    #[inline]
    pub fn from_raw(p: *mut u8) -> Self {
        MallocBuf(p)
    }
    /// The buffer.
    #[inline]
    pub fn get(&self) -> *mut u8 {
        self.0
    }
    /// Gives up ownership.
    #[inline]
    pub fn release(mut self) -> *mut u8 {
        std::mem::replace(&mut self.0, std::ptr::null_mut())
    }
}

impl Default for MallocBuf {
    fn default() -> Self {
        MallocBuf(std::ptr::null_mut())
    }
}

impl Drop for MallocBuf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the buffer came from malloc/posix_memalign and is freed once.
            unsafe { free(self.0 as *mut c_void) };
        }
    }
}

/// A guest-physical address.
pub type Phys = u64;

/// The native stack runs with an identity mapping.
#[inline]
pub fn virt_to_phys(p: *mut u8) -> Phys {
    p as usize as Phys
}

/// The virtio-net device.
pub struct VirtioDevice {
    base: DeviceBase,
    _hw_features: HwFeatures,
    _features: u64,
}

impl VirtioDevice {
    fn setup_features(hw: &mut HwFeatures, opts: &VirtioOptions, lro: &Value<String>) -> u64 {
        let mut engine_supported_features: i64 =
            (VIRTIO_RING_F_INDIRECT_DESC | VIRTIO_NET_F_MRG_RXBUF) as i64;

        if !(opts.event_index.as_bool() && opts.event_index.get_value() == "off") {
            engine_supported_features |= VIRTIO_RING_F_EVENT_IDX as i64;
        }
        if !(opts.csum_offload.as_bool() && opts.csum_offload.get_value() == "off") {
            engine_supported_features |= (VIRTIO_NET_F_CSUM | VIRTIO_NET_F_GUEST_CSUM) as i64;
            hw.tx_csum_l4_offload = true;
            hw.rx_csum_offload = true;
        } else {
            hw.tx_csum_l4_offload = false;
            hw.rx_csum_offload = false;
        }
        if !(opts.tso.as_bool() && opts.tso.get_value() == "off") {
            engine_supported_features |= VIRTIO_NET_F_HOST_TSO4 as i64;
            hw.tx_tso = true;
        } else {
            hw.tx_tso = false;
        }

        if !(lro.as_bool() && lro.get_value() == "off") {
            engine_supported_features |= VIRTIO_NET_F_GUEST_TSO4 as i64;
            hw.rx_lro = true;
        } else {
            hw.rx_lro = false;
        }

        if !(opts.ufo.as_bool() && opts.ufo.get_value() == "off") {
            engine_supported_features |= VIRTIO_NET_F_HOST_UFO as i64;
            engine_supported_features |= VIRTIO_NET_F_GUEST_UFO as i64;
            hw.tx_ufo = true;
        } else {
            hw.tx_ufo = false;
        }

        engine_supported_features |= VIRTIO_NET_F_MAC as i64;
        engine_supported_features as u64
    }

    /// A device with the features `opts` and `lro` enable.
    pub fn new(opts: &VirtioOptions, lro: &Value<String>) -> Self {
        let mut hw = HwFeatures::default();
        let features = Self::setup_features(&mut hw, opts, lro);
        VirtioDevice { base: DeviceBase::default(), _hw_features: hw, _features: features }
    }

    /// The negotiated virtio features.
    #[inline]
    pub fn features(&self) -> u64 {
        self._features
    }
}

impl Device for VirtioDevice {
    fn base(&self) -> &DeviceBase {
        &self.base
    }

    fn hw_address(&self) -> EthernetAddress {
        EthernetAddress::from_list(&[0x12, 0x23, 0x34, 0x56, 0x67, 0x78])
    }

    fn hw_features(&self) -> HwFeatures {
        self._hw_features
    }

    fn init_local_queue(
        &self,
        net_opts: &NativeStackOptions,
        qid: u16,
    ) -> Result<QpPtr, ExceptionPtr> {
        static CALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        assert!(qid == 0);
        assert!(!CALLED.load(Ordering::Relaxed));
        CALLED.store(true, Ordering::Relaxed);
        // The options type is fixed by the signature (see `Device::init_local_queue`);
        // a failure to open the queue is the error returned.
        // SAFETY: the device outlives its queues.
        unsafe { QpVhost::new_node(self as *const VirtioDevice, net_opts) }
    }
}

/// How the guest and the host notify each other of ring activity. With vhost both
/// directions go through eventfds.
pub trait Notifier {
    /// Notifies the host.
    fn notify(&mut self);
    /// Wakes a waiter. A notifier does not need to implement this if its wait is woken by an
    /// external event generated by another process (the default aborts).
    fn wake_wait(&mut self) {
        std::process::abort();
    }
}

/// Notifications through a vhost kick eventfd.
pub struct NotifierVhost {
    _kick: WriteableEventfd,
}

impl Notifier for NotifierVhost {
    fn notify(&mut self) {
        self._kick.signal(1);
    }
}

impl NotifierVhost {
    /// A notifier kicking `kick`.
    pub fn new(kick: WriteableEventfd) -> Self {
        NotifierVhost { _kick: kick }
    }
}

/// Where a ring lives and which features it uses.
#[derive(Clone, Copy, Debug)]
pub struct RingConfig {
    /// The descriptor table.
    pub descs: *mut u8,
    /// The available ring.
    pub avail: *mut u8,
    /// The used ring.
    pub used: *mut u8,
    /// Number of descriptors.
    pub size: u32,
    /// VIRTIO_RING_F_EVENT_IDX was negotiated.
    pub event_index: bool,
    /// Indirect descriptors are used.
    pub indirect: bool,
    /// Mergeable receive buffers are used.
    pub mergable_buffers: bool,
}

impl Default for RingConfig {
    fn default() -> Self {
        RingConfig {
            descs: std::ptr::null_mut(),
            avail: std::ptr::null_mut(),
            used: std::ptr::null_mut(),
            size: 0,
            event_index: false,
            indirect: false,
            mergable_buffers: false,
        }
    }
}

/// A buffer handed to the device.
#[derive(Clone, Copy, Debug, Default)]
pub struct Buffer {
    /// Guest-physical address.
    pub addr: Phys,
    /// Length.
    pub len: u32,
    /// The device writes into it.
    pub writeable: bool,
}

/// A chain of buffers posted to a ring as one descriptor chain.
pub trait BufferChain: Default {
    /// Calls `f` for each buffer of the chain, in order.
    fn for_each_buffer<F: FnMut(Buffer)>(&self, f: F);
}

/// The `Completion` concept.
///
/// The functor lives inside its ring and goes back into the ring's owner (the queue whose
/// member the ring is), so it is called through its root pointer: the ring keeps no
/// reference to itself or to the functor while it runs.
pub trait Completion<B> {
    /// A chain was used by the device; `len` bytes were written.
    ///
    /// # Safety
    /// `this` is the functor's root pointer (projected from its ring's), with no reference
    /// to the ring, the functor or the ring's owner live.
    unsafe fn complete(this: *mut Self, bc: B, len: usize);
    /// `c` chains are about to be completed.
    ///
    /// # Safety
    /// As for `complete`.
    unsafe fn bunch(this: *mut Self, c: u64);
}

/// Descriptor flag bits.
const DESC_HAS_NEXT: u16 = 1 << 0;
const DESC_WRITEABLE: u16 = 1 << 1;
#[allow(dead_code)]
const DESC_INDIRECT: u16 = 1 << 2;

/// A virtqueue descriptor (address, length, flags, next).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Desc {
    _paddr: Phys,
    _len: u32,
    _flags: u16,
    _next: u16,
}

/// Header of the available ring: written by the guest, read by the host.
#[repr(C)]
struct AvailLayout {
    // Mark that we do not need an interrupt for consuming a descriptor
    // from the ring. Unreliable so it's simply an optimization
    _flags: AtomicU16,

    // Where we put the next descriptor
    _idx: AtomicU16,
    // There may be no more entries than the queue size read from device
    _ring: [u16; 0],
    // The used-event index, which asks the host to interrupt only when the used index
    // reaches it, sits after the variable-length ring array, so it is reached through an
    // accessor rather than a field.
}

/// An element of the used ring: descriptor head index and bytes written.
#[repr(C)]
#[derive(Clone, Copy)]
struct UsedElem {
    // Index of start of used _desc chain. (uint32_t for padding reasons)
    _id: u32,
    // Total length of the descriptor chain which was used (written to)
    _len: u32,
}

/// Header of the used ring: written by the host, read by the guest.
#[repr(C)]
struct UsedLayout {
    // Atomic because the host changes it.
    _flags: AtomicU16,
    // Atomic so its accesses carry memory barriers.
    _idx: AtomicU16,
    _used_elements: [UsedElem; 0],
    // The avail-event index, which asks the guest to kick the host only when the available
    // index reaches it, sits after the variable-length ring array, so it is reached through
    // an accessor rather than a field.
}

// used_layout::no_notify: VRING_USED_F_NO_NOTIFY

struct Avail {
    _shared: *mut AvailLayout,
    _head: u16,
    _avail_added_since_kick: u16,
}

impl Avail {
    fn new(conf: RingConfig) -> Self {
        Avail { _shared: conf.avail as *mut AvailLayout, _head: 0, _avail_added_since_kick: 0 }
    }
}

struct Used {
    _shared: *mut UsedLayout,
    _tail: u16,
}

impl Used {
    fn new(conf: RingConfig) -> Self {
        Used { _shared: conf.used as *mut UsedLayout, _tail: 0 }
    }
}

/// A virtqueue.
pub struct Vring<B: BufferChain, C: Completion<B>> {
    _config: RingConfig,
    _complete: C,
    _notifier: Option<Box<dyn Notifier>>,
    _buffer_chains: Box<[B]>,
    _descs: *mut Desc,
    _avail: Avail,
    _used: Used,
    // The event index fields after the two rings. The host reads and writes them
    // concurrently, so this process only
    // ever accesses them through these atomics: the ring memory is zeroed (non-atomically)
    // before the queue is handed to the host, and is never touched with plain accesses
    // afterwards.
    _avail_event: *const AtomicU16,
    _used_event: *const AtomicU16,
    _available_descriptors: Semaphore,
    _free_head: i32,
    _free_last: i32,
    _poller: Option<Poller>,
}

impl<B: BufferChain + 'static, C: Completion<B> + 'static> Vring<B, C> {
    /// Creates a ring over the memory described by `conf`; `start_poller` registers
    /// the completion poller once the ring is at its final address.
    pub fn new(conf: RingConfig, complete: C) -> Self {
        let chains: Vec<B> = (0..conf.size).map(|_| B::default()).collect();
        let avail = Avail::new(conf);
        let used = Used::new(conf);
        // SAFETY: the shared ring memory holds `size` elements after each header, followed
        // by the event index fields, which are 2-byte aligned (the rings are) and are only
        // accessed atomically from here on (see the fields).
        let (avail_event, used_event) = unsafe {
            (
                (std::ptr::addr_of_mut!((*used._shared)._used_elements) as *mut UsedElem)
                    .add(conf.size as usize) as *const AtomicU16,
                (std::ptr::addr_of_mut!((*avail._shared)._ring) as *mut u16).add(conf.size as usize)
                    as *const AtomicU16,
            )
        };
        let mut v = Vring {
            _config: conf,
            _complete: complete,
            _notifier: None,
            _buffer_chains: chains.into_boxed_slice(),
            _descs: conf.descs as *mut Desc,
            _avail: avail,
            _used: used,
            _avail_event: avail_event,
            _used_event: used_event,
            _available_descriptors: Semaphore::new(0),
            _free_head: -1,
            _free_last: -1,
            _poller: None,
        };
        v.setup();
        v
    }

    /// Registers the reactor poller that runs the completions of used buffers.
    ///
    /// # Safety
    /// `this` is the ring's root pointer (projected from its qp's node, which never moves),
    /// with no reference to the ring live.
    pub unsafe fn start_poller(this: *mut Self) {
        // SAFETY: guaranteed by the caller; a short access. In the poller: the poller is owned
        // by the ring, so the ring outlives it, and it runs from the reactor with no other
        // reference to the ring live.
        unsafe { (*this)._poller = Some(Poller::simple(move || Self::do_complete(this))) };
    }

    /// The completion functor.
    pub fn completion_mut(&mut self) -> &mut C {
        &mut self._complete
    }

    /// Installs the notifier.
    pub fn set_notifier(&mut self, notifier: Box<dyn Notifier>) {
        self._notifier = Some(notifier);
    }

    /// The configuration of the ring.
    pub fn getconfig(&self) -> &RingConfig {
        &self._config
    }

    /// Wakes the notifier's waiter.
    pub fn wake_notifier_wait(&mut self) {
        if let Some(n) = self._notifier.as_mut() {
            n.wake_wait();
        }
    }

    /// Total number of descriptors in ring
    #[inline]
    pub fn size(&self) -> i32 {
        self._config.size as i32
    }

    /// The free descriptors.
    #[inline]
    pub fn available_descriptors(&mut self) -> &mut Semaphore {
        &mut self._available_descriptors
    }

    #[inline]
    fn notifications_disabled(&self) -> bool {
        // SAFETY: `_shared` points into the ring memory.
        (unsafe { (*self._used._shared)._flags.load(Ordering::Relaxed) } & VRING_USED_F_NO_NOTIFY)
            != 0
    }

    fn kick(&mut self) {
        let mut need_kick = true;
        // Make sure we see the fresh _idx value writen before kick.
        fence(Ordering::SeqCst);
        if self._config.event_index {
            // SAFETY: both point into the ring memory.
            let avail_idx: u16 = unsafe { (*self._avail._shared)._idx.load(Ordering::Relaxed) };
            // SAFETY: as above.
            let avail_event: u16 = unsafe { (*self._avail_event).load(Ordering::Relaxed) };
            need_kick = avail_idx.wrapping_sub(avail_event).wrapping_sub(1)
                < self._avail._avail_added_since_kick;
        } else if self.notifications_disabled() {
            return;
        }
        if need_kick || (self._avail._avail_added_since_kick >= u16::MAX / 2) {
            if let Some(n) = self._notifier.as_mut() {
                n.notify();
            }
            self._avail._avail_added_since_kick = 0;
        }
    }

    #[inline]
    fn mask(&self) -> usize {
        self.size() as usize - 1
    }

    #[inline]
    fn masked(&self, idx: usize) -> usize {
        idx & self.mask()
    }

    #[inline]
    fn allocate_desc(&mut self) -> u32 {
        assert!(self._free_head != -1);
        let desc = self._free_head;
        if desc == self._free_last {
            self._free_head = -1;
            self._free_last = -1;
        } else {
            // SAFETY: `desc` is a valid descriptor index.
            self._free_head = unsafe { (*self._descs.add(desc as usize))._next } as i32;
        }
        desc as u32
    }

    fn setup(&mut self) {
        for i in 0..self._config.size {
            // SAFETY: `i` is below the ring size.
            unsafe { (*self._descs.add(i as usize))._next = (i + 1) as u16 };
        }
        self._free_head = 0;
        self._free_last = self._config.size as i32 - 1;
        self._available_descriptors.signal(self._config.size as usize);
    }

    /// Posts the chains of `iter` to the device.
    pub fn post<I: Iterator<Item = B>>(&mut self, iter: I) {
        for bc in iter {
            let mut pseudo_head = Desc::default();
            let mut prev: *mut Desc = &mut pseudo_head;
            bc.for_each_buffer(|b| {
                let desc_idx = self.allocate_desc();
                // SAFETY: `prev` is the pseudo head or a descriptor of the table, and
                // `desc_idx` is a valid descriptor index.
                unsafe {
                    (*prev)._flags |= DESC_HAS_NEXT;
                    (*prev)._next = desc_idx as u16;
                    let d = &mut *self._descs.add(desc_idx as usize);
                    d._flags = 0;
                    if b.writeable {
                        d._flags |= DESC_WRITEABLE;
                    }
                    d._paddr = b.addr;
                    d._len = b.len;
                    prev = d;
                }
            });
            let desc_head = pseudo_head._next;
            // SAFETY: `desc_head` is a valid descriptor index.
            unsafe { *self._buffer_chains.get_unchecked_mut(desc_head as usize) = bc };
            let slot = self.masked(self._avail._head as usize);
            self._avail._head = self._avail._head.wrapping_add(1);
            // SAFETY: `slot` is below the ring size.
            unsafe {
                *(std::ptr::addr_of_mut!((*self._avail._shared)._ring) as *mut u16).add(slot) =
                    desc_head;
            }
            self._avail._avail_added_since_kick =
                self._avail._avail_added_since_kick.wrapping_add(1);
        }
        // SAFETY: `_shared` points into the ring memory.
        unsafe { (*self._avail._shared)._idx.store(self._avail._head, Ordering::Release) };
        self.kick();
    }

    /// Runs the completion for every buffer chain the host has used, through the ring's
    /// root pointer: the completion functor goes back
    /// into the ring's owner, which contains the ring, so no reference to the ring is live
    /// while it runs; every access below is a short place access through `this`.
    ///
    /// # Safety
    /// `this` is the ring's root pointer (projected from its qp's node), with no reference to
    /// the ring or its owner live.
    unsafe fn do_complete(this: *mut Self) -> bool {
        use std::ptr::addr_of_mut;
        // SAFETY: guaranteed by the caller. `_shared` points into the ring memory; the
        // completion functor is projected from `this` and called with no reference live.
        unsafe {
            let used_head = (*(*this)._used._shared)._idx.load(Ordering::Acquire);
            // The buffers the device used since the last completion.
            let count = used_head.wrapping_sub((*this)._used._tail);
            let complete: *mut C = addr_of_mut!((*this)._complete);
            C::bunch(complete, count as u64);
            let mask = (*this)._config.size as usize - 1;
            while used_head != (*this)._used._tail {
                let tail = (*this)._used._tail;
                (*this)._used._tail = tail.wrapping_add(1);
                // `slot` is below the ring size.
                let ue = *(addr_of_mut!((*(*this)._used._shared)._used_elements) as *mut UsedElem)
                    .add(tail as usize & mask);
                // The device only returns descriptor heads we posted.
                let chains = &mut (*this)._buffer_chains;
                let bc = std::mem::take(chains.get_unchecked_mut(ue._id as usize));
                C::complete(complete, bc, ue._len as usize);
                let mut id = ue._id;
                let descs = (*this)._descs;
                if (*this)._free_last != -1 {
                    // `_free_last` is a valid descriptor index.
                    (*descs.add((*this)._free_last as usize))._next = id as u16;
                } else {
                    (*this)._free_head = id as i32;
                }
                loop {
                    // `id` is a valid descriptor index.
                    let d = *descs.add(id as usize);
                    if d._flags & DESC_HAS_NEXT == 0 {
                        break;
                    }
                    id = d._next as u32;
                }
                (*this)._free_last = id as i32;
            }
            count != 0
        }
    }
}

/// The virtio-net header that precedes every packet (offload flags, segmentation and
/// checksum offsets).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NetHdr {
    flags: u8, // needs_csum : 1, flags_reserved : 7
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
}

#[allow(dead_code)]
const GSO_NONE: u8 = 0;
const GSO_TCPV4: u8 = 1;
const GSO_UDP: u8 = 3;
#[allow(dead_code)]
const GSO_TCPV6: u8 = 4;
#[allow(dead_code)]
const GSO_ECN: u8 = 0x80;

/// The virtio-net header with the buffer count used with merged receive buffers.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NetHdrMrg {
    hdr: NetHdr,
    num_buffers: u16,
}

fn fragment_to_buffer(f: Fragment) -> Buffer {
    Buffer { addr: virt_to_phys(f.base), len: f.size as u32, writeable: false }
}

/// A packet to send, seen as a buffer chain (header plus one buffer per fragment).
#[derive(Default)]
struct PacketAsBufferChain {
    p: Option<Packet>,
}

impl BufferChain for PacketAsBufferChain {
    fn for_each_buffer<F: FnMut(Buffer)>(&self, mut f: F) {
        if let Some(p) = &self.p {
            for frag in p.fragments().iter() {
                f(fragment_to_buffer(*frag));
            }
        }
    }
}

/// Completion of the transmit ring: frees the packets the host has sent.
struct TxComplete {
    q: *mut Txq,
}

impl Completion<PacketAsBufferChain> for TxComplete {
    unsafe fn complete(this: *mut Self, bc: PacketAsBufferChain, _len: usize) {
        // move the packet here, to be destroyed on scope exit
        let p = bc.p;
        let n = p.as_ref().map(|p| p.nr_frags()).unwrap_or(0);
        // SAFETY: guaranteed by the caller; `q` is the txq's root pointer (set by `fix_up`),
        // and the txq owns the ring that owns this completion. The semaphore is reached by a
        // place projection, with no reference to the ring formed.
        unsafe {
            let q = (*this).q;
            (*std::ptr::addr_of_mut!((*q)._ring._available_descriptors)).signal(n as usize);
        }
    }
    unsafe fn bunch(_this: *mut Self, _c: u64) {}
}

/// The transmit queue. It keeps no reference to its queue pair: `post` receives the
/// header length, the device and the statistics, so that no reference to the queue pair is
/// formed while the queue pair's `send_batch` holds it.
struct Txq {
    _ring: Vring<PacketAsBufferChain, TxComplete>,
    _packets: Vec<PacketAsBufferChain>,
}

impl Txq {
    fn new(config: RingConfig) -> Self {
        Txq {
            _ring: Vring::new(config, TxComplete { q: std::ptr::null_mut() }),
            _packets: Vec::new(),
        }
    }

    fn set_notifier(&mut self, notifier: Box<dyn Notifier>) {
        self._ring.set_notifier(notifier);
    }

    fn getconfig(&self) -> &RingConfig {
        self._ring.getconfig()
    }

    #[allow(dead_code)]
    fn wake_notifier_wait(&mut self) {
        self._ring.wake_notifier_wait();
    }

    /// Posts the packets of `pb` to the transmit ring; `header_len`, `dev` and `stats` are
    /// the queue pair's.
    fn post(
        &mut self,
        pb: &mut CircularBuffer<Packet>,
        header_len: usize,
        dev: *const VirtioDevice,
        stats: &mut QpStats,
    ) -> u32 {
        let mut bytes: u64 = 0;
        let mut nr_frags: u64 = 0;

        self._packets.clear();

        // SAFETY: the device outlives its queues.
        let hw = unsafe { (*dev).hw_features() };
        while !pb.empty() {
            if !((pb.front().nr_frags() as usize) < self._ring.available_descriptors().current()) {
                break;
            }
            let mut vhdr = NetHdrMrg::default();
            // SAFETY: the loop condition says the buffer is not empty.
            let p = unsafe { pb.take_front().unwrap_unchecked() };

            bytes += p.len() as u64;
            nr_frags += p.nr_frags() as u64;

            // Handle TCP checksum offload
            let oi = p.get_offload_info();
            if hw.tx_csum_l4_offload {
                let eth_hdr_len = std::mem::size_of::<EthHdr>() as u16;
                let ip_hdr_len = oi.ip_hdr_len as u16;
                let mtu = hw.mtu;
                if oi.protocol == IpProtocolNum::tcp {
                    let tcp_hdr_len = oi.tcp_hdr_len as u16;
                    if oi.needs_csum {
                        vhdr.hdr.flags |= 1;
                        vhdr.hdr.csum_start = eth_hdr_len + ip_hdr_len;
                        // TCP checksum filed's offset within the TCP header is 16 bytes
                        vhdr.hdr.csum_offset = 16;
                    }
                    if oi.tso_seg_size != 0 {
                        // IPv4 TCP TSO
                        vhdr.hdr.gso_type = GSO_TCPV4;
                        // Sum of Ethernet, IP and TCP header size
                        vhdr.hdr.hdr_len = eth_hdr_len + ip_hdr_len + tcp_hdr_len;
                        // Maximum segment size of packet after the offload
                        vhdr.hdr.gso_size = oi.tso_seg_size;
                    }
                } else if oi.protocol == IpProtocolNum::udp {
                    let udp_hdr_len = oi.udp_hdr_len as u16;
                    if oi.needs_csum {
                        vhdr.hdr.flags |= 1;
                        vhdr.hdr.csum_start = eth_hdr_len + ip_hdr_len;
                        // UDP checksum filed's offset within the UDP header is 6 bytes
                        vhdr.hdr.csum_offset = 6;
                    }
                    if hw.tx_ufo && p.len() > (mtu + eth_hdr_len) as u32 {
                        vhdr.hdr.gso_type = GSO_UDP;
                        vhdr.hdr.hdr_len = eth_hdr_len + ip_hdr_len + udp_hdr_len;
                        vhdr.hdr.gso_size = mtu - ip_hdr_len - udp_hdr_len;
                    }
                }
            }
            // prepend virtio-net header
            let q = Packet::prepend_copy(
                Fragment { base: &mut vhdr as *mut NetHdrMrg as *mut u8, size: header_len },
                p,
            );
            let fut = self._ring.available_descriptors().wait(q.nr_frags() as usize);
            assert!(fut.available()); // how it cannot?
            self._packets.push(PacketAsBufferChain { p: Some(q) });
        }
        let n = self._packets.len() as u32;
        // The chains move into the ring and `_packets` keeps its capacity for the next
        // batch.
        self._ring.post(self._packets.drain(..));

        stats.tx.good.update_frags_stats(nr_frags, bytes);

        n
    }
}

/// A receive buffer: its memory and its guest address.
#[derive(Default)]
struct BufferAndVirt {
    b: Buffer,
    buf: MallocBuf,
}

/// A buffer chain of one receive buffer.
#[derive(Default)]
struct SingleBuffer([BufferAndVirt; 1]);

impl BufferChain for SingleBuffer {
    fn for_each_buffer<F: FnMut(Buffer)>(&self, mut f: F) {
        f(self.0[0].b);
    }
}

/// Completion of the receive ring: hands each filled buffer to the receive queue.
struct RxComplete {
    q: *mut Rxq,
}

impl Completion<SingleBuffer> for RxComplete {
    unsafe fn complete(this: *mut Self, bc: SingleBuffer, len: usize) {
        // SAFETY: guaranteed by the caller; `q` is the rxq's root pointer (set by `fix_up`),
        // and the rxq owns the ring that owns this completion.
        unsafe { Rxq::complete_buffer((*this).q, bc, len) };
    }
    unsafe fn bunch(this: *mut Self, c: u64) {
        // SAFETY: as above.
        unsafe { Rxq::update_rx_count((*this).q, c) };
    }
}

struct Rxq {
    _dev: *mut VirtioQp,
    _ring: Vring<SingleBuffer, RxComplete>,
    _remaining_buffers: u32,
    _fragments: Vec<Fragment>,
    _buffers: Vec<MallocBuf>,
}

impl Rxq {
    fn new(dev: *mut VirtioQp, config: RingConfig) -> Self {
        Rxq {
            _dev: dev,
            _ring: Vring::new(config, RxComplete { q: std::ptr::null_mut() }),
            _remaining_buffers: 0,
            _fragments: Vec::new(),
            _buffers: Vec::new(),
        }
    }

    fn set_notifier(&mut self, notifier: Box<dyn Notifier>) {
        self._ring.set_notifier(notifier);
    }

    fn getconfig(&self) -> &RingConfig {
        self._ring.getconfig()
    }

    /// # Safety
    /// `this` is the rxq's root pointer (projected from its qp's node), with no reference
    /// to the rxq live: `keep_doing` runs the first iterations in line.
    unsafe fn run(this: *mut Rxq) {
        // FIXME: future is discarded
        // At least catch errors and warn about them.
        // SAFETY: the rxq lives as long as its qp, which lives until the reactor exits; each
        // iteration runs with no other reference to it live.
        let _ = keep_doing(move || unsafe { Rxq::prepare_buffers(this) });
    }

    #[allow(dead_code)]
    fn wake_notifier_wait(&mut self) {
        self._ring.wake_notifier_wait();
    }

    /// # Safety
    /// `this` is the rxq's root pointer, with no reference to the rxq or its qp live.
    unsafe fn update_rx_count(this: *mut Rxq, c: u64) {
        // SAFETY: guaranteed by the caller; `_dev` is the qp's root pointer and the
        // statistics are reached by a place projection.
        unsafe {
            (*std::ptr::addr_of_mut!((*(*this)._dev).base._stats)).rx.good.update_pkts_bunch(c)
        };
    }

    /// # Safety
    /// As for `run`.
    unsafe fn prepare_buffers(this: *mut Rxq) -> Future<()> {
        // SAFETY: guaranteed by the caller; the reference ends with the call to `wait`.
        let f = unsafe { (*this)._ring.available_descriptors().wait(1) };
        f.then(move |()| {
            // SAFETY: see `run`; the continuation (in line or later) runs with no other
            // reference to the rxq live, through its root pointer.
            let me = unsafe { &mut *this };
            let mut count: u32 = 1;
            let available = me._ring.available_descriptors();
            let opportunistic = available.current();
            if available.try_wait(opportunistic) {
                count += opportunistic as u32;
            }
            let make_buffer_chain = || {
                let mut bc = SingleBuffer::default();
                // SAFETY: plain malloc of a 4096 byte buffer.
                let buf = MallocBuf::from_raw(unsafe { malloc(4096) } as *mut u8);
                let b = &mut bc.0[0];
                b.b.addr = virt_to_phys(buf.get());
                b.b.len = 4096;
                b.b.writeable = true;
                b.buf = buf;
                bc
            };
            me._ring.post((0..count).map(|_| make_buffer_chain()));
            crate::engine::core::future::make_ready_future(())
        })
    }

    fn debug_mode_adjust_fragments(&mut self) {
        #[cfg(debug_assertions)]
        {
            // For debug mode, reallocate last fragment to detect buffer overruns
            let last = *self
                ._fragments
                .last()
                .unwrap_or(&Fragment { base: std::ptr::null_mut(), size: 0 });
            let sz = last.size;
            // SAFETY: plain malloc of at least one byte; the result is checked for null below
            // and owned by the `MallocBuf`.
            let buf = MallocBuf::from_raw(unsafe { malloc(sz.max(1)) } as *mut u8);
            if buf.get().is_null() {
                std::alloc::handle_alloc_error(
                    std::alloc::Layout::array::<u8>(sz.max(1))
                        .unwrap_or(std::alloc::Layout::new::<u8>()),
                );
            }
            // SAFETY: `_fragments` is non-empty when this is called, so `last` is a real
            // fragment of `sz` readable bytes; the new buffer is a distinct allocation with room
            // for `sz` bytes (checked non-null above).
            unsafe { std::ptr::copy_nonoverlapping(last.base, buf.get(), sz) };
            if let Some(f) = self._fragments.last_mut() {
                *f = Fragment { base: buf.get(), size: sz };
            }
            if let Some(b) = self._buffers.last_mut() {
                *b = buf;
            }
        }
    }

    /// Handles a filled receive buffer of `len` bytes, through the rxq's root pointer:
    /// the packet goes up the stack in line (`l2receive`), and the ring that called the
    /// completion is part of this rxq, so no reference to the rxq or its qp is live across
    /// that call.
    ///
    /// # Safety
    /// `this` is the rxq's root pointer, with no reference to the rxq or its qp live.
    unsafe fn complete_buffer(this: *mut Rxq, bc: SingleBuffer, len: usize) {
        use std::ptr::addr_of_mut;
        let [sb] = bc.0;
        let buf = sb.buf;
        let mut frag_buf = buf.get();
        let mut frag_len = len;
        // SAFETY: the caller guarantees `this` is the rxq's root pointer with no reference to
        // the rxq or its qp live; `_dev` is the qp's root pointer, which
        // owns this rxq and outlives it, and every access is a short place access.
        unsafe {
            let dev = (*this)._dev;
            // First buffer
            if (*this)._remaining_buffers == 0 {
                // The device wrote a net_hdr_mrg at the start of the first buffer.
                let hdr = (frag_buf as *const NetHdrMrg).read_unaligned();
                assert!(hdr.num_buffers >= 1);
                (*this)._remaining_buffers = hdr.num_buffers as u32;
                let header_len = (*dev)._header_len;
                // The header is within the buffer.
                frag_buf = frag_buf.add(header_len);
                frag_len -= header_len;
                (*this)._fragments.clear();
                (*this)._buffers.clear();
            }

            // Append current buffer
            (*this)._fragments.push(Fragment { base: frag_buf, size: frag_len });
            (*this)._buffers.push(buf);
            (*this)._remaining_buffers -= 1;

            // Last buffer
            if (*this)._remaining_buffers == 0 {
                (*this).debug_mode_adjust_fragments();
                let del: Deleter = if (*this)._buffers.len() == 1 {
                    let b = (*this)._buffers.pop().unwrap_or_default();
                    make_free_deleter(b.release() as *mut c_void)
                } else {
                    make_object_deleter(std::mem::take(&mut (*this)._buffers))
                };
                let p = Packet::from_iter((*this)._fragments.iter(), del);

                (*addr_of_mut!((*dev).base._stats))
                    .rx
                    .good
                    .update_frags_stats(p.nr_frags() as u64, p.len() as u64);

                // The device outlives its queues; it is a separate object.
                let d: &dyn Device = &*(*dev)._dev;
                d.l2receive(p);

                let n = (*this)._fragments.len();
                (*addr_of_mut!((*this)._ring._available_descriptors)).signal(n);
            }
        }
    }
}

// Allocate and zero-initialize a buffer which is page-aligned and can be
// used for virt_to_phys (i.e., physically contiguous).
fn virtio_buffer(size: usize) -> MallocBuf {
    let mut ret: *mut c_void = std::ptr::null_mut();
    // SAFETY: plain posix_memalign.
    let r = unsafe { posix_memalign(&mut ret, 4096, size) };
    assert!(r == 0);
    // SAFETY: `ret` has `size` writable bytes.
    unsafe { std::ptr::write_bytes(ret as *mut u8, 0, size) };
    MallocBuf::from_raw(ret as *mut u8)
}

/// The queue pair of a virtio device: transmit and receive queues and their rings.
pub struct VirtioQp {
    /// The qp's address with the provenance of its node, written by `fix_up`; the rings,
    /// queues and `rx_start` derive their pointers from it.
    _this: *mut VirtioQp,
    base: QpBase,
    _dev: *const VirtioDevice,
    _header_len: usize,
    _txq_storage: MallocBuf,
    _rxq_storage: MallocBuf,
    _txq: Txq,
    _rxq: Rxq,
}

impl VirtioQp {
    fn vring_storage_size(ring_size: usize) -> usize {
        // overestimate, but not by much.
        3 * 4096 + ring_size * (16 + 2 + 8)
    }

    fn common_config(dev: &VirtioDevice, r: &mut RingConfig) {
        // SAFETY: the storage holds the descriptor table and both rings.
        unsafe {
            r.avail = r.descs.add(16 * r.size as usize);
            r.used = crate::engine::core::align::align_up_ptr(
                r.avail.add(2 * r.size as usize + 6),
                4096,
            );
        }
        r.event_index = (dev.features() & VIRTIO_RING_F_EVENT_IDX) != 0;
        r.indirect = false;
    }

    fn txq_config(dev: &VirtioDevice, storage: &MallocBuf, tx_ring_size: usize) -> RingConfig {
        let mut r = RingConfig {
            size: tx_ring_size as u32,
            descs: storage.get(),
            mergable_buffers: false,
            ..Default::default()
        };
        Self::common_config(dev, &mut r);
        r
    }

    fn rxq_config(dev: &VirtioDevice, storage: &MallocBuf, rx_ring_size: usize) -> RingConfig {
        let mut r = RingConfig {
            size: rx_ring_size as u32,
            descs: storage.get(),
            mergable_buffers: true,
            ..Default::default()
        };
        Self::common_config(dev, &mut r);
        r
    }

    /// Creates a queue pair with rings of the given sizes; `fix_up` sets
    /// the back-pointers once the object is in place.
    fn new(dev: *const VirtioDevice, rx_ring_size: usize, tx_ring_size: usize) -> Self {
        // SAFETY: the device outlives its queues.
        let d = unsafe { &*dev };
        let txq_storage = virtio_buffer(Self::vring_storage_size(tx_ring_size));
        let rxq_storage = virtio_buffer(Self::vring_storage_size(rx_ring_size));
        let txc = Self::txq_config(d, &txq_storage, tx_ring_size);
        let rxc = Self::rxq_config(d, &rxq_storage, rx_ring_size);
        VirtioQp {
            _this: std::ptr::null_mut(),
            base: QpBase::new_default(),
            _dev: dev,
            _header_len: 0,
            _txq_storage: txq_storage,
            _rxq_storage: rxq_storage,
            _txq: Txq::new(txc),
            _rxq: Rxq::new(std::ptr::null_mut(), rxc),
        }
    }

    /// Sets the back-pointers once the qp is in its node.
    ///
    /// # Safety
    /// `this` is the qp's root pointer (projected from the node that owns it, which never
    /// moves), with no reference to the qp live. Every pointer stored is projected from it.
    unsafe fn fix_up(this: *mut VirtioQp) {
        use std::ptr::addr_of_mut;
        // SAFETY: guaranteed by the caller; short accesses.
        unsafe {
            (*this)._this = this;
            (*this)._rxq._dev = this;
            let tq: *mut Txq = addr_of_mut!((*this)._txq);
            let rq: *mut Rxq = addr_of_mut!((*this)._rxq);
            (*this)._txq._ring.completion_mut().q = tq;
            (*this)._rxq._ring.completion_mut().q = rq;
            Vring::start_poller(addr_of_mut!((*tq)._ring));
            Vring::start_poller(addr_of_mut!((*rq)._ring));
        }
    }

    /// Starts the receive queue, through the qp's root pointer (see `Qp::rx_start_root`).
    ///
    /// # Safety
    /// `q` is the root pointer of a live virtio qp of type `Q`, with no reference to it live.
    unsafe fn rx_start_at<Q: AsVirtioQp>(q: *mut dyn Qp) {
        // SAFETY: guaranteed by the caller; `_this` was projected from the node.
        unsafe {
            let this = Q::virtio_qp(q as *mut Q);
            Rxq::run(std::ptr::addr_of_mut!((*this)._rxq));
        }
    }
}

impl Qp for VirtioQp {
    fn base(&self) -> &QpBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut QpBase {
        &mut self.base
    }
    fn send(&mut self, _p: Packet) -> Future<()> {
        std::process::abort();
    }
    fn send_batch(&mut self, p: &mut CircularBuffer<Packet>) -> u32 {
        // Disjoint borrows of the members the txq reads.
        let VirtioQp { _txq, _header_len, _dev, base, .. } = self;
        _txq.post(p, *_header_len, *_dev, &mut base._stats)
    }
    fn rx_start_root(&self) -> Option<unsafe fn(*mut dyn Qp)> {
        Some(VirtioQp::rx_start_at::<VirtioQp>)
    }
}

/// The virtio qp inside a qp type, projected from its root pointer.
trait AsVirtioQp {
    /// # Safety
    /// `this` is a live qp's root pointer.
    unsafe fn virtio_qp(this: *mut Self) -> *mut VirtioQp;
}

impl AsVirtioQp for VirtioQp {
    #[inline]
    unsafe fn virtio_qp(this: *mut Self) -> *mut VirtioQp {
        this
    }
}

impl AsVirtioQp for QpVhost {
    #[inline]
    unsafe fn virtio_qp(this: *mut Self) -> *mut VirtioQp {
        // SAFETY: guaranteed by the caller; a place projection, no reference.
        unsafe { std::ptr::addr_of_mut!((*this).qp) }
    }
}

/// The vhost `VHOST_SET_VRING_NUM`/`BASE` argument (Linux `vhost_vring_state`).
#[repr(C)]
struct VhostVringState {
    index: u32,
    num: u32,
}

/// The vhost `VHOST_SET_VRING_KICK`/`CALL` argument (Linux `vhost_vring_file`).
#[repr(C)]
struct VhostVringFile {
    index: u32,
    fd: i32,
}

/// The vhost `VHOST_SET_VRING_ADDR` argument (Linux `vhost_vring_addr`).
#[repr(C)]
struct VhostVringAddr {
    index: u32,
    flags: u32,
    desc_user_addr: u64,
    used_user_addr: u64,
    avail_user_addr: u64,
    log_guest_addr: u64,
}

/// A region of the vhost memory table (Linux `vhost_memory_region`).
#[repr(C)]
struct VhostMemoryRegion {
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
    flags_padding: u64,
}

/// The vhost `VHOST_SET_MEM_TABLE` argument with one region (Linux `vhost_memory`).
#[repr(C)]
struct VhostMemory1 {
    nregions: u32,
    padding: u32,
    regions: [VhostMemoryRegion; 1],
}

/// The `ifreq` argument of the tap ioctls (only the name and the flags of the union are
/// used).
#[repr(C)]
struct Ifreq {
    ifrn_name: [u8; 16],
    ifr_flags: i16,
    _pad: [u8; 22],
}

const VHOST_GET_FEATURES: u64 = 0x8008AF00;
const VHOST_SET_FEATURES: u64 = 0x4008AF00;
const VHOST_SET_OWNER: u64 = 0xAF01;
const VHOST_SET_MEM_TABLE: u64 = 0x4008AF03;
const VHOST_SET_VRING_NUM: u64 = 0x4008AF10;
const VHOST_SET_VRING_ADDR: u64 = 0x4028AF11;
const VHOST_SET_VRING_KICK: u64 = 0x4008AF20;
const VHOST_SET_VRING_CALL: u64 = 0x4008AF21;
const VHOST_NET_SET_BACKEND: u64 = 0x4008AF30;
const TUNSETIFF: u64 = 0x400454CA;
const TUNSETOFFLOAD: u64 = 0x400454D0;
const TUNSETVNETHDRSZ: u64 = 0x400454D8;
const IFF_TAP: i16 = 0x0002;
const IFF_NO_PI: i16 = 0x1000;
const IFF_ONE_QUEUE: i16 = 0x2000;
const IFF_VNET_HDR: i16 = 0x4000;
const TUN_F_CSUM: u32 = 0x01;
const TUN_F_TSO4: u32 = 0x02;
const TUN_F_UFO: u32 = 0x10;
const IFNAMSIZ: usize = 16;
const O_RDWR: i32 = 0o2;
const O_NONBLOCK: i32 = 0o4000;

/// The virtio queue pair backed by the vhost-net kernel driver and a tap device.
pub struct QpVhost {
    qp: VirtioQp,
    // The vhost file descriptor needs to remain open throughout the life of
    // this driver, as as soon as we close it, vhost stops servicing us.
    _vhost_fd: FileDesc,
}

fn config_ring_size(opts: &VirtioOptions) -> usize {
    if opts.event_index.as_bool() { *opts.virtio_ring_size.get_value() as usize } else { 256 }
}

impl QpVhost {
    /// Opens vhost-net and the tap device named in `opts`, sets up the rings and returns the
    /// queue pair.
    ///
    /// # Errors
    /// Fails if a device cannot be opened or an ioctl fails.
    ///
    /// The qp is built in its node (never moved) and every pointer it hands out is
    /// projected from the node's root pointer.
    ///
    /// # Safety
    /// `dev` is a live device that outlives its queues.
    pub unsafe fn new_node(
        dev: *const VirtioDevice,
        opts: &NativeStackOptions,
    ) -> Result<QpPtr, ExceptionPtr> {
        let vhost_fd = FileDesc::open("/dev/vhost-net", O_RDWR, 0)?;
        let raw: *mut dyn Qp = NodePtr::new(QpVhost {
            qp: VirtioQp::new(
                dev,
                config_ring_size(&opts.virtio_opts),
                config_ring_size(&opts.virtio_opts),
            ),
            _vhost_fd: vhost_fd,
        })
        .into_raw();
        let root = raw as *mut QpVhost;
        // SAFETY: `raw` came from the node's `into_raw`; the owner frees the qp on every path
        // below (an early `?` drops it).
        let node: QpPtr = unsafe { NodePtr::from_raw(raw) };
        // SAFETY: the root pointer of the qp in its node; nothing else refers to it yet.
        unsafe { VirtioQp::fix_up(std::ptr::addr_of_mut!((*root).qp)) };
        // SAFETY: the node's root pointer; the node never moves and no reference to it is live.
        unsafe { qp_init(raw, false) };
        // SAFETY: a reference for the rest of the setup: the pointers handed out above are
        // projected from the root, and none of them is used before `new_node` returns (pollers and
        // providers run from the reactor).
        let this = unsafe { &mut *root };

        let tap_device = opts.tap_device.get_value().clone();
        let mut vhost_supported_features: i64 = 0;
        // SAFETY: VHOST_GET_FEATURES writes a `__u64` of features into the 8 bytes.
        unsafe { this._vhost_fd.ioctl_data(VHOST_GET_FEATURES, &mut vhost_supported_features) }?;
        // SAFETY: the device outlives its queues.
        let d = unsafe { &*dev };
        vhost_supported_features &= d.features() as i64;
        // SAFETY: VHOST_SET_FEATURES reads a `__u64`.
        unsafe { this._vhost_fd.ioctl_data(VHOST_SET_FEATURES, &mut vhost_supported_features) }?;
        if vhost_supported_features as u64 & VIRTIO_NET_F_MRG_RXBUF != 0 {
            this.qp._header_len = std::mem::size_of::<NetHdrMrg>();
        } else {
            this.qp._header_len = std::mem::size_of::<NetHdr>();
        }

        // Open and set up the tap device, which we'll tell vhost to use.
        // Note that the tap_fd we open here will be closed at the end of
        // this function. It appears that this is fine - i.e., after we pass
        // this fd to VHOST_NET_SET_BACKEND, the Linux kernel keeps the reference
        // to it and it's fine to close the file descriptor.
        let tap_fd = FileDesc::open("/dev/net/tun", O_RDWR | O_NONBLOCK, 0)?;
        assert!(tap_device.len() < IFNAMSIZ);
        let mut ifr = Ifreq {
            ifrn_name: [0; 16],
            ifr_flags: IFF_TAP | IFF_NO_PI | IFF_ONE_QUEUE | IFF_VNET_HDR,
            _pad: [0; 22],
        };
        ifr.ifrn_name[..tap_device.len()].copy_from_slice(tap_device.as_bytes());
        // SAFETY: TUNSETIFF reads and writes a `struct ifreq`, which `Ifreq` lays out.
        unsafe { tap_fd.ioctl_data(TUNSETIFF, &mut ifr) }?;
        let mut offload: u32 = 0;
        let hw_features = d.hw_features();
        if hw_features.tx_csum_l4_offload && hw_features.rx_csum_offload {
            offload = TUN_F_CSUM;
            if hw_features.tx_tso {
                offload |= TUN_F_TSO4;
            }
            if hw_features.tx_ufo {
                offload |= TUN_F_UFO;
            }
        }
        // The ioctl takes the offload flags by value.
        tap_fd.ioctl_int(TUNSETOFFLOAD, offload as i32)?;
        // The ioctl takes a pointer to the header length; the kernel reads an int from it.
        // SAFETY: TUNSETVNETHDRSZ reads an `int`, within the 8 bytes of the usize.
        unsafe { tap_fd.ioctl_data(TUNSETVNETHDRSZ, &mut this.qp._header_len) }?;

        // Additional vhost setup:
        // The ioctl takes no argument (0).
        this._vhost_fd.ioctl_int(VHOST_SET_OWNER, 0)?;
        let mut mem_table = VhostMemory1 {
            nregions: 1,
            padding: 0,
            regions: [VhostMemoryRegion {
                guest_phys_addr: 0,
                memory_size: (1u64 << 47) - 4096,
                userspace_addr: 0,
                flags_padding: 0,
            }],
        };
        // SAFETY: VHOST_SET_MEM_TABLE reads a `vhost_memory` with `nregions` (1) regions,
        // which `VhostMemory1` lays out.
        unsafe { this._vhost_fd.ioctl_data(VHOST_SET_MEM_TABLE, &mut mem_table) }?;
        let mut vvs0 = VhostVringState { index: 0, num: this.qp._rxq.getconfig().size };
        // SAFETY: VHOST_SET_VRING_NUM reads a `vhost_vring_state`.
        unsafe { this._vhost_fd.ioctl_data(VHOST_SET_VRING_NUM, &mut vvs0) }?;
        let mut vvs1 = VhostVringState { index: 1, num: this.qp._txq.getconfig().size };
        // SAFETY: as above.
        unsafe { this._vhost_fd.ioctl_data(VHOST_SET_VRING_NUM, &mut vvs1) }?;
        let tov = |x: *mut u8| x as usize as u64;

        let rc = *this.qp._rxq.getconfig();
        let tc = *this.qp._txq.getconfig();
        let mut addr0 = VhostVringAddr {
            index: 0,
            flags: 0,
            desc_user_addr: tov(rc.descs),
            used_user_addr: tov(rc.used),
            avail_user_addr: tov(rc.avail),
            log_guest_addr: 0,
        };
        // SAFETY: VHOST_SET_VRING_ADDR reads a `vhost_vring_addr`.
        unsafe { this._vhost_fd.ioctl_data(VHOST_SET_VRING_ADDR, &mut addr0) }?;
        let mut addr1 = VhostVringAddr {
            index: 1,
            flags: 0,
            desc_user_addr: tov(tc.descs),
            used_user_addr: tov(tc.used),
            avail_user_addr: tov(tc.avail),
            log_guest_addr: 0,
        };
        // SAFETY: as above.
        unsafe { this._vhost_fd.ioctl_data(VHOST_SET_VRING_ADDR, &mut addr1) }?;

        let txq_notify = ReadableEventfd::new(0)?;
        let txq_kick = WriteableEventfd::new(0)?;
        let rxq_notify = ReadableEventfd::new(0)?;
        let rxq_kick = WriteableEventfd::new(0)?;
        let mut files = [
            (VHOST_SET_VRING_KICK, VhostVringFile { index: 0, fd: rxq_kick.get_read_fd() }),
            (VHOST_SET_VRING_CALL, VhostVringFile { index: 0, fd: rxq_notify.get_write_fd() }),
            (VHOST_SET_VRING_KICK, VhostVringFile { index: 1, fd: txq_kick.get_read_fd() }),
            (VHOST_SET_VRING_CALL, VhostVringFile { index: 1, fd: txq_notify.get_write_fd() }),
        ];
        for (request, file) in files.iter_mut() {
            // SAFETY: VHOST_SET_VRING_KICK and VHOST_SET_VRING_CALL read a `vhost_vring_file`.
            unsafe { this._vhost_fd.ioctl_data(*request, file) }?;
        }
        this.qp._rxq.set_notifier(Box::new(NotifierVhost::new(rxq_kick)));
        this.qp._txq.set_notifier(Box::new(NotifierVhost::new(txq_kick)));

        for index in 0..2 {
            let mut file = VhostVringFile { index, fd: tap_fd.get() };
            // SAFETY: VHOST_NET_SET_BACKEND reads a `vhost_vring_file`.
            unsafe { this._vhost_fd.ioctl_data(VHOST_NET_SET_BACKEND, &mut file) }?;
        }
        // The notify eventfds are closed here.
        drop((txq_notify, rxq_notify));
        Ok(node)
    }
}

impl Qp for QpVhost {
    fn base(&self) -> &QpBase {
        &self.qp.base
    }
    fn base_mut(&mut self) -> &mut QpBase {
        &mut self.qp.base
    }
    fn send(&mut self, p: Packet) -> Future<()> {
        self.qp.send(p)
    }
    fn send_batch(&mut self, p: &mut CircularBuffer<Packet>) -> u32 {
        self.qp.send_batch(p)
    }
    fn rx_start_root(&self) -> Option<unsafe fn(*mut dyn Qp)> {
        Some(VirtioQp::rx_start_at::<QpVhost>)
    }
}

// }

impl Default for VirtioOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioOptions {
    /// The options with their defaults, not yet linked to a group (see
    /// [`VirtioOptions::link`]).
    pub fn new() -> Self {
        VirtioOptions {
            group: OptionGroup::new("Virtio net options"),
            event_index: Value::new(
                "event-index",
                Some("on".to_string()),
                "Enable event-index feature (on / off)",
            ),
            csum_offload: Value::new(
                "csum-offload",
                Some("on".to_string()),
                "Enable checksum offload feature (on / off)",
            ),
            tso: Value::new(
                "tso",
                Some("on".to_string()),
                "Enable TCP segment offload feature (on / off)",
            ),
            ufo: Value::new(
                "ufo",
                Some("on".to_string()),
                "Enable UDP fragmentation offload feature (on / off)",
            ),
            virtio_ring_size: Value::new(
                "virtio-ring-size",
                Some(256u32),
                "Virtio ring size (must be power-of-two)",
            ),
        }
    }

    /// Links the group under `parent_group` and registers every value with it, in place,
    /// in declaration order.
    pub fn link(self: Pin<&mut Self>, parent_group: Option<Pin<&mut OptionGroup>>) {
        crate::link_option_group!(
            self,
            parent_group,
            group,
            event_index,
            csum_offload,
            tso,
            ufo,
            virtio_ring_size
        );
    }
}

/// The virtio-net device described by `opts`.
pub fn create_virtio_net_device(
    opts: &VirtioOptions,
    lro: &Value<String>,
) -> Result<Box<dyn Device>, ExceptionPtr> {
    Ok(Box::new(VirtioDevice::new(opts, lro)))
}
