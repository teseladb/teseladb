//! DNS resolution over c-ares, driven by the reactor.
//!
//! A [`DnsResolver`] resolves host names, addresses and SRV records on the shard that owns
//! it. c-ares does the protocol work; its sockets are replaced by sockets of a
//! [`NetworkStack`] (UDP datagram channels and TCP connected sockets), so queries run on the
//! shard's stack without blocking, and a timer polls c-ares while queries are in flight.
//! The [`dns`] submodule offers the same calls through a lazily created per-shard resolver
//! with the default stack and options.
//!
//! c-ares is declared with `extern "C"` (version 1.34 layout). The features `cares_dnsrec`
//! (c-ares >= 1.28, `ares_query_dnsrec`) and `cares_process_fds` (>= 1.34,
//! `ares_process_fds`) select the newer c-ares entry points.

use std::cell::RefCell;
#[cfg(cares_process_fds)]
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ushort, c_void};
use std::fmt;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, make_exception_future, make_ready_future,
};
use crate::engine::core::future::{InvalidArgument, OutOfRange, SystemError};
use crate::engine::core::gate::Gate;
use crate::engine::core::iostream::{InputStream, OutputStream};
use crate::engine::core::reactor::engine;
use crate::engine::core::shared_ptr::{
    EnableSharedFromThis, SharedFromThis, SharedPtr, make_shared_esft,
};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::core::timer::Timer;
use crate::engine::net::api::{
    ConnectedSocket, ConnectedSocketInputStreamConfig, Datagram, DatagramChannel, NetworkStack,
};
use crate::engine::net::inet_address::{Family, InetAddress, RuntimeError};
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::net::packet::{Fragment, Packet};
use crate::engine::net::posix_stack::iovec;
use crate::engine::net::socket_defs::{
    AF_INET, AF_INET6, AF_UNSPEC, SocketAddress, Transport, in_addr, in6_addr, sockaddr,
    sockaddr_in, sockaddr_in6, socklen_t,
};
use crate::engine::util::log::Logger;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The result of a name or address lookup: the names and addresses of a host.
#[derive(Clone, Debug, Default)]
pub struct Hostent {
    /// Primary name is always first
    pub names: Vec<Sstring>,
    /// Primary address is also always first.
    pub addr_list: Vec<InetAddress>,
}

/// The address family a lookup asks for; `None` accepts any.
pub type OptFamily = Option<Family>;

/// A DNS SRV record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SrvRecord {
    /// Priority.
    pub priority: u16,
    /// Weight.
    pub weight: u16,
    /// Port.
    pub port: u16,
    /// Target host.
    pub target: Sstring,
}

/// The options of a [`DnsResolver`]; each unset option keeps the c-ares default (or the
/// system configuration).
#[derive(Clone, Debug, Default)]
pub struct DnsResolverOptions {
    /// Query over TCP.
    pub use_tcp_query: Option<bool>,
    /// The servers to query.
    pub servers: Option<Vec<InetAddress>>,
    /// The query timeout.
    pub timeout: Option<Duration>,
    /// The TCP port of the servers.
    pub tcp_port: Option<u16>,
    /// The UDP port of the servers.
    pub udp_port: Option<u16>,
    /// The search domains.
    pub domains: Option<Vec<Sstring>>,
}

/// The protocol label of an SRV query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SrvProto {
    /// `_tcp`
    Tcp,
    /// `_udp`
    Udp,
}

/// The SRV records of a service.
pub type SrvRecords = Vec<SrvRecord>;

/// A DNS resolver: the query logic and its networking.
///
/// It can be created with options and a network stack of choice; for ordinary,
/// non-test queries the functions of the [`dns`] module, which use a shard-local default
/// resolver, are simpler.
pub struct DnsResolver {
    _impl: SharedPtr<DnsResolverImpl>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Formats an [`OptFamily`]: the family, or `ANY`.
pub struct OptFamilyDisplay<'a>(pub &'a OptFamily);

impl fmt::Display for OptFamilyDisplay<'_> {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(f) => write!(os, "{}", f),
            None => os.write_str("ANY"),
        }
    }
}

// c-ares declarations

#[allow(non_camel_case_types)]
type ares_socket_t = c_int;
#[allow(non_camel_case_types)]
type ares_channel_t = c_void;

const ARES_SUCCESS: c_int = 0;
const ARES_ENODATA: c_int = 1;
const ARES_EFORMERR: c_int = 2;
const ARES_ESERVFAIL: c_int = 3;
const ARES_ENOTFOUND: c_int = 4;
const ARES_ENOTIMP: c_int = 5;
const ARES_EREFUSED: c_int = 6;
const ARES_EBADQUERY: c_int = 7;
const ARES_EBADNAME: c_int = 8;
const ARES_EBADFAMILY: c_int = 9;
const ARES_EBADRESP: c_int = 10;
const ARES_ECONNREFUSED: c_int = 11;
const ARES_ETIMEOUT: c_int = 12;
const ARES_EOF: c_int = 13;
const ARES_EFILE: c_int = 14;
const ARES_ENOMEM: c_int = 15;
const ARES_EDESTRUCTION: c_int = 16;
const ARES_EBADSTR: c_int = 17;
const ARES_EBADFLAGS: c_int = 18;
const ARES_ENONAME: c_int = 19;
const ARES_EBADHINTS: c_int = 20;
const ARES_ENOTINITIALIZED: c_int = 21;
const ARES_ELOADIPHLPAPI: c_int = 22;
const ARES_EADDRGETNETWORKPARAMS: c_int = 23;
const ARES_ECANCELLED: c_int = 24;

const ARES_LIB_INIT_NONE: c_int = 0;
const ARES_OPT_FLAGS: c_int = 1 << 0;
const ARES_OPT_UDP_PORT: c_int = 1 << 4;
const ARES_OPT_TCP_PORT: c_int = 1 << 5;
const ARES_OPT_SERVERS: c_int = 1 << 6;
const ARES_OPT_DOMAINS: c_int = 1 << 7;
const ARES_OPT_LOOKUPS: c_int = 1 << 8;
const ARES_OPT_TIMEOUTMS: c_int = 1 << 13;
const ARES_FLAG_USEVC: c_int = 1 << 0;
const ARES_FLAG_PRIMARY: c_int = 1 << 1;
const ARES_AI_CANONNAME: c_int = 1 << 0;
const ARES_SOCKET_BAD: ares_socket_t = -1;

const SOCK_STREAM: c_int = 1;
const SOCK_DGRAM: c_int = 2;
const POLLIN: c_int = 0x001;
const POLLOUT: c_int = 0x004;
// Linux values, like the rest of the crate (epoll, io_uring, `__errno_location`): the crate is
// built for Linux only.
const EWOULDBLOCK: c_int = 11;
const ENOTCONN: c_int = 107;

#[repr(C)]
struct AresOptions {
    flags: c_int,
    timeout: c_int,
    tries: c_int,
    ndots: c_int,
    udp_port: c_ushort,
    tcp_port: c_ushort,
    socket_send_buffer_size: c_int,
    socket_receive_buffer_size: c_int,
    servers: *mut in_addr,
    nservers: c_int,
    domains: *mut *mut c_char,
    ndomains: c_int,
    lookups: *mut c_char,
    sock_state_cb: *mut c_void,
    sock_state_cb_data: *mut c_void,
    sortlist: *mut c_void,
    nsort: c_int,
    ednspsz: c_int,
    resolvconf_path: *mut c_char,
    hosts_path: *mut c_char,
    udp_max_queries: c_int,
    maxtimeout: c_int,
    qcache_max_ttl: c_uint,
    evsys: c_int,
    server_failover_opts: [u64; 2],
}

#[repr(C)]
struct AresSocketFunctions {
    asocket: unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void) -> ares_socket_t,
    aclose: unsafe extern "C" fn(ares_socket_t, *mut c_void) -> c_int,
    aconnect: unsafe extern "C" fn(ares_socket_t, *const sockaddr, socklen_t, *mut c_void) -> c_int,
    arecvfrom: unsafe extern "C" fn(
        ares_socket_t,
        *mut c_void,
        usize,
        c_int,
        *mut sockaddr,
        *mut socklen_t,
        *mut c_void,
    ) -> isize,
    asendv: unsafe extern "C" fn(ares_socket_t, *const iovec, c_int, *mut c_void) -> isize,
}

#[repr(C)]
struct AresAddrinfoHints {
    ai_flags: c_int,
    ai_family: c_int,
    ai_socktype: c_int,
    ai_protocol: c_int,
}

#[repr(C)]
struct AresAddrinfoCname {
    ttl: c_int,
    alias: *mut c_char,
    name: *mut c_char,
    next: *mut AresAddrinfoCname,
}

#[repr(C)]
struct AresAddrinfoNode {
    ai_ttl: c_int,
    ai_flags: c_int,
    ai_family: c_int,
    ai_socktype: c_int,
    ai_protocol: c_int,
    ai_addrlen: socklen_t,
    ai_addr: *mut sockaddr,
    ai_next: *mut AresAddrinfoNode,
}

#[repr(C)]
struct AresAddrinfo {
    cnames: *mut AresAddrinfoCname,
    nodes: *mut AresAddrinfoNode,
    name: *mut c_char,
}

#[repr(C)]
struct CHostent {
    h_name: *mut c_char,
    h_aliases: *mut *mut c_char,
    h_addrtype: c_int,
    h_length: c_int,
    h_addr_list: *mut *mut c_char,
}

#[repr(C)]
struct AresSrvReply {
    next: *mut AresSrvReply,
    host: *mut c_char,
    priority: u16,
    weight: u16,
    port: u16,
}

/// `fd_set` of the Linux ABI (`FD_SETSIZE` = 1024).
#[repr(C)]
#[derive(Clone, Copy)]
struct FdSet {
    bits: [u64; 16],
}

impl FdSet {
    const SETSIZE: usize = 1024;
    #[inline]
    fn zero() -> Self {
        FdSet { bits: [0; 16] }
    }
    #[inline]
    fn is_set(&self, fd: c_int) -> bool {
        let fd = fd as usize;
        fd < Self::SETSIZE && (self.bits[fd / 64] >> (fd % 64)) & 1 != 0
    }
    #[inline]
    #[allow(dead_code)]
    fn set(&mut self, fd: c_int) {
        let fd = fd as usize;
        if fd < Self::SETSIZE {
            self.bits[fd / 64] |= 1 << (fd % 64);
        }
    }
    #[inline]
    #[allow(dead_code)]
    fn clr(&mut self, fd: c_int) {
        let fd = fd as usize;
        if fd < Self::SETSIZE {
            self.bits[fd / 64] &= !(1 << (fd % 64));
        }
    }
}

#[cfg(cares_process_fds)]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AresFdEvents {
    fd: ares_socket_t,
    events: c_uint,
}

#[cfg(cares_process_fds)]
const ARES_FD_EVENT_READ: c_uint = 1 << 0;
#[cfg(cares_process_fds)]
const ARES_FD_EVENT_WRITE: c_uint = 1 << 1;
#[cfg(cares_process_fds)]
const ARES_PROCESS_FLAG_NONE: c_uint = 0;

#[cfg(cares_dnsrec)]
const ARES_CLASS_IN: c_int = 1;
#[cfg(cares_dnsrec)]
const ARES_REC_TYPE_SRV: c_int = 33;
#[cfg(cares_dnsrec)]
const ARES_SECTION_ANSWER: c_int = 1;
#[cfg(cares_dnsrec)]
const ARES_RR_SRV_PRIORITY: c_int = ARES_REC_TYPE_SRV * 100 + 2;
#[cfg(cares_dnsrec)]
const ARES_RR_SRV_WEIGHT: c_int = ARES_REC_TYPE_SRV * 100 + 3;
#[cfg(cares_dnsrec)]
const ARES_RR_SRV_PORT: c_int = ARES_REC_TYPE_SRV * 100 + 4;
#[cfg(cares_dnsrec)]
const ARES_RR_SRV_TARGET: c_int = ARES_REC_TYPE_SRV * 100 + 5;

#[cfg(not(cares_dnsrec))]
const NS_C_IN: c_int = 1;
#[cfg(not(cares_dnsrec))]
const NS_T_SRV: c_int = 33;

#[link(name = "cares")]
unsafe extern "C" {
    fn ares_library_init(flags: c_int) -> c_int;
    fn ares_library_cleanup();
    fn ares_init_options(
        channel: *mut *mut ares_channel_t,
        options: *const AresOptions,
        optmask: c_int,
    ) -> c_int;
    fn ares_set_socket_functions(
        channel: *mut ares_channel_t,
        funcs: *const AresSocketFunctions,
        user_data: *mut c_void,
    );
    fn ares_destroy(channel: *mut ares_channel_t);
    fn ares_cancel(channel: *mut ares_channel_t);
    fn ares_getaddrinfo(
        channel: *mut ares_channel_t,
        node: *const c_char,
        service: *const c_char,
        hints: *const AresAddrinfoHints,
        callback: unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut AresAddrinfo),
        arg: *mut c_void,
    );
    fn ares_freeaddrinfo(ai: *mut AresAddrinfo);
    fn ares_gethostbyaddr(
        channel: *mut ares_channel_t,
        addr: *const c_void,
        addrlen: c_int,
        family: c_int,
        callback: unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut CHostent),
        arg: *mut c_void,
    );
    #[cfg(not(cares_dnsrec))]
    fn ares_query(
        channel: *mut ares_channel_t,
        name: *const c_char,
        dnsclass: c_int,
        type_: c_int,
        callback: unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut u8, c_int),
        arg: *mut c_void,
    );
    #[cfg(not(cares_dnsrec))]
    fn ares_parse_srv_reply(abuf: *const u8, alen: c_int, srv_out: *mut *mut AresSrvReply)
    -> c_int;
    #[cfg(not(cares_dnsrec))]
    fn ares_free_data(dataptr: *mut c_void);
    #[cfg(cares_dnsrec)]
    fn ares_query_dnsrec(
        channel: *mut ares_channel_t,
        name: *const c_char,
        dnsclass: c_int,
        type_: c_int,
        callback: unsafe extern "C" fn(*mut c_void, c_int, usize, *const c_void),
        arg: *mut c_void,
        qid: *mut u16,
    ) -> c_int;
    #[cfg(cares_dnsrec)]
    fn ares_dns_record_rr_cnt(dnsrec: *const c_void, sect: c_int) -> usize;
    #[cfg(cares_dnsrec)]
    fn ares_dns_record_rr_get(dnsrec: *mut c_void, sect: c_int, idx: usize) -> *mut c_void;
    #[cfg(cares_dnsrec)]
    fn ares_dns_rr_get_class(rr: *const c_void) -> c_int;
    #[cfg(cares_dnsrec)]
    fn ares_dns_rr_get_type(rr: *const c_void) -> c_int;
    #[cfg(cares_dnsrec)]
    fn ares_dns_rr_get_u16(rr: *const c_void, key: c_int) -> u16;
    #[cfg(cares_dnsrec)]
    fn ares_dns_rr_get_str(rr: *const c_void, key: c_int) -> *const c_char;
    fn ares_fds(channel: *mut ares_channel_t, read_fds: *mut FdSet, write_fds: *mut FdSet)
    -> c_int;
    #[cfg(cares_process_fds)]
    fn ares_process_fds(
        channel: *mut ares_channel_t,
        events: *const AresFdEvents,
        nevents: usize,
        flags: c_uint,
    ) -> c_int;
    #[cfg(not(cares_process_fds))]
    fn ares_process(channel: *mut ares_channel_t, read_fds: *mut FdSet, write_fds: *mut FdSet);
    fn ares_process_fd(
        channel: *mut ares_channel_t,
        read_fd: ares_socket_t,
        write_fd: ares_socket_t,
    );
}

/// Sets the calling thread's `errno`, through the reactor's accessor.
use crate::engine::core::reactor_backend::set_errno;

/// The `dns_resolver` logger, created on first use.
fn dns_log() -> &'static Logger {
    // Creating a logger fails only if its name is taken; "dns_resolver" is registered only
    // here.
    static L: std::sync::LazyLock<std::pin::Pin<Box<Logger>>> = std::sync::LazyLock::new(|| {
        Logger::new("dns_resolver").expect("logger dns_resolver registered once")
    });
    &L
}

/// The error category of c-ares status codes: their name and messages.
pub struct AresErrorCategory;

impl AresErrorCategory {
    /// "C-Ares"
    pub fn name(&self) -> &'static str {
        "C-Ares"
    }

    /// The text of a c-ares error code.
    pub fn message(&self, error: i32) -> &'static str {
        match error {
            /* Server error codes (ARES_ENODATA indicates no relevant answer) */
            ARES_ENODATA => "No data",
            ARES_EFORMERR => "Form error",
            ARES_ESERVFAIL => "Server failure",
            ARES_ENOTFOUND => "Not found",
            ARES_ENOTIMP => "Not implemented",
            ARES_EREFUSED => "Refused",

            /* Locally generated error codes */
            ARES_EBADQUERY => "Bad query",
            ARES_EBADNAME => "Bad name",
            ARES_EBADFAMILY => "Bad family",
            ARES_EBADRESP => "Bad response",
            ARES_ECONNREFUSED => "Connection refused",
            ARES_ETIMEOUT => "Timeout",
            ARES_EOF => "EOF",
            ARES_EFILE => "File error",
            ARES_ENOMEM => "No memory",
            ARES_EDESTRUCTION => "Destruction",
            ARES_EBADSTR => "Bad string",

            /* ares_getnameinfo error codes */
            ARES_EBADFLAGS => "Invalid flags",

            /* ares_getaddrinfo error codes */
            ARES_ENONAME => "No name",
            ARES_EBADHINTS => "Bad hints",

            /* Uninitialized library error code */
            ARES_ENOTINITIALIZED => "Not initialized",

            /* ares_library_init error codes */
            ARES_ELOADIPHLPAPI => "Load PHLPAPI",
            ARES_EADDRGETNETWORKPARAMS => "Get network parameters",

            /* More error codes */
            ARES_ECANCELLED => "Cancelled",
            _ => "Unknown error",
        }
    }
}

/// A failed DNS query: a c-ares status code (see [`AresErrorCategory`]) and its context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsError {
    /// The c-ares error code.
    pub code: i32,
    /// The context of the error.
    pub what: String,
}

impl fmt::Display for DnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.what.is_empty() {
            f.write_str(dns::error_category().message(self.code))
        } else {
            write!(f, "{}: {}", self.what, dns::error_category().message(self.code))
        }
    }
}

impl std::error::Error for DnsError {}

fn check_ares_error(error: c_int) -> Result<(), ExceptionPtr> {
    if error != ARES_SUCCESS {
        return Err(ExceptionPtr::from(DnsError { code: error, what: String::new() }));
    }
    Ok(())
}

struct AresInitializer;

impl AresInitializer {
    fn new() -> Result<Self, ExceptionPtr> {
        // SAFETY: plain library initialization.
        check_ares_error(unsafe { ares_library_init(ARES_LIB_INIT_NONE) })?;
        Ok(AresInitializer)
    }
}

impl Drop for AresInitializer {
    fn drop(&mut self) {
        // SAFETY: paired with ares_library_init.
        unsafe { ares_library_cleanup() };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Type {
    Tcp,
    Udp,
}

struct TcpEntry {
    socket: ConnectedSocket,
    in_: Option<InputStream<u8>>,
    out: Option<OutputStream<u8>>,
    indata: TemporaryBuffer<u8>,
}

impl TcpEntry {
    fn new(s: ConnectedSocket) -> Self {
        TcpEntry { socket: s, in_: None, out: None, indata: TemporaryBuffer::empty_buffer() }
    }
}

struct UdpEntry {
    channel: DatagramChannel,
    in_: Option<Datagram>,
    dst: SocketAddress,
    f: Future<()>,
}

impl UdpEntry {
    fn new(c: DatagramChannel) -> Self {
        UdpEntry { channel: c, in_: None, dst: SocketAddress::default(), f: make_ready_future(()) }
    }
}

/// The kind of socket an entry wraps.
enum SockKind {
    Tcp(TcpEntry),
    Udp(UdpEntry),
}

struct SockEntry {
    kind: SockKind,
    avail: c_int,
    pending: c_int,
    closed: bool,
}

impl SockEntry {
    fn typ(&self) -> Type {
        match self.kind {
            SockKind::Tcp(_) => Type::Tcp,
            SockKind::Udp(_) => Type::Udp,
        }
    }
    fn from_tcp(s: ConnectedSocket) -> Self {
        SockEntry { kind: SockKind::Tcp(TcpEntry::new(s)), avail: 0, pending: 0, closed: false }
    }
    fn from_udp(c: DatagramChannel) -> Self {
        SockEntry { kind: SockKind::Udp(UdpEntry::new(c)), avail: 0, pending: 0, closed: false }
    }
}

// Entries in their own node: callbacks keep a pointer to their entry across calls.
// `NodePtr`, not `Box`, so that rehashing
// the map (which moves the owners) does not invalidate those pointers.
type SocketMap = HashMap<ares_socket_t, NodePtr<SockEntry>>;

/// The state of a [`DnsResolver`]: the c-ares channel, the sockets it opened on the stack,
/// the polling timer and the gate that `close` waits on.
///
/// Aliasing: c-ares calls back into the impl (`do_socket`, `do_recvfrom`…) from inside
/// `ares_getaddrinfo`, `ares_process_fds` and the other `ares_*` calls, through the pointer
/// it was given. The methods that call into c-ares therefore take `this: *mut Self`
/// and hold no reference to the impl across the call; each callback makes the only
/// `&mut Self` while it runs.
pub struct DnsResolverImpl {
    /// The impl's own address with the provenance of its `SharedPtr` node, written once by
    /// `create`; `shared_from_this` takes this one.
    _this: *mut DnsResolverImpl,
    _esft: EnableSharedFromThis,
    _sockets: SocketMap,
    _stack: *mut dyn NetworkStack,
    _channel: *mut ares_channel_t,
    _calls: u64,
    _timeout: Duration,
    _timer: Timer,
    _gate: Gate,
    _closed: bool,
}

/// A query in flight: counts it and arms the polling timer for the first one.
struct DnsCall {
    _i: *mut DnsResolverImpl,
    _c: u64,
}

impl DnsCall {
    /// # Safety
    /// `i` is a live impl and no reference to it is live during the call.
    unsafe fn new(i: *mut DnsResolverImpl) -> Self {
        // SAFETY: guaranteed by the caller.
        unsafe {
            (*i)._calls += 1;
            DnsCall { _i: i, _c: (*i)._calls }
        }
    }
}

impl Drop for DnsCall {
    fn drop(&mut self) {
        // If a query does not immediately complete
        // it might never do so, unless data actually
        // comes back to us and a waiting recv promise
        // is fulfilled.
        // We need to add a timer to do polling at ~timeout
        // ms later, so the ares logic can detect this and
        // tell us we're over.
        // SAFETY: the call lives within a raw-pointer method of the impl, which holds no
        // reference to it here; the timer has a stable address inside the SharedPtr node.
        unsafe {
            let i = self._i;
            if self._c == 1 && (*i)._calls != 0 {
                let t = (*i)._timeout;
                Pin::new_unchecked(&mut (*i)._timer).arm_periodic(t);
            }
        }
    }
}

// Note: cannot use to much here, because fd_sets only handle
// ~1024 fd:s. Set to something below that in case you need to
// debug (maybe)
const SOCKET_OFFSET: ares_socket_t = 1;

/// The impl registered in `ares_set_socket_functions`. The callbacks make the only
/// `&mut DnsResolverImpl` while they run: c-ares calls them from the raw-pointer methods of
/// the impl, which hold no reference to it across the `ares_*` call.
#[inline]
unsafe fn get_impl<'a>(p: *mut c_void) -> &'a mut DnsResolverImpl {
    // SAFETY: c-ares hands back the impl registered in ares_set_socket_functions; see above
    // for exclusivity.
    unsafe { &mut *(p as *mut DnsResolverImpl) }
}

unsafe extern "C" fn cb_socket(
    af: c_int,
    type_: c_int,
    protocol: c_int,
    p: *mut c_void,
) -> ares_socket_t {
    // SAFETY: see get_impl.
    unsafe { get_impl(p) }.do_socket(af, type_, protocol)
}
unsafe extern "C" fn cb_close(s: ares_socket_t, p: *mut c_void) -> c_int {
    // SAFETY: `p` is the impl's root pointer registered in ares_set_socket_functions, and
    // no reference to the impl is live while c-ares runs (see get_impl).
    unsafe { DnsResolverImpl::do_close(p.cast(), s) }
}
unsafe extern "C" fn cb_connect(
    s: ares_socket_t,
    addr: *const sockaddr,
    len: socklen_t,
    p: *mut c_void,
) -> c_int {
    // SAFETY: see get_impl.
    unsafe { get_impl(p) }.do_connect(s, addr, len)
}
unsafe extern "C" fn cb_recvfrom(
    s: ares_socket_t,
    dst: *mut c_void,
    len: usize,
    flags: c_int,
    addr: *mut sockaddr,
    alen: *mut socklen_t,
    p: *mut c_void,
) -> isize {
    // SAFETY: see get_impl.
    unsafe { get_impl(p) }.do_recvfrom(s, dst, len, flags, addr, alen)
}
unsafe extern "C" fn cb_sendv(
    s: ares_socket_t,
    vec: *const iovec,
    len: c_int,
    p: *mut c_void,
) -> isize {
    // SAFETY: as in cb_close.
    unsafe { DnsResolverImpl::do_sendv(p.cast(), s, vec, len) }
}

/// The OS error code of a socket error, which the socket callbacks copy to `errno` for
/// c-ares. The future library reports it as `SystemError` and the POSIX stack as a
/// `std::io::Error` with an OS code; both count.
fn system_error_code(ex: &ExceptionPtr) -> Option<i32> {
    if let Some(e) = ex.downcast_ref::<SystemError>() {
        return Some(e.code());
    }
    ex.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error())
}

/// The impl `sp` points to (null if empty).
#[inline]
fn impl_ptr(sp: &SharedPtr<DnsResolverImpl>) -> *mut DnsResolverImpl {
    sp.get().map_or(std::ptr::null_mut(), |p| p.as_ptr())
}

static CALLBACKS: AresSocketFunctions = AresSocketFunctions {
    asocket: cb_socket,
    aclose: cb_close,
    aconnect: cb_connect,
    arecvfrom: cb_recvfrom,
    asendv: cb_sendv,
};

// The socket functions installed by the impl's destructor before `ares_destroy`, which
// closes every socket through them. The real ones would form a `&mut DnsResolverImpl` while
// the destructor's is live. By then the resolver is closed (its gate too), so `do_close`
// would only mark each entry closed, and the entries are destroyed with `_sockets` right
// after: closing nothing here is the same. The other
// functions are never called during `ares_destroy`; they fail.
unsafe extern "C" fn cb_destroying_socket(
    _: c_int,
    _: c_int,
    _: c_int,
    _: *mut c_void,
) -> ares_socket_t {
    -1
}
unsafe extern "C" fn cb_destroying_close(_: ares_socket_t, _: *mut c_void) -> c_int {
    0
}
unsafe extern "C" fn cb_destroying_connect(
    _: ares_socket_t,
    _: *const sockaddr,
    _: socklen_t,
    _: *mut c_void,
) -> c_int {
    -1
}
unsafe extern "C" fn cb_destroying_recvfrom(
    _: ares_socket_t,
    _: *mut c_void,
    _: usize,
    _: c_int,
    _: *mut sockaddr,
    _: *mut socklen_t,
    _: *mut c_void,
) -> isize {
    -1
}
unsafe extern "C" fn cb_destroying_sendv(
    _: ares_socket_t,
    _: *const iovec,
    _: c_int,
    _: *mut c_void,
) -> isize {
    -1
}

static DESTROYING_CALLBACKS: AresSocketFunctions = AresSocketFunctions {
    asocket: cb_destroying_socket,
    aclose: cb_destroying_close,
    aconnect: cb_destroying_connect,
    arecvfrom: cb_destroying_recvfrom,
    asendv: cb_destroying_sendv,
};

impl DnsResolverImpl {
    /// A resolver state over `stack` with `opts`, inside its `SharedPtr` because c-ares and
    /// the timer keep pointers to it.
    ///
    /// # Errors
    ///
    /// When the c-ares library or channel cannot be initialized, or an option (a domain
    /// with a NUL byte, for one) is invalid.
    pub fn create(
        stack: *mut dyn NetworkStack,
        opts: &DnsResolverOptions,
    ) -> Result<SharedPtr<Self>, ExceptionPtr> {
        let timeout =
            opts.timeout.unwrap_or(Duration::from_millis(5000) /* from ares private */);
        let sp = make_shared_esft(DnsResolverImpl {
            _this: std::ptr::null_mut(),
            _esft: EnableSharedFromThis::new(),
            _sockets: HashMap::new(),
            _stack: stack,
            _channel: std::ptr::null_mut(),
            _calls: 0,
            _timeout: timeout,
            _timer: Timer::new(),
            _gate: Gate::new(),
            _closed: false,
        });
        let this: *mut DnsResolverImpl = impl_ptr(&sp);
        // SAFETY: the impl was just made and nothing else refers to it; `this` is the node's
        // root pointer.
        unsafe { (*this)._this = this };
        // SAFETY: the impl was just made and nothing else refers to it; the timer sits at a
        // stable address inside the SharedPtr node, and is owned by the impl, so the impl
        // outlives the callback, which calls the raw-pointer `poll_sockets`.
        unsafe {
            Pin::new_unchecked(&mut (*this)._timer)
                .set_callback(move || DnsResolverImpl::poll_sockets(this))
        };

        // The c-ares library is initialized once per process; a failed initialization is
        // retried on the next call. A second shard racing here
        // makes a second (reference-counted) ares_library_init, undone when `set` fails.
        static A_INIT: OnceLock<AresInitializer> = OnceLock::new();
        if A_INIT.get().is_none() {
            let _ = A_INIT.set(AresInitializer::new()?);
        }

        // this can "block" ever so slightly, because it will
        // look in resolv.conf etc for query setup. We could
        // do this ourselves, and instead set ares options
        // here, but it seems more error prone (me parsing
        // resolv.conf -> hah!)
        // SAFETY: plain old data; `ares_options a_opts = {}`.
        let mut a_opts: AresOptions = unsafe { std::mem::zeroed() };

        // For now, use the default "fb" query order
        // (set explicitly lest we forget).
        // We only do querying dns server really async.
        // Reading hosts files is doen by c-ares internally
        // and with normal fread calls. Thus they theorectically
        // block. This can potentially be an issue for some application
        // and if so, we need to revisit this. For now, assume
        // it won't block us in any measurable way.
        let mut buf: [c_char; 3] = [b'f' as c_char, b'b' as c_char, 0];
        a_opts.lookups = buf.as_mut_ptr(); // only net
        // Always set the timeout
        a_opts.timeout = timeout.as_millis() as c_int;
        let mut flags = ARES_OPT_LOOKUPS | ARES_OPT_TIMEOUTMS;

        if opts.use_tcp_query == Some(true) {
            a_opts.flags = ARES_FLAG_USEVC | ARES_FLAG_PRIMARY;
            flags |= ARES_OPT_FLAGS;
        }
        let mut addr_tmp: Vec<in_addr> = Vec::new();
        if let Some(servers) = &opts.servers {
            for a in servers {
                if a.in_family() != Family::INET {
                    return Err(ExceptionPtr::from(InvalidArgument(
                        "Servers must be ipv4 addresses".to_string(),
                    )));
                }
                addr_tmp.push(a.to_in_addr()?);
            }
            a_opts.servers = addr_tmp.as_mut_ptr();
            a_opts.nservers = addr_tmp.len() as c_int;
            flags |= ARES_OPT_SERVERS;
        }
        // A domain with a NUL inside is rejected rather than passed truncated.
        let dom_c: Vec<CString> = match &opts.domains {
            Some(d) => d
                .iter()
                .map(|s| CString::new(s.as_bytes()))
                .collect::<Result<_, _>>()
                .map_err(|_| {
                    ExceptionPtr::from(InvalidArgument("domain contains a NUL byte".to_string()))
                })?,
            None => Vec::new(),
        };
        let mut dom_tmp: Vec<*mut c_char> =
            dom_c.iter().map(|s| s.as_ptr() as *mut c_char).collect();
        if opts.domains.is_some() {
            a_opts.domains = dom_tmp.as_mut_ptr();
            a_opts.ndomains = dom_tmp.len() as c_int;
            flags |= ARES_OPT_DOMAINS;
        }
        if let Some(p) = opts.tcp_port {
            a_opts.tcp_port = p;
            flags |= ARES_OPT_TCP_PORT;
        }
        if let Some(p) = opts.udp_port {
            a_opts.udp_port = p;
            flags |= ARES_OPT_UDP_PORT;
        }

        // SAFETY: a_opts and the buffers it points to are valid for the call.
        check_ares_error(unsafe { ares_init_options(&raw mut (*this)._channel, &a_opts, flags) })?;

        // SAFETY: the channel was just created; `this` outlives it (the impl destroys it).
        unsafe { ares_set_socket_functions((*this)._channel, &CALLBACKS, this as *mut c_void) };

        // just in case you need printf-debug.
        // dns_log.set_level(log_level::trace);
        Ok(sp)
    }

    /// The network stack, which outlives the resolvers made on it.
    fn stack(&self) -> *mut dyn NetworkStack {
        self._stack
    }

    fn me(&self) -> SharedPtr<DnsResolverImpl> {
        // SAFETY: every impl is made by make_shared_esft in `create`, which writes `_this`
        // from the shared pointer.
        unsafe { Self::shared_from_this(self._this) }
    }

    /// Resolves `name` to its primary address.
    ///
    /// # Safety
    /// `this` is a live impl on this shard and no reference to it is live during the call.
    pub unsafe fn resolve_name(
        this: *mut Self,
        name: Sstring,
        family: OptFamily,
    ) -> Future<InetAddress> {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::get_host_by_name(this, name, family) }.then(|h: Hostent| {
            match h.addr_list.into_iter().next() {
                Some(a) => make_ready_future::<InetAddress>(a),
                None => make_exception_future(ExceptionPtr::from(DnsError {
                    code: ARES_ENODATA,
                    what: String::new(),
                })),
            }
        })
    }

    /// Resolves a hostname to one or more addresses and aliases.
    ///
    /// # Safety
    /// As for [`DnsResolverImpl::resolve_name`].
    pub unsafe fn get_host_by_name(
        this: *mut Self,
        name: Sstring,
        family: OptFamily,
    ) -> Future<Hostent> {
        struct PromiseWrap {
            p: Promise<Hostent>,
            name: CString,
            sname: Sstring,
        }

        crate::log_debug!(dns_log(), "Query name {} ({})", name, OptFamilyDisplay(&family));

        if family.is_none() {
            let parsed = match InetAddress::parse_numerical(&name) {
                Ok(p) => p,
                Err(e) => return make_exception_future(e),
            };
            if let Some(res) = parsed {
                return make_ready_future::<Hostent>(Hostent {
                    names: vec![name],
                    addr_list: vec![res],
                });
            }
        }

        // A name with a NUL inside is rejected instead of being queried as its truncation.
        let cname = match CString::new(name.as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                return make_exception_future(ExceptionPtr::from(InvalidArgument(
                    "name contains a NUL byte".to_string(),
                )));
            }
        };
        let mut p = Box::new(PromiseWrap { p: Promise::new(), name: cname, sname: name });
        let f = p.p.get_future();

        // SAFETY: guaranteed by the caller.
        let call = unsafe { DnsCall::new(this) };

        let af = match family {
            Some(f) => f as c_int,
            None => AF_UNSPEC as c_int,
        };

        // The following pragma is needed to work around a false-positive warning
        // in Gcc 11 (see https://gcc.gnu.org/bugzilla/show_bug.cgi?id=96003).

        let hints = AresAddrinfoHints {
            ai_flags: ARES_AI_CANONNAME,
            ai_family: af,
            ai_socktype: 0,
            ai_protocol: 0,
        };

        unsafe extern "C" fn cb(
            arg: *mut c_void,
            status: c_int,
            _timeouts: c_int,
            addrinfo: *mut AresAddrinfo,
        ) {
            // we do potentially allocating operations below, so wrap the pointer in a
            // unique here.
            // SAFETY: `arg` is the PromiseWrap leaked below.
            let mut p = unsafe { Box::from_raw(arg as *mut PromiseWrap) };

            match status {
                ARES_SUCCESS => {
                    // SAFETY: c-ares hands a valid addrinfo on success.
                    let h = unsafe { DnsResolverImpl::make_hostent_ai(addrinfo) };
                    p.p.set_value(h);
                }
                _ => {
                    crate::log_debug!(dns_log(), "Query failed: {}", status);
                    p.p.set_exception(ExceptionPtr::from(DnsError {
                        code: status,
                        what: p.sname.to_string(),
                    }));
                }
            }
            // SAFETY: c-ares owns the addrinfo and hands it to us to free.
            unsafe { ares_freeaddrinfo(addrinfo) };
        }
        let name_ptr = p.name.as_ptr();
        // SAFETY: the channel is valid; `p` is leaked to the callback, which frees it.
        // No reference to the impl is live: c-ares may call back into it.
        unsafe {
            ares_getaddrinfo(
                (*this)._channel,
                name_ptr,
                std::ptr::null(),
                &hints,
                cb,
                Box::into_raw(p) as *mut c_void,
            )
        };

        // SAFETY: guaranteed by the caller.
        unsafe { Self::poll_sockets(this) };
        drop(call);

        // SAFETY: the impl outlives its queries (close() waits on them through the gate),
        // and the continuation runs from the reactor, with no other reference live.
        f.finally(move || unsafe { (*this).end_call() })
    }

    /// Resolves an address to one or more addresses and aliases.
    ///
    /// # Safety
    /// As for [`DnsResolverImpl::resolve_name`].
    pub unsafe fn get_host_by_addr(this: *mut Self, addr: InetAddress) -> Future<Hostent> {
        struct PromiseWrap {
            p: Promise<Hostent>,
            addr: InetAddress,
        }

        crate::log_debug!(dns_log(), "Query addr {}", addr);

        let mut p = Box::new(PromiseWrap { p: Promise::new(), addr });
        let f = p.p.get_future();

        // SAFETY: guaranteed by the caller.
        let call = unsafe { DnsCall::new(this) };

        unsafe extern "C" fn cb(
            arg: *mut c_void,
            status: c_int,
            _timeouts: c_int,
            host: *mut CHostent,
        ) {
            // we do potentially allocating operations below, so wrap the pointer in a
            // unique here.
            // SAFETY: `arg` is the PromiseWrap leaked below.
            let mut p = unsafe { Box::from_raw(arg as *mut PromiseWrap) };

            match status {
                ARES_SUCCESS => {
                    // SAFETY: c-ares hands a valid hostent on success.
                    let h = unsafe { DnsResolverImpl::make_hostent_host(&*host) };
                    p.p.set_value(h);
                }
                _ => {
                    crate::log_debug!(dns_log(), "Query failed: {}", status);
                    p.p.set_exception(ExceptionPtr::from(DnsError {
                        code: status,
                        what: p.addr.to_string(),
                    }));
                }
            }
        }
        let (data, size, fam) = (p.addr.data(), p.addr.size(), p.addr.in_family() as c_int);
        // SAFETY: the channel is valid; the address bytes live in `p`, which the callback frees.
        // No reference to the impl is live: c-ares may call back into it.
        unsafe {
            ares_gethostbyaddr(
                (*this)._channel,
                data,
                size as c_int,
                fam,
                cb,
                Box::into_raw(p) as *mut c_void,
            )
        };

        // SAFETY: guaranteed by the caller.
        unsafe { Self::poll_sockets(this) };
        drop(call);

        // SAFETY: see get_host_by_name.
        f.finally(move || unsafe { (*this).end_call() })
    }

    /// Resolves a service in a domain to its SRV records.
    ///
    /// # Safety
    /// As for [`DnsResolverImpl::resolve_name`].
    pub unsafe fn get_srv_records(
        this: *mut Self,
        proto: SrvProto,
        service: &Sstring,
        domain: &Sstring,
    ) -> Future<SrvRecords> {
        let mut p = Box::new(Promise::<SrvRecords>::new());
        let f = p.get_future();

        let query = format!(
            "_{}._{}.{}",
            service,
            if proto == SrvProto::Tcp { "tcp" } else { "udp" },
            domain
        );

        crate::log_debug!(dns_log(), "Query srv {}", query);

        // A NUL inside is rejected rather than queried truncated.
        let cquery = match CString::new(query) {
            Ok(c) => c,
            Err(_) => {
                return make_exception_future(ExceptionPtr::from(InvalidArgument(
                    "query contains a NUL byte".to_string(),
                )));
            }
        };
        // SAFETY: guaranteed by the caller.
        let call = unsafe { DnsCall::new(this) };

        #[cfg(cares_dnsrec)]
        {
            unsafe extern "C" fn cb(
                arg: *mut c_void,
                status: c_int,
                _timeouts: usize,
                dnsrec: *const c_void,
            ) {
                // SAFETY: `arg` is the promise leaked below.
                let mut p = unsafe { Box::from_raw(arg as *mut Promise<SrvRecords>) };
                let mut status = status;
                if status != ARES_SUCCESS {
                    crate::log_debug!(dns_log(), "Query failed: {}", status);
                    p.set_exception(ExceptionPtr::from(DnsError {
                        code: status,
                        what: String::new(),
                    }));
                    return;
                }
                // SAFETY: on success c-ares passes a valid DNS record, alive for the duration of
                // this callback.
                let rr_count = unsafe { ares_dns_record_rr_cnt(dnsrec, ARES_SECTION_ANSWER) };
                let mut replies = SrvRecords::new();
                for i in 0..rr_count {
                    // SAFETY: `dnsrec` is valid (see above) and `i < rr_count`.
                    let rr = unsafe {
                        ares_dns_record_rr_get(dnsrec as *mut c_void, ARES_SECTION_ANSWER, i)
                    };
                    if rr.is_null() {
                        // not likely, but still..
                        status = ARES_EBADRESP;
                        break;
                    }
                    // SAFETY: `rr` is a non-null record of `dnsrec`, which c-ares keeps alive
                    // for the duration of this callback.
                    if unsafe { ares_dns_rr_get_class(rr) } != ARES_CLASS_IN
                        // SAFETY: as above.
                        || unsafe { ares_dns_rr_get_type(rr) } != ARES_REC_TYPE_SRV
                    {
                        continue;
                    }
                    // SAFETY: as above; the key matches the SRV type checked above.
                    let target = unsafe { ares_dns_rr_get_str(rr, ARES_RR_SRV_TARGET) };
                    if target.is_null() {
                        // A record without its target is a bad response.
                        status = ARES_EBADRESP;
                        break;
                    }
                    // SAFETY: a non-null, NUL-terminated string owned by the record, which
                    // outlives this use (it is copied into `Sstring` below).
                    let target = unsafe { CStr::from_ptr(target) };
                    replies.push(SrvRecord {
                        // SAFETY: `rr` is a valid SRV record (see above).
                        priority: unsafe { ares_dns_rr_get_u16(rr, ARES_RR_SRV_PRIORITY) },
                        // SAFETY: as above.
                        weight: unsafe { ares_dns_rr_get_u16(rr, ARES_RR_SRV_WEIGHT) },
                        // SAFETY: as above.
                        port: unsafe { ares_dns_rr_get_u16(rr, ARES_RR_SRV_PORT) },
                        target: Sstring::from(target.to_string_lossy().as_ref()),
                    });
                }
                if status != ARES_SUCCESS {
                    crate::log_debug!(dns_log(), "Parse failed: {}", status);
                    p.set_exception(ExceptionPtr::from(DnsError {
                        code: status,
                        what: String::new(),
                    }));
                    return;
                }
                p.set_value(replies);
            }
            // SAFETY: the channel is valid; `p` is leaked to the callback, which frees it.
            unsafe {
                ares_query_dnsrec(
                    (*this)._channel,
                    cquery.as_ptr(),
                    ARES_CLASS_IN,
                    ARES_REC_TYPE_SRV,
                    cb,
                    Box::into_raw(p) as *mut c_void,
                    std::ptr::null_mut(),
                )
            };
        }
        #[cfg(not(cares_dnsrec))]
        {
            unsafe extern "C" fn cb(
                arg: *mut c_void,
                status: c_int,
                _timeouts: c_int,
                buf: *mut u8,
                len: c_int,
            ) {
                // SAFETY: `arg` is the promise leaked below.
                let mut p = unsafe { Box::from_raw(arg as *mut Promise<SrvRecords>) };
                if status != ARES_SUCCESS {
                    crate::log_debug!(dns_log(), "Query failed: {}", status);
                    p.set_exception(ExceptionPtr::from(DnsError {
                        code: status,
                        what: String::new(),
                    }));
                    return;
                }
                let mut start: *mut AresSrvReply = std::ptr::null_mut();
                // SAFETY: c-ares hands the answer buffer it received.
                let status = unsafe { ares_parse_srv_reply(buf, len, &mut start) };
                if status != ARES_SUCCESS {
                    crate::log_debug!(dns_log(), "Parse failed: {}", status);
                    p.set_exception(ExceptionPtr::from(DnsError {
                        code: status,
                        what: String::new(),
                    }));
                    return;
                }
                // SAFETY: `start` is the list ares_parse_srv_reply made.
                p.set_value(unsafe { DnsResolverImpl::make_srv_records(start) });
                // SAFETY: frees the list ares_parse_srv_reply made.
                unsafe { ares_free_data(start as *mut c_void) };
            }
            // SAFETY: the channel is valid; `p` is leaked to the callback, which frees it.
            unsafe {
                ares_query(
                    (*this)._channel,
                    cquery.as_ptr(),
                    NS_C_IN,
                    NS_T_SRV,
                    cb,
                    Box::into_raw(p) as *mut c_void,
                )
            };
        }

        // SAFETY: guaranteed by the caller.
        unsafe { Self::poll_sockets(this) };
        drop(call);

        // SAFETY: see get_host_by_name.
        f.finally(move || unsafe { (*this).end_call() })
    }

    /// Resolves `addr` to its primary name.
    ///
    /// # Safety
    /// As for [`DnsResolverImpl::resolve_name`].
    pub unsafe fn resolve_addr(this: *mut Self, addr: InetAddress) -> Future<Sstring> {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::get_host_by_addr(this, addr) }.then(|h: Hostent| {
            match h.names.into_iter().next() {
                Some(n) => make_ready_future::<Sstring>(n),
                None => make_exception_future(ExceptionPtr::from(DnsError {
                    code: ARES_ENODATA,
                    what: String::new(),
                })),
            }
        })
    }

    /// Cancels the queries and closes the sockets.
    ///
    /// # Safety
    /// As for [`DnsResolverImpl::resolve_name`].
    pub unsafe fn close(this: *mut Self) -> Future<()> {
        // SAFETY: guaranteed by the caller; each access below is a short borrow, and none is
        // live across ares_cancel, which runs the query callbacks.
        unsafe {
            (*this)._closed = true;
            ares_cancel((*this)._channel);
            crate::log_trace!(dns_log(), "Shutting down {} sockets", (*this)._sockets.len());
            // `for (auto& [fd, e] : _sockets) do_close(fd);`: do_close does not erase, but the
            // keys are copied because `release` may.
            let fds: Vec<ares_socket_t> = (*this)._sockets.keys().copied().collect();
            for fd in fds {
                Self::do_close(this, fd);
            }
            crate::log_trace!(dns_log(), "Closing gate");
            (*this)._gate.close()
        }
    }

    fn end_call(&mut self) {
        self._calls -= 1;
        if self._calls == 0 {
            // SAFETY: the timer sits at a stable address inside the SharedPtr node.
            unsafe { Pin::new_unchecked(&mut self._timer) }.cancel();
        }
    }

    /// # Safety
    /// As for [`DnsResolverImpl::resolve_name`]: c-ares calls back into the impl from
    /// `ares_process_fds`/`ares_process_fd`, so no reference to it is held across them.
    unsafe fn poll_sockets(this: *mut Self) {
        crate::log_trace!(dns_log(), "Poll sockets");

        let mut processed = false;

        loop {
            // Retrieve the set of file descriptors that the library wants us to monitor.
            let mut readers = FdSet::zero();
            let mut writers = FdSet::zero();

            // SAFETY: the channel is valid.
            let nr_fds = unsafe { ares_fds((*this)._channel, &mut readers, &mut writers) };
            crate::log_trace!(dns_log(), "ares_fds: {}", nr_fds);
            if nr_fds == 0 {
                break;
            }

            // avoid allocations on every poll. the ares_process_fds will not reenter this,
            // as any read/write/close callbacks will either complete or create a pending
            // future which cannot execute until the call returns.
            // I.e. we cannot get here while this call is running, so safe to make this
            // thread static
            #[cfg(cares_process_fds)]
            thread_local! {
                static EVENTS: UnsafeCell<[AresFdEvents; FdSet::SETSIZE]> =
                    const { UnsafeCell::new([AresFdEvents { fd: 0, events: 0 }; FdSet::SETSIZE]) };
            }
            let mut processed_fds: usize = 0;

            // SAFETY: the caller guarantees `this` is live; the shared borrow of the map ends
            // with the loop, before c-ares is called, and no `&mut` entry is live (callbacks run
            // only inside the ares_* calls).
            for (&fd, e) in unsafe { (*this)._sockets.iter() } {
                // SAFETY: as above.
                let e = unsafe { e.get() };
                let read_monitor = readers.is_set(fd);
                let write_monitor = writers.is_set(fd);
                let read_avail = e.avail & POLLIN != 0;
                let write_avail = e.avail & POLLOUT != 0;

                crate::log_trace!(
                    dns_log(),
                    "fd {} {}{}/{}{}",
                    fd,
                    if read_monitor { "r" } else { "" },
                    if write_monitor { "w" } else { "" },
                    if read_avail { "r" } else { "" },
                    if write_avail { "w" } else { "" }
                );

                // #2641 - don't do callbacks per fd, instead use
                // ares_process or ares_process_fds if available.
                // Use ares_process_fds if possible, since this
                // is the recommended api, and will avoid a bunch
                // of allocation etc.
                // We are still tied to FD_SET bounds in the use of ares_fds.
                // Once we no longer support pre-1.34 c-ares, move to use
                // socket state callbacks.

                // clear fd state
                #[cfg(cares_process_fds)]
                EVENTS.with(|ev| {
                    // SAFETY: as the comment above says, nothing reenters this function while
                    // it runs, so this is the only reference to the array.
                    let ev = unsafe { &mut *ev.get() };
                    ev[processed_fds] = AresFdEvents::default();
                    // Update read/write state based on our poll info
                    if read_monitor && read_avail {
                        ev[processed_fds].fd = fd;
                        ev[processed_fds].events |= ARES_FD_EVENT_READ;
                    }
                    if write_monitor && write_avail {
                        ev[processed_fds].fd = fd;
                        ev[processed_fds].events |= ARES_FD_EVENT_WRITE;
                    }
                    if ev[processed_fds].events != 0 {
                        processed_fds += 1;
                    }
                });
                #[cfg(not(cares_process_fds))]
                {
                    readers.clr(fd);
                    writers.clr(fd);
                    // Update read/write state based on our poll info
                    if read_monitor && read_avail {
                        readers.set(fd);
                    }
                    if write_monitor && write_avail {
                        writers.set(fd);
                    }
                    if readers.is_set(fd) || writers.is_set(fd) {
                        processed_fds += 1;
                    }
                }
            }
            // no sockets of interest had polling values. done.
            if processed_fds == 0 {
                break;
            }
            // call fd processing. this will clean up and close sockets as well.
            #[cfg(cares_process_fds)]
            EVENTS.with(|ev| {
                // SAFETY: see above; the callbacks of ares_process_fds do not touch EVENTS.
                let ev = unsafe { &*ev.get() };
                // SAFETY: the channel is valid; `ev` holds processed_fds events.
                unsafe {
                    ares_process_fds(
                        (*this)._channel,
                        ev.as_ptr(),
                        processed_fds,
                        ARES_PROCESS_FLAG_NONE,
                    )
                };
            });
            #[cfg(not(cares_process_fds))]
            // SAFETY: the channel is valid.
            unsafe {
                ares_process((*this)._channel, &mut readers, &mut writers)
            };
            processed = true;
        }
        // even if we did not process anything, do a single callback to maybe close
        // broken sockets.
        if !processed {
            // SAFETY: the channel is valid.
            unsafe { ares_process_fd((*this)._channel, ARES_SOCKET_BAD, ARES_SOCKET_BAD) };
        }
    }

    /// # Safety
    /// `start` must be a list made by c-ares (or null).
    #[allow(dead_code)]
    unsafe fn make_srv_records(start: *mut AresSrvReply) -> SrvRecords {
        let mut records = SrvRecords::new();
        let mut reply = start;
        while !reply.is_null() {
            // SAFETY: guaranteed by the caller.
            let r = unsafe { &*reply };
            // SAFETY: `host` is a C string owned by the reply.
            let host = unsafe { CStr::from_ptr(r.host) };
            records.push(SrvRecord {
                priority: r.priority,
                weight: r.weight,
                port: r.port,
                target: Sstring::from(host.to_string_lossy().as_ref()),
            });
            reply = r.next;
        }
        records
    }

    /// # Safety
    /// `ai` must be an addrinfo made by c-ares (or null).
    unsafe fn make_hostent_ai(ai: *const AresAddrinfo) -> Hostent {
        let mut e = Hostent::default();
        if ai.is_null() {
            return e;
        }
        // SAFETY: the caller guarantees `ai` is a non-null (checked above) addrinfo made by
        // c-ares, so its strings are NUL-terminated and its node lists end with null `next`.
        unsafe {
            let ai = &*ai;
            let cstr =
                |p: *const c_char| Sstring::from(CStr::from_ptr(p).to_string_lossy().as_ref());
            if !ai.cnames.is_null() {
                e.names.push(cstr((*ai.cnames).name));
            } else {
                e.names.push(cstr(ai.name));
            }
            let mut cname = ai.cnames;
            while !cname.is_null() {
                if !(*cname).alias.is_null() {
                    e.names.push(cstr((*cname).alias));
                }
                cname = (*cname).next;
            }
            let mut node = ai.nodes;
            while !node.is_null() {
                match (*node).ai_family {
                    x if x == AF_INET as c_int => {
                        let a = &*((*node).ai_addr as *const sockaddr_in);
                        e.addr_list.push(InetAddress::from_in_addr(a.sin_addr));
                    }
                    x if x == AF_INET6 as c_int => {
                        let a = &*((*node).ai_addr as *const sockaddr_in6);
                        e.addr_list.push(InetAddress::from_in6_addr(
                            a.sin6_addr,
                            InetAddress::INVALID_SCOPE,
                        ));
                    }
                    _ => {}
                }
                node = (*node).ai_next;
            }
        }

        if let (Some(n), Some(a)) = (e.names.first(), e.addr_list.first()) {
            crate::log_debug!(dns_log(), "Query success: {}/{}", n, a);
        }

        e
    }

    /// # Safety
    /// `host` must be a hostent made by c-ares.
    unsafe fn make_hostent_host(host: &CHostent) -> Hostent {
        let mut e = Hostent::default();
        // SAFETY: the caller guarantees `host` is a hostent made by c-ares: its name is
        // NUL-terminated and its alias and address lists are null-terminated arrays.
        unsafe {
            let cstr =
                |p: *const c_char| Sstring::from(CStr::from_ptr(p).to_string_lossy().as_ref());
            e.names.push(cstr(host.h_name));
            let mut np = host.h_aliases;
            while !(*np).is_null() {
                e.names.push(cstr(*np));
                np = np.add(1);
            }
            let mut p = host.h_addr_list;
            while !(*p).is_null() {
                match host.h_addrtype {
                    x if x == AF_INET as c_int => {
                        assert!(host.h_length as usize >= std::mem::size_of::<in_addr>());
                        e.addr_list.push(InetAddress::from_in_addr(
                            (*p as *const in_addr).read_unaligned(),
                        ));
                    }
                    x if x == AF_INET6 as c_int => {
                        assert!(host.h_length as usize >= std::mem::size_of::<in6_addr>());
                        e.addr_list.push(InetAddress::from_in6_addr(
                            (*p as *const in6_addr).read_unaligned(),
                            InetAddress::INVALID_SCOPE,
                        ));
                    }
                    _ => {}
                }
                p = p.add(1);
            }
        }

        if let (Some(n), Some(a)) = (e.names.first(), e.addr_list.first()) {
            crate::log_debug!(dns_log(), "Query success: {}/{}", n, a);
        }

        e
    }

    // We need to partially ref-count our socket entries
    // when we have pending reads/writes, so we don't erase the
    // entry to early.
    //
    // The entry is touched through its node pointer only, so that the callers' pointers into
    // it (`ep`) stay valid; callers do not hold a reference into the entry across the call.
    fn use_(&mut self, fd: ares_socket_t) -> Result<(), ExceptionPtr> {
        self._gate.enter()?;
        let e = self
            ._sockets
            .get(&fd)
            .ok_or_else(|| ExceptionPtr::from(OutOfRange(format!("dns socket {fd} not found"))))?;
        // SAFETY: no reference into the entry is live (see above).
        unsafe { (*e.as_ptr()).pending += 1 };
        Ok(())
    }

    fn release(&mut self, fd: ares_socket_t) {
        let Some(e) = self._sockets.get(&fd) else {
            // An unknown descriptor: still leave the gate, so that `close()` does not wait
            // forever for a use that will never be released.
            self._gate.leave();
            return;
        };
        let ep = e.as_ptr();
        // SAFETY: no reference into the entry is live (see `use_`).
        let pending = unsafe {
            (*ep).pending -= 1;
            (*ep).pending
        };
        crate::log_trace!(dns_log(), "Release socket {} -> {}", fd, pending);
        if pending < 0 {
            self._sockets.remove(&fd);
            crate::log_trace!(dns_log(), "Released socket {}", fd);
        }
        self._gate.leave();
    }

    fn do_socket(&mut self, _af: c_int, type_: c_int, _protocol: c_int) -> ares_socket_t {
        if self._closed {
            return -1;
        }
        let fd = self.next_fd();
        match type_ {
            SOCK_STREAM => {
                self._sockets
                    .entry(fd)
                    .or_insert_with(|| NodePtr::new(SockEntry::from_tcp(ConnectedSocket::new())));
                crate::log_trace!(dns_log(), "Created tcp socket {}", fd);
            }
            SOCK_DGRAM => {
                // SAFETY: the network stack outlives the resolvers made on it.
                let Ok(ch) = unsafe { &mut *self.stack() }.make_unbound_datagram_channel(AF_INET)
                else {
                    return -1;
                };
                self._sockets.entry(fd).or_insert_with(|| NodePtr::new(SockEntry::from_udp(ch)));
                crate::log_trace!(dns_log(), "Created udp socket {}", fd);
            }
            _ => return -1,
        }
        fd
    }

    /// Through the impl's root pointer: without an output stream the `finally` below runs in
    /// line on a ready future, and its `release` forms a `&mut` to the impl and may free the
    /// entry, so no reference to the impl or into the entry is live across it.
    ///
    /// # Safety
    /// `this` is the impl's root pointer, with no reference to it live (c-ares callbacks and
    /// `close`, on the impl's shard).
    unsafe fn do_close(this: *mut Self, fd: ares_socket_t) -> c_int {
        crate::log_trace!(dns_log(), "Close socket {}", fd);
        // SAFETY: guaranteed by the caller; a short borrow of the map.
        let Some(ep) = (unsafe { (*this)._sockets.get(&fd) }).map(|e| e.as_ptr()) else {
            return -1;
        };

        // Mark as closed.
        // SAFETY: the node is live; no reference into it is live.
        if unsafe { std::mem::replace(&mut (*ep).closed, true) } {
            return 0;
        }

        // When the gate is closed the callback returns 0, with nothing to release.
        // SAFETY: a short borrow of the gate.
        if unsafe { (*this)._gate.enter() }.is_err() {
            return 0;
        }
        // "leave" is done in release(fd)

        // SAFETY: the entry stays in its node until release() drops it, and no reference into
        // it is live; the borrow of the variant ends where it becomes `tp` (TCP) or before
        // `release` (UDP), so nothing borrowed from it is live across `finally`.
        match unsafe { &mut (*ep).kind } {
            SockKind::Tcp(tcp) => {
                let tp: *mut TcpEntry = tcp;
                // SAFETY: short reads through the node pointers (see above).
                let (pending, has_in, has_out) =
                    unsafe { ((*ep).pending, (*tp).in_.is_some(), (*tp).out.is_some()) };
                crate::log_trace!(dns_log(), "Close tcp socket {}, {} pending", fd, pending);
                let mut f = make_ready_future(());
                if has_in {
                    // SAFETY: a short access through the node pointer.
                    unsafe { (*tp).socket.shutdown_input() };
                    crate::log_trace!(dns_log(), "Closed tcp socket {} input", fd);
                }
                if has_out {
                    f = f
                        .then(move |()| {
                            // SAFETY: the entry stays in its node until the release() below,
                            // which runs after this continuation; a short access.
                            match unsafe { (*tp).out.as_mut() } {
                                Some(o) => o.close(),
                                None => make_ready_future(()),
                            }
                        })
                        .then(move |()| {
                            crate::log_trace!(dns_log(), "Closed tcp socket {} output", fd);
                            make_ready_future(())
                        });
                }
                // SAFETY: guaranteed by the caller; a short borrow.
                let me = unsafe { (*this).me() };
                let _ = f.finally(move || {
                    // SAFETY: from the reactor on this shard, or in line here, with no other
                    // reference to the impl or into the entry live either way.
                    unsafe { (*impl_ptr(&me)).release(fd) };
                });
            }
            SockKind::Udp(udp) => {
                udp.channel.shutdown_input();
                udp.channel.shutdown_output();
                // SAFETY: guaranteed by the caller; the borrow of the entry has ended.
                unsafe { (*this).release(fd) };
            }
        }
        0
    }

    fn sock_addr(
        &self,
        addr: *const sockaddr,
        _len: socklen_t,
    ) -> Result<SocketAddress, ExceptionPtr> {
        // SAFETY: c-ares passes a valid sockaddr.
        if unsafe { (*addr).sa_family } != AF_INET {
            return Err(ExceptionPtr::from(InvalidArgument("No ipv6 yet".to_string())));
        }
        // SAFETY: an AF_INET sockaddr is a sockaddr_in.
        let in_ = unsafe { &*(addr as *const sockaddr_in) };
        Ok(SocketAddress::from_sockaddr_in(in_))
    }

    fn do_connect(&mut self, fd: ares_socket_t, addr: *const sockaddr, len: socklen_t) -> c_int {
        if self._closed {
            return -1;
        }
        let r: Result<c_int, ExceptionPtr> = (|| {
            let ep: *mut SockEntry = self.get_socket_entry(fd)?;
            let sa = self.sock_addr(addr, len)?;
            // SAFETY: the entry stays in its node while it is used; `use_` and `release` touch it
            // only through the node pointer, and `e` is re-derived after them.
            let e = unsafe { &mut *ep };

            crate::log_trace!(dns_log(), "Connect {}({:?})->{}", fd, e.typ(), sa);

            assert!(e.avail == 0);

            e.avail = POLLOUT | POLLIN; // until we know otherwise

            match &mut e.kind {
                SockKind::Tcp(tcp) => {
                    // SAFETY: the network stack outlives the resolvers made on it.
                    let f = unsafe { &mut *self.stack() }.connect(
                        sa,
                        SocketAddress::default(),
                        Transport::TCP,
                    );
                    if !f.available() {
                        crate::log_trace!(dns_log(), "Connection pending: {}", fd);
                        e.avail = 0;
                        self.use_(fd)?;
                        let me = self.me();
                        // FIXME: future is discarded
                        let _ = f.then_wrapped(move |f: Future<ConnectedSocket>| {
                            // SAFETY: `use_` keeps the entry in the map until `release`.
                            let e = unsafe { &mut *ep };
                            match f.get() {
                                Ok(s) => {
                                    if let SockKind::Tcp(t) = &mut e.kind {
                                        t.socket = s;
                                    }
                                    crate::log_trace!(dns_log(), "Connection complete: {}", fd);
                                }
                                Err(ex) => {
                                    crate::log_debug!(dns_log(), "Connect {} failed: {}", fd, ex)
                                }
                            }
                            e.avail = POLLOUT | POLLIN;
                            // SAFETY: the continuation runs from the reactor on this shard; `e` is
                            // not used past this point and no reference to the impl is live.
                            unsafe { DnsResolverImpl::poll_sockets(impl_ptr(&me)) };
                            // SAFETY: as above.
                            unsafe { (*impl_ptr(&me)).release(fd) };
                            make_ready_future(())
                        });
                        set_errno(EWOULDBLOCK);
                        return Ok(-1);
                    }
                    tcp.socket = f.get()?;
                }
                SockKind::Udp(udp) => {
                    // we do not have udp connect, so just keep
                    // track of the destination
                    udp.dst = sa;
                }
            }
            Ok(0)
        })();
        r.unwrap_or(-1)
    }

    fn do_recvfrom(
        &mut self,
        fd: ares_socket_t,
        dst: *mut c_void,
        len: usize,
        _flags: c_int,
        from: *mut sockaddr,
        from_len: *mut socklen_t,
    ) -> isize {
        if self._closed {
            return -1;
        }
        let mut len = len;
        let r: Result<isize, ExceptionPtr> = (|| {
            let ep: *mut SockEntry = self.get_socket_entry(fd)?;
            // SAFETY: the entry stays in its node while it is used; `use_` and `release` touch it
            // only through the node pointer, and `e` is re-derived after them.
            let e = unsafe { &mut *ep };
            crate::log_trace!(dns_log(), "Read {}({:?})", fd, e.typ());
            // check if we're already reading.
            if e.avail & POLLIN == 0 {
                crate::log_trace!(dns_log(), "Read already pending {}", fd);
                set_errno(EWOULDBLOCK);
                return Ok(-1);
            }
            loop {
                match &mut e.kind {
                    SockKind::Tcp(tcp) => {
                        if !tcp.indata.empty() {
                            crate::log_trace!(
                                dns_log(),
                                "Read {}. {} bytes available",
                                fd,
                                tcp.indata.size()
                            );
                            len = std::cmp::min(len, tcp.indata.size());
                            // SAFETY: `dst` has room for `len` bytes (c-ares' buffer).
                            unsafe {
                                std::ptr::copy_nonoverlapping(tcp.indata.get(), dst as *mut u8, len)
                            };
                            tcp.indata.trim_front(len);
                            return Ok(len as isize);
                        }
                        if !tcp.socket.is_valid() {
                            set_errno(ENOTCONN);
                            return Ok(-1);
                        }
                        if tcp.in_.is_none() {
                            tcp.in_ =
                                Some(tcp.socket.input(ConnectedSocketInputStreamConfig::default()));
                        }
                        // SAFETY: just set.
                        let f = unsafe { tcp.in_.as_mut().unwrap_unchecked() }.read_up_to(len);
                        if !f.available() {
                            crate::log_trace!(dns_log(), "Read {}: data unavailable", fd);
                            e.avail &= !POLLIN;
                            self.use_(fd)?;
                            let me = self.me();
                            // FIXME: future is discarded
                            let _ = f.then_wrapped(move |f: Future<TemporaryBuffer<u8>>| {
                                // SAFETY: `use_` keeps the entry in the map until `release`.
                                let e = unsafe { &mut *ep };
                                match f.get() {
                                    Ok(buf) => {
                                        crate::log_trace!(
                                            dns_log(),
                                            "Read {} -> {} bytes",
                                            fd,
                                            buf.size()
                                        );
                                        if let SockKind::Tcp(t) = &mut e.kind {
                                            t.indata = buf;
                                        }
                                    }
                                    Err(ex) => {
                                        crate::log_debug!(dns_log(), "Read {} failed: {}", fd, ex)
                                    }
                                }
                                e.avail |= POLLIN; // always reset state
                                // SAFETY: the continuation runs from the reactor on this shard; `e` is
                                // not used past this point and no reference to the impl is live.
                                unsafe { DnsResolverImpl::poll_sockets(impl_ptr(&me)) };
                                // SAFETY: as above.
                                unsafe { (*impl_ptr(&me)).release(fd) };
                                make_ready_future(())
                            });
                            set_errno(EWOULDBLOCK);
                            return Ok(-1);
                        }

                        match f.get() {
                            Ok(b) => {
                                tcp.indata = b;
                                continue; // loop will take care of data
                            }
                            Err(ex) => {
                                if let Some(c) = system_error_code(&ex) {
                                    set_errno(c);
                                }
                                return Ok(-1);
                            }
                        }
                    }
                    SockKind::Udp(udp) => {
                        if let Some(d) = udp.in_.as_mut() {
                            let src = d.get_src();
                            let p = d.get_data();

                            crate::log_trace!(
                                dns_log(),
                                "Read {}. {} bytes available from {}",
                                fd,
                                p.len(),
                                src
                            );

                            if !from.is_null() {
                                // SAFETY: c-ares passes room for a sockaddr.
                                unsafe { *from = *src.as_posix_sockaddr() };
                                if !from_len.is_null() {
                                    // TODO: ipvv6
                                    // SAFETY: as above.
                                    unsafe {
                                        *from_len = std::mem::size_of::<sockaddr_in>() as socklen_t
                                    };
                                }
                            }

                            len = std::cmp::min(len, p.len() as usize);
                            let mut rem = len;
                            let mut out = dst as *mut u8;
                            for f in p.fragments().iter() {
                                let n = std::cmp::min(rem, f.size);
                                // SAFETY: `dst` has room for `len` bytes; `f` holds f.size bytes.
                                unsafe {
                                    std::ptr::copy_nonoverlapping(f.base, out, n);
                                    out = out.add(n);
                                }
                                rem -= n;
                            }
                            if p.len() as usize == len {
                                udp.in_ = None;
                            } else {
                                p.trim_front(len);
                            }
                            return Ok(len as isize);
                        }
                        let f = udp.channel.receive();
                        if !f.available() {
                            e.avail &= !POLLIN;
                            self.use_(fd)?;
                            crate::log_trace!(dns_log(), "Read {}: data unavailable", fd);
                            let me = self.me();
                            // FIXME: future is discarded
                            let _ = f.then_wrapped(move |f: Future<Datagram>| {
                                // SAFETY: `use_` keeps the entry in the map until `release`.
                                let e = unsafe { &mut *ep };
                                match f.get() {
                                    Ok(mut d) => {
                                        crate::log_trace!(
                                            dns_log(),
                                            "Read {} -> {} bytes",
                                            fd,
                                            d.get_data().len()
                                        );
                                        if let SockKind::Udp(u) = &mut e.kind {
                                            u.in_ = Some(d);
                                        }
                                        e.avail |= POLLIN;
                                    }
                                    Err(ex) => {
                                        crate::log_debug!(dns_log(), "Read {} failed: {}", fd, ex)
                                    }
                                }
                                // SAFETY: the continuation runs from the reactor on this shard; `e` is
                                // not used past this point and no reference to the impl is live.
                                unsafe { DnsResolverImpl::poll_sockets(impl_ptr(&me)) };
                                // SAFETY: as above.
                                unsafe { (*impl_ptr(&me)).release(fd) };
                                make_ready_future(())
                            });
                            set_errno(EWOULDBLOCK);
                            return Ok(-1);
                        }

                        match f.get() {
                            Ok(d) => {
                                udp.in_ = Some(d);
                                continue; // loop will take care of data
                            }
                            Err(ex) => {
                                if let Some(c) = system_error_code(&ex) {
                                    set_errno(c);
                                }
                                return Ok(-1);
                            }
                        }
                    }
                }
            }
        })();
        r.unwrap_or(-1)
    }

    /// Through the impl's root pointer: the UDP sends are chained with `finally`, which runs
    /// in line when the previous send is done (the first send of every socket), and its
    /// `release` then forms a `&mut` to the impl and writes the entry; so no reference to
    /// the impl or into the entry is live across the chain, only short accesses.
    ///
    /// # Safety
    /// `this` is the impl's root pointer, with no reference to it live (a c-ares callback on
    /// the impl's shard); c-ares passes `len` iovecs at `vec`.
    unsafe fn do_sendv(this: *mut Self, fd: ares_socket_t, vec: *const iovec, len: c_int) -> isize {
        // SAFETY: guaranteed by the caller; a short read.
        if unsafe { (*this)._closed } {
            return -1;
        }
        // The accesses to the impl (`this`) and to the entry (`ep`, `tp`, `up`) below are all
        // short ones: the entry stays in its node while `use_` holds it, and `use_` and
        // `release` touch it only through the node pointer.
        let r: Result<isize, ExceptionPtr> = (|| {
            // SAFETY: guaranteed by the caller; a short borrow of the map.
            let ep: *mut SockEntry = unsafe { &(*this)._sockets }
                .get(&fd)
                .ok_or_else(|| {
                    ExceptionPtr::from(OutOfRange(format!("dns socket {fd} not found")))
                })?
                .as_ptr();
            // SAFETY: the node is live; short reads (see above).
            let (typ, avail) = unsafe { ((*ep).typ(), (*ep).avail) };
            crate::log_trace!(dns_log(), "Send {}({:?})", fd, typ);

            // Assume we will be able to send data eventually very soon
            // and just assume that unless we get immediate
            // failures, we'll be ok. If we're not, the
            // timeout logic will have to handle the problem.
            //
            // This saves us on two accounts:
            // 1.) c-ares does not handle EWOULDBLOCK for
            //     udp sockets. Must pretend to finish
            //     immediately there anyway
            // 2.) Doing so for tcp writes saves us having to
            //     match iovec->packet fragments. Downside is we
            //     have to copy the data, but we pretty much
            //     have to anyway, since we could otherwise
            //     get a query time out while we're sending
            //     with zero-copy and suddenly have freed
            //     memory in packets. Bad.

            // check if we're already writing.
            if typ == Type::Tcp && avail & POLLOUT == 0 {
                crate::log_trace!(dns_log(), "Send already pending {}", fd);
                set_errno(EWOULDBLOCK);
                return Ok(-1);
            }

            // Only a TCP entry can lack its socket.
            // SAFETY: a short read (see above).
            if let SockKind::Tcp(t) = unsafe { &(*ep).kind }
                && !t.socket.is_valid()
            {
                set_errno(ENOTCONN);
                return Ok(-1);
            }

            let mut p = Packet::new();
            p.reserve(len);
            for i in 0..len as usize {
                // SAFETY: c-ares passes `len` iovecs (guaranteed by the caller).
                let v = unsafe { *vec.add(i) };
                p = Packet::append_copy(
                    p,
                    Fragment { base: v.iov_base as *mut u8, size: v.iov_len },
                );
            }

            let bytes = p.len() as isize;

            // SAFETY: guaranteed by the caller; a short borrow.
            unsafe { (*this).use_(fd)? };

            // SAFETY: a short borrow of the variant, which ends where it becomes `tp` or `up`.
            let f: Future<()> = match unsafe { &mut (*ep).kind } {
                SockKind::Tcp(tcp) => {
                    let tp: *mut TcpEntry = tcp;
                    // SAFETY: short accesses through the node pointer (see above); `out` is
                    // set just before it is unwrapped.
                    unsafe {
                        if (*tp).out.is_none() {
                            (*tp).out = Some((*tp).socket.output(0));
                        }
                        (*tp).out.as_mut().unwrap_unchecked().write_packet(p)
                    }
                }
                SockKind::Udp(udp) => {
                    // always chain UDP sends
                    let up: *mut UdpEntry = udp;
                    // SAFETY: guaranteed by the caller; a short borrow.
                    let me = unsafe { (*this).me() };
                    // SAFETY: a short access through the node pointer (see above).
                    let prev = unsafe { std::mem::replace(&mut (*up).f, make_ready_future(())) };
                    let chained = prev
                        .finally(move || {
                            // SAFETY: `use_` keeps the entry in the map until `release`, which
                            // runs after this; no other reference into it is live.
                            let u = unsafe { &mut *up };
                            let dst = u.dst;
                            u.channel.send(&dst, p)
                        })
                        .finally(move || {
                            // SAFETY: from the reactor on this shard, or in line here, with no
                            // other reference to the impl or into the entry live either way.
                            unsafe { (*impl_ptr(&me)).release(fd) };
                        });
                    // SAFETY: the entry is still in its node, since this send's `release` only
                    // undoes its own `use_`; this and the accesses to `(*up).f` below are short
                    // place accesses with no other reference into the entry live.
                    unsafe { (*up).f = chained };

                    // SAFETY: as above.
                    if unsafe { (*up).f.available() } {
                        // if we have a fast-fail, give error.
                        // SAFETY: as above.
                        if unsafe { (*up).f.failed() } {
                            // SAFETY: as above.
                            let failed =
                                unsafe { std::mem::replace(&mut (*up).f, make_ready_future(())) };
                            if let Err(ex) = failed.get()
                                && let Some(c) = system_error_code(&ex)
                            {
                                set_errno(c);
                            }
                            return Ok(-1);
                        }
                    } else {
                        // ensure that no exception from channel.send is left uncaught
                        // SAFETY: as above.
                        let pending =
                            unsafe { std::mem::replace(&mut (*up).f, make_ready_future(())) };
                        let handled = pending.handle_exception(|ex: ExceptionPtr| {
                            if system_error_code(&ex).is_some() {
                                crate::log_warn!(dns_log(), "UDP send exception: {}", ex);
                                make_ready_future(())
                            } else {
                                make_exception_future(ex)
                            }
                        });
                        // SAFETY: as above.
                        unsafe { (*up).f = handled };
                    }
                    // c-ares does _not_ use non-blocking retry for udp sockets. We just pretend
                    // all is fine even though we have no idea. Barring stack/adapter failure it
                    // is close to the same guarantee a "normal" message send would have anyway.
                    return Ok(bytes);
                }
            };

            if !f.available() {
                crate::log_trace!(dns_log(), "Send {} unavailable.", fd);
                // SAFETY: a short access through the node pointer (see above).
                unsafe { (*ep).avail &= !POLLOUT };
                // SAFETY: guaranteed by the caller; a short borrow.
                let me = unsafe { (*this).me() };
                // FIXME: future is discarded
                let _ = f.then_wrapped(move |f: Future<()>| {
                    match f.get() {
                        Ok(()) => {
                            crate::log_trace!(dns_log(), "Send {}. {} bytes sent.", fd, bytes)
                        }
                        Err(ex) => crate::log_debug!(dns_log(), "Send {} failed: {}", fd, ex),
                    }
                    // SAFETY: `use_` keeps the entry in the map until `release`; the
                    // continuation runs from the reactor on this shard with no reference to
                    // the impl or into the entry live, and each access is a short one.
                    unsafe {
                        (*ep).avail |= POLLOUT;
                        DnsResolverImpl::poll_sockets(impl_ptr(&me));
                        (*impl_ptr(&me)).release(fd);
                    }
                    make_ready_future(())
                });

                // For tcp we also pretend we're done, to make sure we don't have to deal with
                // matching sent data
                return Ok(bytes);
            }

            // SAFETY: guaranteed by the caller; a short borrow.
            unsafe { (*this).release(fd) };

            if f.failed() {
                if let Err(ex) = f.get()
                    && let Some(c) = system_error_code(&ex)
                {
                    set_errno(c);
                }
                return Ok(-1);
            }

            Ok(bytes)
        })();
        r.unwrap_or(-1)
    }

    fn next_fd(&self) -> ares_socket_t {
        let mut fd = self._sockets.len() as ares_socket_t + SOCKET_OFFSET;
        while self._sockets.contains_key(&fd) {
            fd += 1;
        }
        fd
    }

    fn get_socket_entry(&self, fd: ares_socket_t) -> Result<*mut SockEntry, ExceptionPtr> {
        let e = self
            ._sockets
            .get(&fd)
            .ok_or_else(|| ExceptionPtr::from(OutOfRange(format!("dns socket {fd} not found"))))?;
        let ep = e.as_ptr();
        // SAFETY: the node is live; no reference into it is live.
        if unsafe { (*ep).closed } {
            return Err(ExceptionPtr::from(RuntimeError("Socket closed".to_string())));
        }
        Ok(ep)
    }
}

crate::shared_from_this_impl!(DnsResolverImpl { _esft });

impl Drop for DnsResolverImpl {
    fn drop(&mut self) {
        // SAFETY: the impl is being dropped in place inside its SharedPtr node; the timer
        // has not moved since it was armed.
        unsafe { Pin::new_unchecked(&mut self._timer) }.cancel();
        if !self._channel.is_null() {
            // SAFETY: the channel was created by ares_init_options and is destroyed once. The
            // socket functions are switched first to ones that do not reach the impl (see
            // `DESTROYING_CALLBACKS`); the query callbacks c-ares runs with ARES_EDESTRUCTION
            // only touch their own promise.
            unsafe {
                ares_set_socket_functions(
                    self._channel,
                    &DESTROYING_CALLBACKS,
                    std::ptr::null_mut(),
                );
                ares_destroy(self._channel);
            }
        }
    }
}

impl DnsResolver {
    /// A resolver with the default options on the engine's network stack.
    pub fn new() -> Result<Self, ExceptionPtr> {
        Self::with_options(&DnsResolverOptions::default())
    }

    /// A resolver with `opts` on the engine's network stack.
    pub fn with_options(opts: &DnsResolverOptions) -> Result<Self, ExceptionPtr> {
        // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
        Self::with_stack(unsafe { (*engine()).net() }, opts)
    }

    /// A resolver with `opts` on `stack`.
    pub fn with_stack(
        stack: *mut dyn NetworkStack,
        opts: &DnsResolverOptions,
    ) -> Result<Self, ExceptionPtr> {
        Ok(DnsResolver { _impl: DnsResolverImpl::create(stack, opts)? })
    }

    /// Resolves the host name `name` to its addresses and aliases, restricted to the
    /// address family `family` when given.
    ///
    /// # Errors
    ///
    /// The returned future fails if the lookup fails.
    pub fn get_host_by_name(&mut self, name: &Sstring, family: OptFamily) -> Future<Hostent> {
        // SAFETY: the impl lives in its SharedPtr on this shard and no reference to it is
        // live here; the raw-pointer method holds none across its calls into c-ares.
        unsafe { DnsResolverImpl::get_host_by_name(impl_ptr(&self._impl), name.clone(), family) }
    }

    /// Resolves the address `addr` to its host names and addresses (a reverse lookup).
    ///
    /// # Errors
    ///
    /// The returned future fails if the lookup fails.
    pub fn get_host_by_addr(&mut self, addr: &InetAddress) -> Future<Hostent> {
        // SAFETY: the impl lives in its SharedPtr on this shard and no reference to it is
        // live here; the raw-pointer method holds none across its calls into c-ares.
        unsafe { DnsResolverImpl::get_host_by_addr(impl_ptr(&self._impl), *addr) }
    }

    /// Resolves the host name `name` to its primary address, restricted to the address
    /// family `family` when given.
    ///
    /// # Errors
    ///
    /// The returned future fails if the lookup fails.
    pub fn resolve_name(&mut self, name: &Sstring, family: OptFamily) -> Future<InetAddress> {
        // SAFETY: the impl lives in its SharedPtr on this shard and no reference to it is
        // live here; the raw-pointer method holds none across its calls into c-ares.
        unsafe { DnsResolverImpl::resolve_name(impl_ptr(&self._impl), name.clone(), family) }
    }

    /// Resolves the address `addr` to its primary host name.
    ///
    /// # Errors
    ///
    /// The returned future fails if the lookup fails.
    pub fn resolve_addr(&mut self, addr: &InetAddress) -> Future<Sstring> {
        // SAFETY: the impl lives in its SharedPtr on this shard and no reference to it is
        // live here; the raw-pointer method holds none across its calls into c-ares.
        unsafe { DnsResolverImpl::resolve_addr(impl_ptr(&self._impl), *addr) }
    }

    /// Resolves the service `service` over the protocol `proto` in the domain `domain` to
    /// its SRV records.
    ///
    /// # Errors
    ///
    /// The returned future fails if the lookup fails.
    pub fn get_srv_records(
        &mut self,
        proto: SrvProto,
        service: &Sstring,
        domain: &Sstring,
    ) -> Future<SrvRecords> {
        // SAFETY: the impl lives in its SharedPtr on this shard and no reference to it is
        // live here; the raw-pointer method holds none across its calls into c-ares.
        unsafe { DnsResolverImpl::get_srv_records(impl_ptr(&self._impl), proto, service, domain) }
    }

    /// Shuts the resolver down; mostly useful in tests.
    pub fn close(&mut self) -> Future<()> {
        // SAFETY: as in get_host_by_name.
        unsafe { DnsResolverImpl::close(impl_ptr(&self._impl)) }
    }
}

thread_local! {
    /// This shard's default resolver, created on first use.
    static RESOLVER: RefCell<Option<DnsResolver>> = const { RefCell::new(None) };
}

fn with_resolver<R>(f: impl FnOnce(&mut DnsResolver) -> Future<R>) -> Future<R> {
    RESOLVER.with(|r| {
        let mut r = r.borrow_mut();
        if r.is_none() {
            match DnsResolver::new() {
                Ok(d) => *r = Some(d),
                Err(e) => return make_exception_future(e),
            }
        }
        match r.as_mut() {
            Some(d) => f(d),
            None => {
                make_exception_future(ExceptionPtr::from(RuntimeError("no resolver".to_string())))
            }
        }
    })
}

/// The shard-local default resolver and the error category.
#[allow(clippy::module_inception)]
pub mod dns {
    use super::*;

    // These functions query through a shard-local resolver with the default stack and
    // options.

    /// Resolves a hostname to one or more addresses and aliases.
    pub fn get_host_by_name(name: &Sstring, family: OptFamily) -> Future<Hostent> {
        with_resolver(|r| r.get_host_by_name(name, family))
    }
    /// Resolves an address to one or more addresses and aliases.
    pub fn get_host_by_addr(addr: &InetAddress) -> Future<Hostent> {
        with_resolver(|r| r.get_host_by_addr(addr))
    }
    /// Resolves a hostname to one (primary) address.
    pub fn resolve_name(name: &Sstring, family: OptFamily) -> Future<InetAddress> {
        with_resolver(|r| r.resolve_name(name, family))
    }
    /// Resolves an address to one (primary) name.
    pub fn resolve_addr(addr: &InetAddress) -> Future<Sstring> {
        with_resolver(|r| r.resolve_addr(addr))
    }
    /// Resolves a service in a domain to its SRV records.
    pub fn get_srv_records(
        proto: SrvProto,
        service: &Sstring,
        domain: &Sstring,
    ) -> Future<Vec<SrvRecord>> {
        with_resolver(|r| r.get_srv_records(proto, service, domain))
    }

    /// The error category of the [`DnsError`]s the resolver returns.
    pub fn error_category() -> &'static AresErrorCategory {
        static ARES_ERRORC: AresErrorCategory = AresErrorCategory;
        &ARES_ERRORC
    }
}

impl InetAddress {
    /// The primary name of this address.
    pub fn hostname(&self) -> Future<Sstring> {
        dns::resolve_addr(self)
    }

    /// The names of this address.
    pub fn aliases(&self) -> Future<Vec<Sstring>> {
        dns::get_host_by_addr(self).then(|e: Hostent| make_ready_future::<Vec<Sstring>>(e.names))
    }

    /// Resolves `name` to one address.
    pub fn find(name: &Sstring) -> Future<InetAddress> {
        dns::resolve_name(name, None)
    }

    /// Resolves `name` to one address of family `f`.
    pub fn find_family(name: &Sstring, f: Family) -> Future<InetAddress> {
        dns::resolve_name(name, Some(f))
    }

    /// Resolves `name` to all its addresses.
    pub fn find_all(name: &Sstring) -> Future<Vec<InetAddress>> {
        dns::get_host_by_name(name, None)
            .then(|e: Hostent| make_ready_future::<Vec<InetAddress>>(e.addr_list))
    }

    /// Resolves `name` to all its addresses of family `f`.
    pub fn find_all_family(name: &Sstring, f: Family) -> Future<Vec<InetAddress>> {
        dns::get_host_by_name(name, Some(f))
            .then(|e: Hostent| make_ready_future::<Vec<InetAddress>>(e.addr_list))
    }
}
