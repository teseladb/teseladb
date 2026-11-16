//! A minimal DHCP client (RFC 2131, options of RFC 2132) for the native network stack.
//!
//! [`Dhcp`] runs a discover/request exchange (or a renewal) on an IPv4 layer and resolves to
//! the [`Lease`] the server acknowledged, or to `None` on timeout (a NAK restarts the
//! discovery). It works on the IPv4
//! layer rather than on an interface: while it runs, it installs a packet filter on the IPv4
//! layer of every shard that captures the DHCP replies (forwarding them to shard 0, where
//! the client lives), so other traffic keeps flowing but more slowly. It does not configure
//! the IPv4 layer; applying the lease is up to the caller.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::engine::core::future::{ExceptionPtr, Future, Promise, make_ready_future};
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::smp::Smp;
use crate::engine::core::timer::Timer;
use crate::engine::net::api::UdpChannel;
use crate::engine::net::byteorder::{
    AdjustEndianness, EndianVisitor, hton_struct, htonl, ntoh_struct, ntohl, ntohs,
};
use crate::engine::net::const_::IpProtocolNum;
use crate::engine::net::ethernet::EthernetAddress;
use crate::engine::net::ip::{IpHdr, IpPacketFilter, Ipv4};
use crate::engine::net::ipv4_address::Ipv4Address;
use crate::engine::net::node_ptr::{CrossShard, NodePtr};
use crate::engine::net::packet::Packet;
use crate::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use crate::engine::net::udp::UdpHdr;
use std::pin::Pin;

/// A DHCP client bound to one IPv4 layer.
///
/// It runs a discover/request sequence on the layer: during the exchange the layer is
/// partly taken over through a packet filter, still usable but less efficient. The client
/// does not modify the layer; it only sends queries and records the replies for the NIC, and
/// the caller decides how to use the returned lease.
pub struct Dhcp {
    _impl: NodePtr<DhcpNode>,
}

/// The configuration a DHCP server leased to us.
#[derive(Clone, Debug, Default)]
pub struct Lease {
    /// Our address.
    pub ip: Ipv4Address,
    /// The netmask.
    pub netmask: Ipv4Address,
    /// The broadcast address.
    pub broadcast: Ipv4Address,

    /// The default gateway.
    pub gateway: Ipv4Address,
    /// The server that leased the address.
    pub dhcp_server: Ipv4Address,

    /// The DNS servers.
    pub name_servers: Vec<Ipv4Address>,

    /// How long the lease lasts.
    pub lease_time: Duration,
    /// When to renew it.
    pub renew_time: Duration,
    /// When to rebind it.
    pub rebind_time: Duration,

    /// The MTU of the link, 0 if not given.
    pub mtu: u16,
}

/// The result of a DHCP exchange: the lease, or `None` if it failed or timed out.
pub type ResultType = Future<Option<Lease>>;

/// Prints a line prefixed with `DHCP ` to standard output.
macro_rules! dhcp_log {
    ($($arg:tt)*) => {
        println!("DHCP {}", format_args!($($arg)*))
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
// The state names follow the DHCP client states.
#[allow(clippy::upper_case_acronyms)]
enum State {
    NONE,
    DISCOVER,
    REQUEST,
    DONE,
    FAIL,
}

impl State {
    /// The state stored in the shared byte (see `DhcpFilter::_state`).
    #[inline]
    fn from_u8(v: u8) -> State {
        match v {
            1 => State::DISCOVER,
            2 => State::REQUEST,
            3 => State::DONE,
            4 => State::FAIL,
            _ => State::NONE,
        }
    }
}

/// BOOTP `op` code of a client request.
const BOOTREQUEST: u8 = 1;
/// BOOTP `htype` of Ethernet.
const HTYPE_ETHERNET: u8 = 1;

/// DHCP option codes (RFC 2132, and RFC 3442 for classless routes).
#[allow(dead_code)]
mod opt_type {
    /// Padding.
    pub const PAD: u8 = 0;
    /// Subnet mask.
    pub const SUBNET_MASK: u8 = 1;
    /// Default routers.
    pub const ROUTER: u8 = 3;
    /// DNS servers.
    pub const DOMAIN_NAME_SERVERS: u8 = 6;
    /// Interface MTU.
    pub const INTERFACE_MTU: u8 = 26;
    /// Broadcast address.
    pub const BROADCAST_ADDRESS: u8 = 28;
    /// Requested IP address.
    pub const REQUESTED_ADDRESS: u8 = 50;
    /// IP address lease time.
    pub const LEASE_TIME: u8 = 51;
    /// DHCP message type.
    pub const MESSAGE_TYPE: u8 = 53;
    /// Server identifier.
    pub const DHCP_SERVER: u8 = 54;
    /// Parameter request list.
    pub const PARAMETER_REQUEST_LIST: u8 = 55;
    /// Renewal (T1) time.
    pub const RENEWAL_TIME: u8 = 58;
    /// Rebinding (T2) time.
    pub const REBINDING_TIME: u8 = 59;
    /// Classless static routes.
    pub const CLASSLESS_ROUTE: u8 = 121;
    /// End of options.
    pub const END: u8 = 255;
}

/// DHCP message types (option 53; RFC 2131 and RFC 4388 for lease query).
#[allow(dead_code)]
mod msg_type {
    /// DHCPDISCOVER.
    pub const DISCOVER: u8 = 1;
    /// DHCPOFFER.
    pub const OFFER: u8 = 2;
    /// DHCPREQUEST.
    pub const REQUEST: u8 = 3;
    /// DHCPDECLINE.
    pub const DECLINE: u8 = 4;
    /// DHCPACK.
    pub const ACK: u8 = 5;
    /// DHCPNAK.
    pub const NAK: u8 = 6;
    /// DHCPRELEASE.
    pub const RELEASE: u8 = 7;
    /// DHCPINFORM.
    pub const INFORM: u8 = 8;
    /// DHCPLEASEQUERY.
    pub const LEASEQUERY: u8 = 10;
    /// DHCPLEASEUNASSIGNED.
    pub const LEASEUNASSIGNED: u8 = 11;
    /// DHCPLEASEUNKNOWN.
    pub const LEASEUNKNOWN: u8 = 12;
    /// DHCPLEASEACTIVE.
    pub const LEASEACTIVE: u8 = 13;
    /// Not a valid message type.
    pub const INVALID: u8 = 255;
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DhcpHeader {
    op: u8,              // Message op code / message type.
    type_: u8,           // Hardware address type
    hlen: u8,            // Hardware address length
    hops: u8,            // Client sets to zero, used by relay agents
    xid: u32,            // Client sets Transaction ID, a random number
    secs: u16,           // Client sets seconds elapsed since op start
    flags: u16,          // Flags
    ciaddr: Ipv4Address, // Client IP address
    yiaddr: Ipv4Address, // 'your' (client) IP address.
    siaddr: Ipv4Address, // IP address of next server to use in bootstrap
    giaddr: Ipv4Address, // Relay agent IP address
    chaddr: [u8; 16],    // Client hardware address.
    sname: [u8; 64],     // unused
    file: [u8; 128],     // unused
}

impl Default for DhcpHeader {
    fn default() -> Self {
        DhcpHeader {
            op: BOOTREQUEST,
            type_: HTYPE_ETHERNET,
            hlen: 6,
            hops: 0,
            xid: 0,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Address::default(),
            yiaddr: Ipv4Address::default(),
            siaddr: Ipv4Address::default(),
            giaddr: Ipv4Address::default(),
            chaddr: [0; 16],
            sname: [0; 64],
            file: [0; 128],
        }
    }
}

impl AdjustEndianness for DhcpHeader {
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        crate::adjust_fields!(a, self.xid, self.secs, self.flags);
        for addr in [
            &mut self.ciaddr as *mut Ipv4Address,
            &mut self.yiaddr,
            &mut self.siaddr,
            &mut self.giaddr,
        ] {
            // SAFETY: Ipv4Address is packed (align 1), so the pointers are valid for reads
            // and writes even though the field is in a packed struct.
            let mut v = unsafe { addr.read_unaligned() };
            v.adjust_endianness(a);
            // SAFETY: as above.
            unsafe { addr.write_unaligned(v) };
        }
    }
}

type ReqOptType = [u8; 5];

const REQUESTED_OPTIONS: ReqOptType = [
    opt_type::SUBNET_MASK,
    opt_type::ROUTER,
    opt_type::DOMAIN_NAME_SERVERS,
    opt_type::INTERFACE_MTU,
    opt_type::BROADCAST_ADDRESS,
];

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct OptionMark {
    type_: u8,
}

impl Default for OptionMark {
    fn default() -> Self {
        OptionMark { type_: opt_type::END }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DhcpOption {
    type_: u8,
    len: u8,
}

impl DhcpOption {
    fn new(t: u8, l: u8) -> Self {
        DhcpOption { type_: t, len: l }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct TypeOption {
    base: DhcpOption,
    type_: u8,
}

impl TypeOption {
    fn new(t: u8) -> Self {
        TypeOption { base: DhcpOption::new(opt_type::MESSAGE_TYPE, 1), type_: t }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct MtuOption {
    base: DhcpOption,
    mtu: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IpOption {
    base: DhcpOption,
    ip: u32,
}

impl IpOption {
    fn new(t: u8, ip: &Ipv4Address) -> Self {
        let v = ip.ip;
        IpOption { base: DhcpOption::new(t, std::mem::size_of::<u32>() as u8), ip: htonl(v) }
    }
}

impl Default for IpOption {
    fn default() -> Self {
        IpOption::new(opt_type::BROADCAST_ADDRESS, &Ipv4Address::default())
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct TimeOption {
    base: DhcpOption,
    time: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct RequestedOption {
    base: DhcpOption,
    req: ReqOptType,
}

impl Default for RequestedOption {
    fn default() -> Self {
        RequestedOption {
            base: DhcpOption::new(opt_type::PARAMETER_REQUEST_LIST, REQUESTED_OPTIONS.len() as u8),
            req: REQUESTED_OPTIONS,
        }
    }
}

const CLIENT_PORT: u16 = 68;
const SERVER_PORT: u16 = 67;

type MagicTag = [u8; 4];

const OPTIONS_MAGIC: MagicTag = [0x63, 0x82, 0x53, 0x63];

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DhcpPayload {
    bootp: DhcpHeader,
    magic: MagicTag,
}

impl AdjustEndianness for DhcpPayload {
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        let mut b = self.bootp;
        b.adjust_endianness(a);
        self.bootp = b;
    }
}

impl Default for DhcpPayload {
    fn default() -> Self {
        DhcpPayload { bootp: DhcpHeader::default(), magic: OPTIONS_MAGIC }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct DhcpPacketBase {
    dhp: DhcpPayload,
}

/// A DHCP message with its options: what `send` serializes.
trait DhcpPacket: Copy {
    fn base(&mut self) -> &mut DhcpPacketBase;
}

/// A lease parsed from a server message, plus the message type.
#[derive(Clone, Default)]
struct IpInfo {
    lease: Lease,
    type_: u8,
}

impl IpInfo {
    fn set_ip(&mut self, type_: u8, ip: Ipv4Address) {
        match type_ {
            opt_type::SUBNET_MASK => self.lease.netmask = ip,
            opt_type::ROUTER => self.lease.gateway = ip,
            opt_type::BROADCAST_ADDRESS => self.lease.broadcast = ip,
            opt_type::DHCP_SERVER => self.lease.dhcp_server = ip,
            opt_type::DOMAIN_NAME_SERVERS => self.lease.name_servers.push(ip),
            _ => {}
        }
    }

    fn set_time(&mut self, type_: u8, s: Duration) {
        match type_ {
            opt_type::LEASE_TIME => self.lease.lease_time = s,
            opt_type::RENEWAL_TIME => self.lease.renew_time = s,
            opt_type::REBINDING_TIME => self.lease.rebind_time = s,
            _ => {}
        }
    }

    fn parse_options(&mut self, p: &mut Packet, mut off: usize) {
        loop {
            let m = p.get_header::<OptionMark>(off);
            // SAFETY: `get_header` returns null or a pointer covering a whole header of the
            // requested type inside the packet's linear data; the header types are packed
            // (align 1), so reading through a non-null pointer is sound. The `is_null` check
            // runs first, and `p` is not modified while the pointer is used.
            if m.is_null() || unsafe { (*m).type_ } == opt_type::END {
                break;
            }
            let o = p.get_header::<DhcpOption>(off);
            if o.is_null() {
                // TODO: report broken packet?
                break;
            }
            // SAFETY: `get_header` returned a non-null pointer, so it covers a whole header of this
            // type inside the packet's linear data; the header types are packed (align 1), so the
            // unaligned read is sound, and `p` is not touched while the pointer is used.
            let o = unsafe { *o };

            match o.type_ {
                opt_type::SUBNET_MASK
                | opt_type::ROUTER
                | opt_type::BROADCAST_ADDRESS
                | opt_type::DHCP_SERVER
                | opt_type::DOMAIN_NAME_SERVERS => {
                    let ipo = p.get_header::<IpOption>(off);
                    if !ipo.is_null() {
                        // SAFETY: `get_header` returned a non-null pointer, so it covers a whole header of this
                        // type inside the packet's linear data; the header types are packed (align 1), so the
                        // unaligned read is sound, and `p` is not touched while the pointer is used.
                        let ip = unsafe { (*ipo).ip };
                        self.set_ip(o.type_, Ipv4Address::from_u32(ntohl(ip)));
                    }
                }
                opt_type::MESSAGE_TYPE => {
                    let to = p.get_header::<TypeOption>(off);
                    if !to.is_null() {
                        // SAFETY: `get_header` returned a non-null pointer, so it covers a whole header of this
                        // type inside the packet's linear data; the header types are packed (align 1), so the
                        // unaligned read is sound, and `p` is not touched while the pointer is used.
                        self.type_ = unsafe { (*to).type_ };
                    }
                }
                opt_type::INTERFACE_MTU => {
                    let mo = p.get_header::<MtuOption>(off);
                    if !mo.is_null() {
                        // SAFETY: `get_header` returned a non-null pointer, so it covers a whole header of this
                        // type inside the packet's linear data; the header types are packed (align 1), so the
                        // unaligned read is sound, and `p` is not touched while the pointer is used.
                        self.lease.mtu = ntohs(unsafe { (*mo).mtu });
                    }
                }
                opt_type::LEASE_TIME | opt_type::RENEWAL_TIME | opt_type::REBINDING_TIME => {
                    let to = p.get_header::<TimeOption>(off);
                    if !to.is_null() {
                        // SAFETY: `get_header` returned a non-null pointer, so it covers a whole header of this
                        // type inside the packet's linear data; the header types are packed (align 1), so the
                        // unaligned read is sound, and `p` is not touched while the pointer is used.
                        let t = unsafe { (*to).time };
                        self.set_time(o.type_, Duration::from_secs(ntohl(t) as u64));
                    }
                }
                _ => {}
            }

            off += std::mem::size_of::<DhcpOption>() + o.len as usize;
        }
    }
}

/// The client state machine; it lives on shard 0, in the node it shares with its
/// packet filter.
pub struct DhcpImpl {
    /// The client's address with the provenance of its node, written once by `new`. Timers
    /// and forwarded work keep this one, never a pointer from `&mut self`.
    _this: *mut DhcpImpl,
    _result: Promise<Option<Lease>>,
    /// The client state: the byte of the filter (`DhcpFilter::_state`), which every shard's
    /// packet filter reads; projected from the node's root pointer.
    _state: *const AtomicU8,
    _timer: Timer,
    _retry_timer: Timer,
    _stack: *mut Ipv4,
    _sock: UdpChannel,
    _xid: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Discover {
    base: DhcpPacketBase,
    type_: TypeOption,
    requested_ip: IpOption,
    req: RequestedOption,
    end: OptionMark,
}

impl DhcpPacket for Discover {
    fn base(&mut self) -> &mut DhcpPacketBase {
        &mut self.base
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Request {
    base: DhcpPacketBase,
    type_: TypeOption,
    dhcp_server: IpOption,
    requested_ip: IpOption,
    req: RequestedOption,
    end: OptionMark,
}

impl DhcpPacket for Request {
    fn base(&mut self) -> &mut DhcpPacketBase {
        &mut self.base
    }
}

/// The packet filter of the client, which the ipv4 layer of every shard calls.
///
/// The filter is a separate member of the client's node, so the other shards reach it
/// without forming a reference to the client (which lives on shard 0 and may be changing
/// the state); the state is a relaxed atomic byte, so their reads are plain loads that never
/// race in the language sense.
struct DhcpFilter {
    _state: AtomicU8,
    /// The client's root pointer, used on shard 0 only.
    _client: *mut DhcpImpl,
}

// SAFETY: the filter is shared with the other shards, which only load `_state` (atomic)
// and copy `_client` into the work they send to shard 0; the client is used on shard 0
// only.
unsafe impl Sync for DhcpFilter {}

/// The node of a client: the filter beside the client, so that a reference to the client
/// (shard 0) never covers the filter the other shards read.
struct DhcpNode {
    _filter: DhcpFilter,
    _client: DhcpImpl,
}

impl DhcpImpl {
    /// Creates the client in its own node: its timers and the packet filter registered with
    /// the IPv4 layer point to it.
    ///
    /// # Errors
    /// Fails if the client's UDP socket cannot be opened.
    fn new_node(stack: *mut Ipv4) -> Result<NodePtr<DhcpNode>, ExceptionPtr> {
        // SAFETY: the ipv4 layer outlives its DHCP client.
        let sock =
            unsafe { (*stack).get_udp().make_channel(Ipv4Addr { ip: 0, port: CLIENT_PORT }) }?;
        let node = NodePtr::new(DhcpNode {
            _filter: DhcpFilter {
                _state: AtomicU8::new(State::NONE as u8),
                _client: std::ptr::null_mut(),
            },
            _client: DhcpImpl {
                _this: std::ptr::null_mut(),
                _result: Promise::new(),
                _state: std::ptr::null(),
                _timer: Timer::new(),
                _retry_timer: Timer::new(),
                _stack: stack,
                _sock: sock,
                _xid: 0,
            },
        });
        let root = node.as_ptr();
        // SAFETY: just made; nothing else refers to it. The pointers are projected from the
        // node's root pointer.
        unsafe {
            use std::ptr::{addr_of, addr_of_mut};
            let this = addr_of_mut!((*root)._client);
            (*this)._this = this;
            (*this)._state = addr_of!((*root)._filter._state);
            (*root)._filter._client = this;
        }
        Ok(node)
    }

    /// The current state.
    #[inline]
    fn state(&self) -> State {
        // SAFETY: the filter is in the client's node, which outlives the client.
        State::from_u8(unsafe { (*self._state).load(Ordering::Relaxed) })
    }

    /// Sets the current state.
    #[inline]
    fn set_state(&self, s: State) {
        // SAFETY: as in `state`.
        unsafe { (*self._state).store(s as u8, Ordering::Relaxed) };
    }

    /// A timer of the client, pinned.
    ///
    /// # Safety
    /// `t` is one of the client's timers, which live in its node and never move.
    #[inline]
    unsafe fn pinned(t: &mut Timer) -> Pin<&mut Timer> {
        // SAFETY: guaranteed by the caller.
        unsafe { Pin::new_unchecked(t) }
    }

    fn process_packet(
        &mut self,
        mut p: Packet,
        dhp: *mut DhcpPayload,
        opt_off: usize,
    ) -> Future<()> {
        // SAFETY: a timer of the client (see `pinned`).
        unsafe { Self::pinned(&mut self._retry_timer) }.cancel();

        // SAFETY: `dhp` points into `p`, which is alive here.
        let h = ntoh_struct(unsafe { &*dhp });

        let mut info = IpInfo::default();

        info.lease.ip = h.bootp.yiaddr;
        info.parse_options(&mut p, opt_off);

        match self.state() {
            State::DISCOVER => {
                if info.type_ != msg_type::OFFER {
                    // TODO: log?
                } else {
                    dhcp_log!("Got offer for {}", info.lease.ip);
                    // TODO, check for minimum valid/required fields sent back?
                    return self.send_request(&info.lease);
                }
            }
            State::REQUEST => {
                if info.type_ == msg_type::NAK {
                    dhcp_log!("Got nak on request");
                    self.set_state(State::NONE);
                    return self.send_discover(&Ipv4Address::default());
                }
                if info.type_ == msg_type::ACK {
                    dhcp_log!("Got ack on request");
                    dhcp_log!(" ip: {}", info.lease.ip);
                    dhcp_log!(" nm: {}", info.lease.netmask);
                    dhcp_log!(" gw: {}", info.lease.gateway);
                    self.set_state(State::DONE);
                    self._result.set_value(Some(info.lease));
                }
            }
            _ => {}
        }
        make_ready_future(())
    }

    fn run(&mut self, l: &Lease, timeout: Duration) -> ResultType {
        self.set_state(State::NONE);
        // The timers fire after this call: they keep the root pointer.
        let this = self._this;
        // SAFETY: a timer of the client (see `pinned`).
        unsafe { Self::pinned(&mut self._timer) }.set_callback(move || {
            // SAFETY: `this` is the client's root pointer; the timer is owned by the client,
            // so the client outlives the callback, and the callback runs from the reactor with
            // no other reference to the client live.
            let me = unsafe { &mut *this };
            me.set_state(State::FAIL);
            dhcp_log!("timeout");
            // SAFETY: a timer of the client (see `pinned`).
            unsafe { Self::pinned(&mut me._retry_timer) }.cancel();
            me._result.set_value(None);
        });

        dhcp_log!("sending discover");
        let _ = self.send_discover(&l.ip); // FIXME: ignoring return
        if !timeout.is_zero() {
            // SAFETY: a timer of the client (see `pinned`).
            unsafe { Self::pinned(&mut self._timer) }.arm_in(timeout);
        }
        let ip = l.ip;
        // SAFETY: as above.
        unsafe { Self::pinned(&mut self._retry_timer) }.set_callback(move || {
            // FIXME: ignoring return
            // SAFETY: as in the timeout callback above: the client owns this timer, so `this`
            // is live, and no other reference to the client exists while the reactor runs it.
            let _ = unsafe { (*this).send_discover(&ip) };
        });
        // SAFETY: a timer of the client (see `pinned`).
        unsafe { Self::pinned(&mut self._retry_timer) }.arm_periodic(Duration::from_secs(1));
        self._result.get_future()
    }

    fn send<T: DhcpPacket>(&mut self, mut pkt: T) -> Future<()> {
        pkt.base().dhp.bootp.xid = self._xid;
        // SAFETY: the ipv4 layer and its interface outlive the client.
        let ipf = unsafe { &*(*self._stack).netif() };
        let mac = ipf.hw_address().mac;
        let mut chaddr = pkt.base().dhp.bootp.chaddr;
        chaddr[..mac.len()].copy_from_slice(&mac);
        pkt.base().dhp.bootp.chaddr = chaddr;

        let b = *pkt.base();
        *pkt.base() = DhcpPacketBase { dhp: hton_struct(&b.dhp) };

        // SAFETY: `pkt` is plain old data of size_of::<T>() bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts(&pkt as *const T as *const u8, std::mem::size_of::<T>())
        };
        // FIXME: future is discarded
        let _ = self._sock.send(
            &SocketAddress::from_ipv4_addr(Ipv4Addr { ip: 0xffffffff, port: SERVER_PORT }),
            Packet::from_data(bytes),
        );

        make_ready_future(())
    }

    fn send_discover(&mut self, ip: &Ipv4Address) -> Future<()> {
        let mut d = Discover {
            base: DhcpPacketBase::default(),
            type_: TypeOption::new(msg_type::DISCOVER),
            requested_ip: IpOption::default(),
            req: RequestedOption::default(),
            end: OptionMark::default(),
        };

        d.requested_ip = IpOption::new(opt_type::REQUESTED_ADDRESS, ip);

        // A random transaction id, seeded from /dev/urandom.
        let mut seed = [0u8; 4];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = f.read_exact(&mut seed);
        }
        self._xid = u32::from_ne_bytes(seed);
        self.set_state(State::DISCOVER);
        self.send(d)
    }

    fn send_request(&mut self, info: &Lease) -> Future<()> {
        let mut d = Request {
            base: DhcpPacketBase::default(),
            type_: TypeOption::new(msg_type::REQUEST),
            dhcp_server: IpOption::default(),
            requested_ip: IpOption::default(),
            req: RequestedOption::default(),
            end: OptionMark::default(),
        };

        d.dhcp_server = IpOption::new(opt_type::DHCP_SERVER, &info.dhcp_server);
        d.requested_ip = IpOption::new(opt_type::REQUESTED_ADDRESS, &info.ip);

        dhcp_log!("sending request for {}", info.ip);
        self.set_state(State::REQUEST);
        self.send(d)
    }
}

impl IpPacketFilter for DhcpFilter {
    fn handle(
        &self,
        p: &mut Packet,
        iph: &mut IpHdr,
        _from: EthernetAddress,
        handled: &mut bool,
    ) -> Future<()> {
        if State::from_u8(self._state.load(Ordering::Relaxed)) == State::NONE
            || (p.len() as usize) < std::mem::size_of::<DhcpPacketBase>()
        {
            return make_ready_future(());
        }

        let ipl = iph.ihl() as usize * 4;
        // The farther header first: a `get_header` may linearize the packet, which would free
        // the buffer a nearer header pointer points into.
        let dhp = p.get_header::<DhcpPayload>(ipl + std::mem::size_of::<UdpHdr>());
        let udp = p.get_header::<UdpHdr>(ipl);

        let opt_off = ipl + std::mem::size_of::<UdpHdr>() + std::mem::size_of::<DhcpPayload>();

        if udp.is_null() || dhp.is_null() {
            return make_ready_future(());
        }
        // SAFETY: both headers were returned by get_header (align 1).
        let (dst_port, magic) = unsafe { ((*udp).dst_port, (*dhp).magic) };
        let ip_len = iph.len;
        if iph.ip_proto != IpProtocolNum::udp as u8
            || ntohs(dst_port) != CLIENT_PORT
            || (ip_len as usize) < (opt_off + std::mem::size_of::<OptionMark>())
            || magic != OPTIONS_MAGIC
        {
            return make_ready_future(());
        }
        *handled = true;
        let src_cpu = this_shard_id();
        let pk = std::mem::replace(p, Packet::make_null_packet());
        let this = self._client;
        if src_cpu == 0 {
            // SAFETY: on shard 0, from the ipv4 layer's packet path, with no reference to the
            // client live (its timers and forwarded work run from the reactor).
            return unsafe { (*this).process_packet(pk, dhp, opt_off) };
        }
        // SAFETY: the client lives on shard 0, outlives the forwarded packet and is only
        // touched there by the work below (this shard reaches only the filter); the packet's
        // memory goes back to this shard through `free_on_cpu`, and `dhp` points into that
        // packet. `this` is the client's root pointer.
        let fwd = unsafe { CrossShard::new((this, pk, dhp)) };
        // FIXME: future is discarded
        let _ = Smp::submit_to(0, move || {
            let (this, mut pk, dhp) = fwd.into_inner();
            // SAFETY: see above; the work runs from shard 0's reactor with no other reference
            // to the client live.
            unsafe { (*this).process_packet(pk.free_on_cpu_default(src_cpu), dhp, opt_off) }
        });
        make_ready_future(())
    }
}

impl Dhcp {
    /// Default time a discover or renewal waits for the server.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Creates a DHCP client on `ip`, which it keeps.
    ///
    /// # Errors
    /// Fails if the client's UDP socket cannot be opened.
    ///
    /// # Safety
    /// `ip` is a live ipv4 layer on this shard that outlives the client, with the
    /// provenance of its owner (not a pointer from a temporary reference), and no reference
    /// to it is live during the call.
    pub unsafe fn new(ip: *mut Ipv4) -> Result<Self, ExceptionPtr> {
        Ok(Dhcp { _impl: DhcpImpl::new_node(ip)? })
    }

    /// Runs a discover/request sequence on the IPv4 layer and resolves to the lease offered,
    /// or to `None` if no lease is obtained within `timeout`.
    pub fn discover(&mut self, timeout: Duration) -> ResultType {
        // SAFETY: the only reference to the client while it runs (its timers and the packet
        // filter run from the reactor); projected from the node's root pointer, so it does not
        // cover the filter the other shards read.
        unsafe { &mut *std::ptr::addr_of_mut!((*self._impl.as_ptr())._client) }
            .run(&Lease::default(), timeout)
    }

    /// Renews the lease `l`.
    pub fn renew(&mut self, l: &Lease, timeout: Duration) -> ResultType {
        // SAFETY: as in `discover`.
        unsafe { &mut *std::ptr::addr_of_mut!((*self._impl.as_ptr())._client) }.run(l, timeout)
    }

    /// The packet filter that captures the replies.
    pub fn get_ipv4_filter(&mut self) -> *mut dyn IpPacketFilter {
        // SAFETY: a place projection from the node's root pointer; it creates no reference.
        let p: *mut DhcpFilter = unsafe { std::ptr::addr_of_mut!((*self._impl.as_ptr())._filter) };
        p
    }
}
