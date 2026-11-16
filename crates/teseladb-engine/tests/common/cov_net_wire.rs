#![cfg(target_os = "linux")]
// Each test binary that includes this harness uses only part of it.
#![allow(dead_code)]
//! A wire-level harness for the native network stack.
//!
//! [`FakeDevice`] is a network device without hardware: its queue records every frame the
//! stack hands it (bytes and offload metadata), and [`Wire::inject`] delivers a frame to the
//! stack as if the NIC had received it. [`IpStack`] builds an Ethernet interface and an IPv4
//! layer (with ARP, ICMP, UDP and TCP) over such a device, in place and never freed, because
//! the device's transmit poller keeps calling into them for the rest of the process.
//!
//! The frame builders and parsers below are written from the protocol definitions (RFC 791,
//! 792, 793, 768, 826) and do not use the stack's own header types, so the tests check the
//! stack against an independent encoding.
//!
//! Every waiting helper sleeps on the reactor, so they must run inside an engine thread
//! (`engine_thread_test_case!`).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::net::ethernet::EthernetAddress;
use teseladb::engine::net::ip::{Ipv4, Ipv4Traits};
use teseladb::engine::net::ipv4_address::Ipv4Address;
use teseladb::engine::net::native_stack::NativeStackOptions;
use teseladb::engine::net::net::{
    Device, DeviceBase, HwFeatures, Interface, Qp, QpBase, QpPtr, qp_init,
};
use teseladb::engine::net::node_ptr::NodePtr;
use teseladb::engine::net::packet::{OffloadInfo, Packet};
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use teseladb::engine::net::tcp::Tcp;

/// The MAC address of the stack under test.
pub const STACK_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];
/// The MAC address of the simulated peer.
pub const PEER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];
/// The MAC address of the simulated gateway.
pub const GW_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0xfe];
/// The broadcast MAC address.
pub const BCAST_MAC: [u8; 6] = [0xff; 6];

/// 10.0.0.2, the address of the stack under test.
pub const STACK_IP: u32 = 0x0a00_0002;
/// 10.0.0.1, the simulated peer on the local subnet.
pub const PEER_IP: u32 = 0x0a00_0001;
/// 10.0.0.254, the default gateway.
pub const GW_IP: u32 = 0x0a00_00fe;
/// 255.255.255.0.
pub const NETMASK: u32 = 0xffff_ff00;

/// EtherType of IPv4.
pub const ETH_IPV4: u16 = 0x0800;
/// EtherType of ARP.
pub const ETH_ARP: u16 = 0x0806;

/// IP protocol numbers.
pub const PROTO_ICMP: u8 = 1;
/// TCP.
pub const PROTO_TCP: u8 = 6;
/// UDP.
pub const PROTO_UDP: u8 = 17;

/// TCP flag bits.
pub const FIN: u8 = 0x01;
/// SYN.
pub const SYN: u8 = 0x02;
/// RST.
pub const RST: u8 = 0x04;
/// PSH.
pub const PSH: u8 = 0x08;
/// ACK.
pub const ACK: u8 = 0x10;
/// URG.
pub const URG: u8 = 0x20;

/// How long a helper waits for an expected frame or future before failing the test.
pub const WAIT: Duration = Duration::from_secs(5);

/// Locks `m`, ignoring poisoning (a failed assertion elsewhere must not hide the frames).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sleeps `ms` milliseconds on the reactor (inside an engine thread).
pub fn sleep_ms(ms: u64) {
    sleep_steady(Duration::from_millis(ms)).get().expect("sleep");
}

/// Waits (inside an engine thread) until `f` resolves, failing the test after [`WAIT`].
pub fn wait<T: 'static>(f: Future<T>) -> Result<T, ExceptionPtr> {
    wait_for(f, WAIT)
}

/// Waits until `f` resolves, failing the test after `limit`.
pub fn wait_for<T: 'static>(f: Future<T>, limit: Duration) -> Result<T, ExceptionPtr> {
    let deadline = Instant::now() + limit;
    while !f.available() {
        assert!(Instant::now() < deadline, "a future did not resolve within {limit:?}");
        sleep_ms(1);
    }
    f.get()
}

/// The bytes of `p`, fragment after fragment.
pub fn packet_bytes(p: &Packet) -> Vec<u8> {
    let mut out = Vec::with_capacity(p.len() as usize);
    for f in p.fragments().iter() {
        // SAFETY: a fragment of a live packet points at `size` readable bytes.
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(f.base, f.size) });
    }
    out
}

/// A frame the stack handed to the device.
#[derive(Clone, Debug)]
pub struct Frame {
    /// The frame's bytes, Ethernet header included.
    pub bytes: Vec<u8>,
    /// The offload metadata the stack attached.
    pub oi: OffloadInfo,
    /// How many fragments the packet had.
    pub nr_frags: u32,
}

impl Frame {
    /// The destination MAC address.
    pub fn dst_mac(&self) -> [u8; 6] {
        self.bytes[0..6].try_into().expect("6 bytes")
    }
    /// The source MAC address.
    pub fn src_mac(&self) -> [u8; 6] {
        self.bytes[6..12].try_into().expect("6 bytes")
    }
    /// The EtherType.
    pub fn ethertype(&self) -> u16 {
        u16::from_be_bytes([self.bytes[12], self.bytes[13]])
    }
    /// The payload after the Ethernet header.
    pub fn payload(&self) -> &[u8] {
        &self.bytes[14..]
    }
    /// Whether the frame is an IPv4 packet.
    pub fn is_ipv4(&self) -> bool {
        self.ethertype() == ETH_IPV4
    }
    /// Whether the frame is an ARP packet.
    pub fn is_arp(&self) -> bool {
        self.ethertype() == ETH_ARP
    }
    /// The IPv4 packet.
    ///
    /// # Panics
    /// If the frame is not IPv4.
    pub fn ip(&self) -> IpView {
        assert!(self.is_ipv4(), "not an IPv4 frame: ethertype {:#x}", self.ethertype());
        parse_ip(self.payload())
    }
    /// The ARP packet.
    pub fn arp(&self) -> ArpView {
        assert!(self.is_arp(), "not an ARP frame: ethertype {:#x}", self.ethertype());
        parse_arp(self.payload())
    }
    /// The TCP segment of an IPv4 frame.
    pub fn tcp(&self) -> TcpView {
        let ip = self.ip();
        assert_eq!(ip.proto, PROTO_TCP, "not a TCP packet");
        parse_tcp(&ip.payload)
    }
    /// Whether the L4 checksum of an IPv4 frame verifies against its pseudo header.
    pub fn l4_csum_ok(&self) -> bool {
        let ip = self.ip();
        l4_checksum(ip.src, ip.dst, ip.proto, &ip.payload) == 0
    }
    /// The UDP datagram of an IPv4 frame.
    pub fn udp(&self) -> UdpView {
        let ip = self.ip();
        assert_eq!(ip.proto, PROTO_UDP, "not a UDP packet");
        parse_udp(&ip.payload)
    }
}

/// The queue of a [`FakeDevice`]: records what the stack sends.
pub struct FakeQp {
    base: QpBase,
    sent: Arc<Mutex<VecDeque<Frame>>>,
}

impl Qp for FakeQp {
    fn base(&self) -> &QpBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut QpBase {
        &mut self.base
    }
    fn send(&mut self, p: Packet) -> Future<()> {
        let frame = Frame { bytes: packet_bytes(&p), oi: p.get_offload_info(), nr_frags: p.nr_frags() };
        self.base._stats.tx.good.update_frags_stats(p.nr_frags() as u64, p.len() as u64);
        lock(&self.sent).push_back(frame);
        make_ready_future(())
    }
}

/// Makes a recording queue, finished (`qp_init`) at its final address.
pub fn make_qp(qid: u16, sent: Arc<Mutex<VecDeque<Frame>>>) -> QpPtr {
    let node = NodePtr::new(FakeQp { base: QpBase::new("cov_net_fake", qid as u8), sent });
    let raw: *mut dyn Qp = node.into_raw();
    // SAFETY: `raw` is the root pointer of the queue just leaked from its node; it never moves
    // and nothing else refers to it. Ownership goes back to a `NodePtr` right after.
    unsafe {
        qp_init(raw, true);
        NodePtr::from_raw(raw)
    }
}

/// A network device without hardware.
pub struct FakeDevice {
    base: DeviceBase,
    mac: EthernetAddress,
    hw: HwFeatures,
    queues: u16,
    sent: Arc<Mutex<VecDeque<Frame>>>,
}

impl Device for FakeDevice {
    fn base(&self) -> &DeviceBase {
        &self.base
    }
    fn hw_address(&self) -> EthernetAddress {
        self.mac
    }
    fn hw_features(&self) -> HwFeatures {
        self.hw
    }
    fn hw_queues_count(&self) -> u16 {
        self.queues
    }
    fn init_local_queue(&self, _opts: &NativeStackOptions, qid: u16) -> Result<QpPtr, ExceptionPtr> {
        Ok(make_qp(qid, self.sent.clone()))
    }
}

/// A fake device with its queue registered on the current shard.
pub struct Wire {
    /// The device.
    pub dev: Arc<dyn Device>,
    /// The root pointer of the current shard's queue.
    pub qp: *mut dyn Qp,
    sent: Arc<Mutex<VecDeque<Frame>>>,
}

impl Wire {
    /// A device with `hw` and one queue.
    pub fn new(hw: HwFeatures) -> Wire {
        Self::build(hw, 1, None)
    }

    /// A device whose queue sends on behalf of the shards in `weights` (see
    /// `<dyn Qp>::configure_proxies`).
    pub fn with_proxies(hw: HwFeatures, weights: &BTreeMap<u32, f32>) -> Wire {
        Self::build(hw, 1, Some(weights))
    }

    /// A device with `queues` hardware queues (only the current shard's is created).
    pub fn with_queues(hw: HwFeatures, queues: u16) -> Wire {
        Self::build(hw, queues, None)
    }

    fn build(hw: HwFeatures, queues: u16, weights: Option<&BTreeMap<u32, f32>>) -> Wire {
        let sent = Arc::new(Mutex::new(VecDeque::new()));
        let dev: Arc<dyn Device> = Arc::new(FakeDevice {
            base: DeviceBase::default(),
            mac: EthernetAddress { mac: STACK_MAC },
            hw,
            queues,
            sent: sent.clone(),
        });
        let opts = NativeStackOptions::new();
        let qp = dev.init_local_queue(&opts, 0).expect("the fake queue");
        let raw = qp.as_ptr();
        if let Some(w) = weights {
            // SAFETY: the queue's root pointer; it lives in its node and nothing refers to it.
            unsafe { <dyn Qp>::configure_proxies(raw, w) };
        }
        dev.set_local_queue(qp);
        Wire { dev, qp: raw, sent }
    }

    /// Delivers `frame` to the stack as a received frame.
    pub fn inject(&self, frame: &[u8]) {
        self.dev.l2receive(Packet::from_data(frame));
    }

    /// Delivers `p` to the stack as a received frame.
    pub fn inject_packet(&self, p: Packet) {
        self.dev.l2receive(p);
    }

    /// The oldest recorded frame, if any.
    pub fn take(&self) -> Option<Frame> {
        lock(&self.sent).pop_front()
    }

    /// Lets the transmit poller run, then returns and forgets every recorded frame.
    pub fn drain(&self) -> Vec<Frame> {
        sleep_ms(20);
        lock(&self.sent).drain(..).collect()
    }

    /// Forgets every recorded frame after letting the poller run.
    pub fn clear(&self) {
        let _ = self.drain();
    }

    /// Waits for `d` and returns what was sent meanwhile.
    pub fn quiet_for(&self, d: Duration) -> Vec<Frame> {
        sleep_steady(d).get().expect("sleep");
        lock(&self.sent).drain(..).collect()
    }

    /// The next recorded frame that satisfies `pred`, skipping the others; fails the test if
    /// none comes within `limit`.
    pub fn next_where(&self, limit: Duration, pred: impl Fn(&Frame) -> bool) -> Frame {
        let deadline = Instant::now() + limit;
        loop {
            while let Some(f) = self.take() {
                if pred(&f) {
                    return f;
                }
            }
            assert!(Instant::now() < deadline, "no matching frame was sent within {limit:?}");
            sleep_ms(1);
        }
    }

    /// The next frame, whatever it is.
    pub fn next(&self) -> Frame {
        self.next_where(WAIT, |_| true)
    }

    /// The next IPv4 frame.
    pub fn next_ip(&self) -> Frame {
        self.next_where(WAIT, |f| f.is_ipv4())
    }

    /// The next ARP frame.
    pub fn next_arp(&self) -> Frame {
        self.next_where(WAIT, |f| f.is_arp())
    }

    /// The next TCP segment.
    pub fn next_tcp(&self) -> TcpView {
        self.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_TCP).tcp()
    }

    /// The next TCP segment that satisfies `pred`.
    pub fn next_tcp_where(&self, pred: impl Fn(&TcpView) -> bool) -> TcpView {
        self.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_TCP && pred(&f.tcp())).tcp()
    }

    /// Asserts that no TCP segment is sent within `d`.
    pub fn assert_no_tcp_for(&self, d: Duration) {
        let frames = self.quiet_for(d);
        let tcp: Vec<TcpView> =
            frames.iter().filter(|f| f.is_ipv4() && f.ip().proto == PROTO_TCP).map(|f| f.tcp()).collect();
        assert!(tcp.is_empty(), "unexpected TCP segments: {tcp:?}");
    }
}

/// An IPv4 layer (with ARP, ICMP, UDP and TCP) and its interface over a [`Wire`].
pub struct IpStack {
    /// The device.
    pub wire: Wire,
    /// The interface (leaked).
    pub netif: *mut Interface,
    /// The IPv4 layer (built in place, leaked).
    pub ip: *mut Ipv4,
}

impl IpStack {
    /// A stack at 10.0.0.2/24 with gateway 10.0.0.254 over a device with `hw`.
    pub fn new(hw: HwFeatures) -> IpStack {
        let s = Self::unconfigured_on(Wire::new(hw));
        s.configure();
        s
    }

    /// A stack at 10.0.0.2/24 over `wire`.
    pub fn on(wire: Wire) -> IpStack {
        let s = Self::unconfigured_on(wire);
        s.configure();
        s
    }

    /// A stack without addresses (all 0.0.0.0) over a device with `hw`.
    pub fn unconfigured(hw: HwFeatures) -> IpStack {
        Self::unconfigured_on(Wire::new(hw))
    }

    fn unconfigured_on(wire: Wire) -> IpStack {
        let netif = Interface::new(wire.dev.clone()).into_raw();
        let slot: *mut Ipv4 = Box::into_raw(Box::<Ipv4>::new_uninit()).cast();
        // SAFETY: `slot` is a fresh heap block for an Ipv4 that is never moved nor freed, and
        // the interface is leaked, so it outlives the layer.
        unsafe { Ipv4::init_at(slot, netif) };
        IpStack { wire, netif, ip: slot }
    }

    fn configure(&self) {
        let ip = self.ipv4();
        ip.set_host_address(Ipv4Address::from_u32(STACK_IP));
        ip.set_netmask_address(Ipv4Address::from_u32(NETMASK));
        ip.set_gw_address(Ipv4Address::from_u32(GW_IP));
    }

    /// The IPv4 layer, for a short access from the test (no stack code runs meanwhile).
    #[allow(clippy::mut_from_ref)]
    pub fn ipv4(&self) -> &mut Ipv4 {
        // SAFETY: the layer is never freed; the test thread only touches it between reactor
        // tasks, so no other reference to it is live.
        unsafe { &mut *self.ip }
    }

    /// The TCP layer.
    pub fn tcp(&self) -> *mut Tcp<Ipv4Traits> {
        self.ipv4().get_tcp()
    }

    /// Makes the stack learn the MAC address of `ip` (in its subnet) by receiving a UDP
    /// datagram from it for an unbound port, then forgets the frames sent meanwhile.
    pub fn teach(&self, ip: u32, mac: [u8; 6]) {
        self.wire.inject(&hello_from(ip, mac));
        self.wire.clear();
    }

    /// Makes the stack learn the peer's MAC address.
    pub fn teach_peer(&self) {
        self.teach(PEER_IP, PEER_MAC);
    }
}

/// The socket address `ip:port`.
pub fn sockaddr(ip: u32, port: u16) -> SocketAddress {
    SocketAddress::from_ipv4_addr(Ipv4Addr { ip, port })
}

// ---------------------------------------------------------------------------------------
// Frame builders.
// ---------------------------------------------------------------------------------------

/// The Internet checksum (RFC 1071) of `data`: the one's complement of the one's complement
/// sum of its 16-bit big-endian words.
pub fn checksum(data: &[u8]) -> u16 {
    let mut s: u32 = 0;
    for c in data.chunks(2) {
        let w = if c.len() == 2 { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], 0]) };
        s += w as u32;
    }
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

/// The checksum of an L4 segment with the IPv4 pseudo header.
pub fn l4_checksum(src: u32, dst: u32, proto: u8, seg: &[u8]) -> u16 {
    let mut v = Vec::with_capacity(12 + seg.len());
    v.extend_from_slice(&src.to_be_bytes());
    v.extend_from_slice(&dst.to_be_bytes());
    v.push(0);
    v.push(proto);
    v.extend_from_slice(&(seg.len() as u16).to_be_bytes());
    v.extend_from_slice(seg);
    checksum(&v)
}

/// An Ethernet frame.
pub fn eth(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(14 + payload.len());
    v.extend_from_slice(&dst);
    v.extend_from_slice(&src);
    v.extend_from_slice(&ethertype.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// The fields of an IPv4 header to build.
#[derive(Clone, Debug)]
pub struct IpSpec {
    /// Source address.
    pub src: u32,
    /// Destination address.
    pub dst: u32,
    /// L4 protocol.
    pub proto: u8,
    /// Identification.
    pub id: u16,
    /// Flags and fragment offset (in 8-byte units), as on the wire.
    pub frag: u16,
    /// Time to live.
    pub ttl: u8,
    /// Options (a multiple of 4 bytes).
    pub options: Vec<u8>,
    /// Total length to write instead of the real one.
    pub len_override: Option<u16>,
    /// Corrupts the header checksum.
    pub bad_csum: bool,
}

impl IpSpec {
    /// A header from `src` to `dst` for `proto`, unfragmented.
    pub fn new(src: u32, dst: u32, proto: u8) -> IpSpec {
        IpSpec {
            src,
            dst,
            proto,
            id: 0x1234,
            frag: 0,
            ttl: 64,
            options: Vec::new(),
            len_override: None,
            bad_csum: false,
        }
    }
    /// With identification `id`.
    pub fn id(mut self, id: u16) -> Self {
        self.id = id;
        self
    }
    /// A fragment at byte offset `offset` (a multiple of 8), with the MF flag `mf`.
    pub fn fragment(mut self, offset: u32, mf: bool) -> Self {
        self.frag = ((offset / 8) as u16) | if mf { 1 << 13 } else { 0 };
        self
    }
}

/// An IPv4 packet with a correct header checksum (unless `spec.bad_csum`).
pub fn ipv4_packet(spec: &IpSpec, payload: &[u8]) -> Vec<u8> {
    assert!(spec.options.len() % 4 == 0);
    let hlen = 20 + spec.options.len();
    let total = spec.len_override.unwrap_or((hlen + payload.len()) as u16);
    let mut h = Vec::with_capacity(hlen + payload.len());
    h.push(0x40 | (hlen / 4) as u8);
    h.push(0);
    h.extend_from_slice(&total.to_be_bytes());
    h.extend_from_slice(&spec.id.to_be_bytes());
    h.extend_from_slice(&spec.frag.to_be_bytes());
    h.push(spec.ttl);
    h.push(spec.proto);
    h.extend_from_slice(&[0, 0]);
    h.extend_from_slice(&spec.src.to_be_bytes());
    h.extend_from_slice(&spec.dst.to_be_bytes());
    h.extend_from_slice(&spec.options);
    let mut c = checksum(&h);
    if spec.bad_csum {
        c ^= 0x5555;
    }
    h[10..12].copy_from_slice(&c.to_be_bytes());
    h.extend_from_slice(payload);
    h
}

/// An IPv4 frame from the peer to the stack.
pub fn ip_frame(spec: &IpSpec, payload: &[u8]) -> Vec<u8> {
    eth(STACK_MAC, PEER_MAC, ETH_IPV4, &ipv4_packet(spec, payload))
}

/// The fields of a TCP header to build.
#[derive(Clone, Debug)]
pub struct TcpSpec {
    /// Source port.
    pub sport: u16,
    /// Destination port.
    pub dport: u16,
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgment number.
    pub ack: u32,
    /// Flag bits.
    pub flags: u8,
    /// Window.
    pub window: u16,
    /// Options (padded to a multiple of 4 bytes by the builder).
    pub options: Vec<u8>,
    /// Data offset to write instead of the real one.
    pub data_offset: Option<u8>,
    /// Corrupts the checksum.
    pub bad_csum: bool,
}

impl TcpSpec {
    /// A segment from `sport` to `dport`.
    pub fn new(sport: u16, dport: u16, seq: u32, ack: u32, flags: u8) -> TcpSpec {
        TcpSpec {
            sport,
            dport,
            seq,
            ack,
            flags,
            window: 60000,
            options: Vec::new(),
            data_offset: None,
            bad_csum: false,
        }
    }
    /// With window `w`.
    pub fn window(mut self, w: u16) -> Self {
        self.window = w;
        self
    }
    /// With options `o`.
    pub fn options(mut self, o: &[u8]) -> Self {
        self.options = o.to_vec();
        self
    }
}

/// A TCP segment (header and payload) from `src` to `dst` with a correct checksum (unless
/// `spec.bad_csum`).
pub fn tcp_segment(src: u32, dst: u32, spec: &TcpSpec, payload: &[u8]) -> Vec<u8> {
    let mut opts = spec.options.clone();
    while opts.len() % 4 != 0 {
        opts.push(0);
    }
    let hlen = 20 + opts.len();
    let mut s = Vec::with_capacity(hlen + payload.len());
    s.extend_from_slice(&spec.sport.to_be_bytes());
    s.extend_from_slice(&spec.dport.to_be_bytes());
    s.extend_from_slice(&spec.seq.to_be_bytes());
    s.extend_from_slice(&spec.ack.to_be_bytes());
    s.push(spec.data_offset.unwrap_or((hlen / 4) as u8) << 4);
    s.push(spec.flags);
    s.extend_from_slice(&spec.window.to_be_bytes());
    s.extend_from_slice(&[0, 0]);
    s.extend_from_slice(&[0, 0]);
    s.extend_from_slice(&opts);
    s.extend_from_slice(payload);
    let mut c = l4_checksum(src, dst, PROTO_TCP, &s);
    if spec.bad_csum {
        c ^= 0x5555;
    }
    s[16..18].copy_from_slice(&c.to_be_bytes());
    s
}

/// A TCP frame from the peer to the stack.
pub fn tcp_frame(spec: &TcpSpec, payload: &[u8]) -> Vec<u8> {
    let seg = tcp_segment(PEER_IP, STACK_IP, spec, payload);
    ip_frame(&IpSpec::new(PEER_IP, STACK_IP, PROTO_TCP), &seg)
}

/// A UDP datagram (header and payload) with a correct checksum.
pub fn udp_datagram(src: u32, dst: u32, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let len = (8 + payload.len()) as u16;
    let mut d = Vec::with_capacity(len as usize);
    d.extend_from_slice(&sport.to_be_bytes());
    d.extend_from_slice(&dport.to_be_bytes());
    d.extend_from_slice(&len.to_be_bytes());
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(payload);
    let c = l4_checksum(src, dst, PROTO_UDP, &d);
    d[6..8].copy_from_slice(&c.to_be_bytes());
    d
}

/// A UDP frame from the peer to the stack.
pub fn udp_frame(sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let d = udp_datagram(PEER_IP, STACK_IP, sport, dport, payload);
    ip_frame(&IpSpec::new(PEER_IP, STACK_IP, PROTO_UDP), &d)
}

/// A UDP datagram from `ip` at `mac` to an unbound port of the stack: the stack drops it but
/// learns the sender's MAC address when `ip` is on its subnet.
pub fn hello_from(ip: u32, mac: [u8; 6]) -> Vec<u8> {
    let udp = udp_datagram(ip, STACK_IP, 9, 9, b"hello");
    eth(STACK_MAC, mac, ETH_IPV4, &ipv4_packet(&IpSpec::new(ip, STACK_IP, PROTO_UDP), &udp))
}

/// An ICMP message of `type_` with identifier `id`, sequence `seq` and `payload`.
pub fn icmp_message(type_: u8, id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut m = vec![type_, 0, 0, 0];
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(payload);
    let c = checksum(&m);
    m[2..4].copy_from_slice(&c.to_be_bytes());
    m
}

/// An ARP packet for IPv4 over Ethernet.
pub fn arp_packet(oper: u16, sha: [u8; 6], spa: u32, tha: [u8; 6], tpa: u32) -> Vec<u8> {
    let mut a = Vec::with_capacity(28);
    a.extend_from_slice(&1u16.to_be_bytes());
    a.extend_from_slice(&ETH_IPV4.to_be_bytes());
    a.push(6);
    a.push(4);
    a.extend_from_slice(&oper.to_be_bytes());
    a.extend_from_slice(&sha);
    a.extend_from_slice(&spa.to_be_bytes());
    a.extend_from_slice(&tha);
    a.extend_from_slice(&tpa.to_be_bytes());
    a
}

/// TCP options: MSS `mss`, window scale `ws`, padded with a NOP.
pub fn mss_ws_options(mss: u16, ws: u8) -> Vec<u8> {
    let m = mss.to_be_bytes();
    vec![2, 4, m[0], m[1], 3, 3, ws, 1]
}

// ---------------------------------------------------------------------------------------
// Frame parsers.
// ---------------------------------------------------------------------------------------

/// A parsed IPv4 packet.
#[derive(Clone, Debug)]
pub struct IpView {
    /// Header length in bytes.
    pub hlen: usize,
    /// Total length.
    pub total_len: u16,
    /// Identification.
    pub id: u16,
    /// Flags and fragment offset word.
    pub frag: u16,
    /// Time to live.
    pub ttl: u8,
    /// L4 protocol.
    pub proto: u8,
    /// The header checksum field.
    pub csum: u16,
    /// Whether the header checksum verifies.
    pub csum_ok: bool,
    /// Source address.
    pub src: u32,
    /// Destination address.
    pub dst: u32,
    /// The payload (up to the total length).
    pub payload: Vec<u8>,
}

impl IpView {
    /// The MF flag.
    pub fn mf(&self) -> bool {
        self.frag & (1 << 13) != 0
    }
    /// The fragment offset in bytes.
    pub fn offset(&self) -> u32 {
        (self.frag & 0x1fff) as u32 * 8
    }
}

/// Parses an IPv4 packet.
pub fn parse_ip(b: &[u8]) -> IpView {
    assert!(b.len() >= 20, "IPv4 packet too short: {} bytes", b.len());
    assert_eq!(b[0] >> 4, 4, "not IPv4");
    let hlen = (b[0] & 0x0f) as usize * 4;
    let total_len = u16::from_be_bytes([b[2], b[3]]);
    IpView {
        hlen,
        total_len,
        id: u16::from_be_bytes([b[4], b[5]]),
        frag: u16::from_be_bytes([b[6], b[7]]),
        ttl: b[8],
        proto: b[9],
        csum: u16::from_be_bytes([b[10], b[11]]),
        csum_ok: checksum(&b[..hlen]) == 0,
        src: u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
        dst: u32::from_be_bytes([b[16], b[17], b[18], b[19]]),
        payload: b[hlen..(total_len as usize).min(b.len())].to_vec(),
    }
}

/// A parsed TCP segment.
#[derive(Clone, Debug)]
pub struct TcpView {
    /// Source port.
    pub sport: u16,
    /// Destination port.
    pub dport: u16,
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgment number.
    pub ack: u32,
    /// Header length in bytes.
    pub hlen: usize,
    /// Flag bits.
    pub flags: u8,
    /// Window.
    pub window: u16,
    /// The checksum field.
    pub checksum: u16,
    /// The option bytes.
    pub options: Vec<u8>,
    /// The payload.
    pub payload: Vec<u8>,
}

impl TcpView {
    /// Whether all of `f` are set.
    pub fn has(&self, f: u8) -> bool {
        self.flags & f == f
    }
    /// The MSS option, if present.
    pub fn mss(&self) -> Option<u16> {
        find_option(&self.options, 2).map(|o| u16::from_be_bytes([o[0], o[1]]))
    }
    /// The window scale option, if present.
    pub fn win_scale(&self) -> Option<u8> {
        find_option(&self.options, 3).map(|o| o[0])
    }
}

/// The data of TCP option `kind` in `opts`.
fn find_option(opts: &[u8], kind: u8) -> Option<Vec<u8>> {
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => return None,
            1 => i += 1,
            k => {
                let len = *opts.get(i + 1)? as usize;
                if len < 2 || i + len > opts.len() {
                    return None;
                }
                if k == kind {
                    return Some(opts[i + 2..i + len].to_vec());
                }
                i += len;
            }
        }
    }
    None
}

/// Parses a TCP segment.
pub fn parse_tcp(b: &[u8]) -> TcpView {
    assert!(b.len() >= 20, "TCP segment too short: {} bytes", b.len());
    let hlen = (b[12] >> 4) as usize * 4;
    TcpView {
        sport: u16::from_be_bytes([b[0], b[1]]),
        dport: u16::from_be_bytes([b[2], b[3]]),
        seq: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        ack: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        hlen,
        flags: b[13],
        window: u16::from_be_bytes([b[14], b[15]]),
        checksum: u16::from_be_bytes([b[16], b[17]]),
        options: b[20..hlen].to_vec(),
        payload: b[hlen..].to_vec(),
    }
}

/// A parsed UDP datagram.
#[derive(Clone, Debug)]
pub struct UdpView {
    /// Source port.
    pub sport: u16,
    /// Destination port.
    pub dport: u16,
    /// Length field.
    pub len: u16,
    /// Checksum field.
    pub cksum: u16,
    /// Payload.
    pub payload: Vec<u8>,
}

/// Parses a UDP datagram.
pub fn parse_udp(b: &[u8]) -> UdpView {
    assert!(b.len() >= 8, "UDP datagram too short");
    UdpView {
        sport: u16::from_be_bytes([b[0], b[1]]),
        dport: u16::from_be_bytes([b[2], b[3]]),
        len: u16::from_be_bytes([b[4], b[5]]),
        cksum: u16::from_be_bytes([b[6], b[7]]),
        payload: b[8..].to_vec(),
    }
}

/// A parsed ARP packet.
#[derive(Clone, Debug)]
pub struct ArpView {
    /// Hardware type.
    pub htype: u16,
    /// Protocol type.
    pub ptype: u16,
    /// Operation.
    pub oper: u16,
    /// Sender MAC.
    pub sha: [u8; 6],
    /// Sender IPv4 address.
    pub spa: u32,
    /// Target MAC.
    pub tha: [u8; 6],
    /// Target IPv4 address.
    pub tpa: u32,
}

/// Parses an ARP packet for IPv4 over Ethernet.
pub fn parse_arp(b: &[u8]) -> ArpView {
    assert!(b.len() >= 28, "ARP packet too short");
    assert_eq!(b[4], 6);
    assert_eq!(b[5], 4);
    ArpView {
        htype: u16::from_be_bytes([b[0], b[1]]),
        ptype: u16::from_be_bytes([b[2], b[3]]),
        oper: u16::from_be_bytes([b[6], b[7]]),
        sha: b[8..14].try_into().expect("6 bytes"),
        spa: u32::from_be_bytes([b[14], b[15], b[16], b[17]]),
        tha: b[18..24].try_into().expect("6 bytes"),
        tpa: u32::from_be_bytes([b[24], b[25], b[26], b[27]]),
    }
}
