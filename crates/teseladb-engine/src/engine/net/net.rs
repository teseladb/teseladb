//! The link layer of the native stack: devices, queues, interfaces and L3 protocol
//! registration.
//!
//! - A [`Device`] is a NIC (virtio, DPDK, ...) shared by all shards. It has one queue per
//!   shard, registered with `<dyn Device>::set_local_queue`, and spreads received packets
//!   over the shards with a hash (hardware RSS or Toeplitz) and an optional software
//!   redirection table.
//! - A [`Qp`] is one shard's queue pair. [`QpBase`] holds its common state: the transmit
//!   queue that a poller refills from the registered packet providers and drains through
//!   [`Qp::send_batch`], the receive stream, the proxy queue of packets sent on behalf of
//!   other shards, and the per-queue metrics.
//! - An [`Interface`] binds a device to the shard's protocols: it dispatches each received
//!   Ethernet frame to the [`L3Protocol`] registered for its EtherType, forwarding it to
//!   another shard when the hash says so, and prepends the Ethernet header to the packets
//!   those protocols send.
//!
//! These objects refer to one another through raw pointers and their callbacks hold a
//! pointer to their owner. They are heap-allocated in a [`NodePtr`] and never move after
//! construction, which is what keeps those pointers valid; methods that would re-enter the
//! object take its root pointer instead of `&mut self`.

use std::cell::{Cell, UnsafeCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, c_void};
use std::pin::Pin;
use std::sync::Arc;

use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::future::InvalidArgument;
use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::internal::poll::Poller;
use crate::engine::core::metrics::{self as sm, MetricGroups};
use crate::engine::core::reactor::internal::at_destroy;
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::smp::Smp;
use crate::engine::core::stream::Stream;
use crate::engine::net::byteorder::{hton_struct, ntoh, ntoh_struct};
use crate::engine::net::const_::{ETH_HDR_LEN, EthProtocolNum, IP_PACKET_LEN_MAX};
use crate::engine::net::ethernet::{EthHdr, EthernetAddress};
use crate::engine::net::hash::FxBuildHasher;
use crate::engine::net::inet_address::InetAddress;
use crate::engine::net::native_stack::NativeStackOptions;
use crate::engine::net::node_ptr::{CrossShard, NodePtr};
use crate::engine::net::packet::Packet;
use crate::engine::net::socket_defs::{AF_INET, Ipv4Addr, SocketAddress, in_addr, inet_pton};
use crate::engine::net::toeplitz::{
    DEFAULT_RSSKEY_40BYTES, RssKeyType, ToeplitzData, toeplitz_hash,
};

/// The bytes a protocol extracts from a packet (addresses, ports) to compute its Toeplitz
/// hash when the NIC gave none; at most 64 bytes, multi-byte values pushed low byte first.
#[derive(Clone, Copy)]
pub struct ForwardHash {
    data: [u8; 64],
    end_idx: usize,
}

impl Default for ForwardHash {
    fn default() -> Self {
        ForwardHash { data: [0; 64], end_idx: 0 }
    }
}

impl ForwardHash {
    /// The number of bytes pushed.
    pub fn size(&self) -> usize {
        self.end_idx
    }
    /// Appends a byte.
    ///
    /// # Panics
    ///
    /// If 64 bytes were already pushed.
    pub fn push_back_u8(&mut self, b: u8) {
        assert!(self.end_idx < self.data.len());
        self.data[self.end_idx] = b;
        self.end_idx += 1;
    }
    /// Appends the two bytes of `b`, low byte first.
    ///
    /// # Panics
    ///
    /// If the buffer overflows.
    pub fn push_back_u16(&mut self, b: u16) {
        self.push_back_u8(b as u8);
        self.push_back_u8((b >> 8) as u8);
    }
    /// Appends the four bytes of `b`, low byte first.
    ///
    /// # Panics
    ///
    /// If the buffer overflows.
    pub fn push_back_u32(&mut self, b: u32) {
        self.push_back_u16(b as u16);
        self.push_back_u16((b >> 16) as u16);
    }
}

impl std::ops::Index<usize> for ForwardHash {
    type Output = u8;
    fn index(&self, idx: usize) -> &u8 {
        &self.data[idx]
    }
}

impl ToeplitzData for ForwardHash {
    fn size(&self) -> usize {
        self.end_idx
    }
    fn at(&self, i: usize) -> u8 {
        self.data[i]
    }
}

/// The offloads and limits a device supports.
#[derive(Clone, Copy, Debug)]
pub struct HwFeatures {
    /// Transmit IP header checksum offload.
    pub tx_csum_ip_offload: bool,
    /// Transmit L4 (TCP or UDP) checksum offload.
    pub tx_csum_l4_offload: bool,
    /// Receive checksum offload.
    pub rx_csum_offload: bool,
    /// Large receive offload is enabled.
    pub rx_lro: bool,
    /// Transmit TCP segmentation offload.
    pub tx_tso: bool,
    /// Transmit UDP fragmentation offload.
    pub tx_ufo: bool,
    /// Maximum transmission unit.
    pub mtu: u16,
    /// Maximum packet length when TCP/UDP offload is enabled.
    pub max_packet_len: u16,
}

/// No offloads, an MTU of 1500, and the largest IP packet as `max_packet_len`.
impl Default for HwFeatures {
    fn default() -> Self {
        HwFeatures {
            tx_csum_ip_offload: false,
            tx_csum_l4_offload: false,
            rx_csum_offload: false,
            rx_lro: false,
            tx_tso: false,
            tx_ufo: false,
            mtu: 1500,
            max_packet_len: IP_PACKET_LEN_MAX - ETH_HDR_LEN as u16,
        }
    }
}

/// A packet an L3 protocol hands to the interface for sending.
pub struct L3Packet {
    /// The EtherType of the Ethernet header to prepend.
    pub proto_num: EthProtocolNum,
    /// The destination MAC address.
    pub to: EthernetAddress,
    /// The L3 packet (without Ethernet header).
    pub p: Packet,
}

/// A source of packets to send, polled by the interface; `None` when it has nothing.
pub type L3PacketProviderType = Box<dyn FnMut() -> Option<L3Packet>>;

/// The receive function of an L3 protocol: a packet (Ethernet header removed) and its source
/// MAC address.
pub type RxFn = Box<dyn FnMut(Packet, EthernetAddress) -> Future<()>>;
/// Fills a [`ForwardHash`] from the packet (whose L3 header is at the given offset) to pick
/// the shard that owns it; returns `false` when the packet carries no flow to hash.
pub type ForwardFn = Box<dyn FnMut(&mut ForwardHash, &mut Packet, usize) -> bool>;

/// The registration of an L3 protocol (one EtherType) on an [`Interface`].
pub struct L3Protocol {
    _netif: *mut Interface,
    _proto_num: EthProtocolNum,
}

struct L3RxStream {
    packet_stream: Stream<(Packet, EthernetAddress)>,
    ready: Future<()>,
    forward: ForwardFn,
}

impl L3RxStream {
    /// A receive stream with its forward function, built directly in the map node where it
    /// stays, its `ready` future taken from the stream's `started()`: `started()` and the
    /// subscription need the stream pinned.
    fn new_node(fw: ForwardFn) -> NodePtr<L3RxStream> {
        let node = NodePtr::new(L3RxStream {
            packet_stream: Stream::<(Packet, EthernetAddress)>::new(),
            ready: make_ready_future(()),
            forward: fw,
        });
        let p = node.as_ptr();
        // SAFETY: the node was just made and nothing else refers to it; the stream does not
        // move out of the node, which the map frees only when the interface is destroyed.
        unsafe {
            (*p).ready = Pin::new_unchecked(&mut (*p).packet_stream).started();
        }
        node
    }
}

/// The link layer of one shard: a device plus the L3 protocols registered on it.
pub struct Interface {
    /// The receive stream of each EtherType, each in its own node so that its address is
    /// stable while the map changes.
    _proto_map: HashMap<u16, NodePtr<L3RxStream>, FxBuildHasher>,
    _dev: Arc<dyn Device>,
    _hw_address: EthernetAddress,
    _hw_features: HwFeatures,
    _pkt_providers: Vec<L3PacketProviderType>,
}

impl Interface {
    /// The device's MAC address.
    pub fn hw_address(&self) -> EthernetAddress {
        self._hw_address
    }
    /// The device's offloads and limits.
    pub fn hw_features(&self) -> &HwFeatures {
        &self._hw_features
    }
    /// Adds a source of packets to send; the interface polls its providers round-robin.
    pub fn register_packet_provider(&mut self, func: L3PacketProviderType) {
        self._pkt_providers.push(func);
    }
}

/// Counters of the packets a queue handled successfully, in one direction.
#[derive(Clone, Copy, Debug, Default)]
pub struct QpStatsGood {
    /// Total number of bytes.
    pub bytes: u64,
    /// Total number of fragments.
    pub nr_frags: u64,
    /// Fragments that were copied at L2.
    pub copy_frags: u64,
    /// Bytes that were copied at L2.
    pub copy_bytes: u64,
    /// Total number of packets.
    pub packets: u64,
    /// Number of packets in the last sent or received bunch.
    pub last_bunch: u64,
}

impl QpStatsGood {
    /// Records a bunch of `count` packets: sets the last bunch size and adds to the packet
    /// total.
    pub fn update_pkts_bunch(&mut self, count: u64) {
        self.last_bunch = count;
        self.packets += count;
    }

    /// Records `nr_frags` fragments of `bytes` bytes that were copied rather than handled
    /// zero-copy.
    pub fn update_copy_stats(&mut self, nr_frags: u64, bytes: u64) {
        self.copy_frags += nr_frags;
        self.copy_bytes += bytes;
    }

    /// Records `nfrags` processed fragments of `nbytes` bytes in total.
    pub fn update_frags_stats(&mut self, nfrags: u64, nbytes: u64) {
        self.nr_frags += nfrags;
        self.bytes += nbytes;
    }
}

/// Counters of the received packets a queue dropped.
#[derive(Clone, Copy, Debug, Default)]
pub struct QpStatsRxBad {
    /// Packets dropped because of an allocation failure.
    pub no_mem: u64,
    /// Total number of erroneous packets.
    pub total: u64,
    /// Packets with a bad checksum.
    pub csum: u64,
}

impl QpStatsRxBad {
    /// Counts a packet with a bad checksum.
    pub fn inc_csum_err(&mut self) {
        self.csum += 1;
        self.total += 1;
    }

    /// Counts a packet dropped for lack of memory.
    pub fn inc_no_mem(&mut self) {
        self.no_mem += 1;
        self.total += 1;
    }
}

/// Receive counters of a queue.
#[derive(Clone, Copy, Debug, Default)]
pub struct QpStatsRx {
    /// Packets received.
    pub good: QpStatsGood,
    /// Packets dropped.
    pub bad: QpStatsRxBad,
}

/// Transmit counters of a queue.
#[derive(Clone, Copy, Debug, Default)]
pub struct QpStatsTx {
    /// Packets sent.
    pub good: QpStatsGood,
    /// Number of packets that were linearized (copied into one buffer) before sending.
    pub linearized: u64,
}

/// The counters of a queue, exported as its metrics.
#[derive(Clone, Copy, Debug, Default)]
pub struct QpStats {
    /// Receive side.
    pub rx: QpStatsRx,
    /// Transmit side.
    pub tx: QpStatsTx,
}

/// A source of packets for a queue to send; `None` when it has nothing.
pub type QpPacketProviderType = Box<dyn FnMut() -> Option<Packet>>;

/// The state common to every queue pair: packet providers, software redirection table,
/// proxy and transmit queues, receive stream, transmit poller and metrics.
pub struct QpBase {
    _pkt_providers: Vec<QpPacketProviderType>,
    _sw_reta: Option<[u8; 128]>,
    _proxy_packetq: CircularBuffer<Packet>,
    _rx_stream: Stream<Packet>,
    _tx_poller: Option<Box<Poller>>,
    _tx_packetq: CircularBuffer<Packet>,

    /// The metric group name of the queue's metrics.
    pub _stats_plugin_name: String,
    /// `queue<N>`, the prefix of the queue's metric names.
    pub _queue_name: String,
    /// The registered metrics.
    pub _metrics: MetricGroups,
    /// The counters the metrics read.
    pub _stats: QpStats,
}

/// A device queue pair owned by one shard. Implementations provide the device-specific
/// send and receive start; the shared behaviour is in `impl dyn Qp` and in [`QpBase`].
pub trait Qp {
    /// The common state.
    fn base(&self) -> &QpBase;
    /// The common state, writable.
    fn base_mut(&mut self) -> &mut QpBase;

    /// Sends one packet.
    fn send(&mut self, p: Packet) -> Future<()>;
    /// Sends packets from the front of `p`, removing those it takes; returns how many it
    /// sent. The default sends them all, one by one, discarding each send's future.
    fn send_batch(&mut self, p: &mut CircularBuffer<Packet>) -> u32 {
        let mut sent: u32 = 0;
        while !p.empty() {
            // FIXME: future is discarded
            // SAFETY: the loop condition says the buffer is not empty.
            let front = unsafe { p.take_front().unwrap_unchecked() };
            let _ = self.send(front);
            sent += 1;
        }
        sent
    }
    /// Starts receiving; the default does nothing.
    fn rx_start(&mut self) {}
    /// A qp whose receive start runs work in line that later goes back through its root
    /// pointer (virtio's receive loop) returns the function that does it from that pointer,
    /// so that no `&mut` to the qp is live meanwhile. `None` (the default) means
    /// [`Qp::rx_start`] is called instead.
    fn rx_start_root(&self) -> Option<unsafe fn(*mut dyn Qp)> {
        None
    }
}

/// The owner of a queue. A [`NodePtr`], not a `Box`, because the
/// pollers, providers and metrics a qp registers point into it, and a `Box` reasserts its
/// uniqueness each time it moves (from the device to `set_local_queue`, then to
/// `at_destroy`), which would invalidate them.
pub type QpPtr = NodePtr<dyn Qp>;

/// The `QpBase` of the qp at `this`, projected from `this` so that the pointer keeps the
/// qp's root provenance (a pointer from `base_mut()` keeps that of a temporary `&mut`).
///
/// # Safety
/// `this` is the root pointer of a live qp, with no reference to it live.
#[inline]
pub unsafe fn qp_base_ptr(this: *mut dyn Qp) -> *mut QpBase {
    // SAFETY: guaranteed by the caller. The temporary `&mut` only gives the base's address,
    // which lies inside the qp's allocation; the result is `this` moved by that offset.
    unsafe {
        let base = (*this).base_mut() as *mut QpBase as *mut u8;
        let off = base.offset_from(this as *mut u8);
        (this as *mut u8).offset(off).cast::<QpBase>()
    }
}

impl dyn Qp {
    /// Makes this queue send packets on behalf of the shards in `cpu_weights` (target shard
    /// to weight): registers a provider that drains the proxy queue and builds the software
    /// redirection table. Nothing is done when the only target is the current shard. Takes
    /// the qp's root pointer because the provider keeps a pointer to the qp's base.
    ///
    /// # Panics
    ///
    /// If `cpu_weights` is empty.
    ///
    /// # Safety
    /// `this` is the root pointer of a live qp that never moves, with no reference to it
    /// live.
    pub unsafe fn configure_proxies(this: *mut dyn Qp, cpu_weights: &BTreeMap<u32, f32>) {
        // SAFETY: guaranteed by the caller.
        unsafe { qp_configure_proxies(this, cpu_weights) }
    }
    /// Builds the software redirection table from `cpu_weights` (target shard to weight);
    /// see [`QpBase::build_sw_reta`].
    pub fn build_sw_reta(&mut self, cpu_weights: &BTreeMap<u32, f32>) {
        self.base_mut().build_sw_reta(cpu_weights)
    }
    /// Queues `p`, sent by another shard, for this queue to transmit.
    pub fn proxy_send(&mut self, p: Packet) {
        self.base_mut()._proxy_packetq.push_back(p);
    }
    /// Adds a source of packets to send.
    pub fn register_packet_provider(&mut self, func: QpPacketProviderType) {
        self.base_mut()._pkt_providers.push(func);
    }
    /// One transmit poll: refills the transmit queue from the providers (up to 128 packets)
    /// when it holds fewer than 16, then hands it to [`Qp::send_batch`]. Returns whether
    /// there was anything to send. Takes the qp's root pointer because the providers it calls
    /// go back into the qp (the proxy provider takes from the proxy queue).
    ///
    /// # Safety
    /// `this` is the root pointer of a live qp, with no reference to it live.
    pub unsafe fn poll_tx(this: *mut dyn Qp) -> bool {
        // SAFETY: guaranteed by the caller.
        unsafe { qp_poll_tx(this) }
    }
}

/// The state common to every device: the per-shard queue table and the RSS table width.
pub struct DeviceBase {
    _queues: QueueSlots,
    /// The number of low hash bits the hardware redirection table uses; the software table
    /// is indexed with the bits above them.
    pub _rss_table_bits: Cell<usize>,
}

/// The queue of each shard, as the qp's root pointer: one slot per shard, each written and
/// read only by its own shard.
pub struct QueueSlots(Box<[UnsafeCell<Option<*mut dyn Qp>>]>);

// SAFETY: each slot is written only by the shard whose id indexes it (`set_local_queue`),
// and read by that shard, or by another shard only after the owner registered it during
// device setup (`queue_for_cpu`, the proxy); no slot is written and read concurrently.
unsafe impl Sync for QueueSlots {}
// SAFETY: see above.
unsafe impl Send for QueueSlots {}

impl QueueSlots {
    fn get(&self, cpu: u32) -> Option<*mut dyn Qp> {
        // SAFETY: see the Sync impl above.
        unsafe { *self.0[cpu as usize].get() }
    }
    fn set(&self, cpu: u32, q: *mut dyn Qp) {
        // SAFETY: see the Sync impl above.
        unsafe { *self.0[cpu as usize].get() = Some(q) };
    }
}

impl Default for DeviceBase {
    fn default() -> Self {
        let n = Smp::count() as usize;
        let slots: Vec<UnsafeCell<Option<*mut dyn Qp>>> =
            (0..n).map(|_| UnsafeCell::new(None)).collect();
        DeviceBase { _queues: QueueSlots(slots.into_boxed_slice()), _rss_table_bits: Cell::new(0) }
    }
}

// SAFETY: _rss_table_bits is written once during device setup, before the shards use it.
unsafe impl Sync for DeviceBase {}
// SAFETY: see above.
unsafe impl Send for DeviceBase {}

/// A network device shared by all shards. Implementations provide the hardware specifics;
/// the shared behaviour is in `impl dyn Device`.
pub trait Device: Send + Sync {
    /// The common state.
    fn base(&self) -> &DeviceBase;

    /// The MAC address.
    fn hw_address(&self) -> EthernetAddress;
    /// The offloads and limits.
    fn hw_features(&self) -> HwFeatures;
    /// The RSS key; the default is the standard 40-byte Toeplitz key.
    fn rss_key(&self) -> RssKeyType {
        DEFAULT_RSSKEY_40BYTES
    }
    /// The number of hardware queues; the default is 1.
    fn hw_queues_count(&self) -> u16 {
        1
    }
    /// Resolves when the link is up; the default is ready at once.
    fn link_ready(&self) -> Future<()> {
        make_ready_future(())
    }
    /// Creates the queue `qid` for the current shard.
    ///
    /// # Errors
    ///
    /// When the queue cannot be created.
    fn init_local_queue(&self, opts: &NativeStackOptions, qid: u16) -> Result<QpPtr, ExceptionPtr>;
    /// The hardware queue a hash maps to; the default is `hash % hw_queues_count()`.
    fn hash2qid(&self, hash: u32) -> u32 {
        hash % self.hw_queues_count() as u32
    }
    /// The shard that owns the flow of `hash`: its hardware queue, then the software
    /// redirection table of that queue, if any.
    fn hash2cpu(&self, hash: u32) -> u32 {
        // there is an assumption here that qid == cpu_id which will
        // not necessary be true in the future
        device_forward_dst(self.base(), self.hash2qid(hash), || hash)
    }
}

/// The shard a packet received on the queue of `src_cpuid` belongs to: `src_cpuid` itself
/// when that queue has no software redirection table, otherwise the table entry for the
/// packet's hash (computed by `hashfn` only in that case).
///
/// # Panics
///
/// If no queue is registered for `src_cpuid`.
pub fn device_forward_dst<F: FnOnce() -> u32>(base: &DeviceBase, src_cpuid: u32, hashfn: F) -> u32 {
    let q = base._queues.get(src_cpuid).expect("queue for cpu");
    // SAFETY: the queue was registered with set_local_queue and lives until at_destroy.
    let qp = unsafe { &*q };
    let Some(reta) = qp.base()._sw_reta.as_ref() else {
        return src_cpuid;
    };
    let hash = hashfn() >> base._rss_table_bits.get();
    reta[hash as usize % reta.len()] as u32
}

impl dyn Device {
    /// The queue of shard `cpu`, as the qp's root pointer. A pointer, because the queue is
    /// shared by the device (on every shard) and the qp's own callbacks: callers make short
    /// accesses only, and only from the shard that owns the queue, except the proxy (see
    /// [`proxy`](crate::engine::net::proxy)).
    ///
    /// # Panics
    /// If no queue was registered for `cpu`.
    pub fn queue_for_cpu(&self, cpu: u32) -> *mut dyn Qp {
        let q = self.base()._queues.get(cpu);
        assert!(q.is_some(), "no queue registered for cpu {cpu}");
        // SAFETY: just checked.
        unsafe { q.unwrap_unchecked() }
    }
    /// The current shard's queue, as a pointer (see `<dyn Device>::queue_for_cpu`).
    pub fn local_queue(&self) -> *mut dyn Qp {
        self.queue_for_cpu(this_shard_id())
    }
    /// Delivers `p` to the current shard's receive stream (used for packets forwarded from
    /// another shard). The delivery future is discarded.
    pub fn l2receive(&self, p: Packet) {
        // SAFETY: the local queue lives in its node until at_destroy, which outlives the
        // returned future; a short access on its own shard, with the stream pinned there.
        // FIXME: future is discarded
        let _ = unsafe {
            Pin::new_unchecked(&mut (*self.queue_for_cpu(this_shard_id())).base_mut()._rx_stream)
                .produce(p)
        };
    }
    /// See [`device_forward_dst`].
    pub fn forward_dst<F: FnOnce() -> u32>(&self, src_cpuid: u32, hashfn: F) -> u32 {
        device_forward_dst(self.base(), src_cpuid, hashfn)
    }
}

/// Parses a dotted-quad IPv4 address into its host-order integer.
fn make_address_v4(addr: &str) -> Result<u32, ExceptionPtr> {
    let err = || ExceptionPtr::from(InvalidArgument(format!("invalid address: {}", addr)));
    let c = CString::new(addr).map_err(|_| err())?;
    let mut in_ = in_addr::default();
    // SAFETY: `c` is a valid C string and `in_` has room for an in_addr.
    if unsafe { inet_pton(AF_INET as i32, c.as_ptr(), &mut in_ as *mut in_addr as *mut c_void) }
        != 1
    {
        return Err(err());
    }
    Ok(u32::from_be(in_.s_addr))
}

impl Ipv4Addr {
    /// Parses `a.b.c.d` (port 0) or `a.b.c.d:port`.
    ///
    /// # Errors
    ///
    /// An `InvalidArgument` if the address or the port is invalid, or if there is more than
    /// one `:`.
    pub fn from_string(addr: &str) -> Result<Self, ExceptionPtr> {
        let items: Vec<&str> = addr.split(':').collect();
        if items.len() == 1 {
            Ok(Ipv4Addr { ip: make_address_v4(addr)?, port: 0 })
        } else if items.len() == 2 {
            let ip = make_address_v4(items[0])?;
            let port = items[1]
                .trim_start()
                .parse::<u64>()
                .map_err(|_| ExceptionPtr::from(InvalidArgument("stoul".to_string())))?;
            Ok(Ipv4Addr { ip, port: port as u16 })
        } else {
            Err(ExceptionPtr::from(InvalidArgument(format!("invalid format: {}", addr))))
        }
    }

    /// Parses the dotted-quad address `addr` and pairs it with `port_`.
    ///
    /// # Errors
    ///
    /// An `InvalidArgument` if `addr` is not a valid IPv4 address.
    pub fn from_string_port(addr: &str, port_: u16) -> Result<Self, ExceptionPtr> {
        Ok(Ipv4Addr { ip: make_address_v4(addr)?, port: port_ })
    }

    /// The IPv4 address `a` (or the IPv4 part of an IPv4-mapped address) with `port`.
    ///
    /// # Errors
    ///
    /// An `InvalidArgument` if `a` is an IPv6 address that is not IPv4-mapped.
    pub fn from_inet_address(a: &InetAddress, port: u16) -> Result<Self, ExceptionPtr> {
        Ok(Self::from_in_addr(&a.to_in_addr()?, port))
    }

    /// The address and port of `sa`; `0.0.0.0:0` if `sa` is not an IPv4 (or IPv4-mapped)
    /// address.
    pub fn from_socket_address(sa: &SocketAddress) -> Self {
        Self::from_inet_address(&sa.addr(), sa.port()).unwrap_or_default()
    }

    /// The address `in_` (network order) with port `p`.
    pub fn from_in_addr(in_: &in_addr, p: u16) -> Self {
        Ipv4Addr { ip: ntoh(in_.s_addr), port: p }
    }
}

/// The body of `<dyn Qp>::poll_tx`.
///
/// # Safety
/// As for `<dyn Qp>::poll_tx`. No reference to the qp is live while a provider runs: each
/// access is a short place access through the base projected from `this`.
unsafe fn qp_poll_tx(this: *mut dyn Qp) -> bool {
    // SAFETY: the caller upholds this function's `# Safety` contract (as for
    // `<dyn Qp>::poll_tx`): no reference to the qp is live while a provider runs, and each
    // access is a short place access through the base `b`, projected from the root pointer.
    unsafe {
        let b: *mut QpBase = qp_base_ptr(this);
        if (*b)._tx_packetq.size() < 16 {
            // refill send queue from upper layers
            let mut work: u32;
            loop {
                work = 0;
                // The providers are registered at construction, so the vector does not change
                // while one runs; each is called through a pointer to its slot.
                let n = (*b)._pkt_providers.len();
                let providers: *mut QpPacketProviderType = (*b)._pkt_providers.as_mut_ptr();
                for i in 0..n {
                    let p = (*providers.add(i))();
                    if let Some(p) = p {
                        work += 1;
                        (*b)._tx_packetq.push_back(p);
                        if (*b)._tx_packetq.size() == 128 {
                            break;
                        }
                    }
                }
                if !(work != 0 && (*b)._tx_packetq.size() < 128) {
                    break;
                }
            }
        }
        if !(*b)._tx_packetq.empty() {
            // `send(_tx_packetq)` borrows the whole qp, so the queue is taken out of it for the
            // call (no allocation: an empty circular buffer owns no storage).
            let mut q = std::mem::take(&mut (*b)._tx_packetq);
            let sent = (*this).send_batch(&mut q);
            // packets not taken by send() stay queued, in front of any queued meanwhile
            while let Some(p) = (*b)._tx_packetq.take_front() {
                q.push_back(p);
            }
            (*b)._tx_packetq = q;
            (*b)._stats.tx.good.update_pkts_bunch(sent as u64);
            return true;
        }

        false
    }
}

impl QpBase {
    /// The initial state of queue `qid`, whose metrics go in the group `stats_plugin_name`.
    /// [`qp_init`] finishes the construction once the qp has its final address, because the
    /// poller and the metrics it registers point into the qp.
    pub fn new(stats_plugin_name: &str, qid: u8) -> Self {
        QpBase {
            _pkt_providers: Vec::new(),
            _sw_reta: None,
            _proxy_packetq: CircularBuffer::new(),
            _rx_stream: Stream::<Packet>::new(),
            _tx_poller: None,
            _tx_packetq: CircularBuffer::new(),
            _stats_plugin_name: stats_plugin_name.to_string(),
            _queue_name: format!("queue{}", qid),
            _metrics: MetricGroups::new(),
            _stats: QpStats::default(),
        }
    }

    /// The initial state of queue 0 in the `network` metric group.
    pub fn new_default() -> Self {
        Self::new("network", 0)
    }
}

/// Finishes the construction of a qp: creates its transmit poller and registers its
/// metrics (the copy counters only when `register_copy_stats`).
///
/// # Safety
/// `this` must be the root pointer of a heap-allocated qp that is never moved afterwards
/// (the one its [`QpPtr`] holds), with no reference to it live.
pub unsafe fn qp_init(this: *mut dyn Qp, register_copy_stats: bool) {
    // SAFETY: `this` is a live, pinned qp (see above); the pointers below are projected from
    // it, so they keep its provenance after the temporary references end.
    let bp = unsafe { qp_base_ptr(this) };
    // SAFETY: `bp` is the qp's base, projected from the live root pointer; `addr_of!` only
    // computes the field address, without creating a reference.
    let stats: *const QpStats = unsafe { std::ptr::addr_of!((*bp)._stats) };
    // SAFETY: as for `stats`.
    let txq: *const CircularBuffer<Packet> = unsafe { std::ptr::addr_of!((*bp)._tx_packetq) };
    // SAFETY: as above; a short access.
    let b = unsafe { &mut *bp };
    b._tx_poller = Some(Box::new(Poller::simple(move || {
        // SAFETY: the poller is owned by the qp, so the qp outlives it; `this` is the root
        // pointer and the poller runs with no other reference to the qp live.
        unsafe { <dyn Qp>::poll_tx(this) }
    })));

    let qn = b._queue_name.clone();
    // The metrics are owned by the qp, so `stats` and `txq` outlive the closures below.
    macro_rules! st {
        ($($f:tt)+) => {
            // SAFETY: the metrics are owned by the qp, so `stats` points to its live counters
            // whenever a metric closure runs; the closure only reads them, on the qp's shard.
            move || unsafe { (*stats).$($f)+ }
        };
    }

    // Registration fails only on a double registration of the same group, which cannot
    // happen for a new qp; the result is ignored so that construction cannot fail.
    let _ = b._metrics.add_group(
        &b._stats_plugin_name,
        vec![
            //
            // Packets rate: DERIVE:0:u
            //
            sm::make_counter(&format!("{}_rx_packets", qn), st!(rx.good.packets),
                sm::Description::new("This metric is a receive packet rate for this queue."), vec![]),

            sm::make_counter(&format!("{}_tx_packets", qn), st!(tx.good.packets),
                sm::Description::new("This metric is a transmit packet rate for this queue."), vec![]),
            //
            // Bytes rate: DERIVE:0:U
            //
            sm::make_counter(&format!("{}_rx_bytes", qn), st!(rx.good.bytes),
                sm::Description::new("This metric is a receive throughput for this queue."), vec![]),

            sm::make_counter(&format!("{}_tx_bytes", qn), st!(tx.good.bytes),
                sm::Description::new("This metric is a transmit throughput for this queue."), vec![]),
            //
            // Queue length: GAUGE:0:U
            //
            // Tx
            sm::make_gauge(&format!("{}_tx_packet_queue", qn),
                // SAFETY: see above.
                move || unsafe { (*txq).size() as u64 },
                sm::Description::new("Holds a number of packets pending to be sent. \
                    This metric will have high values if the network backend doesn't keep up with the upper layers or if upper layers send big bursts of packets."), vec![]),

            //
            // Linearization counter: DERIVE:0:U
            //
            sm::make_counter(&format!("{}_xmit_linearized", qn), st!(tx.linearized),
                sm::Description::new("Counts a number of linearized Tx packets. High value indicates that we send too fragmented packets."), vec![]),

            //
            // Number of packets in last bunch: GAUGE:0:U
            //
            // Tx
            sm::make_gauge(&format!("{}_tx_packet_queue_last_bunch", qn), st!(tx.good.last_bunch),
                sm::Description::new(&format!("Holds a number of packets sent in the bunch. \
                    A high value in conjunction with a high value of a {}_tx_packet_queue indicates an efficient Tx packets bulking.", qn)), vec![]),
            // Rx
            sm::make_gauge(&format!("{}_rx_packet_queue_last_bunch", qn), st!(rx.good.last_bunch),
                sm::Description::new("Holds a number of packets received in the last Rx bunch. High value indicates an efficient Rx packets bulking."), vec![]),

            //
            // Fragments rate: DERIVE:0:U
            //
            // Tx
            sm::make_counter(&format!("{}_tx_frags", qn), st!(tx.good.nr_frags),
                sm::Description::new(&format!("Counts a number of sent fragments. Divide this value by a {}_tx_packets to get an average number of fragments in a Tx packet.", qn)), vec![]),
            // Rx
            sm::make_counter(&format!("{}_rx_frags", qn), st!(rx.good.nr_frags),
                sm::Description::new(&format!("Counts a number of received fragments. Divide this value by a {}_rx_packets to get an average number of fragments in an Rx packet.", qn)), vec![]),
        ],
    );

    if register_copy_stats {
        // As above.
        let _ = b._metrics.add_group(
            &b._stats_plugin_name,
            vec![
                //
                // Non-zero-copy data bytes rate: DERIVE:0:u
                //
                // Tx
                sm::make_counter(&format!("{}_tx_copy_bytes", qn), st!(tx.good.copy_bytes),
                    sm::Description::new(&format!("Counts a number of sent bytes that were handled in a non-zero-copy way. Divide this value by a {}_tx_bytes to get a portion of data sent using a non-zero-copy flow.", qn)), vec![]),
                // Rx
                sm::make_counter(&format!("{}_rx_copy_bytes", qn), st!(rx.good.copy_bytes),
                    sm::Description::new(&format!("Counts a number of received bytes that were handled in a non-zero-copy way. Divide this value by an {}_rx_bytes to get a portion of received data handled using a non-zero-copy flow.", qn)), vec![]),

                //
                // Non-zero-copy data fragments rate: DERIVE:0:u
                //
                // Tx
                sm::make_counter(&format!("{}_tx_copy_frags", qn), st!(tx.good.copy_frags),
                    sm::Description::new(&format!("Counts a number of sent fragments that were handled in a non-zero-copy way. Divide this value by a {}_tx_frags to get a portion of fragments sent using a non-zero-copy flow.", qn)), vec![]),
                // Rx
                sm::make_counter(&format!("{}_rx_copy_frags", qn), st!(rx.good.copy_frags),
                    sm::Description::new(&format!("Counts a number of received fragments that were handled in a non-zero-copy way. Divide this value by a {}_rx_frags to get a portion of received fragments handled using a non-zero-copy flow.", qn)), vec![]),
            ],
        );
    }
}

/// The body of `<dyn Qp>::configure_proxies`.
///
/// # Safety
/// As for `<dyn Qp>::configure_proxies`.
unsafe fn qp_configure_proxies(this: *mut dyn Qp, cpu_weights: &BTreeMap<u32, f32>) {
    assert!(!cpu_weights.is_empty());
    if cpu_weights.len() == 1 && *cpu_weights.keys().next().unwrap() == this_shard_id() {
        // special case queue sending to self only, to avoid requiring a hash value
        return;
    }
    // SAFETY: guaranteed by the caller; projected from the root pointer.
    let b: *mut QpBase = unsafe { qp_base_ptr(this) };
    // SAFETY: as above; short accesses.
    unsafe {
        (*this).register_packet_provider(Box::new(move || {
            // SAFETY: the provider is owned by the qp, so the qp outlives it; a short access
            // from the qp's tx poller, with no other reference to the queue live.
            (*b)._proxy_packetq.take_front()
        }));
        (*this).build_sw_reta(cpu_weights);
    }
}

impl QpBase {
    /// Builds the 128-entry software redirection table: each target shard in `cpu_weights`
    /// gets a run of entries proportional to its weight, in shard order.
    pub fn build_sw_reta(&mut self, cpu_weights: &BTreeMap<u32, f32>) {
        let mut total_weight: f32 = 0.0;
        for x in cpu_weights.values() {
            total_weight += *x;
        }
        let mut accum: f32 = 0.0;
        let mut idx: u32 = 0;
        let mut reta = [0u8; 128];
        for (&cpu, &weight) in cpu_weights.iter() {
            accum += weight;
            while (idx as f64) < (accum / total_weight * reta.len() as f32) as f64 - 0.5 {
                reta[idx as usize] = cpu as u8;
                idx += 1;
            }
        }
        self._sw_reta = Some(reta);
    }
}

impl dyn Device {
    /// Subscribes `next_packet` to the current shard's receive stream and starts the queue
    /// receiving; resolves when the subscription ends.
    pub fn receive(&self, next_packet: Box<dyn FnMut(Packet) -> Future<()>>) -> Future<()> {
        let q = self.queue_for_cpu(this_shard_id());
        // SAFETY: the local queue lives until at_destroy; short accesses on its own shard.
        let mut sub =
            unsafe { Pin::new_unchecked(&mut (*q).base_mut()._rx_stream) }.listen_with(next_packet);
        // SAFETY: as above; `q` is the qp's root pointer, and the shared borrow of
        // `rx_start_root` ends before the function runs.
        unsafe {
            match (*q).rx_start_root() {
                Some(start) => start(q),
                None => (*q).rx_start(),
            }
        }
        sub.done()
    }

    /// Registers `dev` as the current shard's queue; it is destroyed when the reactor shuts
    /// down.
    ///
    /// # Panics
    ///
    /// If the shard already has a queue.
    pub fn set_local_queue(&self, dev: QpPtr) {
        assert!(self.base()._queues.get(this_shard_id()).is_none());
        // The table keeps the qp's root pointer; only the raw pointer moves into the
        // destruction hook, and ownership is taken back when it runs.
        let raw: *mut dyn Qp = dev.into_raw();
        self.base()._queues.set(this_shard_id(), raw);
        at_destroy(move || {
            // SAFETY: `raw` came from the qp's `NodePtr` (`into_raw` above) and is freed once,
            // here, at destroy.
            drop(unsafe { NodePtr::from_raw(raw) });
        });
    }
}

impl L3Protocol {
    /// Registers protocol `proto_num` on `netif`, with `func` as its source of packets to
    /// send.
    ///
    /// # Safety
    /// `netif` is a live interface that outlives the protocol.
    pub unsafe fn new(
        netif: *mut Interface,
        proto_num: EthProtocolNum,
        func: L3PacketProviderType,
    ) -> Self {
        // SAFETY: the interface outlives the protocols registered on it.
        unsafe { (*netif).register_packet_provider(func) };
        L3Protocol { _netif: netif, _proto_num: proto_num }
    }

    /// Subscribes `rx_fn` to the packets of this protocol, with `forward` to pick their shard
    /// (see [`Interface::register_l3`]).
    pub fn receive(&mut self, rx_fn: RxFn, forward: ForwardFn) -> Future<()> {
        // SAFETY: the interface outlives the protocols registered on it.
        unsafe { (*self._netif).register_l3(self._proto_num, rx_fn, forward) }
    }
}

impl Interface {
    /// The interface of `dev` on the current shard: subscribes to the device's receive stream
    /// and registers on the local queue a provider that takes packets from the L3 providers
    /// round-robin and prepends their Ethernet header. In its own node because those
    /// callbacks point to the interface.
    pub fn new(dev: Arc<dyn Device>) -> NodePtr<Interface> {
        let hw_address = dev.hw_address();
        let hw_features = dev.hw_features();
        let this = NodePtr::new(Interface {
            _proto_map: HashMap::default(),
            _dev: dev.clone(),
            _hw_address: hw_address,
            _hw_features: hw_features,
            _pkt_providers: Vec::new(),
        });
        let tp: *mut Interface = this.as_ptr();
        // FIXME: ignored future
        let _ = dev.receive(Box::new(move |p: Packet| {
            // SAFETY: the interface outlives the device's rx stream subscription; the stream
            // delivers from the reactor, with no other reference to the interface live.
            unsafe { Interface::dispatch_packet(tp, p) }
        }));
        let mut idx: usize = 0;
        // SAFETY: the local queue lives until at_destroy; a short access on its own shard.
        unsafe {
            (*dev.local_queue()).register_packet_provider(Box::new(move || {
                // SAFETY (closure): the interface outlives the providers registered on its queue.
                // The L3 providers read the interface back (offloads, address), so only short
                // accesses are made; the providers are registered at construction, so the vector
                // does not change while one runs.
                let n = (*tp)._pkt_providers.len();
                for _i in 0..n {
                    let provider: *mut L3PacketProviderType =
                        (*tp)._pkt_providers.as_mut_ptr().add(idx);
                    let l3p = (*provider)();
                    idx += 1;
                    if idx == n {
                        idx = 0;
                    }
                    if let Some(mut l3pv) = l3p {
                        let ehp = l3pv.p.prepend_header::<EthHdr>(0);
                        let hdr = EthHdr {
                            dst_mac: l3pv.to,
                            src_mac: (*tp)._hw_address,
                            eth_proto: l3pv.proto_num as u16,
                        };
                        // SAFETY: prepend_header returned room for an EthHdr (align 1).
                        ehp.write(hton_struct(&hdr));
                        return Some(l3pv.p);
                    }
                }
                None
            }))
        };
        this
    }

    /// Subscribes `next` to the frames of EtherType `proto_num`; `forward` fills the hash
    /// that picks the owning shard when the NIC gave none. Resolves when the subscription
    /// ends.
    ///
    /// # Panics
    ///
    /// If `proto_num` is already registered.
    pub fn register_l3(
        &mut self,
        proto_num: EthProtocolNum,
        next: RxFn,
        forward: ForwardFn,
    ) -> Future<()> {
        let key = proto_num as u16;
        assert!(!self._proto_map.contains_key(&key));
        let l3_rx =
            self._proto_map.entry(key).or_insert_with(|| L3RxStream::new_node(forward)).as_ptr();
        let mut next = next;
        // SAFETY: the stream lives in its map node, which stays in place until the interface
        // is destroyed; no other reference to it is alive here.
        unsafe { Pin::new_unchecked(&mut (*l3_rx).packet_stream) }
            .listen_with(Box::new(move |(p, from): (Packet, EthernetAddress)| next(p, from)))
            .done()
    }

    /// The shard that owns the flow of `hash` (see [`Device::hash2cpu`]).
    pub fn hash2cpu(&self, hash: u32) -> u32 {
        self._dev.hash2cpu(hash)
    }

    /// The device's number of hardware queues.
    pub fn hw_queues_count(&self) -> u16 {
        self._dev.hw_queues_count()
    }

    /// The device's RSS key.
    pub fn rss_key(&self) -> RssKeyType {
        self._dev.rss_key()
    }

    /// Hands the received frame `p` to shard `cpuid`, which delivers it to its own receive
    /// stream. At most 1000 forwards per shard are in flight; beyond that the frame is
    /// dropped.
    pub fn forward(&mut self, cpuid: u32, p: Packet) {
        thread_local! {
            static QUEUE_DEPTH: Cell<u32> = const { Cell::new(0) };
        }

        if QUEUE_DEPTH.with(|q| q.get()) < 1000 {
            QUEUE_DEPTH.with(|q| q.set(q.get() + 1));
            let src_cpu = this_shard_id();
            // The device is shared by every shard and outlives the stack, so its pointer is
            // captured without a reference count (an atomic increment per forwarded packet
            // otherwise).
            // SAFETY: the device is
            // shared by every shard and outlives the stack, and `l2receive` touches only the
            // target shard's queue; the packet's memory goes back to this shard through
            // `free_on_cpu`, which the target shard calls before using it.
            let fwd = unsafe { CrossShard::new((Arc::as_ptr(&self._dev), p)) };
            // FIXME: future is discarded
            let _ = Smp::submit_to(cpuid, move || {
                let (dev, mut p) = fwd.into_inner();
                // SAFETY: see above.
                unsafe { (*dev).l2receive(p.free_on_cpu_default(src_cpu)) };
            })
            .then(|_| {
                QUEUE_DEPTH.with(|q| q.set(q.get() - 1));
            });
        }
    }

    /// Dispatches a received frame: finds the stream of its EtherType (frames of an
    /// unregistered type are dropped), picks the owning shard from the NIC's RSS hash or
    /// the protocol's Toeplitz hash, and either forwards the frame there or strips the
    /// Ethernet header and delivers it, dropping it if the previous delivery has not
    /// completed (the queue is unbounded otherwise). Takes the interface's root pointer: the
    /// L3 protocols it delivers to read the interface back (offloads, address) and may
    /// forward, so no reference to the interface is live across those calls.
    ///
    /// # Safety
    /// `this` is the interface's root pointer, with no reference to it live.
    unsafe fn dispatch_packet(this: *mut Interface, mut p: Packet) -> Future<()> {
        let ehp = p.get_header::<EthHdr>(0);
        if !ehp.is_null() {
            // SAFETY: get_header returned a pointer to an EthHdr (align 1) inside `p`.
            let eh: EthHdr = unsafe { *ehp };
            let proto = ntoh(eh.eth_proto);
            // The device is used through the shared pointer, without taking a reference count.
            // SAFETY: guaranteed by the caller; a short access.
            let dev: *const dyn Device = unsafe { Arc::as_ptr(&(*this)._dev) };
            // One lookup; the entry lives in its own node and is used
            // through a pointer by the hash callback and the delivery below, and the map does
            // not change in between.
            // SAFETY: as above; the shared borrow of the map ends with the lookup.
            let l3: Option<*mut L3RxStream> =
                unsafe { (*this)._proto_map.get(&proto).map(|l3| l3.as_ptr()) };
            if let Some(l3) = l3 {
                // SAFETY: `_dev` keeps the device alive.
                let fw = unsafe { &*dev }.forward_dst(this_shard_id(), || {
                    let hwrss = p.rss_hash();
                    if let Some(h) = hwrss {
                        h
                    } else {
                        // SAFETY: the entry is live (see above) and only this callback uses it now.
                        let l3 = unsafe { &mut *l3 };
                        let mut data = ForwardHash::default();
                        if (l3.forward)(&mut data, &mut p, std::mem::size_of::<EthHdr>()) {
                            // SAFETY: as above.
                            return toeplitz_hash(unsafe { (*dev).rss_key() }, &data);
                        }
                        0u32
                    }
                });
                if fw != this_shard_id() {
                    // SAFETY: guaranteed by the caller; `forward` only submits work to `fw`.
                    unsafe { (*this).forward(fw, p) };
                } else {
                    // the forward function may have linearized the packet: read the header again
                    let ehp = p.get_header::<EthHdr>(0);
                    // SAFETY: as above.
                    let h = ntoh_struct(unsafe { &*ehp });
                    let from = h.src_mac;
                    p.trim_front(std::mem::size_of::<EthHdr>());
                    // SAFETY: the entry is live (see above); the callback no longer uses it.
                    // The entry is a separate node, so this reference does not cover the
                    // interface, which the receiver may read.
                    let l3 = unsafe { &mut *l3 };
                    // avoid chaining, since queue lenth is unlimited
                    // drop instead.
                    if l3.ready.available() {
                        // SAFETY: the stream stays in its map node until the interface is
                        // destroyed, and the interface outlives the returned future.
                        l3.ready =
                            unsafe { Pin::new_unchecked(&mut l3.packet_stream).produce((p, from)) };
                    }
                }
            }
        }
        make_ready_future(())
    }
}
