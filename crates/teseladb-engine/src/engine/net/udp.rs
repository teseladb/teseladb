//! UDP over IPv4 for the native network stack (RFC 768).
//!
//! The protocol object, [`Ipv4Udp`], is a member of the IPv4 layer and is declared in
//! [`crate::engine::net::ip`]; this module holds the UDP header ([`UdpHdr`]), the channel
//! objects handed to users ([`NativeChannel`], [`NativeDatagram`], [`UdpChannelState`]) and
//! the protocol's methods: binding ports, demultiplexing received datagrams to channels and
//! queuing datagrams for the IPv4 layer to send.
//!
//! Everything is shard-local: a channel and its state belong to the shard that bound the
//! port, and received datagrams are delivered to it by the RSS-based forwarding of the
//! IPv4 layer. A full receive queue drops datagrams; senders wait for send-buffer space
//! (212992 bytes per channel).

use crate::engine::core::deleter::make_deleter;
use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::queue::Queue;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use crate::engine::net::api::{
    Datagram, DatagramImpl, UdpChannel, is_ip_unspecified, is_port_unspecified,
};
use crate::engine::net::byteorder::{AdjustEndianness, EndianVisitor, hton_struct, ntoh_struct};
use crate::engine::net::const_::IpProtocolNum;
use crate::engine::net::ethernet::EthernetAddress;
use crate::engine::net::inet_address::RuntimeError;
use crate::engine::net::ip::{Ipv4, Ipv4Traits, Ipv4Udp, L4Packet, Registration};
use crate::engine::net::ip_checksum::Checksummer;
use crate::engine::net::ipv4_address::Ipv4Address;
use crate::engine::net::packet::{OffloadInfo, Packet};
use crate::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use crate::engine::net::stack::DatagramChannelImpl;

/// `EBADF` errno value, reported by a send on a closed channel.
const EBADF: i32 = 9;
/// `EPIPE` errno value.
const EPIPE: i32 = 32;

/// The UDP header (RFC 768).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct UdpHdr {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// Length of header and data.
    pub len: u16,
    /// Checksum.
    pub cksum: u16,
}

impl AdjustEndianness for UdpHdr {
    #[inline]
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        crate::adjust_fields!(a, self.src_port, self.dst_port, self.len, self.cksum);
    }
}

/// The state a UDP channel shares with its protocol: the receive queue and the send
/// buffer budget.
pub struct UdpChannelState {
    /// Received datagrams waiting for `receive`.
    pub _queue: Queue<Datagram>,
    // Limit number of data queued into send queue
    /// Bytes the channel may still queue for sending.
    pub _user_queue_space: Semaphore,
}

impl UdpChannelState {
    /// A state whose receive queue holds `queue_size` datagrams.
    pub fn new(queue_size: usize) -> Self {
        UdpChannelState {
            _queue: Queue::new(queue_size),
            _user_queue_space: Semaphore::new(212992),
        }
    }
    /// Waits until `len` bytes fit in the send buffer.
    #[inline]
    pub fn wait_for_send_buffer(&mut self, len: usize) -> Future<()> {
        self._user_queue_space.wait(len)
    }
    /// Returns `len` bytes to the send buffer.
    #[inline]
    pub fn complete_send(&mut self, len: usize) {
        self._user_queue_space.signal(len);
    }
}

#[inline]
fn to_ipv4_addr(a: Ipv4Address, port: u16) -> Ipv4Addr {
    Ipv4Addr { ip: a.ip, port }
}

/// A datagram received by the native stack.
pub struct NativeDatagram {
    _src: Ipv4Addr,
    _dst: Ipv4Addr,
    _p: Packet,
}

impl NativeDatagram {
    /// Parses the UDP header of `p`, received from `src` for `dst`.
    pub fn new(src: Ipv4Address, dst: Ipv4Address, p: Packet) -> Self {
        let mut p = p;
        let hdr = p.get_header::<UdpHdr>(0);
        // The caller (`received_impl`) checked that `p` holds a whole header.
        debug_assert!(!hdr.is_null());
        // SAFETY: as above; get_header returned a whole UdpHdr (align 1).
        let h = ntoh_struct(unsafe { &*hdr });
        p.trim_front(std::mem::size_of::<UdpHdr>());
        NativeDatagram {
            _src: to_ipv4_addr(src, h.src_port),
            _dst: to_ipv4_addr(dst, h.dst_port),
            _p: p,
        }
    }
}

impl DatagramImpl for NativeDatagram {
    fn get_src(&mut self) -> SocketAddress {
        SocketAddress::from_ipv4_addr(self._src)
    }
    fn get_dst(&mut self) -> SocketAddress {
        SocketAddress::from_ipv4_addr(self._dst)
    }
    fn get_dst_port(&mut self) -> Result<u16, ExceptionPtr> {
        Ok(self._dst.port)
    }
    fn get_data(&mut self) -> &mut Packet {
        &mut self._p
    }
}

/// A UDP channel of the native stack.
pub struct NativeChannel {
    _proto: *mut Ipv4Udp,
    _reg: Registration,
    _closed: bool,
    _state: LwSharedPtr<UdpChannelState>,
}

impl NativeChannel {
    /// A channel bound through `reg`, sharing `state` with `proto`.
    pub fn new(
        proto: *mut Ipv4Udp,
        reg: Registration,
        state: LwSharedPtr<UdpChannelState>,
    ) -> Self {
        NativeChannel { _proto: proto, _reg: reg, _closed: false, _state: state }
    }
}

impl Drop for NativeChannel {
    fn drop(&mut self) {
        if !self._closed {
            self.close();
        }
    }
}

impl DatagramChannelImpl for NativeChannel {
    fn local_address(&self) -> SocketAddress {
        // SAFETY: the protocol outlives its channels.
        let host = unsafe { (*self._proto).inet().host_address() };
        SocketAddress::from_ipv4_addr(Ipv4Addr { ip: host.ip, port: self._reg.port() })
    }

    fn receive(&mut self) -> Future<Datagram> {
        // SAFETY: the channel state is on this shard; the call below is the only access to it
        // while it runs (the protocol touches it only from `received_impl`, never during this).
        unsafe { self._state.get_mut_unchecked()._queue.pop_eventually() }
    }

    fn send_str(&mut self, dst: &SocketAddress, msg: &'static str) -> Future<()> {
        // The bytes are not copied; `'static` keeps them alive until the packet is sent.
        self.send(dst, Packet::from_static_data(msg.as_bytes()))
    }

    fn send(&mut self, dst: &SocketAddress, p: Packet) -> Future<()> {
        let len = p.len() as usize;
        let proto = self._proto;
        let port = self._reg.port();
        let state = self._state.clone();
        let dst = *dst;
        // SAFETY: the channel state is on this shard; the call below is the only access to it
        // while it runs (the protocol touches it only from `received_impl`, never during this).
        unsafe { self._state.get_mut_unchecked() }.wait_for_send_buffer(len).then(move |()| {
            let s = state;
            let p = Packet::append_deleter(
                p,
                make_deleter(move || {
                    // SAFETY: the channel state is on this shard; the call below is the only access to it
                    // while it runs (the protocol touches it only from `received_impl`, never during this).
                    unsafe { s.get_mut_unchecked() }.complete_send(len);
                }),
            );
            // SAFETY: the protocol outlives its channels.
            unsafe { (*proto).send(port, Ipv4Addr::from_socket_address(&dst), p) };
            make_ready_future(())
        })
    }

    fn is_closed(&self) -> bool {
        self._closed
    }

    fn shutdown_input(&mut self) {
        // SAFETY: the channel state is on this shard; the call below is the only access to it
        // while it runs (the protocol touches it only from `received_impl`, never during this).
        unsafe { self._state.get_mut_unchecked() }
            ._queue
            .abort(ExceptionPtr::from(std::io::Error::from_raw_os_error(EBADF)));
    }

    fn shutdown_output(&mut self) {
        // SAFETY: the channel state is on this shard; the call below is the only access to it
        // while it runs (the protocol touches it only from `received_impl`, never during this).
        unsafe { self._state.get_mut_unchecked() }
            ._queue
            .abort(ExceptionPtr::from(std::io::Error::from_raw_os_error(EPIPE)));
    }

    fn close(&mut self) {
        self._reg.unregister();
        self._closed = true;
    }
}

impl Ipv4Udp {
    /// Default receive-queue length of a channel, in datagrams.
    pub const DEFAULT_QUEUE_SIZE: i32 = 1024;

    /// Finishes construction: registers the protocol's packet provider with the IPv4 layer
    /// once the protocol has its final address.
    pub(crate) fn init(this: *mut Ipv4Udp) {
        // SAFETY: `this` is the pinned protocol, owned by the ipv4 it registers with.
        let inet = unsafe { &mut *(*this)._inet };
        inet.register_packet_provider(Box::new(move || {
            // SAFETY: as above.
            unsafe { (*this)._packetq.take_front() }
        }));
    }

    /// Adds the source and destination ports of the UDP header at offset `off` of `p` to the
    /// RSS hash input `out_hash_data`, so the datagram is forwarded to the shard that owns its
    /// flow. Always returns `true` (the datagram can be forwarded).
    pub(crate) fn forward_impl(
        &mut self,
        out_hash_data: &mut crate::engine::net::net::ForwardHash,
        p: &mut Packet,
        off: usize,
    ) -> bool {
        let uh = p.get_header::<UdpHdr>(off);

        if !uh.is_null() {
            // SAFETY: get_header returned a whole UdpHdr (align 1).
            let h = unsafe { *uh };
            out_hash_data.push_back_u16(h.src_port);
            out_hash_data.push_back_u16(h.dst_port);
        }
        true
    }

    /// Delivers a received datagram to the channel bound to its destination port; datagrams
    /// for unbound ports, datagrams shorter than the 8-byte header and datagrams arriving at
    /// a full queue are dropped.
    pub(crate) fn received_impl(&mut self, mut p: Packet, from: Ipv4Address, to: Ipv4Address) {
        // A datagram shorter than its 8-byte header (sent by anyone) makes `get_header`
        // return null; it is dropped.
        if p.get_header::<UdpHdr>(0).is_null() {
            return;
        }
        let mut dgram = Datagram::new(Box::new(NativeDatagram::new(from, to, p)));

        // NativeDatagram::get_dst_port never fails.
        let Ok(port) = dgram.get_dst_port() else { return };
        if let Some(chan) = self._channels.get(&port) {
            // SAFETY: the channel state is on this shard and nothing else touches it during
            // the push (it only completes a promise).
            let state = unsafe { chan.get_mut_unchecked() };
            // A full queue drops the datagram.
            let _ = state._queue.push(dgram);
        }
    }

    /// Sends `p` from `src_port` of this host to `dst`.
    pub fn send(&mut self, src_port: u16, dst: Ipv4Addr, mut p: Packet) {
        // The ipv4 is reached through field projections only (this protocol is one of its
        // fields, mutably borrowed here; see `Ipv4L4`).
        let inet = self.inet();
        let src = inet.host_address();
        let hdrp = p.prepend_header::<UdpHdr>(0);
        // SAFETY: prepend_header returned room for a UdpHdr (align 1). The header is written
        // through the pointer, with no reference kept: the checksum below reads the same
        // bytes from the packet.
        unsafe {
            let h = UdpHdr {
                src_port,
                dst_port: dst.port,
                len: p.len() as u16,
                cksum: 0, /* set below */
            };
            hdrp.write(hton_struct(&h));
        }

        let mut oi = OffloadInfo::default();
        let mut csum = Checksummer::default();
        let dst_address = Ipv4Address::from_ipv4_addr(dst);
        Ipv4Traits::udp_pseudo_header_checksum(&mut csum, src, dst_address, p.len() as u16);
        let needs_frag = Ipv4::needs_frag(&mut p, IpProtocolNum::udp, *inet.hw_features());
        if inet.hw_features().tx_csum_l4_offload && !needs_frag {
            // SAFETY: as above.
            unsafe { (*hdrp).cksum = !csum.get() };
            oi.needs_csum = true;
        } else {
            csum.sum_packet(&p);
            // SAFETY: as above; the packet is not borrowed any more.
            unsafe { (*hdrp).cksum = csum.get() };
            oi.needs_csum = false;
        }
        oi.protocol = IpProtocolNum::udp;
        p.set_offload_info(oi);

        // FIXME: future is discarded
        let f = inet.get_l2_dst_address(dst_address);
        if f.available() {
            // `then` on a ready future runs in line, inside this call: done through `self`,
            // since a write through the root pointer would invalidate it.
            if let Ok(e_dst) = f.get() {
                self._packetq.push_back(L4Packet {
                    to: dst_address,
                    p,
                    e_dst,
                    proto_num: IpProtocolNum::udp,
                });
            }
            return;
        }
        let this = self._this;
        let _ = f.then(move |e_dst: EthernetAddress| {
            // SAFETY: the protocol outlives the address resolution; this runs later from the
            // reactor with no reference to it live, through its root pointer.
            unsafe {
                (*this)._packetq.push_back(L4Packet {
                    to: dst_address,
                    p,
                    e_dst,
                    proto_num: IpProtocolNum::udp,
                })
            };
            make_ready_future(())
        });
    }

    fn next_port(&self, port: u16) -> u16 {
        if port == u16::MAX { Self::MIN_ANONYMOUS_PORT } else { port + 1 }
    }

    /// Opens a channel bound to `addr` (the IP must be unspecified; port 0 picks one).
    pub fn make_channel(&mut self, addr: Ipv4Addr) -> Result<UdpChannel, ExceptionPtr> {
        if !is_ip_unspecified(&addr) {
            return Err(ExceptionPtr::from(RuntimeError(
                "Binding to specific IP not supported yet".to_string(),
            )));
        }

        let bind_port: u16;

        if !is_port_unspecified(&addr) {
            if self._channels.contains_key(&addr.port) {
                return Err(ExceptionPtr::from(RuntimeError("Address already in use".to_string())));
            }
            bind_port = addr.port;
        } else {
            let starting_port = self._next_anonymous_port;
            while self._channels.contains_key(&self._next_anonymous_port) {
                self._next_anonymous_port = self.next_port(self._next_anonymous_port);
                if starting_port == self._next_anonymous_port {
                    return Err(ExceptionPtr::from(RuntimeError("No free port".to_string())));
                }
            }

            bind_port = self._next_anonymous_port;
            self._next_anonymous_port = self.next_port(self._next_anonymous_port);
        }

        let chan_state = make_lw_shared(UdpChannelState::new(self._queue_size as usize));
        self._channels.insert(bind_port, chan_state.clone());
        // The channel and its registration keep the protocol's root pointer.
        let this = self._this;
        Ok(UdpChannel::from_impl(Box::new(NativeChannel::new(
            this,
            Registration::new(this, bind_port),
            chan_state,
        ))))
    }
}
