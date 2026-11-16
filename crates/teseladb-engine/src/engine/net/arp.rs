//! Address Resolution Protocol (RFC 826) for the native network stack.
//!
//! [`Arp`] is the ARP layer of one interface: it receives ARP frames, dispatches them by
//! protocol type to the registered [`ArpForProtocol`] handlers and queues their replies and
//! queries for transmission. [`ArpFor`] is the handler of one L3 protocol (IPv4 in
//! practice): the shard-local table from L3 to Ethernet addresses, lookups that wait for a
//! reply, learning from received packets, and the errors [`ArpTimeoutError`] and
//! [`ArpQueueFullError`]. While an address is unresolved, every second the query is sent
//! again and the lookups waiting at that moment fail with [`ArpTimeoutError`].
//!
//! The protocol objects register raw pointers to themselves with their `Arp` and their timer
//! callbacks keep pointers to them, so they are constructed in place (`init_at`) and never
//! move afterwards. Learned addresses are propagated to the other shards, so every shard
//! has the same table.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::time::Duration;

use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, make_exception_future, make_ready_future,
};
use crate::engine::core::timer::Timer;
use crate::engine::net::byteorder::ntoh;
use crate::engine::net::byteorder::{
    consume_be_slice as consume_be, produce_be_slice as produce_be,
};
use crate::engine::net::const_::EthProtocolNum;
use crate::engine::net::ethernet::{Ethernet, EthernetAddress};
use crate::engine::net::hash::FxBuildHasher;
use crate::engine::net::net::{ForwardHash, Interface, L3Packet, L3PacketProviderType, L3Protocol};
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::net::packet::Packet;
use std::pin::Pin;

/// A per-L3-protocol handler registered with an [`Arp`] under its protocol number.
pub trait ArpForProtocol {
    /// Handles a received ARP packet of this protocol.
    fn received(&mut self, p: Packet) -> Future<()>;
    /// Fills the hash used to pick the shard of a packet; ARP packets are not forwarded.
    #[inline]
    fn forward(&mut self, out_hash_data: &mut ForwardHash, p: &mut Packet, off: usize) -> bool {
        let _ = out_hash_data;
        let _ = p;
        let _ = off;
        false
    }
}

/// The state shared by every [`ArpForProtocol`] implementation.
pub struct ArpForProtocolBase {
    /// The ARP layer the handler is registered with; it outlives the handler.
    pub _arp: *mut Arp,
    /// The L3 protocol number this handler is registered under.
    pub _proto_num: u16,
}

impl ArpForProtocolBase {
    /// Registers `this_` with `a` under `proto_num` and returns the base that records it.
    ///
    /// # Safety
    /// `a` is a live arp that outlives the protocol, and `this_` is the pinned object that
    /// owns this base.
    pub unsafe fn new(a: *mut Arp, proto_num: u16, this_: *mut dyn ArpForProtocol) -> Self {
        // SAFETY: the arp outlives the protocols registered on it (the caller's contract).
        unsafe { (*a).add(proto_num, this_) };
        ArpForProtocolBase { _arp: a, _proto_num: proto_num }
    }
}

impl Drop for ArpForProtocolBase {
    fn drop(&mut self) {
        // SAFETY: the arp outlives the protocols registered on it.
        unsafe { (*self._arp).del(self._proto_num) };
    }
}

/// The fixed part of an ARP header, common to every protocol: hardware type, protocol type
/// and the two address lengths.
#[derive(Clone, Copy, Debug, Default)]
struct ArpHdrCommon {
    htype: u16,
    ptype: u16,
}

impl ArpHdrCommon {
    #[inline]
    fn read(mut p: &[u8]) -> ArpHdrCommon {
        ArpHdrCommon { htype: consume_be::<u16>(&mut p), ptype: consume_be::<u16>(&mut p) }
    }
    #[inline]
    const fn size() -> usize {
        4
    }
}

/// The ARP layer of an interface: dispatches ARP packets to the per-protocol handlers.
pub struct Arp {
    _netif: *mut Interface,
    _proto: L3Protocol,
    _arp_for_protocol: HashMap<u16, *mut dyn ArpForProtocol, FxBuildHasher>,
    _packetq: CircularBuffer<L3Packet>,
}

/// What [`ArpFor`] needs of its L3 protocol.
pub trait ArpL3 {
    /// The protocol's address type.
    type AddressType: ArpL3Address;
    /// The protocol's broadcast address.
    fn broadcast_address() -> Self::AddressType;
    /// The protocol type written in ARP headers (its EtherType).
    fn arp_protocol_type() -> u16;
}

/// What `arp_for<L3>` needs of an L3 address.
pub trait ArpL3Address: Copy + Eq + Hash + fmt::Debug + 'static {
    /// Reads an address and advances `p`.
    fn consume(p: &mut &[u8]) -> Self;
    /// Writes the address and advances `p`.
    fn produce(&self, p: &mut &mut [u8]);
    /// The wire size of the address.
    fn size() -> usize;
}

impl ArpL3Address for crate::engine::net::ipv4_address::Ipv4Address {
    #[inline]
    fn consume(p: &mut &[u8]) -> Self {
        Self::consume(p)
    }
    #[inline]
    fn produce(&self, p: &mut &mut [u8]) {
        Self::produce(self, p)
    }
    #[inline]
    fn size() -> usize {
        Self::size()
    }
}

const MAX_WAITERS: usize = 512;

#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Oper {
    OpRequest = 1,
    OpReply = 2,
}

/// A full ARP header for the addresses of protocol `A`.
#[derive(Clone, Copy, Debug)]
struct ArpHdr<A: ArpL3Address> {
    htype: u16,
    ptype: u16,
    hlen: u8,
    plen: u8,
    oper: u16,
    sender_hwaddr: EthernetAddress,
    sender_paddr: A,
    target_hwaddr: EthernetAddress,
    target_paddr: A,
}

impl<A: ArpL3Address> ArpHdr<A> {
    fn read(mut p: &[u8]) -> Self {
        let htype = consume_be::<u16>(&mut p);
        let ptype = consume_be::<u16>(&mut p);
        let hlen = consume_be::<u8>(&mut p);
        let plen = consume_be::<u8>(&mut p);
        let oper = consume_be::<u16>(&mut p);
        let sender_hwaddr = EthernetAddress::consume(&mut p);
        let sender_paddr = A::consume(&mut p);
        let target_hwaddr = EthernetAddress::consume(&mut p);
        let target_paddr = A::consume(&mut p);
        ArpHdr {
            htype,
            ptype,
            hlen,
            plen,
            oper,
            sender_hwaddr,
            sender_paddr,
            target_hwaddr,
            target_paddr,
        }
    }
    fn write(&self, mut p: &mut [u8]) {
        produce_be::<u16>(&mut p, self.htype);
        produce_be::<u16>(&mut p, self.ptype);
        produce_be::<u8>(&mut p, self.hlen);
        produce_be::<u8>(&mut p, self.plen);
        produce_be::<u16>(&mut p, self.oper);
        self.sender_hwaddr.produce(&mut p);
        self.sender_paddr.produce(&mut p);
        self.target_hwaddr.produce(&mut p);
        self.target_paddr.produce(&mut p);
    }
    #[inline]
    fn size() -> usize {
        8 + 2 * (EthernetAddress::size() + A::size())
    }
}

/// An address resolution in progress: the waiters and the retry timer.
#[derive(Default)]
struct Resolution {
    _waiters: Vec<Promise<EthernetAddress>>,
    _timeout_timer: Timer,
}

/// The ARP table and resolver of one L3 protocol.
pub struct ArpFor<L3: ArpL3> {
    /// The address the object was built at (`slot` of `init_at`), whose provenance the
    /// timer callbacks keep; never one derived from `&mut self`.
    _this: *mut ArpFor<L3>,
    base: ArpForProtocolBase,
    _l3self: L3::AddressType,
    _table: HashMap<L3::AddressType, EthernetAddress, FxBuildHasher>,
    // Each resolution has its own node: its timer callback keeps a pointer to it. `NodePtr`,
    // not `Box`, so that rehashing (which moves the owners) leaves that pointer valid; the
    // resolution is reached through the node pointer only.
    _in_progress: HashMap<L3::AddressType, NodePtr<Resolution>, FxBuildHasher>,
}

impl<L3: ArpL3 + 'static> ArpFor<L3> {
    /// The arp this protocol is registered with (it outlives its protocols). A pointer: the
    /// arp dispatches into this protocol, so each use is a short access.
    #[inline]
    fn arp(&self) -> *mut Arp {
        self.base._arp
    }

    fn make_query_packet(&self, paddr: L3::AddressType) -> Packet {
        let hdr = ArpHdr::<L3::AddressType> {
            htype: Ethernet::arp_hardware_type(),
            ptype: L3::arp_protocol_type(),
            hlen: std::mem::size_of::<EthernetAddress>() as u8,
            plen: std::mem::size_of::<L3::AddressType>() as u8,
            oper: Oper::OpRequest as u16,
            sender_hwaddr: self.l2self(),
            sender_paddr: self._l3self,
            target_hwaddr: Ethernet::broadcast_address(),
            target_paddr: paddr,
        };
        let mut p = Packet::new();
        let size = ArpHdr::<L3::AddressType>::size();
        p.prepend_uninitialized_header(size);
        let h = p.get_header_raw(0, size);
        // SAFETY: the header was just prepended, so `h` points to `size` writable bytes.
        hdr.write(unsafe { std::slice::from_raw_parts_mut(h, size) });
        p
    }

    #[inline]
    fn l2self(&self) -> EthernetAddress {
        // SAFETY: see `arp`.
        unsafe { (*self.arp()).l2self() }
    }

    fn send(&mut self, to: EthernetAddress, p: Packet) {
        // SAFETY: see `arp`; the queue is not borrowed elsewhere while a protocol sends.
        unsafe {
            (*self.arp())._packetq.push_back(L3Packet { proto_num: EthProtocolNum::arp, to, p })
        };
    }

    /// Broadcasts a query for `paddr`.
    pub fn send_query(&mut self, paddr: &L3::AddressType) -> Future<()> {
        let p = self.make_query_packet(*paddr);
        self.send(Ethernet::broadcast_address(), p);
        make_ready_future(())
    }

    /// Constructs the handler in place at `slot` and registers it with `a`; it must be built
    /// in place because the registration and its timers keep its address.
    ///
    /// # Safety
    /// `slot` must be valid for writes and the object must not move afterwards.
    pub unsafe fn init_at(slot: *mut Self, a: *mut Arp) {
        let tp: *mut dyn ArpForProtocol = slot;
        // SAFETY: guaranteed by the caller.
        unsafe {
            slot.write(ArpFor::<L3> {
                _this: slot,
                base: ArpForProtocolBase::new(a, L3::arp_protocol_type(), tp),
                _l3self: L3::broadcast_address(),
                _table: HashMap::default(),
                _in_progress: HashMap::default(),
            });
            (*slot)._table.insert(L3::broadcast_address(), Ethernet::broadcast_address());
        }
    }

    /// Resolves `paddr` to its link-layer address, querying the network if needed.
    pub fn lookup(&mut self, paddr: &L3::AddressType) -> Future<EthernetAddress> {
        if let Some(i) = self._table.get(paddr) {
            return make_ready_future::<EthernetAddress>(*i);
        }
        let first_request = !self._in_progress.contains_key(paddr);
        // The timer runs after this call: it keeps the root pointer.
        let this = self._this;
        let resp: *mut Resolution = self
            ._in_progress
            .entry(*paddr)
            .or_insert_with(|| NodePtr::new(Resolution::default()))
            .as_ptr();

        if first_request {
            let paddr = *paddr;
            // SAFETY: the resolution's node is stable and nothing else refers to it now. In the
            // callback: the resolution and the arp_for outlive the timer, which the resolution
            // owns; it runs from the reactor, with no other reference to either live, and each
            // access is short.
            unsafe {
                Pin::new_unchecked(&mut (*resp)._timeout_timer).set_callback(move || {
                    // FIXME: future is discarded
                    let _ = (*this).send_query(&paddr);
                    for w in (*resp)._waiters.iter_mut() {
                        w.set_exception(ExceptionPtr::from(ArpTimeoutError::new()));
                    }
                    (*resp)._waiters.clear();
                });
                Pin::new_unchecked(&mut (*resp)._timeout_timer)
                    .arm_periodic(Duration::from_secs(1));
            }
            // FIXME: future is discarded
            let _ = self.send_query(&paddr);
        }

        // SAFETY: the node is live (in the map) and no other reference to it is live.
        let res = unsafe { &mut *resp };
        if res._waiters.len() >= MAX_WAITERS {
            return make_exception_future::<EthernetAddress>(ExceptionPtr::from(
                ArpQueueFullError::new(),
            ));
        }

        res._waiters.push(Promise::new());
        // SAFETY: just pushed.
        unsafe { res._waiters.last_mut().unwrap_unchecked() }.get_future()
    }

    /// Records that `paddr` is at `hwaddr` and wakes the lookups waiting for it.
    pub fn learn(&mut self, hwaddr: EthernetAddress, paddr: L3::AddressType) {
        self._table.insert(paddr, hwaddr);
        if let Some(node) = self._in_progress.remove(&paddr) {
            // SAFETY: the node is out of the map; this is the only reference to it, and the
            // timer has not moved since it was armed.
            let res = unsafe { node.get_mut() };
            // SAFETY: the timer lives inside the heap node, which has not moved since the timer
            // was armed and is not moved by `cancel`; the pin is only used for this call.
            unsafe { Pin::new_unchecked(&mut res._timeout_timer) }.cancel();
            for pr in res._waiters.iter_mut() {
                pr.set_value(hwaddr);
            }
        }
    }

    /// Sets the address of this host.
    pub fn set_self_addr(&mut self, addr: L3::AddressType) {
        self._table.remove(&self._l3self);
        let l2 = self.l2self();
        self._table.insert(addr, l2);
        self._l3self = addr;
    }

    fn handle_request(&mut self, ah: &mut ArpHdr<L3::AddressType>) -> Future<()> {
        if ah.target_paddr == self._l3self && self._l3self != L3::broadcast_address() {
            ah.oper = Oper::OpReply as u16;
            ah.target_hwaddr = ah.sender_hwaddr;
            ah.target_paddr = ah.sender_paddr;
            ah.sender_hwaddr = self.l2self();
            ah.sender_paddr = self._l3self;
            let mut p = Packet::new();
            let size = ArpHdr::<L3::AddressType>::size();
            let h = p.prepend_uninitialized_header(size);
            // SAFETY: the header was just prepended, so `h` points to `size` writable bytes.
            ah.write(unsafe { std::slice::from_raw_parts_mut(h, size) });
            self.send(ah.target_hwaddr, p);
        }
        make_ready_future(())
    }
}

impl<L3: ArpL3 + 'static> ArpForProtocol for ArpFor<L3> {
    fn received(&mut self, mut p: Packet) -> Future<()> {
        let size = ArpHdr::<L3::AddressType>::size();
        let ah = p.get_header_raw(0, size);
        if ah.is_null() {
            return make_ready_future(());
        }
        // SAFETY: get_header returned `size` contiguous bytes of the packet.
        let mut h =
            ArpHdr::<L3::AddressType>::read(unsafe { std::slice::from_raw_parts(ah, size) });
        if h.hlen as usize != std::mem::size_of::<EthernetAddress>()
            || h.plen as usize != std::mem::size_of::<L3::AddressType>()
        {
            return make_ready_future(());
        }
        match h.oper {
            x if x == Oper::OpRequest as u16 => self.handle_request(&mut h),
            x if x == Oper::OpReply as u16 => {
                // Learn the sender: on this shard through `self` (this layer is the stack's
                // ARP handler), on the others through their stacks; see `arp_learn_on_others`.
                self.learn(h.sender_hwaddr, h.sender_paddr);
                arp_learn_dispatch::<L3>(h.sender_hwaddr, h.sender_paddr);
                make_ready_future(())
            }
            _ => make_ready_future(()),
        }
    }
}

/// Propagates a learned address pair to the ARP tables of the other shards. Only IPv4 has
/// ARP, so the address is converted to an IPv4 address.
fn arp_learn_dispatch<L3: ArpL3 + 'static>(l2: EthernetAddress, l3: L3::AddressType) {
    use std::any::Any;
    let any: &dyn Any = &l3;
    if let Some(a) = any.downcast_ref::<crate::engine::net::ipv4_address::Ipv4Address>() {
        crate::engine::net::native_stack::arp_learn_on_others(l2, *a);
    }
}

/// An ARP failure, carrying its message.
#[derive(Clone, Debug)]
pub struct ArpError(pub String);

impl ArpError {
    /// An ARP error with the given message.
    pub fn new(msg: &str) -> Self {
        ArpError(msg.to_string())
    }
}

impl fmt::Display for ArpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ArpError {}

/// A lookup that got no reply in time.
#[derive(Clone, Debug)]
pub struct ArpTimeoutError(pub ArpError);

impl ArpTimeoutError {
    /// "ARP timeout"
    pub fn new() -> Self {
        ArpTimeoutError(ArpError::new("ARP timeout"))
    }
}

impl Default for ArpTimeoutError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ArpTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ArpTimeoutError {}

/// A lookup refused because too many lookups already wait for the same address.
#[derive(Clone, Debug)]
pub struct ArpQueueFullError(pub ArpError);

impl ArpQueueFullError {
    /// "ARP waiter's queue is full"
    pub fn new() -> Self {
        ArpQueueFullError(ArpError::new("ARP waiter's queue is full"))
    }
}

impl Default for ArpQueueFullError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ArpQueueFullError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ArpQueueFullError {}

impl Arp {
    /// Constructs the ARP layer of `netif` in place at `slot`; it must be built in place
    /// because the callbacks it registers with the interface keep its address.
    ///
    /// # Safety
    /// `slot` must be valid for writes and the object must not move afterwards; `netif`
    /// outlives the arp.
    pub unsafe fn init_at(slot: *mut Arp, netif: *mut Interface) {
        let tp = slot;
        let provider: L3PacketProviderType = Box::new(move || {
            // SAFETY: the arp owns its l3_protocol registration and outlives it.
            unsafe { (*tp).get_packet() }
        });
        // SAFETY: the caller keeps `netif` alive while the arp lives.
        let proto = unsafe { L3Protocol::new(netif, EthProtocolNum::arp, provider) };
        // SAFETY: guaranteed by the caller.
        let this = unsafe {
            slot.write(Arp {
                _netif: netif,
                _proto: proto,
                _arp_for_protocol: HashMap::default(),
                _packetq: CircularBuffer::new(),
            });
            &mut *slot
        };
        // FIXME: ignored future
        let _ = this._proto.receive(
            Box::new(move |p: Packet, ea: EthernetAddress| {
                // SAFETY: as above.
                unsafe { Arp::process_packet(tp, p, ea) }
            }),
            Box::new(move |out_hash_data: &mut ForwardHash, p: &mut Packet, off: usize| {
                // SAFETY: as above.
                unsafe { Arp::forward(tp, out_hash_data, p, off) }
            }),
        );
    }

    #[inline]
    fn l2self(&self) -> EthernetAddress {
        // SAFETY: the interface outlives its arp.
        unsafe { (*self._netif).hw_address() }
    }

    fn get_packet(&mut self) -> Option<L3Packet> {
        self._packetq.take_front()
    }

    /// The protocol registered for `ptype`, read with a short access to the table.
    ///
    /// # Safety
    /// `this` is the arp's root pointer, with no reference to the arp live.
    #[inline]
    unsafe fn protocol_for(this: *mut Arp, ptype: u16) -> Option<*mut dyn ArpForProtocol> {
        // SAFETY: guaranteed by the caller; the shared borrow of the table ends here.
        unsafe { (*this)._arp_for_protocol.get(&ptype).copied() }
    }

    /// Forwarding decision for a received ARP frame, through the root pointer (see
    /// `process_packet`): asks the handler of the frame's protocol type.
    ///
    /// # Safety
    /// `this` is the arp's root pointer, with no reference to the arp live.
    unsafe fn forward(
        this: *mut Arp,
        out_hash_data: &mut ForwardHash,
        p: &mut Packet,
        off: usize,
    ) -> bool {
        let ah = p.get_header_raw(off, ArpHdrCommon::size());
        if ah.is_null() {
            // a frame too short for the common header is not forwarded
            return false;
        }
        // SAFETY: get_header returned the 4 bytes of the common header.
        let ptype = ntoh(unsafe { (ah.add(2) as *const u16).read_unaligned() });
        // SAFETY: guaranteed by the caller. Registered protocols unregister themselves before
        // they are destroyed, and no reference to the arp is live while the protocol runs.
        unsafe {
            if let Some(i) = Self::protocol_for(this, ptype) {
                return (*i).forward(out_hash_data, p, off);
            }
        }
        false
    }

    /// Registers the handler of the L3 protocol `proto_num`.
    pub fn add(&mut self, proto_num: u16, afp: *mut dyn ArpForProtocol) {
        self._arp_for_protocol.insert(proto_num, afp);
    }

    /// Unregisters the handler of the L3 protocol `proto_num`.
    pub fn del(&mut self, proto_num: u16) {
        self._arp_for_protocol.remove(&proto_num);
    }

    /// Handles a received ARP frame, through the root pointer: the
    /// protocol it delivers to answers requests by queueing into this arp (`_packetq`) and
    /// reads its address, so no reference to the arp is live across that call.
    ///
    /// # Safety
    /// `this` is the arp's root pointer, with no reference to the arp live.
    unsafe fn process_packet(this: *mut Arp, mut p: Packet, _from: EthernetAddress) -> Future<()> {
        let h = p.get_header_raw(0, ArpHdrCommon::size());
        if h.is_null() {
            return make_ready_future(());
        }
        // SAFETY: get_header returned the 4 bytes of the common header.
        let ah = ArpHdrCommon::read(unsafe { std::slice::from_raw_parts(h, ArpHdrCommon::size()) });
        let _ = ah.htype;
        // SAFETY: as in `forward`.
        unsafe {
            if let Some(i) = Self::protocol_for(this, ah.ptype) {
                return (*i).received(p);
            }
        }
        make_ready_future(())
    }
}
