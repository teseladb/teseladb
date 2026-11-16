//! TCP for the native network stack (RFC 793, with the MSS, window scale and timestamp
//! options of RFC 7323, RFC 6298 retransmission timers, RFC 5681 congestion control and
//! RFC 6528 initial sequence numbers).
//!
//! [`Tcp`] is the protocol layer of one shard: it demultiplexes received segments to the
//! connection control blocks ([`Tcb`]) by four-tuple, keeps the [`Listener`]s by port,
//! answers segments for unknown connections with a reset, and is polled by the IPv4 layer
//! for segments to send. A [`Connection`] is the user's handle on a [`Tcb`]: sending,
//! waiting for and reading data, and closing either direction. [`tcpv4_listen`] and
//! [`tcpv4_socket`] adapt the layer to the generic socket API.
//!
//! Everything is shard-local. A [`Tcb`] is shared through a shard-local [`LwSharedPtr`] and
//! mutated through it; its timers, the connection and the listener keep raw back-pointers to
//! it and to the layer. The objects they point to live in stable allocations (the
//! reference-counted node, the layer's own node) and outlive the pointers.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use crate::engine::core::byteorder::{read_be, write_be};
use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, make_exception_future, make_ready_future,
};
use crate::engine::core::lowres_clock::{LowresClock, LowresTimePoint};
use crate::engine::core::metrics::{self as sm, MetricGroups};
use crate::engine::core::queue::Queue;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::shared_ptr::{EnableLwSharedFromThis, LwSharedPtr, make_lw_shared};
use crate::engine::core::smp::Smp;
use crate::engine::core::timer::Timer;
use crate::engine::net::api::{ListenOptions, ServerSocket, Socket};
use crate::engine::net::arp::{ArpQueueFullError, ArpTimeoutError};
use crate::engine::net::const_::{IpProtocolNum, TCP_HDR_LEN_MIN};
use crate::engine::net::ethernet::EthernetAddress;
use crate::engine::net::hash::FxBuildHasher;
use crate::engine::net::ip::{
    InetTraits, IpProtocol, Ipv4L4, Ipv4Tcp, Ipv4Traits, L4Connid, L4Packet,
};
use crate::engine::net::ip_checksum::Checksummer;
use crate::engine::net::ipv4_address::Ipv4Address;
use crate::engine::net::native_stack_impl::{NativeServerSocketImpl, NativeSocketImpl};
use crate::engine::net::net::{ForwardHash, HwFeatures};
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::net::packet::{OffloadInfo, Packet};
use crate::engine::net::packet_util::{MergerOffset, PacketMerger};
use crate::engine::net::socket_defs::SocketAddress;

/// `ECONNRESET` errno value: the peer reset the connection.
const ECONNRESET: i32 = 104;
/// `ECONNABORTED` errno value: the connection attempt failed.
const ECONNABORTED: i32 = 103;
/// `ECONNREFUSED` errno value: the peer refused the connection.
const ECONNREFUSED: i32 = 111;

/// An I/O error carrying the errno value `err`.
#[inline]
pub fn tcp_error(err: i32) -> std::io::Error {
    std::io::Error::from_raw_os_error(err)
}

/// The error of a connection reset by the peer.
#[inline]
pub fn tcp_reset_error() -> std::io::Error {
    tcp_error(ECONNRESET)
}

/// The error of an aborted connection attempt.
#[inline]
pub fn tcp_connect_error() -> std::io::Error {
    tcp_error(ECONNABORTED)
}

/// The error of a refused connection.
#[inline]
pub fn tcp_refused_error() -> std::io::Error {
    tcp_error(ECONNREFUSED)
}

/// The states of RFC 793, one bit each so that sets of states can be tested at once.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// The state names are those of RFC 793.
#[allow(non_camel_case_types)]
pub enum TcpState {
    /// No connection.
    CLOSED = 1 << 0,
    /// Waiting for a connection request.
    LISTEN = 1 << 1,
    /// Waiting for a matching connection request after sending one.
    SYN_SENT = 1 << 2,
    /// Waiting for the acknowledgment of a connection request.
    SYN_RECEIVED = 1 << 3,
    /// Open connection.
    ESTABLISHED = 1 << 4,
    /// Waiting for a termination request or the ack of ours.
    FIN_WAIT_1 = 1 << 5,
    /// Waiting for a termination request.
    FIN_WAIT_2 = 1 << 6,
    /// Waiting for the local user to close.
    CLOSE_WAIT = 1 << 7,
    /// Waiting for the ack of our termination request.
    CLOSING = 1 << 8,
    /// Waiting for the ack of our termination request after the peer's.
    LAST_ACK = 1 << 9,
    /// Waiting for the peer to receive our ack.
    TIME_WAIT = 1 << 10,
}

/// A set of [`TcpState`]s, as a bit mask; built with `|`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpStateSet(pub u16);

impl std::ops::BitOr for TcpState {
    type Output = TcpStateSet;
    #[inline]
    fn bitor(self, s2: TcpState) -> TcpStateSet {
        TcpStateSet(self as u16 | s2 as u16)
    }
}

impl std::ops::BitOr<TcpState> for TcpStateSet {
    type Output = TcpStateSet;
    #[inline]
    fn bitor(self, s2: TcpState) -> TcpStateSet {
        TcpStateSet(self.0 | s2 as u16)
    }
}

impl From<TcpState> for TcpStateSet {
    #[inline]
    fn from(s: TcpState) -> Self {
        TcpStateSet(s as u16)
    }
}

/// Prints a debug trace of the TCP layer; compiled to nothing unless TCP debugging is
/// enabled at build time.
#[macro_export]
macro_rules! tcp_debug {
    ($($arg:tt)*) => {
        #[cfg(feature = "tcp_debug")]
        {
            print!($($arg)*);
        }
    };
}

/// The kind field of a TCP option, fixed by the protocol.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionKind {
    /// Maximum segment size.
    Mss = 2,
    /// Window scale.
    WinScale = 3,
    /// SACK permitted.
    Sack = 4,
    /// Timestamps.
    Timestamps = 8,
    /// No operation.
    Nop = 1,
    /// End of option list.
    Eol = 0,
}

/// The len field of a TCP option, fixed by the protocol.
pub struct OptionLen;

impl OptionLen {
    /// Maximum segment size.
    pub const MSS: u8 = 4;
    /// Window scale.
    pub const WIN_SCALE: u8 = 3;
    /// SACK permitted.
    pub const SACK: u8 = 2;
    /// Timestamps.
    pub const TIMESTAMPS: u8 = 10;
    /// No operation.
    pub const NOP: u8 = 1;
    /// End of option list.
    pub const EOL: u8 = 1;
}

/// TCP option negotiation state and wire helpers.
#[derive(Clone, Copy, Debug)]
pub struct TcpOption {
    // For option negotiattion
    /// The peer sent an MSS option.
    pub _mss_received: bool,
    /// The peer sent a window scale option.
    pub _win_scale_received: bool,
    /// The peer sent a timestamps option.
    pub _timestamps_received: bool,
    /// The peer sent a SACK-permitted option.
    pub _sack_received: bool,

    // Option data
    /// The MSS of the peer.
    pub _remote_mss: u16,
    /// Our MSS.
    pub _local_mss: u16,
    /// The window scale of the peer.
    pub _remote_win_scale: u8,
    /// Our window scale.
    pub _local_win_scale: u8,
}

impl Default for TcpOption {
    fn default() -> Self {
        TcpOption {
            _mss_received: false,
            _win_scale_received: false,
            _timestamps_received: false,
            _sack_received: false,
            _remote_mss: 536,
            _local_mss: 0,
            _remote_win_scale: 0,
            _local_win_scale: 0,
        }
    }
}

/// The maximum segment size option.
#[derive(Clone, Copy, Debug, Default)]
pub struct OptMss {
    /// The segment size.
    pub mss: u16,
}

/// The window scale option.
#[derive(Clone, Copy, Debug, Default)]
pub struct OptWinScale {
    /// The shift count.
    pub shift: u8,
}

/// The SACK-permitted option.
#[derive(Clone, Copy, Debug, Default)]
pub struct OptSack;

/// The timestamps option.
#[derive(Clone, Copy, Debug, Default)]
pub struct OptTimestamps {
    /// TSval.
    pub t1: u32,
    /// TSecr.
    pub t2: u32,
}

/// The no-operation option (padding).
#[derive(Clone, Copy, Debug, Default)]
pub struct OptNop;

/// The end-of-option-list option.
#[derive(Clone, Copy, Debug, Default)]
pub struct OptEol;

impl TcpOption {
    /// Writes the kind and, for options longer than one byte, the len, at the start of `out`.
    ///
    /// # Panics
    /// If `out` is shorter than `len` (or empty).
    #[inline]
    pub fn write(out: &mut [u8], kind: OptionKind, len: u8) {
        out[0] = kind as u8;
        if len > 1 {
            out[1] = len;
        }
    }

    /// The alignment of the option block.
    pub const ALIGN: u8 = 4;

    /// The most option bytes a header carries (a data offset of 15 words less the fixed
    /// header).
    pub const MAX_LEN: usize = 15 * 4 - TcpHdr::LEN;
}

impl OptMss {
    /// The option kind.
    pub const KIND: OptionKind = OptionKind::Mss;
    /// The option length.
    pub const LEN: u8 = OptionLen::MSS;
    /// Reads a whole option (kind, len and data).
    #[inline]
    pub fn read(opt: &[u8; OptionLen::MSS as usize]) -> OptMss {
        OptMss { mss: u16::from_be_bytes([opt[2], opt[3]]) }
    }
    /// Writes the whole option.
    #[inline]
    pub fn write(&self, out: &mut [u8; OptionLen::MSS as usize]) {
        TcpOption::write(out, Self::KIND, Self::LEN);
        out[2..4].copy_from_slice(&self.mss.to_be_bytes());
    }
}

impl OptWinScale {
    /// The option kind.
    pub const KIND: OptionKind = OptionKind::WinScale;
    /// The option length.
    pub const LEN: u8 = OptionLen::WIN_SCALE;
    /// Reads a whole option (kind, len and data).
    #[inline]
    pub fn read(opt: &[u8; OptionLen::WIN_SCALE as usize]) -> OptWinScale {
        OptWinScale { shift: opt[2] }
    }
    /// Writes the whole option.
    #[inline]
    pub fn write(&self, out: &mut [u8; OptionLen::WIN_SCALE as usize]) {
        TcpOption::write(out, Self::KIND, Self::LEN);
        out[2] = self.shift;
    }
}

impl OptSack {
    /// The option kind.
    pub const KIND: OptionKind = OptionKind::Sack;
    /// The option length.
    pub const LEN: u8 = OptionLen::SACK;
    /// Reads the option (it has no data).
    #[inline]
    pub fn read(_opt: &[u8; OptionLen::SACK as usize]) -> OptSack {
        OptSack
    }
    /// Writes the whole option.
    #[inline]
    pub fn write(&self, out: &mut [u8; OptionLen::SACK as usize]) {
        TcpOption::write(out, Self::KIND, Self::LEN);
    }
}

impl OptTimestamps {
    /// The option kind.
    pub const KIND: OptionKind = OptionKind::Timestamps;
    /// The option length.
    pub const LEN: u8 = OptionLen::TIMESTAMPS;
    /// Reads a whole option (kind, len and data).
    #[inline]
    pub fn read(opt: &[u8; OptionLen::TIMESTAMPS as usize]) -> OptTimestamps {
        OptTimestamps {
            t1: u32::from_be_bytes([opt[2], opt[3], opt[4], opt[5]]),
            t2: u32::from_be_bytes([opt[6], opt[7], opt[8], opt[9]]),
        }
    }
    /// Writes the whole option.
    #[inline]
    pub fn write(&self, out: &mut [u8; OptionLen::TIMESTAMPS as usize]) {
        TcpOption::write(out, Self::KIND, Self::LEN);
        out[2..6].copy_from_slice(&self.t1.to_be_bytes());
        out[6..10].copy_from_slice(&self.t2.to_be_bytes());
    }
}

impl OptNop {
    /// The option kind.
    pub const KIND: OptionKind = OptionKind::Nop;
    /// The option length.
    pub const LEN: u8 = OptionLen::NOP;
    /// Writes the option.
    #[inline]
    pub fn write(&self, out: &mut [u8; OptionLen::NOP as usize]) {
        TcpOption::write(out, Self::KIND, Self::LEN);
    }
}

impl OptEol {
    /// The option kind.
    pub const KIND: OptionKind = OptionKind::Eol;
    /// The option length.
    pub const LEN: u8 = OptionLen::EOL;
    /// Writes the option.
    #[inline]
    pub fn write(&self, out: &mut [u8; OptionLen::EOL as usize]) {
        TcpOption::write(out, Self::KIND, Self::LEN);
    }
}

/// A TCP sequence number, compared modulo 2^32.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct TcpSeq {
    /// The raw 32-bit value.
    pub raw: u32,
}

/// Converts a sequence number from network to host byte order.
#[inline]
pub fn ntoh_seq(s: TcpSeq) -> TcpSeq {
    TcpSeq { raw: u32::from_be(s.raw) }
}

/// Converts a sequence number from host to network byte order.
#[inline]
pub fn hton_seq(s: TcpSeq) -> TcpSeq {
    TcpSeq { raw: s.raw.to_be() }
}

impl fmt::Display for TcpSeq {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(os, "{}", self.raw)
    }
}

impl fmt::Debug for TcpSeq {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(os, "{}", self.raw)
    }
}

/// A sequence number from its raw 32-bit value.
#[inline]
pub fn make_seq(raw: u32) -> TcpSeq {
    TcpSeq { raw }
}

impl std::ops::AddAssign<i32> for TcpSeq {
    #[inline]
    fn add_assign(&mut self, n: i32) {
        self.raw = self.raw.wrapping_add(n as u32);
    }
}
impl std::ops::SubAssign<i32> for TcpSeq {
    #[inline]
    fn sub_assign(&mut self, n: i32) {
        self.raw = self.raw.wrapping_sub(n as u32);
    }
}
impl std::ops::Add<i32> for TcpSeq {
    type Output = TcpSeq;
    #[inline]
    fn add(mut self, n: i32) -> TcpSeq {
        self += n;
        self
    }
}
impl std::ops::Sub<i32> for TcpSeq {
    type Output = TcpSeq;
    #[inline]
    fn sub(mut self, n: i32) -> TcpSeq {
        self -= n;
        self
    }
}
impl std::ops::Sub<TcpSeq> for TcpSeq {
    type Output = i32;
    #[inline]
    fn sub(self, q: TcpSeq) -> i32 {
        self.raw.wrapping_sub(q.raw) as i32
    }
}
// Equality compares the raw values (derived, consistent with `Hash`).
impl PartialOrd for TcpSeq {
    #[inline]
    fn partial_cmp(&self, q: &TcpSeq) -> Option<std::cmp::Ordering> {
        Some(self.cmp(q))
    }
}
/// Sequence-number ordering (`s < q` when `s - q` is negative as an `i32`): a modular
/// comparison, not a total order, used as it is by the ordered maps keyed by sequence number.
impl Ord for TcpSeq {
    #[inline]
    fn cmp(&self, q: &TcpSeq) -> std::cmp::Ordering {
        (*self - *q).cmp(&0)
    }
}

impl MergerOffset for TcpSeq {
    #[inline]
    fn add_len(self, len: u32) -> Self {
        self + len as i32
    }
    #[inline]
    fn diff(self, earlier: Self) -> usize {
        (self - earlier) as u32 as usize
    }
}

/// The TCP header in host order. The data offset and the flag bits are kept in the two
/// bytes they occupy on the wire.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpHdr {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// Sequence number.
    pub seq: TcpSeq,
    /// Acknowledgment number.
    pub ack: TcpSeq,
    off_byte: u8,
    flags_byte: u8,
    /// Window.
    pub window: u16,
    /// Checksum.
    pub checksum: u16,
    /// Urgent pointer.
    pub urgent: u16,
}

macro_rules! tcp_flag {
    ($get:ident, $set:ident, $bit:expr) => {
        /// The flag bit.
        #[inline]
        pub fn $get(&self) -> bool {
            (self.flags_byte >> $bit) & 1 != 0
        }
        /// Sets the flag bit.
        #[inline]
        pub fn $set(&mut self, v: bool) {
            self.flags_byte = (self.flags_byte & !(1 << $bit)) | ((v as u8) << $bit);
        }
    };
}

impl TcpHdr {
    /// The length of a header without options.
    pub const LEN: usize = 20;

    /// The reserved bits of byte 12.
    #[inline]
    pub fn rsvd1(&self) -> u8 {
        self.off_byte & 15
    }
    /// Header length in 32-bit words.
    #[inline]
    pub fn data_offset(&self) -> u8 {
        self.off_byte >> 4
    }
    /// Sets the header length in 32-bit words.
    #[inline]
    pub fn set_data_offset(&mut self, v: u8) {
        self.off_byte = (self.off_byte & 15) | (v << 4);
    }
    tcp_flag!(f_fin, set_f_fin, 0);
    tcp_flag!(f_syn, set_f_syn, 1);
    tcp_flag!(f_rst, set_f_rst, 2);
    tcp_flag!(f_psh, set_f_psh, 3);
    tcp_flag!(f_ack, set_f_ack, 4);
    tcp_flag!(f_urg, set_f_urg, 5);
    /// The reserved bits of byte 13.
    #[inline]
    pub fn rsvd2(&self) -> u8 {
        (self.flags_byte >> 6) & 3
    }

    /// Parses a header from the wire.
    ///
    /// # Safety
    /// `p` must point to `LEN` readable bytes.
    #[inline]
    pub unsafe fn read(p: *const u8) -> TcpHdr {
        // SAFETY: guaranteed by the caller; every offset is below LEN.
        unsafe {
            TcpHdr {
                src_port: read_be::<u16>(p),
                dst_port: read_be::<u16>(p.add(2)),
                seq: TcpSeq { raw: read_be::<u32>(p.add(4)) },
                ack: TcpSeq { raw: read_be::<u32>(p.add(8)) },
                off_byte: *p.add(12),
                flags_byte: *p.add(13),
                window: read_be::<u16>(p.add(14)),
                checksum: read_be::<u16>(p.add(16)),
                urgent: read_be::<u16>(p.add(18)),
            }
        }
    }

    /// Writes the header to the wire.
    ///
    /// # Safety
    /// `p` must point to `LEN` writable bytes.
    #[inline]
    pub unsafe fn write(&self, p: *mut u8) {
        // SAFETY: guaranteed by the caller; every offset is below LEN.
        unsafe {
            write_be::<u16>(p, self.src_port);
            write_be::<u16>(p.add(2), self.dst_port);
            write_be::<u32>(p.add(4), self.seq.raw);
            write_be::<u32>(p.add(8), self.ack.raw);
            *p.add(12) = self.off_byte;
            *p.add(13) = self.flags_byte;
            write_be::<u16>(p.add(14), self.window);
            write_be::<u16>(p.add(16), self.checksum);
            write_be::<u16>(p.add(18), self.urgent);
        }
    }

    /// Stores a checksum already in network order.
    ///
    /// # Safety
    /// `p` must point to `LEN` writable bytes.
    #[inline]
    pub unsafe fn write_nbo_checksum(p: *mut u8, checksum_in_network_byte_order: u16) {
        // SAFETY: guaranteed by the caller.
        unsafe { (p.add(16) as *mut u16).write_unaligned(checksum_in_network_byte_order) };
    }
}

/// The [`MergerTag`](crate::engine::net::packet_util::MergerTag) of TCP out-of-order data.
pub struct TcpTag;

thread_local! {
    /// Packets linearized by the TCP out-of-order merger on this thread.
    static TCP_LINEARIZATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl crate::engine::net::packet_util::MergerTag for TcpTag {
    #[inline]
    fn linearization_count() -> &'static std::thread::LocalKey<std::cell::Cell<u64>> {
        &TCP_LINEARIZATIONS
    }
}
/// The merger of out-of-order received data, keyed by sequence number.
pub type TcpPacketMerger = PacketMerger<TcpSeq, TcpTag>;

/// The inet traits [`Tcp`] is generic over. The layer reaches through `InetType` into the
/// IPv4 layer and builds IPv4 [`L4Packet`]s, so only IPv4 fits.
pub trait TcpInetTraits:
    InetTraits<
        AddressType = Ipv4Address,
        InetType = Ipv4L4<{ IpProtocolNum::tcp as u8 }>,
        L4Packet = L4Packet,
    >
{
}
impl<T> TcpInetTraits for T where
    T: InetTraits<
            AddressType = Ipv4Address,
            InetType = Ipv4L4<{ IpProtocolNum::tcp as u8 }>,
            L4Packet = L4Packet,
        >
{
}

/// A sent segment waiting for its acknowledgment, kept for retransmission.
struct UnackedSegment {
    p: Packet,
    data_len: u16,
    nr_transmits: u32,
    tx_time: LowresTimePoint,
}

/// The send sequence variables (RFC 793 section 3.2) and send queues of a connection.
struct Send {
    unacknowledged: TcpSeq,
    next: TcpSeq,
    window: u32,
    window_scale: u8,
    mss: u16,
    // Kept for completeness of the RFC 793 send variables; never read.
    #[allow(dead_code)]
    urgent: TcpSeq,
    wl1: TcpSeq,
    wl2: TcpSeq,
    initial: TcpSeq,
    data: VecDeque<UnackedSegment>,
    unsent: VecDeque<Packet>,
    unsent_len: u32,
    closed: bool,
    _window_opened: Promise<()>,
    // Wait for all data are acked
    _all_data_acked_promise: Option<Promise<()>>,
    // Limit number of data queued into send queue
    max_queue_space: usize,
    current_queue_space: usize,
    // wait for there is at least one byte available in the queue
    _send_available_promise: Option<Promise<()>>,
    // Round-trip time variation
    rttvar: Duration,
    // Smoothed round-trip time
    srtt: Duration,
    first_rto_sample: bool,
    syn_tx_time: LowresTimePoint,
    // Congestion window
    cwnd: u32,
    // Slow start threshold
    ssthresh: u32,
    // Duplicated ACKs
    dupacks: u16,
    syn_retransmit: u32,
    fin_retransmit: u32,
    limited_transfer: u32,
    partial_ack: u32,
    recover: TcpSeq,
    window_probe: bool,
    zero_window_probing_out: u8,
}

impl Default for Send {
    fn default() -> Self {
        Send {
            unacknowledged: TcpSeq::default(),
            next: TcpSeq::default(),
            window: 0,
            window_scale: 0,
            mss: 0,
            urgent: TcpSeq::default(),
            wl1: TcpSeq::default(),
            wl2: TcpSeq::default(),
            initial: TcpSeq::default(),
            data: VecDeque::new(),
            unsent: VecDeque::new(),
            unsent_len: 0,
            closed: false,
            _window_opened: Promise::new(),
            _all_data_acked_promise: None,
            max_queue_space: 212992,
            current_queue_space: 0,
            _send_available_promise: None,
            rttvar: Duration::ZERO,
            srtt: Duration::ZERO,
            first_rto_sample: true,
            syn_tx_time: LowresTimePoint::from_nanos(0),
            cwnd: 0,
            ssthresh: 0,
            dupacks: 0,
            syn_retransmit: 0,
            fin_retransmit: 0,
            limited_transfer: 0,
            partial_ack: 0,
            recover: TcpSeq::default(),
            window_probe: false,
            zero_window_probing_out: 0,
        }
    }
}

/// The receive sequence variables and receive queues of a connection.
struct Receive {
    next: TcpSeq,
    window: u32,
    window_scale: u8,
    mss: u16,
    urgent: TcpSeq,
    initial: TcpSeq,
    data: VecDeque<Packet>,
    // The total size of the data stored in `data`
    data_size: usize,
    out_of_order: TcpPacketMerger,
    _data_received_promise: Option<Promise<()>>,
    // The maximun memory buffer size allowed for receiving
    // Currently, it is the same as default receive window size when window scaling is enabled
    max_receive_buf_size: usize,
}

impl Default for Receive {
    fn default() -> Self {
        Receive {
            next: TcpSeq::default(),
            window: 0,
            window_scale: 0,
            mss: 0,
            urgent: TcpSeq::default(),
            initial: TcpSeq::default(),
            data: VecDeque::new(),
            data_size: 0,
            out_of_order: TcpPacketMerger::default(),
            _data_received_promise: None,
            max_receive_buf_size: 3737600,
        }
    }
}

/// The 512-bit secret key for generating initial sequence numbers (RFC 6528).
struct IsnSecret {
    key: [u32; 16],
}

impl IsnSecret {
    fn new() -> Self {
        // A random key, read from /dev/urandom.
        let mut key = [0u32; 16];
        let mut bytes = [0u8; 64];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = f.read_exact(&mut bytes);
        }
        for (i, k) in key.iter_mut().enumerate() {
            *k = u32::from_ne_bytes([
                bytes[4 * i],
                bytes[4 * i + 1],
                bytes[4 * i + 2],
                bytes[4 * i + 3],
            ]);
        }
        IsnSecret { key }
    }
}

/// The process-wide ISN secret, created on first use.
fn isn_secret() -> &'static IsnSecret {
    static S: std::sync::OnceLock<IsnSecret> = std::sync::OnceLock::new();
    S.get_or_init(IsnSecret::new)
}

/// The transmission control block of one connection.
pub struct Tcb<T: TcpInetTraits> {
    /// The tcb's address with the provenance of its reference-counted node, written once by
    /// `Tcb::create`. Everything stored for later (continuations, `shared_from_this`) derives
    /// from it, never from a `&mut self`, whose tag stops being valid as soon as the tcb is
    /// entered again through its shared pointer.
    _this: *mut Tcb<T>,
    _state: TcpState,
    /// The layer's root pointer (`Tcp::_this`).
    _tcp: *mut Tcp<T>,
    // There is no back-pointer to the connection: writing it from the connection's
    // constructor would alias the `&mut Tcb` of the input handler that hands the connection
    // to the listener.
    _connect_done: Promise<()>,
    _fin_recvd_promise: Option<Promise<()>>,
    _local_ip: Ipv4Address,
    _foreign_ip: Ipv4Address,
    _local_port: u16,
    _foreign_port: u16,
    _snd: Send,
    _rcv: Receive,
    _option: TcpOption,
    _delayed_ack: Timer<LowresClock>,
    // Retransmission timeout
    _rto: Duration,
    _persist_time_out: Duration,
    _retransmit: Timer<LowresClock>,
    _persist: Timer<LowresClock>,
    _nr_full_seg_received: u16,
    _packetq: CircularBuffer<L4Packet>,
    _poll_active: bool,
}

type TcbPtr<T> = LwSharedPtr<Tcb<T>>;

// A tcb can produce a shared pointer to itself.
impl<T: TcpInetTraits> EnableLwSharedFromThis for Tcb<T> {}

impl<T: TcpInetTraits> Tcb<T> {
    const CLOSED: TcpState = TcpState::CLOSED;
    const LISTEN: TcpState = TcpState::LISTEN;
    const SYN_SENT: TcpState = TcpState::SYN_SENT;
    const SYN_RECEIVED: TcpState = TcpState::SYN_RECEIVED;
    const ESTABLISHED: TcpState = TcpState::ESTABLISHED;
    const FIN_WAIT_1: TcpState = TcpState::FIN_WAIT_1;
    const FIN_WAIT_2: TcpState = TcpState::FIN_WAIT_2;
    const CLOSE_WAIT: TcpState = TcpState::CLOSE_WAIT;
    const CLOSING: TcpState = TcpState::CLOSING;
    const LAST_ACK: TcpState = TcpState::LAST_ACK;
    const TIME_WAIT: TcpState = TcpState::TIME_WAIT;

    const RTO_MIN: Duration = Duration::from_millis(1000);
    const RTO_MAX: Duration = Duration::from_millis(60000);
    // Clock granularity
    const RTO_CLK_GRANULARITY: Duration = Duration::from_millis(1);
    const MAX_NR_RETRANSMIT: u16 = 5;

    /// The tcp layer, which owns its tcbs (through `_tcbs`) and outlives them.
    ///
    /// Returned as a pointer: the layer is entered from a tcb method (and enters tcbs back),
    /// so each use is a short `(*self.tcp()).f()` while no other reference to the layer is
    /// live (the layer's own entry points, `received`, `connect` and the packet provider,
    /// hold none across their calls into tcbs).
    #[inline]
    fn tcp(&self) -> *mut Tcp<T> {
        self._tcp
    }

    /// A timer of this tcb, pinned.
    ///
    /// # Safety
    /// `t` is one of the timers of a tcb, which live inside the tcb's reference-counted
    /// node and never move.
    #[inline]
    unsafe fn pinned(t: &mut Timer<LowresClock>) -> Pin<&mut Timer<LowresClock>> {
        // SAFETY: guaranteed by the caller.
        unsafe { Pin::new_unchecked(t) }
    }

    #[inline]
    fn get_default_receive_window_size(&self) -> u32 {
        // Linux's default window size
        const SIZE: u32 = 29200;
        SIZE << self._rcv.window_scale
    }

    // Returns the current receive window according to available receiving buffer size
    #[inline]
    fn get_modified_receive_window_size(&self) -> u32 {
        let left: u32 = if self._rcv.data_size > self._rcv.max_receive_buf_size {
            0
        } else {
            (self._rcv.max_receive_buf_size - self._rcv.data_size) as u32
        };
        std::cmp::min(left, self.get_default_receive_window_size())
    }

    /// Removes this tcb from the connection table.
    pub fn remove_from_tcbs(&mut self) {
        let id = L4Connid::<T> {
            local_ip: self._local_ip,
            foreign_ip: self._foreign_ip,
            local_port: self._local_port,
            foreign_port: self._foreign_port,
        };
        // SAFETY: see `tcp`; the caller keeps a reference to this tcb, so removing the table's
        // reference does not destroy it.
        unsafe { (*self.tcp())._tcbs.remove(&id) };
    }

    /// Schedules this tcb for transmission once the peer's link address is known.
    pub fn output(&mut self) {
        if !self._poll_active {
            self._poll_active = true;
            // SAFETY: `_this` comes from the tcb's reference-counted node (Tcb::create), so it
            // covers the counter before the tcb; only the counter is touched.
            let me = unsafe { Self::shared_from_this(self._this) };
            let tcp = self.tcp();
            // FIXME: future is discarded
            // SAFETY: see `tcp`.
            let f = unsafe { (*tcp).poll_tcb(self._foreign_ip, me) };
            if f.available() {
                // `then_wrapped` on a ready future runs in line, inside this call: handled
                // through `self`, since a second `&mut` from `_this` would alias it.
                self.on_polled(f.get());
                return;
            }
            let this = self._this;
            let _ = f.then_wrapped(move |f: Future<()>| {
                // SAFETY: runs later from the reactor, with no other reference to the tcb live;
                // the shared pointer handed to poll_tcb keeps the tcb alive until the packet is
                // polled, and on failure the tcb is still referenced by `_tcbs` or a
                // connection. `this` is the root pointer.
                unsafe { (*this).on_polled(f.get()) };
                make_ready_future(())
            });
        }
    }

    /// The continuation of `output`'s `poll_tcb`.
    fn on_polled(&mut self, r: Result<(), ExceptionPtr>) {
        if let Err(ex) = r {
            if ex.downcast_ref::<ArpQueueFullError>().is_some() {
                // retry later
                self._poll_active = false;
                self.start_retransmit_timer();
            } else if let Some(e) = ex.downcast_ref::<ArpTimeoutError>()
                && self.in_state(Self::SYN_SENT.into())
            {
                self._connect_done.set_exception(ExceptionPtr::from(e.clone()));
                self.cleanup();
            }
            // in other states connection should time out
        }
    }

    /// Resolves when the connection is established.
    #[inline]
    pub fn connect_done(&mut self) -> Future<()> {
        self._connect_done.get_future()
    }

    /// The state of the connection.
    #[inline]
    pub fn state(&mut self) -> &mut TcpState {
        &mut self._state
    }

    #[inline]
    fn retransmit_one(&mut self) {
        let data_retransmit = true;
        self.output_one(data_retransmit);
    }

    #[inline]
    fn start_retransmit_timer(&mut self) {
        let now = LowresClock::now();
        self.start_retransmit_timer_at(now);
    }

    #[inline]
    fn start_retransmit_timer_at(&mut self, now: LowresTimePoint) {
        let tp = now + self._rto;
        // SAFETY: a timer of this tcb (see `pinned`).
        unsafe { Self::pinned(&mut self._retransmit) }.rearm(tp, None);
    }

    #[inline]
    fn stop_retransmit_timer(&mut self) {
        // SAFETY: a timer of this tcb (see `pinned`).
        unsafe { Self::pinned(&mut self._retransmit) }.cancel();
    }

    #[inline]
    fn start_persist_timer(&mut self) {
        let now = LowresClock::now();
        self.start_persist_timer_at(now);
    }

    #[inline]
    fn start_persist_timer_at(&mut self, now: LowresTimePoint) {
        let tp = now + self._persist_time_out;
        // SAFETY: a timer of this tcb (see `pinned`).
        unsafe { Self::pinned(&mut self._persist) }.rearm(tp, None);
    }

    #[inline]
    fn stop_persist_timer(&mut self) {
        // SAFETY: a timer of this tcb (see `pinned`).
        unsafe { Self::pinned(&mut self._persist) }.cancel();
    }

    fn can_send(&mut self) -> u32 {
        if self._snd.window_probe {
            return 1;
        }

        // Can not send if send window is zero
        if self._snd.window == 0 {
            return 0;
        }

        // Can not send if send window is less than unacknowledged data size
        let window_used = (self._snd.next - self._snd.unacknowledged) as u32;
        if window_used > self._snd.window {
            return 0;
        }

        // Can not send more than advertised window allows or unsent data size
        let mut x = std::cmp::min(self._snd.window - window_used, self._snd.unsent_len);

        // Can not send more than congestion window allows
        x = std::cmp::min(self._snd.cwnd, x);
        if self._snd.dupacks == 1 || self._snd.dupacks == 2 {
            // RFC5681 Step 3.1
            // Send cwnd + 2 * smss per RFC3042
            let flight = self.flight_size();
            let max = self._snd.cwnd + 2 * self._snd.mss as u32;
            x = if flight <= max { std::cmp::min(x, max - flight) } else { 0 };
            self._snd.limited_transfer += x;
        } else if self._snd.dupacks >= 3 {
            // RFC5681 Step 3.5
            // Sent 1 full-sized segment at most
            x = std::cmp::min(self._snd.mss as u32, x);
        }
        x
    }

    #[inline]
    fn flight_size(&self) -> u32 {
        let mut size: u32 = 0;
        for seg in self._snd.data.iter() {
            size += seg.p.len();
        }
        size
    }

    #[inline]
    fn local_mss(&self) -> u16 {
        // SAFETY: see `tcp`.
        unsafe { (*self.tcp()).hw_features().mtu }
            .wrapping_sub(TCP_HDR_LEN_MIN as u16)
            .wrapping_sub(T::IP_HDR_LEN_MIN as u16)
    }

    #[inline]
    fn queue_packet(&mut self, p: Packet) {
        self._packetq.push_back(L4Packet {
            to: self._foreign_ip,
            p,
            e_dst: EthernetAddress::default(),
            proto_num: IpProtocolNum::unused,
        });
    }

    #[inline]
    fn signal_data_received(&mut self) {
        if let Some(mut p) = self._rcv._data_received_promise.take() {
            p.set_value(());
        }
    }

    #[inline]
    fn signal_all_data_acked(&mut self) {
        if self._snd._all_data_acked_promise.is_some()
            && self._snd.unsent_len == 0
            && let Some(mut p) = self._snd._all_data_acked_promise.take()
        {
            p.set_value(());
        }
    }

    #[inline]
    fn signal_send_available(&mut self) {
        if self._snd._send_available_promise.is_some()
            && self._snd.max_queue_space > self._snd.current_queue_space
            && let Some(mut p) = self._snd._send_available_promise.take()
        {
            p.set_value(());
        }
    }

    fn do_syn_sent(&mut self) {
        self._state = Self::SYN_SENT;
        self._snd.syn_tx_time = LowresClock::now();
        // Send <SYN> to remote
        self.output();
    }

    fn do_syn_received(&mut self) {
        self._state = Self::SYN_RECEIVED;
        self._snd.syn_tx_time = LowresClock::now();
        // Send <SYN,ACK> to remote
        self.output();
    }

    fn do_established(&mut self) {
        self._state = Self::ESTABLISHED;
        let t = self._snd.syn_tx_time;
        self.update_rto(t);
        self._connect_done.set_value(());
    }

    fn do_reset(&mut self) {
        self._state = Self::CLOSED;
        self.cleanup();
        if let Some(mut p) = self._rcv._data_received_promise.take() {
            p.set_exception(ExceptionPtr::from(tcp_reset_error()));
        }
        if let Some(mut p) = self._snd._all_data_acked_promise.take() {
            p.set_exception(ExceptionPtr::from(tcp_reset_error()));
        }
        if let Some(mut p) = self._snd._send_available_promise.take() {
            p.set_exception(ExceptionPtr::from(tcp_reset_error()));
        }
    }

    fn do_time_wait(&mut self) {
        // FIXME: Implement TIME_WAIT state timer
        self._state = Self::TIME_WAIT;
        self.cleanup();
    }

    fn do_closed(&mut self) {
        self._state = Self::CLOSED;
        self.cleanup();
    }

    fn do_setup_isn(&mut self) {
        self._snd.initial = self.get_isn();
        self._snd.unacknowledged = self._snd.initial;
        self._snd.next = self._snd.initial + 1;
        self._snd.recover = self._snd.initial;
    }

    fn do_local_fin_acked(&mut self) {
        self._snd.unacknowledged += 1;
        self._snd.next += 1;
    }

    #[inline]
    fn syn_needs_on(&self) -> bool {
        self.in_state(Self::SYN_SENT | Self::SYN_RECEIVED)
    }

    #[inline]
    fn fin_needs_on(&self) -> bool {
        self.in_state(Self::FIN_WAIT_1 | Self::CLOSING | Self::LAST_ACK)
            && self._snd.closed
            && self._snd.unsent_len == 0
    }

    #[inline]
    fn ack_needs_on(&self) -> bool {
        !self.in_state(Self::CLOSED | Self::LISTEN | Self::SYN_SENT)
    }

    #[inline]
    fn foreign_will_not_send(&self) -> bool {
        self.in_state(
            Self::CLOSING | Self::TIME_WAIT | Self::CLOSE_WAIT | Self::LAST_ACK | Self::CLOSED,
        )
    }

    #[inline]
    fn in_state(&self, state: TcpStateSet) -> bool {
        (self._state as u16 & state.0) != 0
    }

    #[inline]
    fn exit_fast_recovery(&mut self) {
        self._snd.dupacks = 0;
        self._snd.limited_transfer = 0;
        self._snd.partial_ack = 0;
    }
}

/// The tcp layer of one protocol.
pub struct Tcp<T: TcpInetTraits> {
    /// The layer's address with the provenance of its node, written once by `Tcp::new`;
    /// tcbs, listeners and continuations keep this one.
    _this: *mut Tcp<T>,
    /// A connection the accept queue rejected while a tcb input handler ran. Its destructor
    /// (which enters the tcb) runs once the handler has returned (`Tcp::received`).
    _rejected: Option<Connection<T>>,
    // A copy of the `ipv4_l4` view (one pointer), not a pointer into `ipv4_tcp`: the layer is
    // entered while `ipv4_tcp` is mutably borrowed (see `Ipv4L4`).
    _inet: Ipv4L4<{ IpProtocolNum::tcp as u8 }>,
    _tcbs: HashMap<L4Connid<T>, TcbPtr<T>, FxBuildHasher>,
    _listening: HashMap<u16, *mut ListenerState<T>, FxBuildHasher>,
    _e: u64,
    _poll_tcbs: CircularBuffer<(TcbPtr<T>, EthernetAddress)>,
    // queue for packets that do not belong to any tcb
    _packetq: CircularBuffer<L4Packet>,
    _queue_space: Semaphore,
    _metrics: MetricGroups,
}

/// The range of local ports chosen at random for outgoing connections.
const PORT_DIST_MIN: u16 = 41952;
const PORT_DIST_MAX: u16 = 65535;

impl<T: TcpInetTraits> Tcp<T> {
    /// The layer's root pointer, with the provenance of its node: what the sockets and
    /// connections made from it keep.
    #[inline]
    pub fn root_ptr(&self) -> *mut Tcp<T> {
        self._this
    }

    /// The ipv4 view this layer uses.
    #[inline]
    pub fn inet(&self) -> &Ipv4L4<{ IpProtocolNum::tcp as u8 }> {
        &self._inet
    }

    fn port_dist(&mut self) -> u16 {
        self._e = (self._e * 16807) % 2147483647;
        let range = (PORT_DIST_MAX - PORT_DIST_MIN) as u64 + 1;
        PORT_DIST_MIN + ((self._e - 1) % range) as u16
    }
}

/// A connection handed to the user: keeps its tcb alive.
pub struct Connection<T: TcpInetTraits> {
    _tcb: Option<TcbPtr<T>>,
}

impl<T: TcpInetTraits> Connection<T> {
    /// Wraps the tcb `tcbp`. The tcb keeps no pointer back to the connection, so a
    /// connection moves freely.
    #[inline]
    pub fn new(tcbp: TcbPtr<T>) -> Self {
        Connection { _tcb: Some(tcbp) }
    }

    /// The tcb. Each use is a short `(*self.tcb()).f()`: the user-facing calls of a
    /// connection never run while a tcb method is on the stack (they come from the reactor).
    #[inline]
    fn tcb(&self) -> *mut Tcb<T> {
        // SAFETY: `_tcb` is only cleared by the destructor.
        unsafe { self._tcb.as_ref().unwrap_unchecked() }.get()
    }

    /// Resolves when the connection is established.
    #[inline]
    pub fn connected(&mut self) -> Future<()> {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb()).connect_done() }
    }
    /// Queues `p` for sending.
    #[inline]
    pub fn send(&mut self, p: Packet) -> Future<()> {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb()).send(p) }
    }
    /// Resolves when data can be read.
    #[inline]
    pub fn wait_for_data(&mut self) -> Future<()> {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb()).wait_for_data() }
    }
    /// Resolves when the peer shuts down its sending side.
    #[inline]
    pub fn wait_input_shutdown(&mut self) -> Future<()> {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb()).wait_input_shutdown() }
    }
    /// Takes the received data.
    #[inline]
    pub fn read(&mut self) -> Packet {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb()).read() }
    }
    /// The peer's address.
    #[inline]
    pub fn foreign_ip(&self) -> Ipv4Address {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb())._foreign_ip }
    }
    /// The peer's port.
    #[inline]
    pub fn foreign_port(&self) -> u16 {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb())._foreign_port }
    }
    /// Our address.
    #[inline]
    pub fn local_ip(&self) -> Ipv4Address {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb())._local_ip }
    }
    /// Our port.
    #[inline]
    pub fn local_port(&self) -> u16 {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb())._local_port }
    }
}

/// The part of a listener the tcp layer reaches through `_listening`: in its own node, so
/// that the [`Listener`] handle moves freely.
pub struct ListenerState<T: TcpInetTraits> {
    _q: Queue<Connection<T>>,
    _pending: usize,
}

impl<T: TcpInetTraits> ListenerState<T> {
    /// Whether the accept queue is full.
    #[inline]
    pub fn full(&self) -> bool {
        self._pending + self._q.size() >= self._q.max_size()
    }
    /// One more connection is being established.
    #[inline]
    pub fn inc_pending(&mut self) {
        self._pending += 1;
    }
    /// A connection finished being established.
    #[inline]
    pub fn dec_pending(&mut self) {
        self._pending -= 1;
    }
}

/// A listening port: accepted connections are queued here.
pub struct Listener<T: TcpInetTraits> {
    _tcp: *mut Tcp<T>,
    _port: u16,
    _state: NodePtr<ListenerState<T>>,
}

impl<T: TcpInetTraits> Listener<T> {
    /// Listens on `port` of `t` with an accept queue of `queue_length` connections,
    /// registering the listener's state with the layer. The registration goes through `t`,
    /// the caller's live reference; the listener keeps the layer's root pointer.
    fn new(t: &mut Tcp<T>, port: u16, queue_length: usize) -> Self {
        let state = NodePtr::new(ListenerState { _q: Queue::new(queue_length), _pending: 0 });
        t._listening.entry(port).or_insert(state.as_ptr());
        Listener { _tcp: t._this, _port: port, _state: state }
    }

    /// The state, for a short access (the tcp layer touches it only from `received`, which
    /// never runs while a listener method is on the stack).
    #[inline]
    fn state(&mut self) -> &mut ListenerState<T> {
        // SAFETY: as above; `&mut self` makes this the only access from the handle.
        unsafe { self._state.get_mut() }
    }

    /// Resolves with the next accepted connection.
    #[inline]
    pub fn accept(&mut self) -> Future<Connection<T>> {
        // SAFETY: the queue lives in the listener's state, which outlives its accepts.
        unsafe { self.state()._q.pop_eventually() }
    }
    /// Fails the pending and future accepts.
    #[inline]
    pub fn abort_accept(&mut self) {
        self.state()._q.abort(ExceptionPtr::from(std::io::Error::from_raw_os_error(ECONNABORTED)));
    }
    /// Whether the accept queue is full.
    #[inline]
    pub fn full(&self) -> bool {
        // SAFETY: see `state`; a shared access.
        unsafe { self._state.get() }.full()
    }
    /// One more connection is being established.
    #[inline]
    pub fn inc_pending(&mut self) {
        self.state().inc_pending();
    }
    /// A connection finished being established.
    #[inline]
    pub fn dec_pending(&mut self) {
        self.state().dec_pending();
    }
    /// The tcp layer.
    #[inline]
    pub fn get_tcp(&self) -> &Tcp<T> {
        // SAFETY: the tcp layer outlives its listeners; this accessor is only used outside
        // the layer's own entry points.
        unsafe { &*self._tcp }
    }
    /// The listening port.
    #[inline]
    pub fn port(&self) -> u16 {
        self._port
    }
}

impl<T: TcpInetTraits> Drop for Listener<T> {
    fn drop(&mut self) {
        if self._port != 0 {
            // SAFETY: the tcp layer outlives its listeners; no reference to it is live here.
            unsafe { (*self._tcp)._listening.remove(&self._port) };
        }
    }
}

impl<T: TcpInetTraits> Tcp<T> {
    /// The offloads of the interface.
    #[inline]
    pub fn hw_features(&self) -> &HwFeatures {
        self.inet().hw_features()
    }

    /// Queues an accepted connection on its listener.
    pub fn add_connected_tcb(&mut self, tcbp: TcbPtr<T>, local_port: u16) {
        if let Some(&l) = self._listening.get(&local_port) {
            // SAFETY: listeners unregister their state before it is destroyed, and no
            // listener method is on the stack (this runs from `received`).
            let l = unsafe { &mut *l };
            // A full queue drops the connection (which closes it). Its destructor enters the
            // tcb whose input handler is running, so it is kept until that handler returns
            // (see `_rejected`).
            if let Err(rejected) = l._q.push(Connection::new(tcbp)) {
                self._rejected = Some(rejected);
            }
            l.dec_pending();
        }
    }

    /// Creates the TCP layer over `inet`, in its own node because the packet provider it
    /// registers keeps its address.
    pub fn new(inet: Ipv4L4<{ IpProtocolNum::tcp as u8 }>) -> NodePtr<Self> {
        let mut seed = [0u8; 4];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = f.read_exact(&mut seed);
        }
        let e = (u32::from_ne_bytes(seed) as u64 % 2147483647).max(1);
        let this = NodePtr::new(Tcp::<T> {
            _this: std::ptr::null_mut(),
            _rejected: None,
            _inet: inet,
            _tcbs: HashMap::default(),
            _listening: HashMap::default(),
            _e: e,
            _poll_tcbs: CircularBuffer::new(),
            _packetq: CircularBuffer::new(),
            _queue_space: Semaphore::new(212992),
            _metrics: MetricGroups::new(),
        });

        let tp: *mut Tcp<T> = this.as_ptr();
        // SAFETY: just made; nothing else refers to it yet. `tp` is the node's root pointer.
        unsafe { (*tp)._this = tp };
        // Registration fails only on a double registration, which cannot happen for a new
        // layer; this constructor cannot fail.
        // SAFETY: as above.
        let _ = unsafe { &mut (*tp)._metrics }.add_group(
            "tcp",
            vec![sm::make_counter(
                "linearizations",
                TcpPacketMerger::linearizations,
                sm::Description::new(
                    "Counts a number of times a buffer linearization was invoked during the buffers merge process. \
                     Divide it by a total TCP receive packet rate to get an everage number of lineraizations per TCP packet.",
                ),
                vec![],
            )],
        );

        let mut tcb_polled: u32 = 0;
        // SAFETY: as above.
        unsafe { &mut (*tp)._inet }.register_packet_provider(Box::new(move || {
            // The tcp layer owns this provider's registration and outlives it; the provider
            // runs from the interface's poller, with no reference to the layer live, and makes
            // only short accesses through `tp`, none across `get_packet` (which enters the layer
            // back through `Tcb::tcp`).
            let mut l4p: Option<L4Packet> = None;
            // SAFETY: `tp` is live (see above); a short access to one field.
            let mut c = unsafe { (*tp)._poll_tcbs.size() };
            // SAFETY: as above.
            if unsafe { !(*tp)._packetq.empty() } && (tcb_polled.is_multiple_of(128) || c == 0) {
                // SAFETY: as above.
                l4p = unsafe { (*tp)._packetq.take_front() };
                if let Some(l) = &l4p {
                    // SAFETY: as above; the borrow of `l4p` is local, not into the layer.
                    unsafe { (*tp)._queue_space.signal(l.p.len() as usize) };
                }
            } else {
                while c > 0 {
                    c -= 1;
                    tcb_polled = tcb_polled.wrapping_add(1);
                    // SAFETY: as above; the borrow ends when `take_front` returns.
                    let Some((tcb, dst)) = (unsafe { (*tp)._poll_tcbs.take_front() }) else {
                        break;
                    };
                    // SAFETY: the provider holds no reference to the tcb or the layer.
                    l4p = unsafe { tcb.get_mut_unchecked() }.get_packet();
                    if let Some(l) = l4p.as_mut() {
                        l.e_dst = dst;
                        break;
                    }
                }
            }
            l4p
        }));
        this
    }

    /// Queues `tcb` to be polled for a packet once the link address of `to` is known.
    pub fn poll_tcb(&mut self, to: Ipv4Address, tcb: TcbPtr<T>) -> Future<()> {
        let f = self.inet().get_l2_dst_address(to);
        if f.available() {
            // `then` on a ready future runs in line: done through `self`, which a write
            // through `_this` would invalidate while this call is live.
            return match f.get() {
                Ok(dst) => {
                    self._poll_tcbs.push_back((tcb, dst));
                    make_ready_future(())
                }
                Err(ex) => make_exception_future(ex),
            };
        }
        let this = self._this;
        f.then(move |dst: EthernetAddress| {
            // SAFETY: the tcp layer outlives the address resolution; this runs later from the
            // reactor with no reference to the layer live, through its root pointer.
            unsafe { (*this)._poll_tcbs.push_back((tcb, dst)) };
            make_ready_future(())
        })
    }

    /// Starts listening on `port`.
    pub fn listen(&mut self, port: u16, queue_length: usize) -> Listener<T> {
        Listener::new(self, port, queue_length)
    }

    /// `connect` through the layer's pointer: the new tcb's `connect` enters the layer back
    /// (`output` → `poll_tcb`), so no reference to it is held across that call.
    ///
    /// # Safety
    /// `this` is a live tcp layer on this shard, with no reference to it live.
    pub unsafe fn connect_raw(this: *mut Tcp<T>, sa: SocketAddress) -> Connection<T> {
        // SAFETY: guaranteed by the caller; `prepare_connect` returns before the tcb runs.
        let tcbp = unsafe { (*this).prepare_connect(sa) };
        // SAFETY: the tcb was just made; nothing else refers to it.
        unsafe { tcbp.get_mut_unchecked() }.connect();
        Connection::new(tcbp)
    }

    /// The part of `connect` before the tcb starts: picks the port and makes the tcb.
    fn prepare_connect(&mut self, sa: SocketAddress) -> TcbPtr<T> {
        let mut id: L4Connid<T>;
        let src_ip = self.inet().host_address();
        let dst_ip = Ipv4Address::from_ipv4_addr(
            crate::engine::net::socket_defs::Ipv4Addr::from_socket_address(&sa),
        );
        let dst_port = u16::from_be(sa.as_posix_sockaddr_in().sin_port);

        if Smp::count() > 1 {
            loop {
                id = L4Connid {
                    local_ip: src_ip,
                    foreign_ip: dst_ip,
                    local_port: self.port_dist(),
                    foreign_port: dst_port,
                };
                // SAFETY: the interface outlives the stack.
                let netif = unsafe { &mut *self.inet().netif() };
                if !(netif.hash2cpu(id.hash(netif.rss_key())) != this_shard_id()
                    || self._tcbs.contains_key(&id))
                {
                    break;
                }
            }
        } else {
            id = L4Connid {
                local_ip: src_ip,
                foreign_ip: dst_ip,
                local_port: self.port_dist(),
                foreign_port: dst_port,
            };
        }

        // The tcb keeps the layer's root pointer, not one from `self`.
        let tcbp = Tcb::<T>::create(self._this, id);
        self._tcbs.insert(id, tcbp.clone());
        tcbp
    }

    /// Fills the hash used to pick the shard of a packet: the two ports.
    pub fn forward(&mut self, out_hash_data: &mut ForwardHash, p: &mut Packet, off: usize) -> bool {
        let th = p.get_header_raw(off, TcpHdr::LEN);
        if !th.is_null() {
            // src_port, dst_port in network byte order
            // SAFETY: get_header returned TcpHdr::LEN contiguous bytes.
            unsafe {
                out_hash_data.push_back_u8(*th);
                out_hash_data.push_back_u8(*th.add(1));
                out_hash_data.push_back_u8(*th.add(2));
                out_hash_data.push_back_u8(*th.add(3));
            }
        }
        true
    }

    /// Handles a segment received from `from` for `to`.
    ///
    /// The tcb input handlers enter the layer back (`add_connected_tcb`, `respond_with_reset`,
    /// `poll_tcb`), so this works through `this` with short accesses only.
    ///
    /// # Safety
    /// `this` is the layer's root pointer (`Tcp::_this`), live on this shard, with no
    /// reference to it live.
    pub unsafe fn received(this: *mut Tcp<T>, p: Packet, from: Ipv4Address, to: Ipv4Address) {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::received_segment(this, p, from, to) };
        // A connection rejected by a full accept queue is destroyed here, after the tcb's
        // input handler has returned: its destructor enters that tcb.
        // SAFETY: as above; a short access, and the drop below runs with no reference live.
        let rejected = unsafe { (*this)._rejected.take() };
        drop(rejected);
    }

    /// The body of `received`.
    ///
    /// # Safety
    /// As for `received`.
    unsafe fn received_segment(
        this: *mut Tcp<T>,
        mut p: Packet,
        from: Ipv4Address,
        to: Ipv4Address,
    ) {
        let th = p.get_header_raw(0, TcpHdr::LEN);
        if th.is_null() {
            return;
        }
        // data_offset is correct even before ntoh()
        // SAFETY: get_header returned TcpHdr::LEN contiguous bytes.
        let data_offset = unsafe { *th.add(12) } >> 4;
        if (data_offset as usize * 4) < TcpHdr::LEN {
            return;
        }
        // A segment whose data offset points past its end would make the input handlers read
        // the options through a null `get_header` (for example a 20-byte SYN with
        // data_offset = 15). Such a segment is dropped.
        if data_offset as usize * 4 > p.len() as usize {
            return;
        }

        // SAFETY: the caller upholds this function's `# Safety` contract (as for `received`). Each
        // `(*this)` access below is a short borrow that ends before a tcb method runs.
        unsafe {
            if !(*this).hw_features().rx_csum_offload {
                let mut csum = Checksummer::default();
                T::tcp_pseudo_header_checksum(&mut csum, from, to, p.len() as u16);
                csum.sum_packet(&p);
                if csum.get() != 0 {
                    return;
                }
            }
            let mut h = TcpHdr::read(th);
            let id = L4Connid::<T> {
                local_ip: to,
                foreign_ip: from,
                local_port: h.dst_port,
                foreign_port: h.src_port,
            };
            let tcbp: TcbPtr<T>;
            match (*this)._tcbs.get(&id).cloned() {
                None => {
                    let listener = (*this)._listening.get(&id.local_port).copied();
                    // listeners unregister their state before it is destroyed.
                    match listener.filter(|&l| !(*l).full()) {
                        None => {
                            // 1) In CLOSE state
                            // 1.1 all data in the incoming segment is discarded.  An incoming
                            // segment containing a RST is discarded. An incoming segment not
                            // containing a RST causes a RST to be sent in response.
                            // FIXME:
                            //      if ACK off: <SEQ=0><ACK=SEG.SEQ+SEG.LEN><CTL=RST,ACK>
                            //      if ACK on:  <SEQ=SEG.ACK><CTL=RST>
                            (*this).respond_with_reset(&h, id.local_ip, id.foreign_ip)
                        }
                        Some(listener) => {
                            // 2) In LISTEN state
                            // 2.1 first check for an RST
                            if h.f_rst() {
                                // An incoming RST should be ignored
                                return;
                            }
                            // 2.2 second check for an ACK
                            if h.f_ack() {
                                // Any acknowledgment is bad if it arrives on a connection
                                // still in the LISTEN state.
                                // <SEQ=SEG.ACK><CTL=RST>
                                return (*this).respond_with_reset(&h, id.local_ip, id.foreign_ip);
                            }
                            // 2.3 third check for a SYN
                            if h.f_syn() {
                                // check the security
                                // NOTE: Ignored for now
                                tcbp = Tcb::<T>::create(this, id);
                                (*this)._tcbs.insert(id, tcbp.clone());
                                // TODO: we need to remove the tcb and decrease the pending if
                                // it stays SYN_RECEIVED state forever.
                                (*listener).inc_pending();

                                // no reference to the new tcb exists besides this one.
                                tcbp.get_mut_unchecked().input_handle_listen_state(&mut h, p)
                            }
                            // 2.4 fourth other text or control
                            // So you are unlikely to get here, but if you do, drop the
                            // segment, and return.
                        }
                    }
                }
                Some(t) => {
                    // the input handlers below are the only reference to the tcb while they
                    // run (it is entered from the reactor only).
                    tcbp = t;
                    let tcb = tcbp.get_mut_unchecked();
                    if *tcb.state() == TcpState::SYN_SENT {
                        // 3) In SYN_SENT State
                        tcb.input_handle_syn_sent_state(&mut h, p)
                    } else {
                        // 4) In other state, can be one of the following:
                        // SYN_RECEIVED, ESTABLISHED, FIN_WAIT_1, FIN_WAIT_2
                        // CLOSE_WAIT, CLOSING, LAST_ACK, TIME_WAIT
                        tcb.input_handle_other_state(&mut h, p)
                    }
                }
            }
        }
    }

    // Send packet does not belong to any tcb
    fn send_packet_without_tcb(&mut self, _from: Ipv4Address, to: Ipv4Address, p: Packet) {
        if self._queue_space.try_wait(p.len() as usize) {
            // drop packets that do not fit the queue
            // FIXME: future is discarded
            let f = self.inet().get_l2_dst_address(to);
            if f.available() {
                // `then` on a ready future runs in line: done through `self` (see poll_tcb).
                if let Ok(e_dst) = f.get() {
                    self._packetq.push_back(L4Packet {
                        to,
                        p,
                        e_dst,
                        proto_num: IpProtocolNum::tcp,
                    });
                }
                return;
            }
            let this = self._this;
            let _ = f.then(move |e_dst: EthernetAddress| {
                // SAFETY: the tcp layer outlives the address resolution; this runs later from
                // the reactor with no reference to the layer live, through its root pointer.
                unsafe {
                    (*this)._packetq.push_back(L4Packet {
                        to,
                        p,
                        e_dst,
                        proto_num: IpProtocolNum::tcp,
                    })
                };
                make_ready_future(())
            });
        }
    }
}

impl<T: TcpInetTraits> Drop for Connection<T> {
    fn drop(&mut self) {
        if self._tcb.is_some() {
            self.close_read();
            self.close_write();
        }
    }
}

impl<T: TcpInetTraits> Tcb<T> {
    /// Creates the tcb of connection `id` inside a new reference-counted node; the timer
    /// callbacks keep the tcb's final address.
    /// `t` is the layer's root pointer (`Tcp::_this`); it is only stored here.
    pub fn create(t: *mut Tcp<T>, id: L4Connid<T>) -> TcbPtr<T> {
        let p = make_lw_shared(Tcb::<T> {
            _this: std::ptr::null_mut(),
            _state: TcpState::CLOSED,
            _tcp: t,
            _connect_done: Promise::new(),
            _fin_recvd_promise: Some(Promise::new()),
            _local_ip: id.local_ip,
            _foreign_ip: id.foreign_ip,
            _local_port: id.local_port,
            _foreign_port: id.foreign_port,
            _snd: Send::default(),
            _rcv: Receive::default(),
            _option: TcpOption::default(),
            _delayed_ack: Timer::new(),
            _rto: Duration::from_millis(1000),
            _persist_time_out: Duration::from_millis(1000),
            _retransmit: Timer::new(),
            _persist: Timer::new(),
            _nr_full_seg_received: 0,
            _packetq: CircularBuffer::new(),
            _poll_active: false,
        });
        let tp: *mut Tcb<T> = p.get();
        // SAFETY: the tcb was just made and nothing else refers to it; its timers sit inside
        // the reference-counted node. In the callbacks: the timers are owned by the tcb, so it
        // outlives them, and they run from the reactor with no other reference to it live.
        // `tp` is the root pointer of the node, kept in `_this`.
        unsafe {
            (*tp)._this = tp;
            Self::pinned(&mut (*tp)._delayed_ack).set_callback(move || {
                (*tp)._nr_full_seg_received = 0;
                (*tp).output();
            });
            Self::pinned(&mut (*tp)._retransmit).set_callback(move || (*tp).retransmit());
            Self::pinned(&mut (*tp)._persist).set_callback(move || (*tp).persist());
        }
        p
    }

    fn respond_with_reset(&mut self, rth: &TcpHdr) {
        let (l, f) = (self._local_ip, self._foreign_ip);
        // SAFETY: see `tcp`.
        unsafe { (*self.tcp()).respond_with_reset(rth, l, f) };
    }
}

impl<T: TcpInetTraits> Tcp<T> {
    fn respond_with_reset(&mut self, rth: &TcpHdr, local_ip: Ipv4Address, foreign_ip: Ipv4Address) {
        if rth.f_rst() {
            return;
        }
        let mut p = Packet::new();
        let th = p.prepend_uninitialized_header(TcpHdr::LEN);
        let mut h = TcpHdr { src_port: rth.dst_port, dst_port: rth.src_port, ..Default::default() };
        if rth.f_ack() {
            h.seq = rth.ack;
        }
        // If this RST packet is in response to a SYN packet. We ACK the ISN.
        if rth.f_syn() {
            h.ack = rth.seq + 1;
            h.set_f_ack(true);
        }
        h.set_f_rst(true);
        h.set_data_offset((TcpHdr::LEN / 4) as u8);
        h.checksum = 0;
        // SAFETY: the header was just prepended.
        unsafe { h.write(th) };

        let mut csum = Checksummer::default();
        let mut oi = OffloadInfo::default();
        T::tcp_pseudo_header_checksum(&mut csum, local_ip, foreign_ip, TcpHdr::LEN as u16);
        let checksum: u16;
        if self.hw_features().tx_csum_l4_offload {
            checksum = !csum.get();
            oi.needs_csum = true;
        } else {
            csum.sum_packet(&p);
            checksum = csum.get();
            oi.needs_csum = false;
        }
        // SAFETY: as above.
        unsafe { TcpHdr::write_nbo_checksum(th, checksum) };

        oi.protocol = IpProtocolNum::tcp;
        oi.tcp_hdr_len = TcpHdr::LEN as u8;
        p.set_offload_info(oi);

        self.send_packet_without_tcb(local_ip, foreign_ip, p);
    }
}

/// Copies the option bytes of the segment `p` (header `th`, not yet trimmed) into a buffer of
/// its own, so that parsing them does not depend on the packet's fragments; returns the buffer
/// and the number of option bytes in it.
///
/// `received_segment` drops segments whose data offset is below the fixed header or past the
/// end of the packet; a header that does not pass those checks here yields no options.
fn copy_options(th: &TcpHdr, p: &mut Packet) -> ([u8; TcpOption::MAX_LEN], usize) {
    let mut buf = [0u8; TcpOption::MAX_LEN];
    let hdr_len = th.data_offset() as usize * 4;
    let Some(opt_len) = hdr_len.checked_sub(TcpHdr::LEN) else {
        return (buf, 0);
    };
    let hdr = p.get_header_raw(0, hdr_len);
    if hdr.is_null() {
        return (buf, 0);
    }
    // SAFETY: `get_header_raw` returned a non-null pointer to `hdr_len` contiguous, initialized
    // bytes of the packet (linearizing them if needed), live while `p` is not modified, which
    // holds until the copy below ends; the options are the last `opt_len` of them. `opt_len`
    // is at most `MAX_LEN` because the data offset is a 4-bit field.
    let opts = unsafe { std::slice::from_raw_parts(hdr.add(TcpHdr::LEN), opt_len) };
    buf[..opt_len].copy_from_slice(opts);
    (buf, opt_len)
}

impl<T: TcpInetTraits> Tcb<T> {
    fn data_segment_acked(&mut self, seg_ack: TcpSeq) -> u32 {
        let mut total_acked_bytes: u32 = 0;
        // Full ACK of segment
        while let Some(front) = self._snd.data.front() {
            if !(self._snd.unacknowledged + front.p.len() as i32 <= seg_ack) {
                break;
            }
            let acked_bytes = front.p.len();
            let nr_transmits = front.nr_transmits;
            let tx_time = front.tx_time;
            let data_len = front.data_len;
            self._snd.unacknowledged += acked_bytes as i32;
            // Ignore retransmitted segments when setting the RTO
            if nr_transmits == 0 {
                self.update_rto(tx_time);
            }
            self.update_cwnd(acked_bytes);
            total_acked_bytes += acked_bytes;
            self._snd.current_queue_space -= data_len as usize;
            self.signal_send_available();
            self._snd.data.pop_front();
        }
        // Partial ACK of segment
        if self._snd.unacknowledged < seg_ack {
            let acked_bytes = (seg_ack - self._snd.unacknowledged) as u32;
            if let Some(unacked_seg) = self._snd.data.front_mut() {
                unacked_seg.p.trim_front(acked_bytes as usize);
            }
            self._snd.unacknowledged = seg_ack;
            self.update_cwnd(acked_bytes);
            total_acked_bytes += acked_bytes;
        }
        total_acked_bytes
    }

    fn segment_acceptable(&self, seg_seq: TcpSeq, seg_len: u32) -> bool {
        let wnd = self._rcv.window as i32;
        if seg_len == 0 && self._rcv.window == 0 {
            // SEG.SEQ = RCV.NXT
            seg_seq == self._rcv.next
        } else if seg_len == 0 && self._rcv.window > 0 {
            // RCV.NXT =< SEG.SEQ < RCV.NXT+RCV.WND
            (self._rcv.next <= seg_seq) && (seg_seq < self._rcv.next + wnd)
        } else if seg_len > 0 && self._rcv.window > 0 {
            // RCV.NXT =< SEG.SEQ < RCV.NXT+RCV.WND
            //    or
            // RCV.NXT =< SEG.SEQ+SEG.LEN-1 < RCV.NXT+RCV.WND
            let x = (self._rcv.next <= seg_seq) && seg_seq < (self._rcv.next + wnd);
            let last = seg_seq + seg_len as i32 - 1;
            let y = (self._rcv.next <= last) && (last < self._rcv.next + wnd);
            x || y
        } else {
            // SEG.LEN > 0 RCV.WND = 0, not acceptable
            false
        }
    }

    fn init_from_options(&mut self, th: &TcpHdr, opts: &[u8]) {
        // Handle tcp options
        self._option.parse(opts);

        // Remote receive window scale factor
        self._snd.window_scale = self._option._remote_win_scale;
        // Local receive window scale factor
        self._rcv.window_scale = self._option._local_win_scale;

        // Maximum segment size remote can receive
        self._snd.mss = self._option._remote_mss;
        // Maximum segment size local can receive
        self._option._local_mss = self.local_mss();
        self._rcv.mss = self._option._local_mss;

        self._rcv.window = self.get_default_receive_window_size();
        self._snd.window = (th.window as u32) << self._snd.window_scale;

        // Segment sequence number used for last window update
        self._snd.wl1 = th.seq;
        // Segment acknowledgment number used for last window update
        self._snd.wl2 = th.ack;

        // Setup initial congestion window
        let mss = self._snd.mss as u32;
        if 2190 < mss {
            self._snd.cwnd = 2 * mss;
        } else if 1095 < mss && mss <= 2190 {
            self._snd.cwnd = 3 * mss;
        } else {
            self._snd.cwnd = 4 * mss;
        }

        // Setup initial slow start threshold
        self._snd.ssthresh = (th.window as u32) << self._snd.window_scale;
    }

    /// Handles the SYN that created this tcb on a listening port.
    pub fn input_handle_listen_state(&mut self, th: &mut TcpHdr, mut p: Packet) {
        let (opt_buf, opt_len) = copy_options(th, &mut p);
        let opts = &opt_buf[..opt_len];
        p.trim_front(th.data_offset() as usize * 4);
        let seg_seq = th.seq;

        // Set RCV.NXT to SEG.SEQ+1, IRS is set to SEG.SEQ
        self._rcv.next = seg_seq + 1;
        self._rcv.initial = seg_seq;

        // ISS should be selected and a SYN segment sent of the form:
        // <SEQ=ISS><ACK=RCV.NXT><CTL=SYN,ACK>
        // SND.NXT is set to ISS+1 and SND.UNA to ISS
        // NOTE: In previous code, _snd.next is set to ISS + 1 only when SYN is
        // ACKed. Now, we set _snd.next to ISS + 1 here, so in output_one(): we
        // have
        //     th->seq = syn_on ? _snd.initial : _snd.next
        // to make sure retransmitted SYN has correct SEQ number.
        self.do_setup_isn();

        self._rcv.urgent = self._rcv.next;

        crate::tcp_debug!("listen: LISTEN -> SYN_RECEIVED\n");
        self.init_from_options(th, opts);
        self.do_syn_received();
    }
}

impl<T: TcpInetTraits> Tcb<T> {
    /// Handles a segment received while our SYN is outstanding.
    pub fn input_handle_syn_sent_state(&mut self, th: &mut TcpHdr, mut p: Packet) {
        let (opt_buf, opt_len) = copy_options(th, &mut p);
        let opts = &opt_buf[..opt_len];
        p.trim_front(th.data_offset() as usize * 4);
        let seg_seq = th.seq;
        let seg_ack = th.ack;

        let mut acceptable = false;
        // 3.1 first check the ACK bit
        if th.f_ack() {
            // If SEG.ACK =< ISS, or SEG.ACK > SND.NXT, send a reset (unless the
            // RST bit is set, if so drop the segment and return)
            if seg_ack <= self._snd.initial || seg_ack > self._snd.next {
                return self.respond_with_reset(th);
            }

            // If SND.UNA =< SEG.ACK =< SND.NXT then the ACK is acceptable.
            acceptable = self._snd.unacknowledged <= seg_ack && seg_ack <= self._snd.next;
        }

        // 3.2 second check the RST bit
        if th.f_rst() {
            // If the ACK was acceptable then signal the user "error: connection
            // reset", drop the segment, enter CLOSED state, delete TCB, and
            // return.  Otherwise (no ACK) drop the segment and return.
            if acceptable {
                self._connect_done.set_exception(ExceptionPtr::from(tcp_refused_error()));
                return self.do_reset();
            } else {
                return;
            }
        }

        // 3.3 third check the security and precedence
        // NOTE: Ignored for now

        // 3.4 fourth check the SYN bit
        if th.f_syn() {
            // RCV.NXT is set to SEG.SEQ+1, IRS is set to SEG.SEQ.  SND.UNA should
            // be advanced to equal SEG.ACK (if there is an ACK), and any segments
            // on the retransmission queue which are thereby acknowledged should be
            // removed.
            self._rcv.next = seg_seq + 1;
            self._rcv.initial = seg_seq;
            if th.f_ack() {
                // TODO: clean retransmission queue
                self._snd.unacknowledged = seg_ack;
            }
            if self._snd.unacknowledged > self._snd.initial {
                // If SND.UNA > ISS (our SYN has been ACKed), change the connection
                // state to ESTABLISHED, form an ACK segment
                // <SEQ=SND.NXT><ACK=RCV.NXT><CTL=ACK>
                crate::tcp_debug!("syn: SYN_SENT -> ESTABLISHED\n");
                self.init_from_options(th, opts);
                self.do_established();
                self.output();
            } else {
                // Otherwise enter SYN_RECEIVED, form a SYN,ACK segment
                // <SEQ=ISS><ACK=RCV.NXT><CTL=SYN,ACK>
                crate::tcp_debug!("syn: SYN_SENT -> SYN_RECEIVED\n");
                self.do_syn_received();
            }
        }

        // 3.5 fifth, if neither of the SYN or RST bits is set then drop the
        // segment and return.
    }

    fn update_window(&mut self, th: &TcpHdr, seg_seq: TcpSeq, seg_ack: TcpSeq) {
        crate::tcp_debug!(
            "window update seg_seq={}, seg_ack={}, old window={} new window={}\n",
            seg_seq,
            seg_ack,
            self._snd.window,
            (th.window as u32) << self._snd.window_scale
        );
        self._snd.window = (th.window as u32) << self._snd.window_scale;
        self._snd.wl1 = seg_seq;
        self._snd.wl2 = seg_ack;
        self._snd.zero_window_probing_out = 0;
        if self._snd.window == 0 {
            self._persist_time_out = self._rto;
            self.start_persist_timer();
        } else {
            self.stop_persist_timer();
        }
    }

    fn set_retransmit_timer(&mut self) {
        if self._snd.data.is_empty() {
            // All outstanding segments are acked, turn off the timer.
            self.stop_retransmit_timer();
            // Signal the waiter of this event
            self.signal_all_data_acked();
        } else {
            // Restart the timer becasue new data is acked.
            self.start_retransmit_timer();
        }
    }

    /// Handles a segment in any state other than LISTEN and SYN_SENT.
    pub fn input_handle_other_state(&mut self, th: &mut TcpHdr, mut p: Packet) {
        p.trim_front(th.data_offset() as usize * 4);
        let mut do_output = false;
        let mut do_output_data = false;
        let mut seg_seq = th.seq;
        let seg_ack = th.ack;
        let mut seg_len = p.len();

        // 4.1 first check sequence number
        if !self.segment_acceptable(seg_seq, seg_len) {
            //<SEQ=SND.NXT><ACK=RCV.NXT><CTL=ACK>
            return self.output();
        }

        // In the following it is assumed that the segment is the idealized
        // segment that begins at RCV.NXT and does not exceed the window.
        if seg_seq < self._rcv.next {
            // ignore already acknowledged data
            let dup = std::cmp::min((self._rcv.next - seg_seq) as u32, seg_len);
            p.trim_front(dup as usize);
            seg_len -= dup;
            seg_seq += dup as i32;
        }
        // FIXME: We should trim data outside the right edge of the receive window as well

        if seg_seq != self._rcv.next {
            self.insert_out_of_order(seg_seq, p);
            // A TCP receiver SHOULD send an immediate duplicate ACK
            // when an out-of-order segment arrives.
            return self.output();
        }

        // 4.2 second check the RST bit
        if th.f_rst() {
            if self.in_state(Self::SYN_RECEIVED.into()) {
                // If this connection was initiated with a passive OPEN (i.e.,
                // came from the LISTEN state), then return this connection to
                // LISTEN state and return.  The user need not be informed.  If
                // this connection was initiated with an active OPEN (i.e., came
                // from SYN_SENT state) then the connection was refused, signal
                // the user "connection refused".  In either case, all segments
                // on the retransmission queue should be removed.  And in the
                // active OPEN case, enter the CLOSED state and delete the TCB,
                // and return.
                self._connect_done.set_exception(ExceptionPtr::from(tcp_refused_error()));
                return self.do_reset();
            }
            if self.in_state(
                Self::ESTABLISHED | Self::FIN_WAIT_1 | Self::FIN_WAIT_2 | Self::CLOSE_WAIT,
            ) {
                // If the RST bit is set then, any outstanding RECEIVEs and SEND
                // should receive "reset" responses.  All segment queues should be
                // flushed.  Users should also receive an unsolicited general
                // "connection reset" signal.  Enter the CLOSED state, delete the
                // TCB, and return.
                return self.do_reset();
            }
            if self.in_state(Self::CLOSING | Self::LAST_ACK | Self::TIME_WAIT) {
                // If the RST bit is set then, enter the CLOSED state, delete the
                // TCB, and return.
                return self.do_closed();
            }
        }

        // 4.3 third check security and precedence
        // NOTE: Ignored for now

        // 4.4 fourth, check the SYN bit
        if th.f_syn() {
            // SYN_RECEIVED, ESTABLISHED, FIN_WAIT_1, FIN_WAIT_2
            // CLOSE_WAIT, CLOSING, LAST_ACK, TIME_WAIT

            // If the SYN is in the window it is an error, send a reset, any
            // outstanding RECEIVEs and SEND should receive "reset" responses,
            // all segment queues should be flushed, the user should also
            // receive an unsolicited general "connection reset" signal, enter
            // the CLOSED state, delete the TCB, and return.
            self.respond_with_reset(th);
            return self.do_reset();

            // If the SYN is not in the window this step would not be reached
            // and an ack would have been sent in the first step (sequence
            // number check).
        }

        // 4.5 fifth check the ACK field
        if !th.f_ack() {
            // if the ACK bit is off drop the segment and return
            return;
        } else {
            // SYN_RECEIVED STATE
            if self.in_state(Self::SYN_RECEIVED.into()) {
                // If SND.UNA =< SEG.ACK =< SND.NXT then enter ESTABLISHED state
                // and continue processing.
                if self._snd.unacknowledged <= seg_ack && seg_ack <= self._snd.next {
                    crate::tcp_debug!("SYN_RECEIVED -> ESTABLISHED\n");
                    self.do_established();
                    // SAFETY: `_this` comes from the tcb's reference-counted node
                    // (Tcb::create); only the counter before the tcb is touched.
                    let me = unsafe { Self::shared_from_this(self._this) };
                    let port = self._local_port;
                    // SAFETY: see `tcp`; queuing the connection does not enter this tcb.
                    unsafe { (*self.tcp()).add_connected_tcb(me, port) };
                } else {
                    // <SEQ=SEG.ACK><CTL=RST>
                    return self.respond_with_reset(th);
                }
            }
            // ESTABLISHED STATE or
            // CLOSE_WAIT STATE: Do the same processing as for the ESTABLISHED state.
            if self.in_state(Self::ESTABLISHED | Self::CLOSE_WAIT) {
                // When we are in zero window probing phase and packets_out = 0 we bypass "duplicated ack" check
                let packets_out: i32 = (self._snd.next - self._snd.unacknowledged)
                    - self._snd.zero_window_probing_out as i32;
                // If SND.UNA < SEG.ACK =< SND.NXT then, set SND.UNA <- SEG.ACK.
                if self._snd.unacknowledged < seg_ack && seg_ack <= self._snd.next {
                    // Remote ACKed data we sent
                    let acked_bytes = self.data_segment_acked(seg_ack);

                    // If SND.UNA < SEG.ACK =< SND.NXT, the send window should be updated.
                    if self._snd.wl1 < seg_seq
                        || (self._snd.wl1 == seg_seq && self._snd.wl2 <= seg_ack)
                    {
                        self.update_window(th, seg_seq, seg_ack);
                    }

                    // some data is acked, try send more data
                    do_output_data = true;

                    if self._snd.dupacks >= 3 {
                        // We are in fast retransmit / fast recovery phase
                        let smss: u32 = self._snd.mss as u32;
                        if seg_ack > self._snd.recover {
                            crate::tcp_debug!("ack: full_ack\n");
                            // Set cwnd to min (ssthresh, max(FlightSize, SMSS) + SMSS)
                            self._snd.cwnd = std::cmp::min(
                                self._snd.ssthresh,
                                std::cmp::max(self.flight_size(), smss) + smss,
                            );
                            // Exit the fast recovery procedure
                            self.exit_fast_recovery();
                            self.set_retransmit_timer();
                        } else {
                            crate::tcp_debug!("ack: partial_ack\n");
                            // Retransmit the first unacknowledged segment
                            self.fast_retransmit();
                            // Deflate the congestion window by the amount of new data
                            // acknowledged by the Cumulative Acknowledgment field
                            self._snd.cwnd = self._snd.cwnd.wrapping_sub(acked_bytes);
                            // If the partial ACK acknowledges at least one SMSS of new
                            // data, then add back SMSS bytes to the congestion window
                            if acked_bytes >= smss {
                                self._snd.cwnd = self._snd.cwnd.wrapping_add(smss);
                            }
                            // Send a new segment if permitted by the new value of
                            // cwnd.  Do not exit the fast recovery procedure For
                            // the first partial ACK that arrives during fast
                            // recovery, also reset the retransmit timer.
                            self._snd.partial_ack += 1;
                            if self._snd.partial_ack == 1 {
                                self.start_retransmit_timer();
                            }
                        }
                    } else {
                        // RFC5681: The fast retransmit algorithm uses the arrival
                        // of 3 duplicate ACKs (as defined in section 2, without
                        // any intervening ACKs which move SND.UNA) as an
                        // indication that a segment has been lost.
                        //
                        // So, here we reset dupacks to zero becasue this ACK moves
                        // SND.UNA.
                        self.exit_fast_recovery();
                        self.set_retransmit_timer();
                    }
                } else if (packets_out > 0)
                    && !self._snd.data.is_empty()
                    && seg_len == 0
                    && !th.f_fin()
                    && !th.f_syn()
                    && th.ack == self._snd.unacknowledged
                    && ((th.window as u32) << self._snd.window_scale) == self._snd.window
                {
                    // Note:
                    // RFC793 states:
                    // If the ACK is a duplicate (SEG.ACK < SND.UNA), it can be ignored
                    // RFC5681 states:
                    // The TCP sender SHOULD use the "fast retransmit" algorithm to detect
                    // and repair loss, based on incoming duplicate ACKs.
                    // Here, We follow RFC5681.
                    self._snd.dupacks += 1;
                    let smss: u32 = self._snd.mss as u32;
                    // 3 duplicated ACKs trigger a fast retransmit
                    if self._snd.dupacks == 1 || self._snd.dupacks == 2 {
                        // RFC5681 Step 3.1
                        // Send cwnd + 2 * smss per RFC3042
                        do_output_data = true;
                    } else if self._snd.dupacks == 3 {
                        // RFC6582 Step 3.2
                        if seg_ack - 1 > self._snd.recover {
                            self._snd.recover = self._snd.next - 1;
                            // RFC5681 Step 3.2
                            self._snd.ssthresh = std::cmp::max(
                                self.flight_size().wrapping_sub(self._snd.limited_transfer) / 2,
                                2 * smss,
                            );
                            self.fast_retransmit();
                        } else {
                            // Do not enter fast retransmit and do not reset ssthresh
                        }
                        // RFC5681 Step 3.3
                        self._snd.cwnd = self._snd.ssthresh + 3 * smss;
                    } else if self._snd.dupacks > 3 {
                        // RFC5681 Step 3.4
                        self._snd.cwnd += smss;
                        // RFC5681 Step 3.5
                        do_output_data = true;
                    }
                } else if seg_ack > self._snd.next {
                    // If the ACK acks something not yet sent (SEG.ACK > SND.NXT)
                    // then send an ACK, drop the segment, and return
                    return self.output();
                } else if self._snd.window == 0 && th.window > 0 {
                    self.update_window(th, seg_seq, seg_ack);
                    do_output_data = true;
                }
            }
            // FIN_WAIT_1 STATE
            if self.in_state(Self::FIN_WAIT_1.into()) {
                // In addition to the processing for the ESTABLISHED state, if
                // our FIN is now acknowledged then enter FIN-WAIT-2 and continue
                // processing in that state.
                if seg_ack == self._snd.next + 1 {
                    crate::tcp_debug!("ack: FIN_WAIT_1 -> FIN_WAIT_2\n");
                    self._state = Self::FIN_WAIT_2;
                    self.do_local_fin_acked();
                }
            }
            // FIN_WAIT_2 STATE
            if self.in_state(Self::FIN_WAIT_2.into()) {
                // In addition to the processing for the ESTABLISHED state, if
                // the retransmission queue is empty, the user’s CLOSE can be
                // acknowledged ("ok") but do not delete the TCB.
                // TODO
            }
            // CLOSING STATE
            if self.in_state(Self::CLOSING.into()) {
                if seg_ack == self._snd.next + 1 {
                    crate::tcp_debug!("ack: CLOSING -> TIME_WAIT\n");
                    self.do_local_fin_acked();
                    return self.do_time_wait();
                } else {
                    return;
                }
            }
            // LAST_ACK STATE
            if self.in_state(Self::LAST_ACK.into()) && seg_ack == self._snd.next + 1 {
                crate::tcp_debug!("ack: LAST_ACK -> CLOSED\n");
                self.do_local_fin_acked();
                return self.do_closed();
            }
            // TIME_WAIT STATE
            if self.in_state(Self::TIME_WAIT.into()) {
                // The only thing that can arrive in this state is a
                // retransmission of the remote FIN. Acknowledge it, and restart
                // the 2 MSL timeout.
                // TODO
            }
        }

        // 4.6 sixth, check the URG bit
        if th.f_urg() {
            // TODO
        }

        // 4.7 seventh, process the segment text
        if self.in_state(Self::ESTABLISHED | Self::FIN_WAIT_1 | Self::FIN_WAIT_2) {
            if p.len() != 0 {
                // Once the TCP takes responsibility for the data it advances
                // RCV.NXT over the data accepted, and adjusts RCV.WND as
                // apporopriate to the current buffer availability.  The total of
                // RCV.NXT and RCV.WND should not be reduced.
                self._rcv.data_size += p.len() as usize;
                self._rcv.data.push_back(p);
                self._rcv.next += seg_len as i32;
                let merged = self.merge_out_of_order();
                self._rcv.window = self.get_modified_receive_window_size();
                self.signal_data_received();
                // Send an acknowledgment of the form:
                // <SEQ=SND.NXT><ACK=RCV.NXT><CTL=ACK>
                // This acknowledgment should be piggybacked on a segment being
                // transmitted if possible without incurring undue delay.
                if merged {
                    // TCP receiver SHOULD send an immediate ACK when the
                    // incoming segment fills in all or part of a gap in the
                    // sequence space.
                    do_output = true;
                } else {
                    do_output = self.should_send_ack(seg_len as u16);
                }
            }
        } else if self.in_state(Self::CLOSE_WAIT | Self::CLOSING | Self::LAST_ACK | Self::TIME_WAIT)
        {
            // This should not occur, since a FIN has been received from the
            // remote side. Ignore the segment text.
            return;
        }

        // 4.8 eighth, check the FIN bit
        if th.f_fin() {
            if let Some(mut pr) = self._fin_recvd_promise.take() {
                pr.set_value(());
            }
            if self.in_state(Self::CLOSED | Self::LISTEN | Self::SYN_SENT) {
                // Do not process the FIN if the state is CLOSED, LISTEN or SYN-SENT
                // since the SEG.SEQ cannot be validated; drop the segment and return.
                return;
            }
            let fin_seq = seg_seq + seg_len as i32;
            if fin_seq == self._rcv.next {
                self._rcv.next = fin_seq + 1;
                self.signal_data_received();

                // If this <FIN> packet contains data as well, we can ACK both data
                // and <FIN> in a single packet, so canncel the previous ACK.
                self.clear_delayed_ack();
                do_output = false;
                // Send ACK for the FIN!
                self.output();

                if self.in_state(Self::SYN_RECEIVED | Self::ESTABLISHED) {
                    crate::tcp_debug!("fin: SYN_RECEIVED or ESTABLISHED -> CLOSE_WAIT\n");
                    self._state = Self::CLOSE_WAIT;
                }
                if self.in_state(Self::FIN_WAIT_1.into()) {
                    // If our FIN has been ACKed (perhaps in this segment), then
                    // enter TIME-WAIT, start the time-wait timer, turn off the other
                    // timers; otherwise enter the CLOSING state.
                    // Note: If our FIN has been ACKed, we should be in FIN_WAIT_2
                    // not FIN_WAIT_1 if we reach here.
                    crate::tcp_debug!("fin: FIN_WAIT_1 -> CLOSING\n");
                    self._state = Self::CLOSING;
                }
                if self.in_state(Self::FIN_WAIT_2.into()) {
                    crate::tcp_debug!("fin: FIN_WAIT_2 -> TIME_WAIT\n");
                    return self.do_time_wait();
                }
            }
        }
        if do_output || (do_output_data && self.can_send() != 0) {
            // Since we will do output, we can canncel scheduled delayed ACK.
            self.clear_delayed_ack();
            self.output();
        }
    }

    fn get_transmit_packet(&mut self) -> Packet {
        // easy case: empty queue
        if self._snd.unsent.is_empty() {
            return Packet::new();
        }
        let mut can_send = self.can_send();
        // Max number of TCP payloads we can pass to NIC
        // SAFETY: see `tcp`.
        let hw = unsafe { *(*self.tcp()).hw_features() };
        let len: u32 = if hw.tx_tso {
            // FIXME: Info tap device the size of the splitted packet
            hw.max_packet_len as u32 - TCP_HDR_LEN_MIN as u32 - T::IP_HDR_LEN_MIN as u32
        } else {
            std::cmp::min(hw.mtu - TCP_HDR_LEN_MIN as u16 - T::IP_HDR_LEN_MIN as u16, self._snd.mss)
                as u32
        };
        can_send = std::cmp::min(can_send, len);
        // easy case: one small packet
        if self._snd.unsent.len() == 1
            // SAFETY: `unsent.len() == 1` is checked first (short-circuit), so `front()` is `Some`.
            && unsafe { self._snd.unsent.front().unwrap_unchecked() }.len() <= can_send
        {
            // SAFETY: as above.
            let p = unsafe { self._snd.unsent.pop_front().unwrap_unchecked() };
            self._snd.unsent_len -= p.len();
            return p;
        }
        // moderate case: need to split one packet
        // SAFETY: as above.
        let front = unsafe { self._snd.unsent.front_mut().unwrap_unchecked() };
        if front.len() > can_send {
            let p = front.share_range(0, can_send as usize);
            front.trim_front(can_send as usize);
            self._snd.unsent_len -= p.len();
            return p;
        }
        // hard case: merge some packets, possibly split last
        // SAFETY: as above.
        let mut p = unsafe { self._snd.unsent.pop_front().unwrap_unchecked() };
        can_send -= p.len();
        while let Some(f) = self._snd.unsent.front() {
            if f.len() > can_send {
                break;
            }
            can_send -= f.len();
            // SAFETY: front() just returned Some.
            p.append(unsafe { self._snd.unsent.pop_front().unwrap_unchecked() });
        }
        if can_send != 0
            && let Some(q) = self._snd.unsent.front_mut()
        {
            p.append(q.share_range(0, can_send as usize));
            q.trim_front(can_send as usize);
        }
        self._snd.unsent_len -= p.len();
        p
    }
}

// gnutls_hash_* are declared, with `#[link(name = "gnutls")]`, in `net/tls.rs`.
#[cfg(feature = "tls")]
use crate::engine::net::tls::{
    GNUTLS_DIG_MD5, GnutlsHashHdT, gnutls_hash, gnutls_hash_deinit, gnutls_hash_get_len,
    gnutls_hash_init,
};

impl<T: TcpInetTraits> Tcb<T> {
    /// Builds and queues one segment: new data, or with `data_retransmit` the oldest
    /// unacknowledged segment.
    pub fn output_one(&mut self, data_retransmit: bool) {
        if self.in_state(Self::CLOSED.into()) {
            return;
        }

        let mut p = if data_retransmit {
            // SAFETY: retransmissions are only requested with outstanding data.
            unsafe { self._snd.data.front_mut().unwrap_unchecked() }.p.share()
        } else {
            self.get_transmit_packet()
        };
        let clone = p.share(); // early clone to prevent share() from calling packet::unuse_internal_data() on header.
        let len: u16 = p.len() as u16;
        let syn_on = self.syn_needs_on();
        let ack_on = self.ack_needs_on();

        let options_size = self._option.get_size(syn_on, ack_on);
        let th = p.prepend_uninitialized_header(TcpHdr::LEN + options_size as usize);
        let mut h = TcpHdr {
            src_port: self._local_port,
            dst_port: self._foreign_port,
            ..Default::default()
        };

        h.set_f_syn(syn_on);
        h.set_f_ack(ack_on);
        if ack_on {
            self.clear_delayed_ack();
        }
        h.set_f_urg(false);
        h.set_f_psh(false);

        let seq: TcpSeq;
        if data_retransmit {
            seq = self._snd.unacknowledged;
        } else {
            seq = if syn_on { self._snd.initial } else { self._snd.next };
            self._snd.next += len as i32;
        }
        h.seq = seq;
        h.ack = self._rcv.next;
        h.set_data_offset(((TcpHdr::LEN + options_size as usize) / 4) as u8);
        h.window = (self._rcv.window >> self._rcv.window_scale) as u16;
        h.checksum = 0;

        // FIXME: does the FIN have to fit in the window?
        let fin_on = self.fin_needs_on();
        h.set_f_fin(fin_on);

        // Add tcp options
        // SAFETY: `prepend_uninitialized_header` returned `TcpHdr::LEN + options_size`
        // writable bytes at `th`, owned by `p` and not aliased; the options area is the last
        // `options_size` of them. It is zeroed before the slice over it is made, so the slice
        // covers initialized bytes, and the slice ends before `p` is touched again.
        let opts = unsafe {
            let area = th.add(TcpHdr::LEN);
            std::ptr::write_bytes(area, 0, options_size as usize);
            std::slice::from_raw_parts_mut(area, options_size as usize)
        };
        self._option.fill(opts, &h);
        // SAFETY: the header and options were just prepended.
        unsafe { h.write(th) };

        let mut oi = OffloadInfo::default();
        let mut csum = Checksummer::default();
        let mut pseudo_hdr_seg_len: u16 = 0;

        oi.tcp_hdr_len = TcpHdr::LEN as u8 + options_size;

        // SAFETY: see `tcp`.
        let hw = unsafe { *(*self.tcp()).hw_features() };
        if hw.tx_csum_l4_offload {
            oi.needs_csum = true;

            //
            // tx checksum offloading: both virtio-net's VIRTIO_NET_F_CSUM dpdk's
            // PKT_TX_TCP_CKSUM - requires th->checksum to be initialized to ones'
            // complement sum of the pseudo header.
            //
            // For TSO the csum should be calculated for a pseudo header with
            // segment length set to 0. All the rest is the same as for a TCP Tx
            // CSUM offload case.
            //
            if hw.tx_tso && len > self._snd.mss {
                oi.tso_seg_size = self._snd.mss;
            } else {
                pseudo_hdr_seg_len = TcpHdr::LEN as u16 + options_size as u16 + len;
            }
        } else {
            pseudo_hdr_seg_len = TcpHdr::LEN as u16 + options_size as u16 + len;
            oi.needs_csum = false;
        }

        T::tcp_pseudo_header_checksum(
            &mut csum,
            self._local_ip,
            self._foreign_ip,
            pseudo_hdr_seg_len,
        );

        let checksum: u16 = if hw.tx_csum_l4_offload {
            !csum.get()
        } else {
            csum.sum_packet(&p);
            csum.get()
        };
        // SAFETY: as above.
        unsafe { TcpHdr::write_nbo_checksum(th, checksum) };

        oi.protocol = IpProtocolNum::tcp;

        p.set_offload_info(oi);

        if !data_retransmit && (len != 0 || syn_on || fin_on) {
            let now = LowresClock::now();
            if len != 0 {
                let nr_transmits: u32 = 0;
                self._snd.data.push_back(UnackedSegment {
                    p: clone,
                    data_len: len,
                    nr_transmits,
                    tx_time: now,
                });
            }
            if !self._retransmit.armed() {
                self.start_retransmit_timer_at(now);
            }
        }

        // if advertised TCP receive window is 0 we may only transmit zero window probing segment.
        // Payload size of this segment is 1. Queueing anything bigger when _snd.window == 0 is bug
        // and violation of RFC
        assert!((self._snd.window > 0) || ((self._snd.window == 0) && (len <= 1)));
        self.queue_packet(p);
    }

    /// Resolves when received data is available (or the peer will send no more).
    pub fn wait_for_data(&mut self) -> Future<()> {
        if !self._rcv.data.is_empty() || self.foreign_will_not_send() {
            return make_ready_future(());
        }
        let pr = self._rcv._data_received_promise.insert(Promise::new());
        pr.get_future()
    }

    /// Resolves when the peer's FIN arrives (or the reader is aborted).
    pub fn wait_input_shutdown(&mut self) -> Future<()> {
        match self._fin_recvd_promise.as_mut() {
            None => make_ready_future(()),
            Some(p) => p.get_future(),
        }
    }

    /// Fails a pending read and releases a pending input-shutdown wait.
    pub fn abort_reader(&mut self) {
        if let Some(mut p) = self._rcv._data_received_promise.take() {
            p.set_exception(ExceptionPtr::from(std::io::Error::from_raw_os_error(ECONNABORTED)));
        }
        if let Some(mut p) = self._fin_recvd_promise.take() {
            p.set_value(());
        }
    }

    /// Resolves when every queued byte was acknowledged.
    pub fn wait_for_all_data_acked(&mut self) -> Future<()> {
        if self._snd.data.is_empty() && self._snd.unsent_len == 0 {
            return make_ready_future(());
        }
        let pr = self._snd._all_data_acked_promise.insert(Promise::new());
        pr.get_future()
    }

    /// Starts an active open.
    pub fn connect(&mut self) {
        // An initial send sequence number (ISS) is selected.  A SYN segment of the
        // form <SEQ=ISS><CTL=SYN> is sent.  Set SND.UNA to ISS, SND.NXT to ISS+1,
        // enter SYN-SENT state, and return.
        self.do_setup_isn();

        // Local receive window scale factor
        self._option._local_win_scale = 7;
        self._rcv.window_scale = 7;
        // Maximum segment size local can receive
        self._option._local_mss = self.local_mss();
        self._rcv.mss = self._option._local_mss;
        self._rcv.window = self.get_default_receive_window_size();

        self.do_syn_sent();
    }

    /// Takes all the received data.
    pub fn read(&mut self) -> Packet {
        let mut p = Packet::new();
        for q in self._rcv.data.drain(..) {
            p.append(q);
        }
        self._rcv.data_size = 0;
        self._rcv.window = self.get_default_receive_window_size();
        p
    }

    /// Resolves when the send queue has room.
    pub fn wait_send_available(&mut self) -> Future<()> {
        if self._snd.max_queue_space > self._snd.current_queue_space {
            return make_ready_future(());
        }
        let pr = self._snd._send_available_promise.insert(Promise::new());
        pr.get_future()
    }

    /// Queues `p` for sending.
    pub fn send(&mut self, p: Packet) -> Future<()> {
        // We can not send after the connection is closed
        if self._snd.closed || self.in_state(Self::CLOSED.into()) {
            return make_exception_future(ExceptionPtr::from(tcp_reset_error()));
        }

        let len = p.len();
        self._snd.current_queue_space += len as usize;
        self._snd.unsent_len += len;
        self._snd.unsent.push_back(p);

        if self.can_send() > 0 {
            self.output();
        }

        self.wait_send_available()
    }

    /// Sends a FIN once every queued byte is acknowledged.
    pub fn close(&mut self) {
        if self.in_state(Self::CLOSED.into()) || self._snd.closed {
            return;
        }
        // TODO: We should return a future to upper layer
        let f = self.wait_for_all_data_acked();
        if f.available() {
            // `then` on a ready future runs in line, inside this call: done through `self`,
            // since a `&mut` from the shared pointer would alias it.
            if f.get().is_ok() {
                self.finish_close();
            }
            return;
        }
        // SAFETY: `_this` comes from the tcb's reference-counted node (Tcb::create); only the
        // counter before the tcb is touched.
        let zis = unsafe { Self::shared_from_this(self._this) };
        let _ = f.then(move |()| {
            // SAFETY: the continuation runs later from the reactor, with no other reference to
            // the tcb live; `zis` keeps it alive and is its root pointer.
            unsafe { zis.get_mut_unchecked() }.finish_close();
            make_ready_future(())
        });
    }

    /// The continuation of `close`.
    fn finish_close(&mut self) {
        self._snd.closed = true;
        crate::tcp_debug!("close: unsent_len={}\n", self._snd.unsent_len);
        if self.in_state(Self::CLOSE_WAIT.into()) {
            crate::tcp_debug!("close: CLOSE_WAIT -> LAST_ACK\n");
            self._state = Self::LAST_ACK;
        } else if self.in_state(Self::ESTABLISHED.into()) {
            crate::tcp_debug!("close: ESTABLISHED -> FIN_WAIT_1\n");
            self._state = Self::FIN_WAIT_1;
        }
        // Send <FIN> to remote
        // Note: we call output_one to make sure a packet with FIN actually
        // sent out. If we only call output() and _packetq is not empty,
        // `get_packet` would not generate the packet with FIN.
        self.output_one(false);
        self.output();
    }

    fn should_send_ack(&mut self, seg_len: u16) -> bool {
        // We've received a TSO packet, do ack immediately
        if seg_len > self._rcv.mss {
            self._nr_full_seg_received = 0;
            // SAFETY: a timer of this tcb (see `pinned`).
            unsafe { Self::pinned(&mut self._delayed_ack) }.cancel();
            return true;
        }

        // We've received a full sized segment, ack for every second full sized segment
        if seg_len == self._rcv.mss {
            let n = self._nr_full_seg_received;
            self._nr_full_seg_received += 1;
            if n >= 1 {
                self._nr_full_seg_received = 0;
                // SAFETY: a timer of this tcb (see `pinned`).
                unsafe { Self::pinned(&mut self._delayed_ack) }.cancel();
                return true;
            }
        }

        // If the timer is armed and its callback hasn't been run.
        if self._delayed_ack.armed() {
            return false;
        }

        // If the timer is not armed, schedule a delayed ACK.
        // The maximum delayed ack timer allowed by RFC1122 is 500ms, most
        // implementations use 200ms.
        // SAFETY: a timer of this tcb (see `pinned`).
        unsafe { Self::pinned(&mut self._delayed_ack) }.arm_in(Duration::from_millis(200));
        false
    }

    #[inline]
    fn clear_delayed_ack(&mut self) {
        // SAFETY: a timer of this tcb (see `pinned`).
        unsafe { Self::pinned(&mut self._delayed_ack) }.cancel();
    }

    fn merge_out_of_order(&mut self) -> bool {
        let mut merged = false;
        if self._rcv.out_of_order.map.is_empty() {
            return merged;
        }
        while let Some((&seg_beg, _)) = self._rcv.out_of_order.map.iter().next() {
            // SAFETY: the key was just read from the map.
            let seg_len0 =
                unsafe { self._rcv.out_of_order.map.get(&seg_beg).unwrap_unchecked() }.len();
            let mut seg_len = seg_len0;
            let seg_end = seg_beg + seg_len as i32;
            if seg_beg <= self._rcv.next && self._rcv.next < seg_end {
                // This segment has been received out of order and its previous
                // segment has been received now
                // SAFETY: as above.
                let mut p =
                    unsafe { self._rcv.out_of_order.map.remove(&seg_beg).unwrap_unchecked() };
                let trim = (self._rcv.next - seg_beg) as u32;
                if trim != 0 {
                    p.trim_front(trim as usize);
                    seg_len -= trim;
                }
                self._rcv.next += seg_len as i32;
                self._rcv.data_size += p.len() as usize;
                self._rcv.data.push_back(p);
                // `erase` returns the following element
                merged = true;
            } else if self._rcv.next >= seg_end {
                // This segment has been receive already, drop it
                self._rcv.out_of_order.map.remove(&seg_beg);
            } else {
                // seg_beg > _rcv.need, can not merge. Note, seg_beg can grow only,
                // so we can stop looking here.
                break;
            }
        }
        merged
    }

    #[inline]
    fn insert_out_of_order(&mut self, seg: TcpSeq, p: Packet) {
        self._rcv.out_of_order.merge(seg, p);
    }

    /// Trimming received data beyond the window is not implemented: aborts.
    #[allow(dead_code)]
    fn trim_receive_data_after_window(&mut self) {
        std::process::abort();
    }

    fn persist(&mut self) {
        crate::tcp_debug!("persist timer fired\n");
        // Send 1 byte packet to probe peer's window size
        self._snd.window_probe = true;
        self._snd.zero_window_probing_out = self._snd.zero_window_probing_out.wrapping_add(1);
        self.output_one(false);
        self._snd.window_probe = false;

        self.output();
        // Perform binary exponential back-off per RFC1122
        self._persist_time_out = std::cmp::min(self._persist_time_out * 2, Self::RTO_MAX);
        self.start_persist_timer();
    }

    fn output_update_rto(&mut self) {
        self.output();
        // According to RFC6298, Update RTO <- RTO * 2 to perform binary exponential back-off
        self._rto = std::cmp::min(self._rto * 2, Self::RTO_MAX);
        self.start_retransmit_timer();
    }

    fn retransmit(&mut self) {
        // Retransmit SYN
        if self.syn_needs_on() {
            let n = self._snd.syn_retransmit;
            self._snd.syn_retransmit += 1;
            if n < Self::MAX_NR_RETRANSMIT as u32 {
                self.output_update_rto();
            } else {
                self._connect_done.set_exception(ExceptionPtr::from(tcp_connect_error()));
                self.cleanup();
                return;
            }
        }

        // Retransmit FIN
        if self.fin_needs_on() {
            let n = self._snd.fin_retransmit;
            self._snd.fin_retransmit += 1;
            if n < Self::MAX_NR_RETRANSMIT as u32 {
                self.output_update_rto();
            } else {
                self.cleanup();
                return;
            }
        }

        // Retransmit Data
        if self._snd.data.is_empty() {
            return;
        }

        // If there are unacked data, retransmit the earliest segment
        // SAFETY: checked non-empty above.
        let nr_transmits = unsafe { self._snd.data.front().unwrap_unchecked() }.nr_transmits;

        // According to RFC5681
        // Update ssthresh only for the first retransmit
        let smss: u32 = self._snd.mss as u32;
        if nr_transmits == 0 {
            self._snd.ssthresh = std::cmp::max(self.flight_size() / 2, 2 * smss);
        }
        // RFC6582 Step 4
        self._snd.recover = self._snd.next - 1;
        // Start the slow start process
        self._snd.cwnd = smss;
        // End fast recovery
        self.exit_fast_recovery();

        if nr_transmits < Self::MAX_NR_RETRANSMIT as u32 {
            // SAFETY: as above.
            unsafe { self._snd.data.front_mut().unwrap_unchecked() }.nr_transmits += 1;
        } else {
            // Delete connection when max num of retransmission is reached
            self.do_reset();
            return;
        }
        self.retransmit_one();

        self.output_update_rto();
    }

    fn fast_retransmit(&mut self) {
        if let Some(unacked_seg) = self._snd.data.front_mut() {
            unacked_seg.nr_transmits += 1;
            self.retransmit_one();
            self.output();
        }
    }

    fn update_rto(&mut self, tx_time: LowresTimePoint) {
        // Update RTO according to RFC6298
        // duration_cast<milliseconds> truncates; the lowres clock is steady, so the difference is not negative.
        let r = Duration::from_millis(((LowresClock::now() - tx_time) / 1_000_000) as u64);
        if self._snd.first_rto_sample {
            self._snd.first_rto_sample = false;
            // RTTVAR <- R/2
            // SRTT <- R
            self._snd.rttvar = r / 2;
            self._snd.srtt = r;
        } else {
            // RTTVAR <- (1 - beta) * RTTVAR + beta * |SRTT - R'|
            // SRTT <- (1 - alpha) * SRTT + alpha * R'
            // where alpha = 1/8 and beta = 1/4
            let delta = self._snd.srtt.abs_diff(r);
            self._snd.rttvar = self._snd.rttvar * 3 / 4 + delta / 4;
            self._snd.srtt = self._snd.srtt * 7 / 8 + r / 8;
        }
        // RTO <- SRTT + max(G, K * RTTVAR)
        self._rto = self._snd.srtt + std::cmp::max(Self::RTO_CLK_GRANULARITY, 4 * self._snd.rttvar);

        // Make sure 1 sec << _rto << 60 sec
        self._rto = std::cmp::max(self._rto, Self::RTO_MIN);
        self._rto = std::cmp::min(self._rto, Self::RTO_MAX);
    }

    fn update_cwnd(&mut self, acked_bytes: u32) {
        let smss: u32 = self._snd.mss as u32;
        if self._snd.cwnd < self._snd.ssthresh {
            // In slow start phase
            self._snd.cwnd += std::cmp::min(acked_bytes, smss);
        } else {
            // In congestion avoidance phase
            let round_up: u32 = 1;
            self._snd.cwnd += std::cmp::max(round_up, smss * smss / self._snd.cwnd);
        }
    }

    fn cleanup(&mut self) {
        self._snd.unsent.clear();
        self._snd.data.clear();
        self._rcv.out_of_order.map.clear();
        self._rcv.data_size = 0;
        self._rcv.data.clear();
        self.stop_retransmit_timer();
        self.clear_delayed_ack();
        self.remove_from_tcbs();
    }

    fn get_isn(&self) -> TcpSeq {
        // Per RFC6528, TCP SHOULD generate its Initial Sequence Numbers
        // with the expression:
        //   ISN = M + F(localip, localport, remoteip, remoteport, secretkey)
        //   M is the 4 microsecond timer
        let mut hash = [0u32; 4];
        hash[0] = self._local_ip.ip;
        hash[1] = self._foreign_ip.ip;
        hash[2] = ((self._local_port as u32) << 16) + self._foreign_port as u32;
        let key = &isn_secret().key;
        #[cfg(feature = "tls")]
        let mut md5_hash_handle: GnutlsHashHdT = std::ptr::null_mut();
        #[cfg(feature = "tls")]
        // SAFETY: the buffers passed are valid for the lengths given; the digest of MD5 is
        // 16 bytes, the size of `hash` (asserted below).
        unsafe {
            // GnuTLS digests do not init at all, so this should never fail.
            gnutls_hash_init(&mut md5_hash_handle, GNUTLS_DIG_MD5);
            gnutls_hash(md5_hash_handle, hash.as_ptr() as *const _, 3 * std::mem::size_of::<u32>());
            gnutls_hash(md5_hash_handle, key.as_ptr() as *const _, std::mem::size_of_val(key));
            // reuse "hash" for the output of digest
            assert!(std::mem::size_of_val(&hash) == gnutls_hash_get_len(GNUTLS_DIG_MD5) as usize);
            gnutls_hash_deinit(md5_hash_handle, hash.as_mut_ptr() as *mut _);
        }
        // The same digest without gnutls: the bytes of the first three words as they lie in
        // memory, then those of the key, and the 16 bytes of output read back as `hash`.
        #[cfg(not(feature = "tls"))]
        {
            let mut input = [0u8; 3 * std::mem::size_of::<u32>()];
            for (chunk, word) in input.as_chunks_mut::<4>().0.iter_mut().zip(&hash[..3]) {
                *chunk = word.to_ne_bytes();
            }
            let mut key_bytes = [0u8; std::mem::size_of::<[u32; 16]>()];
            for (chunk, word) in key_bytes.as_chunks_mut::<4>().0.iter_mut().zip(key) {
                *chunk = word.to_ne_bytes();
            }
            let digest = crate::engine::net::md5::digest(&[&input, &key_bytes]);
            for (word, chunk) in hash.iter_mut().zip(digest.as_chunks::<4>().0) {
                *word = u32::from_ne_bytes(*chunk);
            }
        }
        let mut seq = hash[0];
        let m = LowresClock::now().time_since_epoch().as_micros() as u64;
        seq = seq.wrapping_add((m / 4) as u32);
        make_seq(seq)
    }

    /// Hands the next queued segment to the IP layer.
    pub fn get_packet(&mut self) -> Option<L4Packet> {
        self._poll_active = false;
        if self._packetq.empty() {
            self.output_one(false);
        }

        if self.in_state(Self::CLOSED.into()) {
            return None;
        }

        assert!(!self._packetq.empty());

        let p = self._packetq.take_front();
        if !self._packetq.empty()
            || (self._snd.dupacks < 3 && self.can_send() > 0 && (self._snd.window > 0))
        {
            // If there are packets to send in the queue or tcb is allowed to send
            // more add tcp back to polling set to keep sending. In addition, dupacks >= 3
            // is an indication that an segment is lost, stop sending more in this case.
            // Finally - we can't send more until window is opened again.
            self.output();
        }
        p
    }
}

impl<T: TcpInetTraits> Connection<T> {
    /// Stops reading.
    pub fn close_read(&mut self) {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb()).abort_reader() };
    }

    /// Closes the sending side.
    pub fn close_write(&mut self) {
        // SAFETY: see `tcb`.
        unsafe { (*self.tcb()).close() };
    }

    /// Cancels a connection attempt, or closes an established connection.
    pub fn shutdown_connect(&mut self) {
        let tcb = self.tcb();
        // SAFETY: see `tcb`; each access is a short borrow.
        if unsafe { (*tcb).syn_needs_on() } {
            // SAFETY: as above.
            unsafe {
                (*tcb)._connect_done.set_exception(ExceptionPtr::from(tcp_refused_error()));
                (*tcb).cleanup();
            }
        } else {
            self.close_read();
            self.close_write();
        }
    }
}

/// The `N` bytes of `out` at `off`, for writing one option.
///
/// # Panics
/// If `out` has fewer than `off + N` bytes.
#[inline]
fn option_area<const N: usize>(out: &mut [u8], off: usize) -> &mut [u8; N] {
    let len = out.len();
    match out.get_mut(off..off + N) {
        Some(area) => area.try_into().expect("a range of N bytes"),
        None => panic!("tcp options area too short: {len} bytes, option at {off} needs {N}"),
    }
}

impl TcpOption {
    /// Parses the options of a received header (the bytes after the fixed header, up to the
    /// data offset).
    ///
    /// The option list is untrusted: a NOP or EOL takes one byte, any other option must have
    /// room for its kind and len bytes, a len of at least 2 and at most the bytes left. On the
    /// first option that breaks those rules the rest of the list is ignored, keeping what was
    /// parsed before it. An MSS, window-scale or SACK-permitted option whose len is not the
    /// fixed one of its kind is skipped by its len without being read, like an option we do
    /// not understand.
    pub fn parse(&mut self, opts: &[u8]) {
        const MSS: u8 = OptionKind::Mss as u8;
        const WIN_SCALE: u8 = OptionKind::WinScale as u8;
        const SACK: u8 = OptionKind::Sack as u8;
        const NOP: u8 = OptionKind::Nop as u8;
        const EOL: u8 = OptionKind::Eol as u8;

        let mut rest = opts;
        while let Some((&kind, after_kind)) = rest.split_first() {
            match kind {
                EOL => return,
                NOP => {
                    rest = after_kind;
                    continue;
                }
                _ => {}
            }
            // Make sure there is enough room for this option
            let Some(&len) = after_kind.first() else {
                return;
            };
            let len = len as usize;
            if len < 2 || len > rest.len() {
                return;
            }
            let (opt, next) = rest.split_at(len);
            match kind {
                MSS => {
                    if let Ok(opt) = opt.try_into() {
                        self._mss_received = true;
                        self._remote_mss = OptMss::read(opt).mss;
                    }
                }
                WIN_SCALE => {
                    if let Ok(opt) = opt.try_into() {
                        self._win_scale_received = true;
                        self._remote_win_scale = OptWinScale::read(opt).shift;
                        // We can turn on win_scale option, 7 is Linux's default win scale size
                        self._local_win_scale = 7;
                    }
                }
                SACK if opt.len() == OptionLen::SACK as usize => {
                    self._sack_received = true;
                }
                // Ignore options we do not understand
                _ => {}
            }
            rest = next;
        }
    }

    /// Writes the options of a segment with the flags of `th` into `out`, the options area
    /// right after the fixed header; returns their size.
    ///
    /// # Panics
    /// If `out.len()` is not [`get_size`](Self::get_size) for these flags (the size the
    /// segment builder reserved).
    pub fn fill(&self, out: &mut [u8], th: &TcpHdr) -> u8 {
        let mut off = 0usize;
        let mut size: u8 = 0;
        let syn_on = th.f_syn();
        let ack_on = th.f_ack();

        if syn_on {
            if self._mss_received || !ack_on {
                let mss = OptMss { mss: self._local_mss };
                mss.write(option_area(out, off));
                off += OptMss::LEN as usize;
                size += OptMss::LEN;
            }
            if self._win_scale_received || !ack_on {
                let win_scale = OptWinScale { shift: self._local_win_scale };
                win_scale.write(option_area(out, off));
                off += OptWinScale::LEN as usize;
                size += OptWinScale::LEN;
            }
        }
        if size > 0 {
            // Insert NOP option
            let size_max = crate::engine::core::align::align_up(size + 1, TcpOption::ALIGN);
            while size < size_max - OptionLen::EOL {
                OptNop.write(option_area(out, off));
                off += OptionLen::NOP as usize;
                size += OptionLen::NOP;
            }
            OptEol.write(option_area(out, off));
            size += OptionLen::EOL;
        }
        assert!(size as usize == out.len());

        size
    }

    /// The size of the options a segment with these flags carries.
    pub fn get_size(&self, syn_on: bool, ack_on: bool) -> u8 {
        let mut size: u8 = 0;
        if syn_on {
            if self._mss_received || !ack_on {
                size += OptionLen::MSS;
            }
            if self._win_scale_received || !ack_on {
                size += OptionLen::WIN_SCALE;
            }
        }
        if size > 0 {
            size += OptionLen::EOL;
            // Insert NOP option to align on 32-bit
            size = crate::engine::core::align::align_up(size, TcpOption::ALIGN);
        }
        size
    }
}

impl Ipv4Tcp {
    /// Creates the TCP protocol object of `inet`; `init` creates the tcp layer once the
    /// object is at its final address.
    pub fn new(inet: *mut crate::engine::net::ip::Ipv4) -> Self {
        Ipv4Tcp { _inet_l4: Ipv4L4::new(inet), _tcp: None }
    }

    /// Creates the tcp layer. Invariant of `Ipv4::new`: it
    /// calls `init` once the ipv4 is at its final address, before any packet arrives.
    pub fn init(&mut self) {
        self._tcp = Some(Tcp::<Ipv4Traits>::new(self._inet_l4));
    }

    /// The tcp layer (in its own node; see [`Tcp::new`]).
    #[inline]
    pub fn tcp_ptr(&self) -> *mut Tcp<Ipv4Traits> {
        debug_assert!(self._tcp.is_some(), "Ipv4Tcp::init not called");
        // SAFETY: `init` runs during the construction of the owning ipv4 (see `init`).
        unsafe { self._tcp.as_ref().unwrap_unchecked() }.as_ptr()
    }
}

impl IpProtocol for Ipv4Tcp {
    fn received(&mut self, p: Packet, from: Ipv4Address, to: Ipv4Address) {
        // SAFETY: the tcp layer lives in its own node, outside `self`; nothing else refers to
        // it while a packet is delivered.
        unsafe { Tcp::received(self.tcp_ptr(), p, from, to) };
    }

    fn forward(&mut self, out_hash_data: &mut ForwardHash, p: &mut Packet, off: usize) -> bool {
        // SAFETY: as above; `forward` does not enter tcbs.
        unsafe { (*self.tcp_ptr()).forward(out_hash_data, p, off) }
    }
}

/// A server socket listening on `port` of `tcpv4`.
pub fn tcpv4_listen(tcpv4: &mut Tcp<Ipv4Traits>, port: u16, opts: ListenOptions) -> ServerSocket {
    ServerSocket::from_impl(Box::new(NativeServerSocketImpl::<Ipv4Traits>::new(tcpv4, port, opts)))
}

/// A client socket of `tcpv4`.
pub fn tcpv4_socket(tcpv4: &mut Tcp<Ipv4Traits>) -> Socket {
    Socket::from_impl(Box::new(NativeSocketImpl::<Ipv4Traits>::new(tcpv4)))
}
