//! A TCP stack in user space for the clients' port on AF_XDP (012 FR-033), sans-I/O: it takes the
//! Ethernet frames that reached the port's queue and gives the frames to send, with the time given
//! by the caller in microseconds. It is the passive side of RFC 9293 for one listening port over
//! IPv4: the three-way handshake with the MSS and window scale options (RFC 7323), in-order
//! delivery with a bounded queue of segments that came early, retransmission on the timer of RFC
//! 6298 with Karn's rule, the fast retransmit and congestion window of RFC 5681, the probes of a
//! closed window, and both closes, with `TIME-WAIT`. The initial sequence numbers follow RFC 6528.
//!
//! The kernel keeps everything the stack does not take: ARP, ICMP, IPv6, and IPv4 with options or
//! fragments (the XDP program of `teseladb_sys::xdp` passes them on). A reply goes back to the MAC
//! and IP address its connection's first segment came from, so the stack needs no table of
//! neighbours or routes.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

const ETH_HEADER: usize = 14;
const IP_HEADER: usize = 20;
const TCP_HEADER: usize = 20;
/// Ethernet, IPv4 and TCP headers without options.
pub const HEADERS: usize = ETH_HEADER + IP_HEADER + TCP_HEADER;
const ETHERTYPE_IPV4: u16 = 0x0800;
const PROTO_TCP: u8 = 6;
const IP_DONT_FRAGMENT: u16 = 0x4000;
const IP_FRAGMENT_BITS: u16 = 0x3fff;
const TTL: u8 = 64;

const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const PSH: u8 = 0x08;
const ACK: u8 = 0x10;

const OPT_END: u8 = 0;
const OPT_NOP: u8 = 1;
const OPT_MSS: u8 = 2;
const OPT_WSCALE: u8 = 3;

/// The MSS a peer that names none takes (RFC 9293, 3.7.1).
const DEFAULT_PEER_MSS: u16 = 536;
/// The shift of the window this stack announces: up to 4 MiB.
const WSCALE: u8 = 6;
/// The largest shift RFC 7323 allows.
const MAX_WSCALE: u8 = 14;
/// Bytes a connection holds that its application has not read, and the window it announces.
pub const RECEIVE_BUFFER: usize = 1 << 20;
/// Initial retransmission timeout (RFC 6298, 2.1).
const INITIAL_RTO_US: u64 = 1_000_000;
/// Floor of the timeout: 200 ms, as Linux, below the 1 s of RFC 6298 on a LAN's round trips.
const MIN_RTO_US: u64 = 200_000;
const MAX_RTO_US: u64 = 60_000_000;
/// Retransmissions of a segment before the connection is given up.
const MAX_RETRIES: u32 = 12;
/// Retransmissions of a `SYN-ACK` before the half-open connection is forgotten.
const MAX_SYNACK_RETRIES: u32 = 5;
/// How long a connection stays in `TIME-WAIT`: 2 MSL, as Linux keeps it (60 s).
const TIME_WAIT_US: u64 = 60_000_000;
/// Duplicate acknowledgements that start a fast retransmit (RFC 5681, 3.2).
const DUP_ACKS: u32 = 3;
/// Initial congestion window, in segments (RFC 6928).
const INITIAL_WINDOW: usize = 10;

/// Sequence number `a` comes before `b`, modulo 2^32.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn at_or_before(a: u32, b: u32) -> bool {
    a == b || before(a, b)
}

/// The two ends of a connection as its first segment named them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Path {
    local_mac: [u8; 6],
    peer_mac: [u8; 6],
    local_ip: [u8; 4],
    peer_ip: [u8; 4],
    local_port: u16,
    peer_port: u16,
}

impl Path {
    fn key(&self) -> (u32, u16, u32) {
        (u32::from_be_bytes(self.peer_ip), self.peer_port, u32::from_be_bytes(self.local_ip))
    }

    /// A hash of the ends, never 0.
    fn flow(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.key().hash(&mut h);
        self.local_port.hash(&mut h);
        h.finish().max(1)
    }

    fn peer(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(self.peer_ip), self.peer_port))
    }

    fn local(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(self.local_ip), self.local_port))
    }
}

/// A TCP segment read from a frame.
#[derive(Debug)]
struct Segment<'a> {
    path: Path,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    mss: Option<u16>,
    wscale: Option<u8>,
    payload: &'a [u8],
}

impl Segment<'_> {
    /// Sequence numbers it takes: its bytes, and one each for `SYN` and `FIN`.
    fn len(&self) -> u32 {
        let n = self.payload.len() as u32;
        n + u32::from(self.flags & SYN != 0) + u32::from(self.flags & FIN != 0)
    }
}

/// The one's complement sum of `bytes` added to `sum`.
fn sum16(mut sum: u32, bytes: &[u8]) -> u32 {
    let (pairs, rest) = bytes.as_chunks::<2>();
    for c in pairs {
        sum += u32::from(u16::from_be_bytes(*c));
    }
    if let [last] = rest {
        sum += u32::from(*last) << 8;
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// The TCP checksum of `segment` (header and data) between `src` and `dst`.
fn tcp_checksum(src: [u8; 4], dst: [u8; 4], segment: &[u8]) -> u16 {
    let mut sum = sum16(0, &src);
    sum = sum16(sum, &dst);
    sum += u32::from(PROTO_TCP) + segment.len() as u32;
    fold(sum16(sum, segment))
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The TCP segment of IPv4 frame `frame`, if it is one whose checksums hold.
fn parse(frame: &[u8]) -> Option<Segment<'_>> {
    if frame.len() < HEADERS || be16(frame, 12) != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = &frame[ETH_HEADER..];
    let ihl = usize::from(ip[0] & 0x0f) * 4;
    if ip[0] >> 4 != 4 || ihl < IP_HEADER || ip[9] != PROTO_TCP {
        return None;
    }
    let total = usize::from(be16(ip, 2));
    if total < ihl + TCP_HEADER || total > ip.len() || be16(ip, 6) & IP_FRAGMENT_BITS != 0 {
        return None;
    }
    if fold(sum16(0, &ip[..ihl])) != 0 {
        return None;
    }
    let src: [u8; 4] = ip[12..16].try_into().ok()?;
    let dst: [u8; 4] = ip[16..20].try_into().ok()?;
    let tcp = &ip[ihl..total];
    if tcp_checksum(src, dst, tcp) != 0 {
        return None;
    }
    let offset = usize::from(tcp[12] >> 4) * 4;
    if offset < TCP_HEADER || offset > tcp.len() {
        return None;
    }
    let (mut mss, mut wscale) = (None, None);
    let mut opts = &tcp[TCP_HEADER..offset];
    while let Some(&kind) = opts.first() {
        match kind {
            OPT_END => break,
            OPT_NOP => opts = &opts[1..],
            _ => {
                let len = usize::from(*opts.get(1)?);
                if len < 2 || len > opts.len() {
                    return None;
                }
                match (kind, len) {
                    (OPT_MSS, 4) => mss = Some(be16(opts, 2)),
                    (OPT_WSCALE, 3) => wscale = Some(opts[2].min(MAX_WSCALE)),
                    _ => {}
                }
                opts = &opts[len..];
            }
        }
    }
    let path = Path {
        local_mac: frame[0..6].try_into().ok()?,
        peer_mac: frame[6..12].try_into().ok()?,
        local_ip: dst,
        peer_ip: src,
        local_port: be16(tcp, 2),
        peer_port: be16(tcp, 0),
    };
    Some(Segment {
        path,
        seq: be32(tcp, 4),
        ack: be32(tcp, 8),
        flags: tcp[13],
        window: be16(tcp, 14),
        mss,
        wscale,
        payload: &tcp[offset..],
    })
}

/// What a frame to send carries.
struct Out<'a> {
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    options: &'a [u8],
    payload: &'a [u8],
}

/// Writes the frame of `o` on `path` into `buf`.
fn build(buf: &mut Vec<u8>, path: &Path, ip_id: u16, o: &Out<'_>) {
    buf.clear();
    let tcp_len = TCP_HEADER + o.options.len() + o.payload.len();
    let total = IP_HEADER + tcp_len;
    buf.extend_from_slice(&path.peer_mac);
    buf.extend_from_slice(&path.local_mac);
    buf.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let ip_at = buf.len();
    buf.extend_from_slice(&[0x45, 0]);
    buf.extend_from_slice(&(total as u16).to_be_bytes());
    buf.extend_from_slice(&ip_id.to_be_bytes());
    buf.extend_from_slice(&IP_DONT_FRAGMENT.to_be_bytes());
    buf.extend_from_slice(&[TTL, PROTO_TCP, 0, 0]);
    buf.extend_from_slice(&path.local_ip);
    buf.extend_from_slice(&path.peer_ip);
    let ip_sum = fold(sum16(0, &buf[ip_at..ip_at + IP_HEADER]));
    buf[ip_at + 10..ip_at + 12].copy_from_slice(&ip_sum.to_be_bytes());
    let tcp_at = buf.len();
    buf.extend_from_slice(&path.local_port.to_be_bytes());
    buf.extend_from_slice(&path.peer_port.to_be_bytes());
    buf.extend_from_slice(&o.seq.to_be_bytes());
    buf.extend_from_slice(&o.ack.to_be_bytes());
    let words = ((TCP_HEADER + o.options.len()) / 4) as u8;
    buf.extend_from_slice(&[words << 4, o.flags]);
    buf.extend_from_slice(&o.window.to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(o.options);
    buf.extend_from_slice(o.payload);
    let sum = tcp_checksum(path.local_ip, path.peer_ip, &buf[tcp_at..]);
    buf[tcp_at + 16..tcp_at + 18].copy_from_slice(&sum.to_be_bytes());
}

/// The states of a connection the stack keeps (RFC 9293, 3.3.2); `LISTEN` is the stack itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The `SYN-ACK` went out; the handshake's last `ACK` has not come.
    SynReceived,
    /// Open both ways.
    Established,
    /// The application closed: the `FIN` waits behind the data or for its `ACK`.
    FinWait1,
    /// The `FIN` was acknowledged; the peer's has not come.
    FinWait2,
    /// Both `FIN`s crossed.
    Closing,
    /// The peer closed first.
    CloseWait,
    /// The peer closed first and this side's `FIN` waits for its `ACK`.
    LastAck,
    /// Both closed: the connection answers a late `FIN` until it goes.
    TimeWait,
}

/// What the application learns from the stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Connection `token` is open, from `peer` to `local`.
    Accepted {
        /// The connection.
        token: u64,
        /// The client.
        peer: SocketAddr,
        /// The address it reached.
        local: SocketAddr,
    },
    /// Bytes arrived on connection `token` ([`TcpStack::read`]).
    Readable(u64),
    /// Data of connection `token` was acknowledged: [`TcpStack::queued`] went down.
    Sent(u64),
    /// Connection `token` ended for the application: the peer closed or reset it, it timed out,
    /// or [`TcpStack::close`] or [`TcpStack::abort`] ended it. Nothing more comes for it.
    Closed(u64),
}

/// One connection.
#[derive(Debug)]
struct Tcb {
    path: Path,
    state: State,
    /// Ended for the application ([`Event::Closed`] given).
    ended: bool,
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    /// The peer's window, in bytes.
    snd_wnd: usize,
    snd_wl1: u32,
    snd_wl2: u32,
    /// Bytes from `snd_una` on: acknowledged ones leave the front.
    send_buf: VecDeque<u8>,
    /// The application closed: a `FIN` follows the data.
    fin_wanted: bool,
    /// The `FIN` went out (it is at `snd_nxt - 1`).
    fin_sent: bool,
    rcv_nxt: u32,
    /// Bytes arrived in order that the application has not read.
    rcv_buf: Vec<u8>,
    /// Segments that came before their turn, by sequence number.
    early: BTreeMap<u32, Vec<u8>>,
    early_bytes: usize,
    /// The window last announced, in bytes.
    announced: usize,
    peer_mss: usize,
    peer_wscale: u8,
    wscale: u8,
    /// An `ACK` is owed.
    ack_due: bool,
    srtt: Option<u64>,
    rttvar: u64,
    rto: u64,
    /// The segment being timed: the sequence number after it and when it left (Karn).
    timing: Option<(u32, u64)>,
    /// When the oldest unacknowledged segment is due again, or the window is probed.
    deadline: Option<u64>,
    retries: u32,
    cwnd: usize,
    ssthresh: usize,
    dup_acks: u32,
    /// Send the first unacknowledged segment again now (fast retransmit).
    resend_first: bool,
    /// Send one byte past a closed window now.
    probe: bool,
}

impl Tcb {
    fn in_flight(&self) -> usize {
        self.snd_nxt.wrapping_sub(self.snd_una) as usize
    }

    /// Bytes of `send_buf` sent at least once.
    fn sent_bytes(&self) -> usize {
        (self.in_flight() - usize::from(self.fin_sent)).min(self.send_buf.len())
    }

    fn mss(&self, own: usize) -> usize {
        self.peer_mss.min(own).max(1)
    }

    /// The window to announce now, in bytes.
    fn window(&self) -> usize {
        RECEIVE_BUFFER.saturating_sub(self.rcv_buf.len() + self.early_bytes)
    }

    fn window_field(&self) -> u16 {
        (self.window() >> self.wscale).min(usize::from(u16::MAX)) as u16
    }

    fn rtt_sample(&mut self, rtt: u64) {
        // RFC 6298, 2.2 and 2.3.
        match self.srtt {
            None => {
                self.srtt = Some(rtt);
                self.rttvar = rtt / 2;
            }
            Some(srtt) => {
                self.rttvar = (3 * self.rttvar + srtt.abs_diff(rtt)) / 4;
                self.srtt = Some((7 * srtt + rtt) / 8);
            }
        }
        let srtt = self.srtt.unwrap_or(rtt);
        self.rto = (srtt + (4 * self.rttvar).max(1_000)).clamp(MIN_RTO_US, MAX_RTO_US);
    }
}

/// The port's TCP connections over IPv4.
#[derive(Debug)]
pub struct TcpStack {
    port: u16,
    /// The only destination address taken, if the port is bound to one.
    address: Option<[u8; 4]>,
    /// The MSS this side announces: the interface's MTU less the IPv4 and TCP headers.
    mss: usize,
    max_connections: usize,
    secret: u64,
    conns: HashMap<u64, Tcb>,
    by_path: HashMap<(u32, u16, u32), u64>,
    next_token: u64,
    ip_id: u16,
    events: VecDeque<Event>,
    /// Frames that belong to no connection (resets).
    control: VecDeque<Vec<u8>>,
    accepting: bool,
    scratch: Vec<u8>,
    /// Flows of the connections made and forgotten since [`TcpStack::take_flows`].
    born: Vec<u64>,
    died: Vec<u64>,
}

/// What [`TcpStack::receive`] did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// It belongs to a connection of the stack, which took it.
    Taken,
    /// A segment of the port that no connection of the stack has.
    Stranger,
    /// Not a segment of the port (another port or address, not TCP over IPv4, or a checksum
    /// that fails).
    Ignored,
}

/// The flow of frame `frame`, the same for every segment of one connection whichever queue
/// brings it: a hash of the client's address and port and the address and port it reached.
pub fn flow_of(frame: &[u8]) -> Option<u64> {
    parse(frame).map(|s| s.path.flow())
}

impl TcpStack {
    /// A stack that listens on `port` (of `address` only, or of any address) for an interface of
    /// `mtu` bytes, with at most `max_connections`, and `secret` for its initial sequence
    /// numbers.
    pub fn new(
        port: u16,
        address: Option<Ipv4Addr>,
        mtu: usize,
        max_connections: usize,
        secret: u64,
    ) -> TcpStack {
        TcpStack {
            port,
            address: address.map(|a| a.octets()),
            mss: mtu.saturating_sub(IP_HEADER + TCP_HEADER).clamp(1, usize::from(u16::MAX)),
            max_connections,
            secret,
            conns: HashMap::new(),
            by_path: HashMap::new(),
            next_token: 1,
            ip_id: 0,
            events: VecDeque::new(),
            control: VecDeque::new(),
            accepting: true,
            scratch: Vec::with_capacity(mtu + ETH_HEADER),
            born: Vec::new(),
            died: Vec::new(),
        }
    }

    /// The largest frame it sends.
    pub fn frame_size(&self) -> usize {
        ETH_HEADER + IP_HEADER + TCP_HEADER + self.mss
    }

    /// Connections it keeps, `TIME-WAIT` included.
    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    /// Connections the application still has.
    pub fn open(&self) -> usize {
        self.conns.values().filter(|t| !t.ended && t.state != State::SynReceived).count()
    }

    /// The state of connection `token`.
    pub fn state(&self, token: u64) -> Option<State> {
        self.conns.get(&token).map(|t| t.state)
    }

    /// The next event.
    pub fn event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Takes no new connection from now on; those open go on.
    pub fn stop_accepting(&mut self) {
        self.accepting = false;
    }

    /// The bytes of connection `token` that arrived and were not read yet.
    pub fn read(&mut self, token: u64) -> Vec<u8> {
        let Some(t) = self.conns.get_mut(&token).filter(|t| !t.ended) else { return Vec::new() };
        let before = t.window();
        let bytes = std::mem::take(&mut t.rcv_buf);
        // A window that grows by two segments or past half the buffer is announced at once
        // (RFC 9293, 3.8.6.2.2).
        let grown = t.window() - before;
        if t.announced < RECEIVE_BUFFER / 2 && grown >= 2 * t.peer_mss.min(self.mss) {
            t.ack_due = true;
        }
        bytes
    }

    /// Queues `bytes` on connection `token`; `false` if it is gone or closing.
    pub fn send(&mut self, token: u64, bytes: &[u8]) -> bool {
        match self.conns.get_mut(&token) {
            Some(t) if !t.ended && !t.fin_wanted => {
                t.send_buf.extend(bytes);
                true
            }
            _ => false,
        }
    }

    /// Bytes of connection `token` not acknowledged yet.
    pub fn queued(&self, token: u64) -> usize {
        self.conns.get(&token).filter(|t| !t.ended).map_or(0, |t| t.send_buf.len())
    }

    /// Ends connection `token` for the application: its `FIN` goes after what is queued.
    pub fn close(&mut self, token: u64) {
        let Some(t) = self.conns.get_mut(&token).filter(|t| !t.ended) else { return };
        t.fin_wanted = true;
        t.ended = true;
        t.state = match t.state {
            State::CloseWait => State::LastAck,
            State::Established | State::SynReceived => State::FinWait1,
            other => other,
        };
        self.events.push_back(Event::Closed(token));
    }

    /// Resets connection `token` and forgets it.
    pub fn abort(&mut self, token: u64) {
        let Some(t) = self.conns.remove(&token) else { return };
        self.by_path.remove(&t.path.key());
        self.died.push(t.path.flow());
        let o = Out {
            seq: t.snd_nxt,
            ack: t.rcv_nxt,
            flags: RST | ACK,
            window: 0,
            options: &[],
            payload: &[],
        };
        let mut frame = Vec::new();
        self.ip_id = self.ip_id.wrapping_add(1);
        build(&mut frame, &t.path, self.ip_id, &o);
        self.control.push_back(frame);
        if !t.ended {
            self.events.push_back(Event::Closed(token));
        }
    }

    /// When [`TcpStack::tick`] has work next.
    pub fn next_deadline(&self) -> Option<u64> {
        self.conns.values().filter_map(|t| t.deadline).min()
    }

    fn isn(&self, path: &Path, now: u64) -> u32 {
        // RFC 6528: a clock of 4 µs plus a keyed hash of the connection's ends.
        let mut h = DefaultHasher::new();
        self.secret.hash(&mut h);
        path.hash(&mut h);
        (h.finish() as u32).wrapping_add((now / 4) as u32)
    }

    /// Takes frame `frame` from the interface at `now`; a frame of no connection is left to the
    /// caller ([`TcpStack::adopt`]), which may hand it to the stack that has its connection.
    pub fn receive(&mut self, frame: &[u8], now: u64) -> Arrival {
        let Some(seg) = parse(frame) else { return Arrival::Ignored };
        if seg.path.local_port != self.port || self.address.is_some_and(|a| a != seg.path.local_ip)
        {
            return Arrival::Ignored;
        }
        match self.by_path.get(&seg.path.key()).copied() {
            Some(token) => {
                self.segment(token, &seg, now);
                Arrival::Taken
            }
            None => Arrival::Stranger,
        }
    }

    /// Takes frame `frame` of no connection of this stack as its own ([`Arrival::Stranger`]):
    /// a `SYN` opens a connection, anything else but a reset is reset.
    pub fn adopt(&mut self, frame: &[u8], now: u64) {
        if let Some(seg) = parse(frame) {
            self.listen(&seg, now);
        }
    }

    /// The flows ([`flow_of`]) this stack took and forgot since the last call.
    pub fn take_flows(&mut self) -> (Vec<u64>, Vec<u64>) {
        (std::mem::take(&mut self.born), std::mem::take(&mut self.died))
    }

    /// A segment of no connection: a `SYN` opens one, anything else but a reset is reset.
    fn listen(&mut self, seg: &Segment<'_>, now: u64) {
        if seg.flags & RST != 0 {
            return;
        }
        if seg.flags & ACK != 0 || seg.flags & SYN == 0 {
            self.reset(seg);
            return;
        }
        if !self.accepting || self.conns.len() >= self.max_connections {
            // The client tries again: a full port drops the `SYN`, as a full backlog does.
            return;
        }
        let iss = self.isn(&seg.path, now);
        let (wscale, peer_wscale) = match seg.wscale {
            Some(w) => (WSCALE, w),
            None => (0, 0),
        };
        let peer_mss = usize::from(seg.mss.unwrap_or(DEFAULT_PEER_MSS));
        let t = Tcb {
            path: seg.path,
            state: State::SynReceived,
            ended: false,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_wnd: usize::from(seg.window),
            snd_wl1: seg.seq,
            snd_wl2: iss,
            send_buf: VecDeque::new(),
            fin_wanted: false,
            fin_sent: false,
            rcv_nxt: seg.seq.wrapping_add(1),
            rcv_buf: Vec::new(),
            early: BTreeMap::new(),
            early_bytes: 0,
            announced: 0,
            peer_mss,
            peer_wscale,
            wscale,
            ack_due: false,
            srtt: None,
            rttvar: 0,
            rto: INITIAL_RTO_US,
            timing: None,
            deadline: None,
            retries: 0,
            cwnd: INITIAL_WINDOW * peer_mss.min(self.mss),
            ssthresh: usize::MAX,
            dup_acks: 0,
            resend_first: false,
            probe: false,
        };
        let token = self.next_token;
        self.next_token += 1;
        self.by_path.insert(seg.path.key(), token);
        self.born.push(seg.path.flow());
        self.conns.insert(token, t);
    }

    /// Answers `seg`, which belongs to no connection, with a reset (RFC 9293, 3.10.7.1).
    fn reset(&mut self, seg: &Segment<'_>) {
        let o = if seg.flags & ACK != 0 {
            Out { seq: seg.ack, ack: 0, flags: RST, window: 0, options: &[], payload: &[] }
        } else {
            let ack = seg.seq.wrapping_add(seg.len());
            Out { seq: 0, ack, flags: RST | ACK, window: 0, options: &[], payload: &[] }
        };
        let mut frame = Vec::new();
        self.ip_id = self.ip_id.wrapping_add(1);
        build(&mut frame, &seg.path, self.ip_id, &o);
        self.control.push_back(frame);
    }

    /// Forgets connection `token`, telling the application if it still had it.
    fn drop_conn(&mut self, token: u64) {
        if let Some(t) = self.conns.remove(&token) {
            self.by_path.remove(&t.path.key());
            self.died.push(t.path.flow());
            // A half-open connection was never the application's.
            if !t.ended && t.state != State::SynReceived {
                self.events.push_back(Event::Closed(token));
            }
        }
    }

    /// Segment `seg` of connection `token` (RFC 9293, 3.10.7.4).
    fn segment(&mut self, token: u64, seg: &Segment<'_>, now: u64) {
        let own_mss = self.mss;
        let Some(t) = self.conns.get_mut(&token) else { return };
        if t.state == State::SynReceived && seg.flags & SYN != 0 && seg.flags & ACK == 0 {
            // The client sent its `SYN` again: the `SYN-ACK` goes again.
            if seg.seq.wrapping_add(1) == t.rcv_nxt {
                t.snd_nxt = t.snd_una;
                t.deadline = Some(now);
            }
            return;
        }
        if t.state == State::TimeWait && seg.flags & SYN != 0 && seg.flags & ACK == 0 {
            // A new connection of the same ends may start once the old one's numbers are behind
            // it (RFC 9293, 3.6.1); this one goes.
            if before(t.rcv_nxt, seg.seq) {
                self.drop_conn(token);
                self.listen(seg, now);
            }
            return;
        }
        // Acceptability (3.10.7.4, first check).
        let window = t.window() as u32;
        let len = seg.len();
        let in_window =
            |s: u32| at_or_before(t.rcv_nxt, s) && before(s, t.rcv_nxt.wrapping_add(window.max(1)));
        let acceptable = match (len, window) {
            (0, 0) => seg.seq == t.rcv_nxt,
            (0, _) => in_window(seg.seq),
            (_, 0) => false,
            (n, _) => in_window(seg.seq) || in_window(seg.seq.wrapping_add(n - 1)),
        };
        if !acceptable {
            if seg.flags & RST == 0 {
                t.ack_due = true;
            }
            return;
        }
        if seg.flags & RST != 0 {
            // RFC 5961: only the exact number resets; one inside the window gets an `ACK`.
            if seg.seq == t.rcv_nxt {
                self.drop_conn(token);
            } else {
                t.ack_due = true;
            }
            return;
        }
        if seg.flags & SYN != 0 {
            // A `SYN` inside the window: a challenge `ACK` (RFC 5961, 4).
            t.ack_due = true;
            return;
        }
        if seg.flags & ACK == 0 {
            return;
        }
        if t.state == State::SynReceived {
            if seg.ack != t.iss.wrapping_add(1) {
                self.reset(seg);
                return;
            }
            t.state = State::Established;
            t.snd_una = seg.ack;
            t.snd_wnd = usize::from(seg.window) << t.peer_wscale;
            t.snd_wl1 = seg.seq;
            t.snd_wl2 = seg.ack;
            t.retries = 0;
            t.deadline = None;
            if let Some((end, sent)) = t.timing.take()
                && at_or_before(end, seg.ack)
            {
                t.rtt_sample(now.saturating_sub(sent));
            }
            let (peer, local) = (t.path.peer(), t.path.local());
            self.events.push_back(Event::Accepted { token, peer, local });
        } else {
            Self::acknowledge(t, seg, now, own_mss, token, &mut self.events);
        }
        let Some(t) = self.conns.get_mut(&token) else { return };
        Self::take_data(t, seg, token, &mut self.events);
        Self::take_fin(t, seg, now, token, &mut self.events);
        if t.state == State::Closing && !t.ended {
            t.ended = true;
            self.events.push_back(Event::Closed(token));
        }
        let gone = match t.state {
            State::LastAck => t.fin_sent && t.snd_una == t.snd_nxt,
            _ => false,
        };
        if gone {
            self.drop_conn(token);
        }
    }

    /// The `ACK` of `seg` on an open connection (3.10.7.4, fifth check).
    fn acknowledge(
        t: &mut Tcb,
        seg: &Segment<'_>,
        now: u64,
        own_mss: usize,
        token: u64,
        events: &mut VecDeque<Event>,
    ) {
        let mss = t.mss(own_mss);
        if before(t.snd_nxt, seg.ack) {
            // It acknowledges what was never sent.
            t.ack_due = true;
            return;
        }
        let window = usize::from(seg.window) << t.peer_wscale;
        if before(t.snd_una, seg.ack) {
            let acked = seg.ack.wrapping_sub(t.snd_una) as usize;
            let fin_acked = t.fin_sent && seg.ack == t.snd_nxt;
            let data = acked - usize::from(fin_acked);
            t.send_buf.drain(..data.min(t.send_buf.len()));
            t.snd_una = seg.ack;
            t.retries = 0;
            if let Some((end, sent)) = t.timing
                && at_or_before(end, seg.ack)
            {
                t.timing = None;
                t.rtt_sample(now.saturating_sub(sent));
            }
            // RFC 5681, 3.1: slow start, then one segment a round trip.
            if t.dup_acks >= DUP_ACKS {
                t.cwnd = t.ssthresh;
            } else if t.cwnd < t.ssthresh {
                t.cwnd += acked.min(mss);
            } else {
                t.cwnd += (mss * mss / t.cwnd).max(1);
            }
            t.dup_acks = 0;
            t.deadline = (t.in_flight() > 0).then(|| now + t.rto);
            if data > 0 && !t.ended {
                events.push_back(Event::Sent(token));
            }
            if fin_acked {
                t.state = match t.state {
                    State::FinWait1 => State::FinWait2,
                    State::Closing => {
                        t.deadline = Some(now + TIME_WAIT_US);
                        State::TimeWait
                    }
                    other => other,
                };
            }
        } else if seg.ack == t.snd_una
            && seg.payload.is_empty()
            && seg.flags & FIN == 0
            && window == t.snd_wnd
            && t.in_flight() > 0
        {
            // A duplicate acknowledgement (RFC 5681, 2).
            t.dup_acks += 1;
            if t.dup_acks == DUP_ACKS {
                t.ssthresh = (t.in_flight() / 2).max(2 * mss);
                t.cwnd = t.ssthresh + DUP_ACKS as usize * mss;
                t.resend_first = true;
                t.timing = None;
            } else if t.dup_acks > DUP_ACKS {
                t.cwnd += mss;
            }
        }
        // The window, from a segment no older than the last that set it.
        if before(t.snd_wl1, seg.seq) || (t.snd_wl1 == seg.seq && at_or_before(t.snd_wl2, seg.ack))
        {
            let was_closed = t.snd_wnd == 0;
            t.snd_wnd = window;
            t.snd_wl1 = seg.seq;
            t.snd_wl2 = seg.ack;
            if was_closed && window > 0 && t.in_flight() == 0 {
                t.deadline = None;
            }
        }
        let _ = token;
    }

    /// The data of `seg`: in order to the application, early to wait (3.10.7.4, seventh check).
    fn take_data(t: &mut Tcb, seg: &Segment<'_>, token: u64, events: &mut VecDeque<Event>) {
        if seg.payload.is_empty() {
            return;
        }
        if !matches!(t.state, State::Established | State::FinWait1 | State::FinWait2) {
            return;
        }
        t.ack_due = true;
        // What lies before `rcv_nxt` came already.
        let skip = t.rcv_nxt.wrapping_sub(seg.seq) as usize;
        let (seq, bytes) = if before(seg.seq, t.rcv_nxt) {
            (t.rcv_nxt, seg.payload.get(skip..).unwrap_or(&[]))
        } else {
            (seg.seq, seg.payload)
        };
        let room = t.window();
        if seq != t.rcv_nxt {
            let ahead = seq.wrapping_sub(t.rcv_nxt) as usize;
            if ahead + bytes.len() <= room && !t.early.contains_key(&seq) {
                t.early_bytes += bytes.len();
                t.early.insert(seq, bytes.to_vec());
            }
            return;
        }
        let take = bytes.len().min(room);
        let had = t.rcv_buf.len();
        t.rcv_buf.extend_from_slice(&bytes[..take]);
        t.rcv_nxt = t.rcv_nxt.wrapping_add(take as u32);
        // The early segments that now follow.
        while let Some((&s, _)) = t.early.first_key_value() {
            if before(t.rcv_nxt, s) {
                break;
            }
            let Some(b) = t.early.remove(&s) else { break };
            t.early_bytes -= b.len();
            let skip = t.rcv_nxt.wrapping_sub(s) as usize;
            if let Some(rest) = b.get(skip..) {
                t.rcv_buf.extend_from_slice(rest);
                t.rcv_nxt = t.rcv_nxt.wrapping_add(rest.len() as u32);
            }
        }
        if t.rcv_buf.len() > had && !t.ended {
            events.push_back(Event::Readable(token));
        }
    }

    /// The `FIN` of `seg`, once every byte before it arrived (3.10.7.4, eighth check).
    fn take_fin(
        t: &mut Tcb,
        seg: &Segment<'_>,
        now: u64,
        token: u64,
        events: &mut VecDeque<Event>,
    ) {
        if seg.flags & FIN == 0 {
            return;
        }
        let fin_at = seg.seq.wrapping_add(seg.payload.len() as u32);
        if fin_at != t.rcv_nxt {
            return;
        }
        t.rcv_nxt = t.rcv_nxt.wrapping_add(1);
        t.ack_due = true;
        t.state = match t.state {
            State::Established => State::CloseWait,
            State::FinWait1 if t.fin_sent && t.snd_una == t.snd_nxt => {
                t.deadline = Some(now + TIME_WAIT_US);
                State::TimeWait
            }
            State::FinWait1 => State::Closing,
            State::FinWait2 => {
                t.deadline = Some(now + TIME_WAIT_US);
                State::TimeWait
            }
            State::TimeWait => {
                t.deadline = Some(now + TIME_WAIT_US);
                State::TimeWait
            }
            other => other,
        };
        // The peer is done: the application's connection ends, as a socket's read of 0 bytes
        // ends it; the `FIN` of this side follows what is queued.
        if t.state == State::CloseWait && !t.ended {
            t.ended = true;
            t.fin_wanted = true;
            t.state = State::LastAck;
            events.push_back(Event::Closed(token));
        }
    }

    /// Runs the timers due at `now`: retransmissions, window probes and the end of `TIME-WAIT`.
    pub fn tick(&mut self, now: u64) {
        let due: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, t)| t.deadline.is_some_and(|d| d <= now))
            .map(|(k, _)| *k)
            .collect();
        let own_mss = self.mss;
        for token in due {
            let Some(t) = self.conns.get_mut(&token) else { continue };
            if t.state == State::TimeWait {
                self.drop_conn(token);
                continue;
            }
            if t.state == State::SynReceived && t.snd_nxt == t.snd_una {
                // A `SYN-ACK` asked for again, sent at the next transmit.
                t.deadline = None;
                continue;
            }
            let limit =
                if t.state == State::SynReceived { MAX_SYNACK_RETRIES } else { MAX_RETRIES };
            if t.retries >= limit {
                self.drop_conn(token);
                continue;
            }
            t.retries += 1;
            t.rto = (t.rto * 2).min(MAX_RTO_US);
            t.timing = None;
            if t.in_flight() == 0 {
                // A closed window with data waiting: a probe of one byte (RFC 9293, 3.8.6.1).
                t.probe = true;
                t.deadline = None;
                continue;
            }
            // RFC 5681, 3.1, equation 4, and go back to the first unacknowledged byte.
            let mss = t.mss(own_mss);
            t.ssthresh = (t.in_flight() / 2).max(2 * mss);
            t.cwnd = mss;
            t.dup_acks = 0;
            t.snd_nxt = t.snd_una;
            t.fin_sent = false;
            t.deadline = None;
        }
    }

    /// Hands the frames to send at `now` to `emit`, one at a time, until it refuses one (no room
    /// to send): what is left goes at the next call.
    pub fn transmit(&mut self, now: u64, emit: &mut dyn FnMut(&[u8]) -> bool) {
        while let Some(frame) = self.control.front() {
            if !emit(frame) {
                return;
            }
            self.control.pop_front();
        }
        let tokens: Vec<u64> = self.conns.keys().copied().collect();
        for token in tokens {
            if !self.transmit_one(token, now, emit) {
                return;
            }
        }
    }

    /// The frames of connection `token`; `false` once `emit` refused one.
    fn transmit_one(&mut self, token: u64, now: u64, emit: &mut dyn FnMut(&[u8]) -> bool) -> bool {
        let own_mss = self.mss;
        let mut frame = std::mem::take(&mut self.scratch);
        let ok = self.segments_of(token, now, own_mss, &mut frame, emit);
        self.scratch = frame;
        ok
    }

    fn segments_of(
        &mut self,
        token: u64,
        now: u64,
        own_mss: usize,
        frame: &mut Vec<u8>,
        emit: &mut dyn FnMut(&[u8]) -> bool,
    ) -> bool {
        let Some(t) = self.conns.get_mut(&token) else { return true };
        if t.state == State::SynReceived {
            if t.snd_nxt != t.snd_una {
                return true;
            }
            let mut options = vec![OPT_MSS, 4];
            options.extend_from_slice(&(own_mss as u16).to_be_bytes());
            if t.wscale > 0 {
                options.extend_from_slice(&[OPT_NOP, OPT_WSCALE, 3, t.wscale]);
            }
            let window = t.window().min(usize::from(u16::MAX)) as u16;
            let o = Out {
                seq: t.iss,
                ack: t.rcv_nxt,
                flags: SYN | ACK,
                window,
                options: &options,
                payload: &[],
            };
            self.ip_id = self.ip_id.wrapping_add(1);
            build(frame, &t.path, self.ip_id, &o);
            if !emit(frame) {
                return false;
            }
            t.snd_nxt = t.iss.wrapping_add(1);
            t.announced = usize::from(window);
            if t.timing.is_none() && t.retries == 0 {
                t.timing = Some((t.snd_nxt, now));
            }
            t.deadline = Some(now + t.rto);
            return true;
        }
        if t.state == State::TimeWait {
            if t.ack_due {
                return Self::pure_ack(t, &mut self.ip_id, frame, emit);
            }
            return true;
        }
        let mss = t.mss(own_mss);
        let mut sent_any = false;
        if t.resend_first && t.in_flight() > 0 {
            let n = t.send_buf.len().min(mss);
            let payload: Vec<u8> = t.send_buf.iter().take(n).copied().collect();
            let fin = n == t.send_buf.len() && t.fin_sent;
            let flags = ACK | if fin { FIN } else { 0 } | if n > 0 { PSH } else { 0 };
            let o = Out {
                seq: t.snd_una,
                ack: t.rcv_nxt,
                flags,
                window: t.window_field(),
                options: &[],
                payload: &payload,
            };
            self.ip_id = self.ip_id.wrapping_add(1);
            build(frame, &t.path, self.ip_id, &o);
            if !emit(frame) {
                return false;
            }
            t.resend_first = false;
            t.ack_due = false;
            t.announced = t.window();
            sent_any = true;
            if t.deadline.is_none() {
                t.deadline = Some(now + t.rto);
            }
        }
        loop {
            let sent = t.sent_bytes();
            let unsent = t.send_buf.len() - sent;
            let usable = t.snd_wnd.min(t.cwnd).saturating_sub(t.in_flight());
            let probe = std::mem::take(&mut t.probe) && unsent > 0 && t.snd_wnd == 0;
            let n = if probe { 1 } else { unsent.min(usable).min(mss) };
            let fin = t.fin_wanted
                && !t.fin_sent
                && sent + n == t.send_buf.len()
                && (n > 0 || usable > 0 || t.snd_wnd == 0 && unsent == 0);
            if n == 0 && !fin {
                break;
            }
            let payload: Vec<u8> = t.send_buf.iter().skip(sent).take(n).copied().collect();
            let flags =
                ACK | if fin { FIN } else { 0 } | if n > 0 && n == unsent { PSH } else { 0 };
            let seq = t.snd_nxt;
            let o = Out {
                seq,
                ack: t.rcv_nxt,
                flags,
                window: t.window_field(),
                options: &[],
                payload: &payload,
            };
            self.ip_id = self.ip_id.wrapping_add(1);
            build(frame, &t.path, self.ip_id, &o);
            if !emit(frame) {
                return false;
            }
            t.snd_nxt = seq.wrapping_add(n as u32 + u32::from(fin));
            if fin {
                t.fin_sent = true;
            }
            if t.timing.is_none() && t.retries == 0 {
                t.timing = Some((t.snd_nxt, now));
            }
            if t.deadline.is_none() {
                t.deadline = Some(now + t.rto);
            }
            t.ack_due = false;
            t.announced = t.window();
            sent_any = true;
            if fin || probe {
                break;
            }
        }
        // A closed window with data waiting probes on the timer.
        if t.snd_wnd == 0
            && t.in_flight() == 0
            && t.sent_bytes() < t.send_buf.len()
            && t.deadline.is_none()
        {
            t.deadline = Some(now + t.rto);
        }
        if t.ack_due && !sent_any {
            return Self::pure_ack(t, &mut self.ip_id, frame, emit);
        }
        true
    }

    fn pure_ack(
        t: &mut Tcb,
        ip_id: &mut u16,
        frame: &mut Vec<u8>,
        emit: &mut dyn FnMut(&[u8]) -> bool,
    ) -> bool {
        let o = Out {
            seq: t.snd_nxt,
            ack: t.rcv_nxt,
            flags: ACK,
            window: t.window_field(),
            options: &[],
            payload: &[],
        };
        *ip_id = ip_id.wrapping_add(1);
        build(frame, &t.path, *ip_id, &o);
        if !emit(frame) {
            return false;
        }
        t.ack_due = false;
        t.announced = t.window();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_compare_across_the_wrap() {
        assert!(before(u32::MAX - 1, 2));
        assert!(!before(2, u32::MAX - 1));
        assert!(at_or_before(5, 5));
    }

    #[test]
    fn the_checksum_of_a_frame_it_builds_holds() {
        let path = Path {
            local_mac: [2; 6],
            peer_mac: [4; 6],
            local_ip: [10, 0, 0, 1],
            peer_ip: [10, 0, 0, 2],
            local_port: 27017,
            peer_port: 40000,
        };
        let mut f = Vec::new();
        let o =
            Out { seq: 7, ack: 9, flags: ACK | PSH, window: 512, options: &[], payload: b"odd" };
        build(&mut f, &path, 1, &o);
        // Read back from the other side's point of view: the ends swap.
        let seg = parse(&f).unwrap();
        assert_eq!((seg.seq, seg.ack, seg.payload), (7, 9, &b"odd"[..]));
        assert_eq!(seg.path.local_port, 40000);
        let mut bad = f.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(parse(&bad).is_none());
    }
}
