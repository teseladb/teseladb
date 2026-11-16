//! The IPv4 layer of the native network stack (RFC 791), with ICMP echo (RFC 792) and the
//! protocol objects of UDP and TCP.
//!
//! [`Ipv4`] receives frames from the Ethernet interface, filters and reassembles them
//! (fragments are merged with a [`PacketMerger`] under a memory limit and a timeout),
//! forwards each packet to the shard that owns its flow (by RSS hash of addresses and
//! ports) and delivers it to its L4 protocol: [`Ipv4Tcp`], [`Ipv4Udp`] or [`Ipv4Icmp`].
//! On the send side it polls the L4 packet providers round-robin, resolves the next hop
//! through ARP, adds the IP header and fragments packets larger than the MTU when the
//! device cannot segment them. An optional [`IpPacketFilter`] (the DHCP client) may consume
//! received packets first.
//!
//! The L4 protocol objects keep raw pointers to their `Ipv4` and register callbacks that
//! capture their own address, so the `Ipv4` is constructed in place in a stable allocation
//! ([`Ipv4::init_at`]) and never moves; the L4 layers reach it only through the field
//! projections of [`Ipv4L4`]. Each shard has its own `Ipv4`.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CString, c_void};
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::time::Duration;

use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::lowres_clock::{LowresClock, LowresTimePoint};
use crate::engine::core::metrics::{self as sm, MetricGroups};
use crate::engine::core::semaphore::Semaphore;
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::shared_ptr::LwSharedPtr;
use crate::engine::core::smp::Smp;
use crate::engine::core::timer::Timer;
use crate::engine::net::arp::{Arp, ArpFor, ArpL3};
use crate::engine::net::byteorder::{
    AdjustEndianness, EndianVisitor, hton, hton_struct, ntoh_struct,
};
use crate::engine::net::const_::{
    EthProtocolNum, IP_PACKET_LEN_MAX, IPV4_HDR_LEN_MIN, IpProtocolNum,
};
use crate::engine::net::ethernet::{EthHdr, EthernetAddress};
use crate::engine::net::hash::{FxBuildHasher, IdentityHash};
use crate::engine::net::inet_address::RuntimeError;
use crate::engine::net::ip_checksum::Checksummer;
use crate::engine::net::ipv4_address::Ipv4Address;
use crate::engine::net::net::{ForwardHash, HwFeatures, Interface, L3Packet, L3Protocol};
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::net::packet::{OffloadInfo, Packet};
use crate::engine::net::packet_util::PacketMerger;
use crate::engine::net::socket_defs::{AF_INET, in_addr, inet_pton};
use crate::engine::net::tcp::Tcp;
use crate::engine::net::toeplitz::{RssKeyType, toeplitz_hash};
use crate::engine::net::udp::UdpChannelState;

/// The traits of an internet protocol that `tcp<InetTraits>` and `l4connid` are generic
/// over.
pub trait InetTraits: 'static {
    /// The address type of the protocol.
    type AddressType: Copy + Eq + Hash + Default + std::fmt::Display + std::fmt::Debug + 'static;
    /// The network layer type the L4 protocols run over.
    type InetType;
    /// The packet type an L4 protocol hands to the network layer.
    type L4Packet;
    /// Adds the TCP pseudo header to `csum`.
    fn tcp_pseudo_header_checksum(
        csum: &mut Checksummer,
        src: Self::AddressType,
        dst: Self::AddressType,
        len: u16,
    );
    /// Adds the UDP pseudo header to `csum`.
    fn udp_pseudo_header_checksum(
        csum: &mut Checksummer,
        src: Self::AddressType,
        dst: Self::AddressType,
        len: u16,
    );
    /// Minimum header length of the protocol, in bytes.
    const IP_HDR_LEN_MIN: u8;
    /// The address as the 32-bit network-order word hashed for RSS.
    fn hton_ip(a: &Self::AddressType) -> u32;
}

/// A packet an L4 protocol hands to the IPv4 layer for sending.
pub struct L4Packet {
    /// Destination address.
    pub to: Ipv4Address,
    /// The L4 packet.
    pub p: Packet,
    /// Link-layer destination.
    pub e_dst: EthernetAddress,
    /// L4 protocol.
    pub proto_num: IpProtocolNum,
}

/// A source of L4 packets to send, polled by the IPv4 layer; `None` when it has nothing.
pub type Ipv4PacketProviderType = Box<dyn FnMut() -> Option<L4Packet>>;

/// The [`InetTraits`] of IPv4.
pub struct Ipv4Traits;

impl Ipv4Traits {
    /// Adds the TCP pseudo-header (RFC 793: addresses, protocol and TCP length) to `csum`.
    #[inline]
    pub fn tcp_pseudo_header_checksum(
        csum: &mut Checksummer,
        src: Ipv4Address,
        dst: Ipv4Address,
        len: u16,
    ) {
        let (s, d) = (src.ip, dst.ip);
        crate::checksum_sum_many!(csum, s, d, 0u8, IpProtocolNum::tcp as u8, len);
    }
    /// Adds the UDP pseudo-header (RFC 768: addresses, protocol and UDP length) to `csum`.
    #[inline]
    pub fn udp_pseudo_header_checksum(
        csum: &mut Checksummer,
        src: Ipv4Address,
        dst: Ipv4Address,
        len: u16,
    ) {
        let (s, d) = (src.ip, dst.ip);
        crate::checksum_sum_many!(csum, s, d, 0u8, IpProtocolNum::udp as u8, len);
    }
    /// Minimum IPv4 header length, in bytes.
    pub const IP_HDR_LEN_MIN: u8 = IPV4_HDR_LEN_MIN;
}

impl InetTraits for Ipv4Traits {
    type AddressType = Ipv4Address;
    type InetType = Ipv4L4<{ IpProtocolNum::tcp as u8 }>;
    type L4Packet = L4Packet;
    #[inline]
    fn tcp_pseudo_header_checksum(
        csum: &mut Checksummer,
        src: Ipv4Address,
        dst: Ipv4Address,
        len: u16,
    ) {
        Ipv4Traits::tcp_pseudo_header_checksum(csum, src, dst, len)
    }
    #[inline]
    fn udp_pseudo_header_checksum(
        csum: &mut Checksummer,
        src: Ipv4Address,
        dst: Ipv4Address,
        len: u16,
    ) {
        Ipv4Traits::udp_pseudo_header_checksum(csum, src, dst, len)
    }
    const IP_HDR_LEN_MIN: u8 = IPV4_HDR_LEN_MIN;
    #[inline]
    fn hton_ip(a: &Ipv4Address) -> u32 {
        let ip = a.ip;
        hton(ip)
    }
}

/// The view of the IPv4 layer an L4 protocol uses.
///
/// The L4 protocols are fields of the `ipv4` (`_tcp`, `_icmp`, `_udp`) and are entered with a
/// `&mut` to themselves, so they must not make a reference to the whole `ipv4` (it would
/// overlap theirs): this view reaches the ipv4 through field projections only (its
/// addresses, its interface pointer, its ARP table). It is `Copy` (one pointer), so a layer
/// keeps its own copy instead of pointing into its owner.
#[derive(Clone, Copy)]
pub struct Ipv4L4<const PROTO_NUM: u8> {
    /// The IPv4 layer; it outlives the L4 protocols, which are its fields.
    pub _inet: *mut Ipv4,
}

impl<const PROTO_NUM: u8> Ipv4L4<PROTO_NUM> {
    /// A view of `inet`.
    #[inline]
    pub fn new(inet: *mut Ipv4) -> Self {
        Ipv4L4 { _inet: inet }
    }
    /// The host address of the IPv4 layer.
    #[inline]
    pub fn host_address(&self) -> Ipv4Address {
        // SAFETY: the ipv4 owns its l4 protocols and outlives them; a field read.
        unsafe { Ipv4::host_address_of(self._inet) }
    }
    /// The hardware features (offloads, MTU) of the interface.
    #[inline]
    pub fn hw_features(&self) -> &HwFeatures {
        // SAFETY: as above; the features live in the interface, outside the ipv4.
        unsafe { &*Ipv4::hw_features_of(self._inet) }
    }
    /// The interface the IPv4 layer runs on.
    #[inline]
    pub fn netif(&self) -> *mut Interface {
        // SAFETY: as above; a field read.
        unsafe { (*self._inet)._netif }
    }
}

/// A layer-4 protocol on top of IPv4.
pub trait IpProtocol {
    /// Handles a packet received from `from` for `to`.
    fn received(&mut self, p: Packet, from: Ipv4Address, to: Ipv4Address);
    /// Fills the hash used to pick the shard of a packet.
    #[inline]
    fn forward(&mut self, out_hash_data: &mut ForwardHash, p: &mut Packet, off: usize) -> bool {
        let _ = out_hash_data;
        let _ = p;
        let _ = off;
        true
    }
}

/// The four-tuple of a connection: local and foreign addresses and ports.
pub struct L4Connid<T: InetTraits> {
    /// Local address.
    pub local_ip: T::AddressType,
    /// Remote address.
    pub foreign_ip: T::AddressType,
    /// Local port.
    pub local_port: u16,
    /// Remote port.
    pub foreign_port: u16,
}

impl<T: InetTraits> Clone for L4Connid<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: InetTraits> Copy for L4Connid<T> {}

impl<T: InetTraits> PartialEq for L4Connid<T> {
    #[inline]
    fn eq(&self, x: &Self) -> bool {
        self.local_ip == x.local_ip
            && self.foreign_ip == x.foreign_ip
            && self.local_port == x.local_port
            && self.foreign_port == x.foreign_port
    }
}
impl<T: InetTraits> Eq for L4Connid<T> {}

impl<T: InetTraits> L4Connid<T> {
    /// The Toeplitz hash of the connection, as the NIC computes it.
    pub fn hash(&self, rss_key: RssKeyType) -> u32 {
        let mut hash_data = ForwardHash::default();
        hash_data.push_back_u32(T::hton_ip(&self.foreign_ip));
        hash_data.push_back_u32(T::hton_ip(&self.local_ip));
        hash_data.push_back_u16(hton(self.foreign_port));
        hash_data.push_back_u16(hton(self.local_port));
        toeplitz_hash(rss_key, &hash_data)
    }
}

/// The hash of one tuple field for `connid_hash`: the identity on the address integer (no
/// SipHash; the table mixes the result once with `FxBuildHasher`).
#[inline]
fn std_hash<V: Hash>(v: &V) -> u64 {
    IdentityHash::of(v)
}

impl<T: InetTraits> Hash for L4Connid<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        let v = std_hash(&self.local_ip)
            ^ std_hash(&self.foreign_ip)
            ^ self.local_port as u64
            ^ self.foreign_port as u64;
        state.write_u64(v);
    }
}

/// The TCP protocol object of the IPv4 layer; its methods are in
/// [`crate::engine::net::tcp`].
pub struct Ipv4Tcp {
    /// The view of the ipv4 layer TCP uses.
    pub _inet_l4: Ipv4L4<{ IpProtocolNum::tcp as u8 }>,
    /// The TCP layer, in a node others point into.
    pub _tcp: Option<NodePtr<Tcp<Ipv4Traits>>>,
}

/// ICMP message type (RFC 792).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IcmpMsgType {
    /// Echo reply.
    #[default]
    EchoReply = 0,
    /// Echo request.
    EchoRequest = 8,
}

/// The ICMP header.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct IcmpHdr {
    /// Message type (raw byte, so that any value read from the wire is valid).
    pub type_: u8,
    /// Message code.
    pub code: u8,
    /// Checksum.
    pub csum: u16,
    /// The rest of the header.
    pub rest: u32,
}

impl AdjustEndianness for IcmpHdr {
    #[inline]
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        crate::adjust_fields!(a, self.csum);
    }
}

/// The ICMP layer: answers echo requests.
pub struct Icmp {
    /// The address the object was built at (`slot` of `init_at`), whose provenance the
    /// continuations keep; never one from `&mut self`.
    _this: *mut Icmp,
    _inet: Ipv4L4<{ IpProtocolNum::icmp as u8 }>,
    _packetq: CircularBuffer<L4Packet>,
    _queue_space: Semaphore,
}

impl Icmp {
    /// Constructs the ICMP layer in place at `slot`; it must be built in place because the
    /// packet provider it registers keeps its address.
    ///
    /// # Safety
    /// `slot` must be valid for writes and must not move afterwards; `inet` must outlive it.
    pub unsafe fn init_at(slot: *mut Icmp, inet: Ipv4L4<{ IpProtocolNum::icmp as u8 }>) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            slot.write(Icmp {
                _this: slot,
                _inet: inet,
                _packetq: CircularBuffer::new(),
                _queue_space: Semaphore::new(212992),
            });
            let mut l4 = inet;
            ipv4_l4_register_packet_provider::<{ IpProtocolNum::icmp as u8 }>(
                &mut l4,
                Box::new(move || {
                    let me = &mut *slot;
                    let mut l4p: Option<L4Packet> = None;
                    if !me._packetq.empty() {
                        l4p = me._packetq.take_front();
                        if let Some(l) = &l4p {
                            me._queue_space.signal(l.p.len() as usize);
                        }
                    }
                    l4p
                }),
            );
        }
    }
}

/// The ICMP protocol object of the IPv4 layer.
pub struct Ipv4Icmp {
    _inet_l4: Ipv4L4<{ IpProtocolNum::icmp as u8 }>,
    _icmp: Icmp,
}

impl Ipv4Icmp {
    /// Constructs the ICMP protocol object in place at `slot`.
    ///
    /// # Safety
    /// As `Icmp::init_at`.
    pub unsafe fn init_at(slot: *mut Ipv4Icmp, inet: *mut Ipv4) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            std::ptr::addr_of_mut!((*slot)._inet_l4).write(Ipv4L4::new(inet));
            Icmp::init_at(std::ptr::addr_of_mut!((*slot)._icmp), Ipv4L4::new(inet));
        }
    }
}

impl IpProtocol for Ipv4Icmp {
    fn received(&mut self, p: Packet, from: Ipv4Address, to: Ipv4Address) {
        self._icmp.received(p, from, to);
    }
}

/// The UDP protocol object of the IPv4 layer; its methods are in [`crate::engine::net::udp`].
pub struct Ipv4Udp {
    /// The address the object was built at (`slot` of `init_at`). Channels, registrations
    /// and continuations keep this one, never a pointer from `&mut self`.
    pub(crate) _this: *mut Ipv4Udp,
    /// The IPv4 layer; it outlives the protocol.
    pub(crate) _inet: *mut Ipv4,
    /// The channel state of every bound port.
    pub(crate) _channels: HashMap<u16, LwSharedPtr<UdpChannelState>, FxBuildHasher>,
    /// Receive-queue length of new channels, in datagrams.
    pub(crate) _queue_size: i32,
    /// Next port tried when binding an unspecified port.
    pub(crate) _next_anonymous_port: u16,
    /// Datagrams waiting for the IPv4 layer to send them.
    pub(crate) _packetq: CircularBuffer<L4Packet>,
}

impl Ipv4Udp {
    /// The first port handed out to channels bound to port 0.
    pub const MIN_ANONYMOUS_PORT: u16 = 32768;

    /// Constructs the UDP protocol object in place at `slot` (see `Ipv4Udp::init`).
    ///
    /// # Safety
    /// `slot` must be valid for writes and must not move afterwards; `inet` must outlive it.
    pub unsafe fn init_at(slot: *mut Ipv4Udp, inet: *mut Ipv4) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            slot.write(Ipv4Udp {
                _this: slot,
                _inet: inet,
                _channels: HashMap::default(),
                _queue_size: Self::DEFAULT_QUEUE_SIZE,
                _next_anonymous_port: Self::MIN_ANONYMOUS_PORT,
                _packetq: CircularBuffer::new(),
            });
        }
        Ipv4Udp::init(slot);
    }

    /// Sets the receive queue size of the channels created from now on.
    #[inline]
    pub fn set_queue_size(&mut self, size: i32) {
        self._queue_size = size;
    }

    /// The view of the ipv4 layer this protocol uses (field projections only; see
    /// [`Ipv4L4`]).
    #[inline]
    pub fn inet(&self) -> Ipv4L4<{ IpProtocolNum::udp as u8 }> {
        Ipv4L4::new(self._inet)
    }
}

impl IpProtocol for Ipv4Udp {
    fn received(&mut self, p: Packet, from: Ipv4Address, to: Ipv4Address) {
        self.received_impl(p, from, to)
    }
    fn forward(&mut self, out_hash_data: &mut ForwardHash, p: &mut Packet, off: usize) -> bool {
        self.forward_impl(out_hash_data, p, off)
    }
}

/// The binding of a UDP channel to its port; unregistering frees the port.
#[derive(Clone, Copy)]
pub struct Registration {
    _proto: *mut Ipv4Udp,
    _port: u16,
}

impl Registration {
    /// The binding of `port` in `proto`.
    #[inline]
    pub fn new(proto: *mut Ipv4Udp, port: u16) -> Self {
        Registration { _proto: proto, _port: port }
    }

    /// Releases the port.
    pub fn unregister(&self) {
        // SAFETY: the protocol outlives its channels.
        unsafe { (*self._proto)._channels.remove(&self._port) };
    }

    /// The bound port.
    #[inline]
    pub fn port(&self) -> u16 {
        self._port
    }
}

/// A hook that may consume received IP packets before the L4 protocols see them.
pub trait IpPacketFilter {
    /// Handles `p`; sets `handled` when the packet must not be processed further.
    ///
    /// The filter is shared by the ipv4 layers of every shard (the DHCP client installs one
    /// filter on all of them), so it is called through a shared reference.
    fn handle(
        &self,
        p: &mut Packet,
        iph: &mut IpHdr,
        from: EthernetAddress,
        handled: &mut bool,
    ) -> Future<()>;
}

/// The key of a datagram being reassembled.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ipv4FragId {
    /// Source address.
    pub src_ip: Ipv4Address,
    /// Destination address.
    pub dst_ip: Ipv4Address,
    /// IP identification.
    pub identification: u16,
    /// L4 protocol.
    pub protocol: u8,
}

impl Hash for Ipv4FragId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let (s, d) = (self.src_ip.ip, self.dst_ip.ip);
        state.write_usize(
            s as usize ^ d as usize ^ self.identification as usize ^ self.protocol as usize,
        );
    }
}

/// The [`MergerTag`](crate::engine::net::packet_util::MergerTag) of IPv4 fragment
/// reassembly.
pub struct Ipv4Tag;

thread_local! {
    /// Packets linearized by IPv4 fragment reassembly on this thread.
    static IPV4_LINEARIZATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl crate::engine::net::packet_util::MergerTag for Ipv4Tag {
    #[inline]
    fn linearization_count() -> &'static std::thread::LocalKey<std::cell::Cell<u64>> {
        &IPV4_LINEARIZATIONS
    }
}
/// The fragment merger of IPv4 reassembly, keyed by fragment offset.
pub type Ipv4PacketMerger = PacketMerger<u32, Ipv4Tag>;

/// A datagram being reassembled.
struct Frag {
    header: Packet,
    data: Ipv4PacketMerger,
    rx_time: LowresTimePoint,
    mem_size: u32,
    // fragment with MF == 0 inidates it is the last fragment
    last_frag_received: bool,
}

impl Default for Frag {
    fn default() -> Self {
        Frag {
            header: Packet::make_null_packet(),
            data: Ipv4PacketMerger::default(),
            rx_time: LowresTimePoint::from_nanos(0),
            mem_size: 0,
            last_frag_received: false,
        }
    }
}

/// The IPv4 layer of the native stack.
///
/// The fields are declared in the reverse of their initialization order: Rust drops fields
/// in declaration order, and each field depends on the ones initialized before it (`_arp`
/// unregisters from `_global_arp`, the L4 layers from the IPv4 layer). `init_at` writes
/// them in initialization order.
pub struct Ipv4 {
    _metrics: MetricGroups,
    _pkt_provider_idx: u32,
    _packetq: CircularBuffer<L3Packet>,
    _frag_timer: Timer<LowresClock>,
    _frag_mem: u32,
    _frags_age: VecDeque<Ipv4FragId>,
    _frags: HashMap<Ipv4FragId, Frag, FxBuildHasher>,
    _packet_filter: Option<*mut dyn IpPacketFilter>,
    _l4: [Option<*mut dyn IpProtocol>; 256],
    _udp: Ipv4Udp,
    _icmp: Ipv4Icmp,
    _tcp: Ipv4Tcp,
    _l3: L3Protocol,
    _netmask: Ipv4Address,
    _gw_address: Ipv4Address,
    _host_address: Ipv4Address,
    _arp: ArpFor<Ipv4>,
    _global_arp: Arp,
    _pkt_providers: Vec<Ipv4PacketProviderType>,
    _netif: *mut Interface,
}

impl ArpL3 for Ipv4 {
    type AddressType = Ipv4Address;
    #[inline]
    fn broadcast_address() -> Ipv4Address {
        Ipv4::broadcast_address()
    }
    #[inline]
    fn arp_protocol_type() -> u16 {
        Ipv4::arp_protocol_type()
    }
}

impl Ipv4 {
    /// The limited broadcast address.
    #[inline]
    pub fn broadcast_address() -> Ipv4Address {
        Ipv4Address::from_u32(0xffffffff)
    }
    /// The ethertype ARP resolves for.
    #[inline]
    pub fn arp_protocol_type() -> u16 {
        EthProtocolNum::ipv4 as u16
    }

    const FRAG_TIMEOUT: Duration = Duration::from_secs(30);
    const FRAG_LOW_THRESH: u32 = 3 * 1024 * 1024;
    const FRAG_HIGH_THRESH: u32 = 4 * 1024 * 1024;

    fn frag_arm_at(&mut self, now: LowresTimePoint) {
        let tp = now + Self::FRAG_TIMEOUT;
        // SAFETY: an ipv4 is constructed in place (`init_at`) and never moves.
        unsafe { Pin::new_unchecked(&mut self._frag_timer) }.arm(tp, None);
    }

    fn frag_arm(&mut self) {
        let now = LowresClock::now();
        self.frag_arm_at(now);
    }

    /// The interface this layer runs on.
    #[inline]
    pub fn netif(&self) -> *mut Interface {
        self._netif
    }

    /// The TCP layer.
    #[inline]
    pub fn get_tcp(&self) -> *mut Tcp<Ipv4Traits> {
        // `_tcp._tcp` is set during construction and never cleared before drop. A pointer:
        // the layer is entered back from its tcbs, so callers make short accesses only.
        self._tcp.tcp_ptr()
    }

    /// The UDP layer.
    #[inline]
    pub fn get_udp(&mut self) -> &mut Ipv4Udp {
        &mut self._udp
    }

    /// The offloads of the interface.
    #[inline]
    pub fn hw_features(&self) -> &HwFeatures {
        // SAFETY: the interface outlives the ipv4 layer.
        unsafe { (*self._netif).hw_features() }
    }

    /// Records that `l3` is at `l2`.
    #[inline]
    pub fn learn(&mut self, l2: EthernetAddress, l3: Ipv4Address) {
        self._arp.learn(l2, l3);
    }

    /// Adds a source of outgoing L4 packets.
    #[inline]
    pub fn register_packet_provider(&mut self, func: Ipv4PacketProviderType) {
        self._pkt_providers.push(func);
    }
}

/// Registers `func` as a packet provider of the L4 protocol `l4` with the IPv4 layer.
#[inline]
pub fn ipv4_l4_register_packet_provider<const PROTO_NUM: u8>(
    l4: &mut Ipv4L4<PROTO_NUM>,
    mut func: Ipv4PacketProviderType,
) {
    // SAFETY: the ipv4 owns its l4 protocols and outlives them.
    let inet = unsafe { &mut *l4._inet };
    inet.register_packet_provider(Box::new(move || {
        let mut l4p = func();
        if let Some(l) = l4p.as_mut() {
            l.proto_num = IpProtocolNum::from_u8(PROTO_NUM);
        }
        l4p
    }));
}

impl<const PROTO_NUM: u8> Ipv4L4<PROTO_NUM> {
    /// Adds a source of outgoing packets of this protocol.
    #[inline]
    pub fn register_packet_provider(&mut self, func: Ipv4PacketProviderType) {
        ipv4_l4_register_packet_provider::<PROTO_NUM>(self, func)
    }

    /// Resolves the link-layer address packets to `to` must be sent to.
    #[inline]
    pub fn get_l2_dst_address(&self, to: Ipv4Address) -> Future<EthernetAddress> {
        // SAFETY: the ipv4 owns its l4 protocols and outlives them; field projections only.
        unsafe { Ipv4::l2_dst_address(self._inet, to) }
    }
}

/// Flag and offset fields of the IPv4 `frag` word.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragBits {
    /// More fragments.
    Mf = 13,
    /// Don't fragment.
    Df = 14,
    /// Reserved.
    Reserved = 15,
    /// Shift from the offset field to bytes.
    OffsetShift = 3,
}

/// The IPv4 header. The bit-fields `ihl:4, ver:4` and `dscp:6, ecn:2` are allocated from
/// the least significant bit, as GCC does on the targets Engine supports.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct IpHdr {
    ihl_ver: u8,
    dscp_ecn: u8,
    /// Total length.
    pub len: u16,
    /// Identification.
    pub id: u16,
    /// Flags and fragment offset.
    pub frag: u16,
    /// Time to live.
    pub ttl: u8,
    /// L4 protocol.
    pub ip_proto: u8,
    /// Header checksum.
    pub csum: u16,
    /// Source address.
    pub src_ip: Ipv4Address,
    /// Destination address.
    pub dst_ip: Ipv4Address,
    /// Start of the options (variable length, `ihl * 4 - 20` bytes).
    pub options: [u8; 0],
}

impl AdjustEndianness for IpHdr {
    #[inline]
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        crate::adjust_fields!(a, self.len, self.id, self.frag, self.csum);
        let mut s = self.src_ip;
        s.adjust_endianness(a);
        self.src_ip = s;
        let mut d = self.dst_ip;
        d.adjust_endianness(a);
        self.dst_ip = d;
    }
}

impl IpHdr {
    /// Header length in 32-bit words.
    #[inline]
    pub fn ihl(&self) -> u8 {
        self.ihl_ver & 0x0f
    }
    /// Sets the header length.
    #[inline]
    pub fn set_ihl(&mut self, v: u8) {
        self.ihl_ver = (self.ihl_ver & 0xf0) | (v & 0x0f);
    }
    /// IP version.
    #[inline]
    pub fn ver(&self) -> u8 {
        self.ihl_ver >> 4
    }
    /// Sets the IP version.
    #[inline]
    pub fn set_ver(&mut self, v: u8) {
        self.ihl_ver = (self.ihl_ver & 0x0f) | (v << 4);
    }
    /// DSCP.
    #[inline]
    pub fn dscp(&self) -> u8 {
        self.dscp_ecn & 0x3f
    }
    /// Sets the DSCP.
    #[inline]
    pub fn set_dscp(&mut self, v: u8) {
        self.dscp_ecn = (self.dscp_ecn & 0xc0) | (v & 0x3f);
    }
    /// ECN.
    #[inline]
    pub fn ecn(&self) -> u8 {
        self.dscp_ecn >> 6
    }
    /// Sets the ECN.
    #[inline]
    pub fn set_ecn(&mut self, v: u8) {
        self.dscp_ecn = (self.dscp_ecn & 0x3f) | (v << 6);
    }
    /// More fragments follow.
    #[inline]
    pub fn mf(&self) -> bool {
        let f = self.frag;
        f & (1 << FragBits::Mf as u8) != 0
    }
    /// Don't fragment.
    #[inline]
    pub fn df(&self) -> bool {
        let f = self.frag;
        f & (1 << FragBits::Df as u8) != 0
    }
    /// Fragment offset in bytes.
    #[inline]
    pub fn offset(&self) -> u16 {
        let f = self.frag;
        f << FragBits::OffsetShift as u8
    }
}

// l4connid<InetTraits>::connid_hash: the Hash impl of L4Connid above.

// void arp_learn(ethernet_address l2, ipv4_address l3);  (crate::engine::net::native_stack)
pub use crate::engine::net::native_stack::arp_learn;

// }

impl Ipv4Address {
    /// Parses a dotted-quad IPv4 address.
    ///
    /// # Errors
    /// Fails if `addr` is not a valid IPv4 address.
    pub fn from_string(addr: &str) -> Result<Self, ExceptionPtr> {
        let err = || {
            ExceptionPtr::from(RuntimeError(format!(
                "Wrong format for IPv4 address {}. Please ensure it's in dotted-decimal format",
                addr
            )))
        };
        let c = CString::new(addr).map_err(|_| err())?;
        let mut a = in_addr::default();
        // SAFETY: `c` is a valid C string and `a` has room for an in_addr.
        if unsafe { inet_pton(AF_INET as i32, c.as_ptr(), &mut a as *mut in_addr as *mut c_void) }
            != 1
        {
            return Err(err());
        }
        Ok(Ipv4Address::from_u32(u32::from_be(a.s_addr)))
    }
}

impl Ipv4 {
    /// Constructs the IPv4 layer of `netif` in place in `slot`.
    ///
    /// # Safety
    /// `slot` must be valid for writes and the object must not move afterwards; `netif`
    /// must outlive it.
    pub unsafe fn init_at(slot: *mut Ipv4, netif: *mut Interface) {
        let this = slot;
        // SAFETY: by the `# Safety` contract `slot` is valid for writes and does not move; each field is written once
        // before the object is used, in initialization order.
        unsafe {
            use std::ptr::addr_of_mut;
            addr_of_mut!((*this)._netif).write(netif);
            addr_of_mut!((*this)._pkt_providers).write(Vec::new());
            Arp::init_at(addr_of_mut!((*this)._global_arp), netif);
            ArpFor::<Ipv4>::init_at(addr_of_mut!((*this)._arp), addr_of_mut!((*this)._global_arp));
            addr_of_mut!((*this)._host_address).write(Ipv4Address::from_u32(0));
            addr_of_mut!((*this)._gw_address).write(Ipv4Address::from_u32(0));
            addr_of_mut!((*this)._netmask).write(Ipv4Address::from_u32(0));
            addr_of_mut!((*this)._l3).write(L3Protocol::new(
                netif,
                EthProtocolNum::ipv4,
                Box::new(move || Ipv4::get_packet(this)),
            ));
            addr_of_mut!((*this)._tcp).write(Ipv4Tcp::new(this));
            // The tcp layer is built first, so its packet provider is registered before those
            // of icmp and udp (the round-robin order of `get_packet`).
            (*this)._tcp.init();
            Ipv4Icmp::init_at(addr_of_mut!((*this)._icmp), this);
            Ipv4Udp::init_at(addr_of_mut!((*this)._udp), this);
            let mut l4: [Option<*mut dyn IpProtocol>; 256] = [None; 256];
            l4[IpProtocolNum::tcp as usize] =
                Some(addr_of_mut!((*this)._tcp) as *mut dyn IpProtocol);
            l4[IpProtocolNum::icmp as usize] =
                Some(addr_of_mut!((*this)._icmp) as *mut dyn IpProtocol);
            l4[IpProtocolNum::udp as usize] =
                Some(addr_of_mut!((*this)._udp) as *mut dyn IpProtocol);
            addr_of_mut!((*this)._l4).write(l4);
            addr_of_mut!((*this)._packet_filter).write(None);
            addr_of_mut!((*this)._frags).write(HashMap::default());
            addr_of_mut!((*this)._frags_age).write(VecDeque::new());
            addr_of_mut!((*this)._frag_mem).write(0);
            addr_of_mut!((*this)._frag_timer).write(Timer::new());
            addr_of_mut!((*this)._packetq).write(CircularBuffer::new());
            addr_of_mut!((*this)._pkt_provider_idx).write(0);
            addr_of_mut!((*this)._metrics).write(MetricGroups::new());

            // The interface calls these from its dispatcher, with no reference to the ipv4
            // live; they work through `this` (see `handle_received_packet`).
            // FIXME: ignored future
            let _ = (*this)._l3.receive(
                Box::new(move |p: Packet, ea: EthernetAddress| {
                    Ipv4::handle_received_packet(this, p, ea)
                }),
                Box::new(move |out_hash_data: &mut ForwardHash, p: &mut Packet, off: usize| {
                    Ipv4::forward(this, out_hash_data, p, off)
                }),
            );

            // Registration fails only on a double registration, which cannot happen for a new
            // layer; this constructor cannot fail.
            let _ = (*this)._metrics.add_group(
                "ipv4",
                vec![
                    //
                    // Linearized events: DERIVE:0:u
                    //
                    sm::make_counter(
                        "linearizations",
                        Ipv4PacketMerger::linearizations,
                        sm::Description::new(
                            "Counts a number of times a buffer linearization was invoked during buffers merge process. \
                             Divide it by a total IPv4 receive packet rate to get an average number of lineraizations per packet.",
                        ),
                        vec![],
                    ),
                ],
            );
            // the timer is a field of the in-place ipv4 (never moves); the callback runs from
            // the reactor with no reference to the ipv4 live.
            Pin::new_unchecked(&mut (*this)._frag_timer)
                .set_callback(move || (*this).frag_timeout());
        }
    }

    // `ipv4` is only constructed in place (`init_at`, in `native_network_stack`): a `Box`
    // owner would assert uniqueness on every move and invalidate the pointers to the object
    // that its callbacks and l4 layers hold.

    /// # Safety
    /// `this` is a live ipv4 with no reference to it live (the dispatcher's call).
    unsafe fn forward(
        this: *mut Ipv4,
        out_hash_data: &mut ForwardHash,
        p: &mut Packet,
        off: usize,
    ) -> bool {
        let iph = p.get_header::<IpHdr>(off);
        // A frame too short for an IP header is not forwarded by its fields.
        if iph.is_null() {
            return true;
        }
        // SAFETY: get_header returned a whole IpHdr (align 1).
        let hdr = unsafe { *iph };
        let (s, d) = (hdr.src_ip.ip, hdr.dst_ip.ip);
        out_hash_data.push_back_u32(s);
        out_hash_data.push_back_u32(d);

        let h = ntoh_struct(&hdr);
        // SAFETY: guaranteed by the caller; a short read of the table.
        if let Some(l4) = unsafe { (*this)._l4[h.ip_proto as usize] }
            && !h.mf()
            && h.offset() == 0
        {
            // This IP datagram is atomic, forward according to tcp or udp connection hash
            // SAFETY: the registered protocols are members of this ipv4; no reference to
            // the ipv4 is live.
            unsafe { (*l4).forward(out_hash_data, p, off + std::mem::size_of::<IpHdr>()) };
        }
        // else forward according to ip fields only
        true
    }

    fn in_my_netmask(&self, a: Ipv4Address) -> bool {
        let (ai, hi, mi) = (a.ip, self._host_address.ip, self._netmask.ip);
        ((ai ^ hi) & mi) == 0
    }

    /// Whether `p` must be fragmented to be sent with `hw_features`.
    pub fn needs_frag(p: &mut Packet, prot_num: IpProtocolNum, hw_features: HwFeatures) -> bool {
        if p.len() + IPV4_HDR_LEN_MIN as u32 <= hw_features.mtu as u32 {
            return false;
        }

        if (prot_num == IpProtocolNum::tcp && hw_features.tx_tso)
            || (prot_num == IpProtocolNum::udp && hw_features.tx_ufo)
        {
            return false;
        }

        true
    }

    /// The L3 receive handler. The L4 layers it delivers to enter the ipv4 back (ICMP and
    /// TCP send, TCP resolves addresses), so it holds a reference to the ipv4 (`me`) only
    /// between those calls, re-deriving it from `this` after each.
    ///
    /// # Safety
    /// `this` is a live ipv4 with no reference to it live (the dispatcher's call).
    unsafe fn handle_received_packet(
        this: *mut Ipv4,
        mut p: Packet,
        from: EthernetAddress,
    ) -> Future<()> {
        // SAFETY: guaranteed by the caller.
        let mut me = unsafe { &mut *this };
        let iph = p.get_header::<IpHdr>(0);
        if iph.is_null() {
            return make_ready_future(());
        }

        // Skip checking csum of reassembled IP datagram
        if !me.hw_features().rx_csum_offload && !p.offload_info_ref().reassembled {
            let mut csum = Checksummer::default();
            // SAFETY: get_header returned a whole IpHdr.
            unsafe { csum.sum_raw(iph as *const u8, std::mem::size_of::<IpHdr>()) };
            if csum.get() != 0 {
                return make_ready_future(());
            }
        }

        // SAFETY: get_header returned a whole IpHdr (align 1).
        let mut h = ntoh_struct(unsafe { &*iph });
        let ip_len = h.len as u32;
        let ip_hdr_len = h.ihl() as u32 * 4;
        let pkt_len = p.len();
        let offset = h.offset();
        // Drop a header length below the minimum header or past the total length.
        if ip_hdr_len < std::mem::size_of::<IpHdr>() as u32 || ip_hdr_len > ip_len {
            return make_ready_future(());
        }
        if pkt_len > ip_len {
            // Trim extra data in the packet beyond IP total length
            p.trim_back((pkt_len - ip_len) as usize);
        } else if pkt_len < ip_len {
            // Drop if it contains less than IP total length
            return make_ready_future(());
        }
        // Drop if the reassembled datagram will be larger than maximum IP size
        if offset as u32 + p.len() > IP_PACKET_LEN_MAX as u32 {
            return make_ready_future(());
        }

        // FIXME: process options
        if me.in_my_netmask(h.src_ip) && h.src_ip != me._host_address {
            me._arp.learn(from, h.src_ip);
        }

        if let Some(f) = me._packet_filter {
            let mut handled = false;
            // SAFETY: the filter outlives its registration. The filter may
            // send through this ipv4 (the DHCP client on shard 0), so `me` is not used across
            // the call and is re-derived from `this` after it.
            let r = unsafe { (*f).handle(&mut p, &mut h, from, &mut handled) };
            if handled {
                return r;
            }
            // SAFETY: as at the start; no other reference to the ipv4 is live.
            me = unsafe { &mut *this };
        }

        if h.dst_ip != me._host_address {
            // FIXME: forward
            return make_ready_future(());
        }

        // Does this IP datagram need reassembly
        let mf = h.mf();
        if mf || offset != 0 {
            me.frag_limit_mem();
            let frag_id = Ipv4FragId {
                src_ip: h.src_ip,
                dst_ip: h.dst_ip,
                identification: h.id,
                protocol: h.ip_proto,
            };
            let frag = me._frags.entry(frag_id).or_default();
            if !mf {
                frag.last_frag_received = true;
            }
            // This is a newly created frag_id
            let is_new = frag.mem_size == 0;
            if is_new {
                frag.rx_time = LowresClock::now();
            }
            let added_size = frag.merge(&h, offset, p);
            let complete = frag.is_complete();
            if is_new {
                me._frags_age.push_back(frag_id);
            }
            me._frag_mem = me._frag_mem.wrapping_add(added_size as u32);
            if complete {
                // All the fragments are received
                // SAFETY: the entry was just inserted or found above.
                let frag = unsafe { me._frags.get_mut(&frag_id).unwrap_unchecked() };
                let dropped_size = frag.mem_size;
                // Choose a cpu to forward this packet
                if let Some(l4) = me._l4[h.ip_proto as usize] {
                    // SAFETY: is_complete() implies one merged segment.
                    let ip_data = unsafe { frag.data.map.values_mut().next().unwrap_unchecked() };
                    if Smp::count() == 1 {
                        let d = std::mem::replace(ip_data, Packet::make_null_packet());
                        // SAFETY: the registered protocols are members of this ipv4.
                        unsafe { (*l4).received(d, h.src_ip, h.dst_ip) };
                    } else {
                        let l4_offset = 0usize;
                        let mut hash_data = ForwardHash::default();
                        let (s, d) = (h.src_ip.ip, h.dst_ip.ip);
                        hash_data.push_back_u32(hton(s));
                        hash_data.push_back_u32(hton(d));
                        // SAFETY: as above.
                        let forwarded =
                            unsafe { (*l4).forward(&mut hash_data, ip_data, l4_offset) };
                        if forwarded {
                            // SAFETY: the interface outlives the ipv4 layer.
                            let netif = unsafe { &mut *me._netif };
                            let cpu_id = netif.hash2cpu(toeplitz_hash(netif.rss_key(), &hash_data));
                            // No need to forward if the dst cpu is the current cpu
                            if cpu_id == this_shard_id() {
                                let d = std::mem::replace(ip_data, Packet::make_null_packet());
                                // SAFETY: as above.
                                unsafe { (*l4).received(d, h.src_ip, h.dst_ip) };
                            } else {
                                let to = netif.hw_address();
                                let pkt = frag.get_assembled_packet(from, to);
                                netif.forward(cpu_id, pkt);
                            }
                        }
                    }
                }

                // Delete this frag from _frags and _frags_age
                // SAFETY: re-derived after the L4 delivery above (see the function's note).
                me = unsafe { &mut *this };
                me.frag_drop(frag_id, dropped_size);
                me._frags_age.retain(|x| *x != frag_id);
            } else {
                // Some of the fragments are missing
                if !me._frag_timer.armed() {
                    me.frag_arm();
                }
            }
            return make_ready_future(());
        }

        if let Some(l4) = me._l4[h.ip_proto as usize] {
            // Trim IP header and pass to upper layer
            p.trim_front(ip_hdr_len as usize);
            // SAFETY: the registered protocols are members of this ipv4.
            unsafe { (*l4).received(p, h.src_ip, h.dst_ip) };
        }
        make_ready_future(())
    }

    /// Resolves the link-layer address packets to `to` must be sent to.
    pub fn get_l2_dst_address(&mut self, to: Ipv4Address) -> Future<EthernetAddress> {
        // SAFETY: `self` is a live ipv4 and the only reference to it here.
        unsafe { Ipv4::l2_dst_address(self, to) }
    }

    /// Resolves the link-layer destination for `to` through field projections, for the L4
    /// layers (see [`Ipv4L4`]): `to` itself when it is on the local subnet, otherwise the
    /// default gateway; reads the addresses and borrows only `_arp`.
    ///
    /// # Safety
    /// `this` is a live ipv4 whose address fields and ARP table are not borrowed.
    pub unsafe fn l2_dst_address(this: *mut Ipv4, to: Ipv4Address) -> Future<EthernetAddress> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live ipv4
        // whose address fields and ARP table are not borrowed).
        unsafe {
            // Figure out where to send the packet to. If it is a directly connected
            // host, send to it directly, otherwise send to the default gateway.
            let (ai, hi, mi) = (to.ip, (*this)._host_address.ip, (*this)._netmask.ip);
            let dst: Ipv4Address = if ((ai ^ hi) & mi) == 0 { to } else { (*this)._gw_address };

            (*std::ptr::addr_of_mut!((*this)._arp)).lookup(&dst)
        }
    }

    /// The host address, through a field projection (see [`Ipv4L4`]).
    ///
    /// # Safety
    /// `this` is a live ipv4 whose `_host_address` is not mutably borrowed.
    #[inline]
    pub unsafe fn host_address_of(this: *const Ipv4) -> Ipv4Address {
        // SAFETY: guaranteed by the caller.
        unsafe { (*this)._host_address }
    }

    /// The interface's hardware features, through a field projection (see [`Ipv4L4`]).
    ///
    /// # Safety
    /// `this` is a live ipv4; its interface outlives it.
    #[inline]
    pub unsafe fn hw_features_of(this: *const Ipv4) -> *const HwFeatures {
        // SAFETY: guaranteed by the caller.
        unsafe { (*(*this)._netif).hw_features() }
    }

    // Sends one packet or fragment; the per-send state is passed as parameters.
    #[allow(clippy::too_many_arguments)]
    fn send_pkt(
        &mut self,
        to: Ipv4Address,
        proto_num: IpProtocolNum,
        needs_frag: bool,
        e_dst: EthernetAddress,
        mut pkt: Packet,
        remaining: u16,
        offset: u16,
    ) {
        let iphp = pkt.prepend_header::<IpHdr>(0);
        // SAFETY: prepend_header returned room for an IpHdr (align 1).
        let iph = unsafe { &mut *iphp };
        iph.set_ihl((std::mem::size_of::<IpHdr>() / 4) as u8);
        iph.set_ver(4);
        iph.set_dscp(0);
        iph.set_ecn(0);
        iph.len = pkt.len() as u16;
        // FIXME: a proper id
        iph.id = 0;
        if needs_frag {
            let mf: u16 = (remaining > 0) as u16;
            // The fragment offset is measured in units of 8 octets (64 bits)
            let off = offset / 8;
            iph.frag = (mf << FragBits::Mf as u8) | off;
        } else {
            iph.frag = 0;
        }
        iph.ttl = 64;
        iph.ip_proto = proto_num as u8;
        iph.csum = 0;
        iph.src_ip = self._host_address;
        iph.dst_ip = to;
        *iph = hton_struct(iph);

        if self.hw_features().tx_csum_ip_offload {
            iph.csum = 0;
            pkt.offload_info_ref().needs_ip_csum = true;
        } else {
            let mut csum = Checksummer::default();
            // SAFETY: `iphp` points to the whole header just written; `iph` is not used again,
            // the checksum is stored through the pointer.
            unsafe {
                csum.sum_raw(iphp as *const u8, std::mem::size_of::<IpHdr>());
                (*iphp).csum = csum.get();
            }
        }

        self._packetq.push_back(L3Packet { proto_num: EthProtocolNum::ipv4, to: e_dst, p: pkt });
    }

    /// Sends `p` to `to` as protocol `proto_num`, fragmenting it if needed.
    pub fn send(
        &mut self,
        to: Ipv4Address,
        proto_num: IpProtocolNum,
        mut p: Packet,
        e_dst: EthernetAddress,
    ) {
        let needs_frag = Self::needs_frag(&mut p, proto_num, *self.hw_features());

        if needs_frag {
            let mut offset: u16 = 0;
            let mut remaining: u16 = p.len() as u16;
            let mtu = self.hw_features().mtu;

            while remaining != 0 {
                let can_send = std::cmp::min(mtu - IPV4_HDR_LEN_MIN as u16, remaining);
                remaining -= can_send;
                let pkt = p.share_range(offset as usize, can_send as usize);
                self.send_pkt(to, proto_num, needs_frag, e_dst, pkt, remaining, offset);
                offset += can_send;
            }
        } else {
            // The whole packet can be send in one shot
            self.send_pkt(to, proto_num, needs_frag, e_dst, p, 0, 0);
        }
    }

    /// The L3 packet provider. The L4 providers it calls read the ipv4 back (offloads,
    /// addresses), so it holds no reference to it across them.
    ///
    /// # Safety
    /// `this` is a live ipv4 with no reference to it live (the interface's poller).
    unsafe fn get_packet(this: *mut Ipv4) -> Option<L3Packet> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live ipv4
        // with no reference to it live, as in the interface's poller). Short accesses only;
        // providers are registered at construction, so the vector does not change while one
        // runs.
        unsafe {
            // _packetq will be mostly empty here unless it hold remnants of previously
            // fragmented packet
            if (*this)._packetq.empty() {
                let n = (*this)._pkt_providers.len();
                for _i in 0..n {
                    let idx = (*this)._pkt_provider_idx as usize;
                    (*this)._pkt_provider_idx += 1;
                    let provider: *mut Ipv4PacketProviderType =
                        (*this)._pkt_providers.as_mut_ptr().add(idx);
                    let l4p = (*provider)();
                    if (*this)._pkt_provider_idx as usize == n {
                        (*this)._pkt_provider_idx = 0;
                    }
                    if let Some(l4pv) = l4p {
                        (*this).send(l4pv.to, l4pv.proto_num, l4pv.p, l4pv.e_dst);
                        break;
                    }
                }
            }

            (*this)._packetq.take_front()
        }
    }

    /// Sets the address of this host.
    pub fn set_host_address(&mut self, ip: Ipv4Address) {
        self._host_address = ip;
        self._arp.set_self_addr(ip);
    }

    /// The address of this host.
    pub fn host_address(&self) -> Ipv4Address {
        self._host_address
    }

    /// Sets the default gateway.
    pub fn set_gw_address(&mut self, ip: Ipv4Address) {
        self._gw_address = ip;
    }

    /// The default gateway.
    pub fn gw_address(&self) -> Ipv4Address {
        self._gw_address
    }

    /// Sets the netmask.
    pub fn set_netmask_address(&mut self, ip: Ipv4Address) {
        self._netmask = ip;
    }

    /// The netmask.
    pub fn netmask_address(&self) -> Ipv4Address {
        self._netmask
    }

    // TODO or something. Should perhaps truly be a list
    // of filters. With ordering. And blackjack. Etc.
    // But for now, a simple single raw pointer suffices
    /// Installs (or, with `None`, removes) the packet filter.
    pub fn set_packet_filter(&mut self, f: Option<*mut dyn IpPacketFilter>) {
        self._packet_filter = f;
    }

    /// The packet filter.
    pub fn packet_filter(&self) -> Option<*mut dyn IpPacketFilter> {
        self._packet_filter
    }

    /// Drops the oldest partially reassembled datagrams while the reassembly memory is above
    /// the high threshold, down to the low threshold.
    fn frag_limit_mem(&mut self) {
        if self._frag_mem <= Self::FRAG_HIGH_THRESH {
            return;
        }
        let mut drop = self._frag_mem - Self::FRAG_LOW_THRESH;
        while drop != 0 {
            let Some(frag_id) = self._frags_age.pop_front() else {
                return;
            };
            // Drop the oldest frag (first element) from _frags_age

            // Drop from _frags as well
            let dropped_size = self._frags.entry(frag_id).or_default().mem_size;
            self.frag_drop(frag_id, dropped_size);

            drop -= std::cmp::min(drop, dropped_size);
        }
    }

    fn frag_timeout(&mut self) {
        if self._frags.is_empty() {
            return;
        }
        let now = LowresClock::now();
        while let Some(&frag_id) = self._frags_age.front() {
            let frag = self._frags.entry(frag_id).or_default();
            if now > frag.rx_time + Self::FRAG_TIMEOUT {
                let dropped_size = frag.mem_size;
                // Drop from _frags
                self.frag_drop(frag_id, dropped_size);
                // Drop from _frags_age
                self._frags_age.pop_front();
            } else {
                // The further items can only be younger
                break;
            }
        }
        if !self._frags.is_empty() {
            self.frag_arm_at(now);
        } else {
            self._frag_mem = 0;
        }
    }

    fn frag_drop(&mut self, frag_id: Ipv4FragId, dropped_size: u32) {
        self._frags.remove(&frag_id);
        self._frag_mem = self._frag_mem.wrapping_sub(dropped_size);
    }
}

impl Frag {
    fn merge(&mut self, h: &IpHdr, offset: u16, mut p: Packet) -> i32 {
        let old = self.mem_size;
        let ip_hdr_len = h.ihl() as usize * 4;
        // Store IP header
        if offset == 0 {
            self.header = p.share_range(0, ip_hdr_len);
        }
        // Sotre IP payload
        p.trim_front(ip_hdr_len);
        self.data.merge(offset as u32, p);
        // Update mem size
        self.mem_size = if self.header.is_valid() { self.header.memory() } else { 0 };
        for x in self.data.map.values() {
            self.mem_size += x.memory();
        }
        let added_size = self.mem_size.wrapping_sub(old);
        added_size as i32
    }

    fn is_complete(&self) -> bool {
        // If all the fragments are received, the merger has merged them into a single
        // packet
        let offset = self.data.map.keys().next().copied().unwrap_or(1);
        let nr_packet = self.data.map.len();
        self.last_frag_received && nr_packet == 1 && offset == 0
    }

    fn get_assembled_packet(&mut self, from: EthernetAddress, to: EthernetAddress) -> Packet {
        let mut ip_header = std::mem::replace(&mut self.header, Packet::make_null_packet());
        let ip_data = match self.data.map.values_mut().next() {
            Some(d) => std::mem::replace(d, Packet::make_null_packet()),
            None => Packet::make_null_packet(),
        };
        // Append a ethernet header, needed for forwarding
        let ehp = ip_header.prepend_header::<EthHdr>(0);
        // SAFETY: prepend_header returned room for an EthHdr (align 1).
        let eh = unsafe { &mut *ehp };
        eh.src_mac = from;
        eh.dst_mac = to;
        eh.eth_proto = EthProtocolNum::ipv4 as u16;
        *eh = hton_struct(eh);
        // Prepare a packet contains both ethernet header, ip header and ip data
        ip_header.append(ip_data);
        let mut pkt = ip_header;
        let iph = pkt.get_header::<IpHdr>(std::mem::size_of::<EthHdr>());
        // SAFETY: the header was stored by merge(), so the packet holds a whole IpHdr.
        let iph = unsafe { &mut *iph };
        // len is the sum of each fragment
        iph.len = hton((pkt.len() as usize - std::mem::size_of::<EthHdr>()) as u16);
        // No fragmentation for the assembled datagram
        iph.frag = 0;
        // Since each fragment's csum is checked, no need to csum
        // again for the assembled datagram
        let oi = OffloadInfo { reassembled: true, ..Default::default() };
        pkt.set_offload_info(oi);
        pkt
    }
}

impl Icmp {
    /// Answers an echo request.
    pub fn received(&mut self, mut p: Packet, from: Ipv4Address, _to: Ipv4Address) {
        let hdrp = p.get_header::<IcmpHdr>(0);
        if hdrp.is_null() {
            return;
        }
        // SAFETY: get_header returned a whole IcmpHdr (align 1) at offset 0; the
        // header is written through the pointer only, and the checksum reads the packet's
        // fragments after those writes.
        unsafe {
            if (*hdrp).type_ != IcmpMsgType::EchoRequest as u8 {
                return;
            }
            (*hdrp).type_ = IcmpMsgType::EchoReply as u8;
            (*hdrp).code = 0;
            (*hdrp).csum = 0;
            let mut csum = Checksummer::default();
            // The sum goes over the fragments of the whole message, which starts at the header
            // (offset 0), so a reassembled (fragmented) ping is summed correctly.
            csum.sum_packet(&p);
            (*hdrp).csum = csum.get();
        }

        if self._queue_space.try_wait(p.len() as usize) {
            // drop packets that do not fit the queue
            // FIXME: future is discarded
            let f = self._inet.get_l2_dst_address(from);
            if f.available() {
                // `then` on a ready future runs in line, inside this call: done through `self`,
                // since a write through the root pointer would invalidate it.
                if let Ok(e_dst) = f.get() {
                    self._packetq.push_back(L4Packet {
                        to: from,
                        p,
                        e_dst,
                        proto_num: IpProtocolNum::icmp,
                    });
                }
                return;
            }
            let this = self._this;
            let _ = f.then(move |e_dst: EthernetAddress| {
                // SAFETY: the icmp layer outlives the address resolution; this runs later from
                // the reactor with no reference to it live, through the root pointer.
                unsafe {
                    (*this)._packetq.push_back(L4Packet {
                        to: from,
                        p,
                        e_dst,
                        proto_num: IpProtocolNum::icmp,
                    })
                };
                make_ready_future(())
            });
        }
    }
}
