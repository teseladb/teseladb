//! The RPC core: protocol, client, server and their connections.
//!
//! A [`Protocol`] holds a serializer and the handlers of its verbs; it makes clients
//! ([`Client`]) and servers ([`Server`]), which keep pointers to it. A connection first
//! exchanges a negotiation frame (magic, features: compression, timeouts, connection id,
//! stream parent, isolation), then carries request and response frames, each with a
//! message id; a client matches responses to its outstanding calls, applies timeouts and
//! cancellation, and a server runs the handler of each request under the resource limits
//! of the server. Streams ([`RpcSinkImpl`], [`RpcSourceImpl`]) run over their own
//! connections, which may be used from another shard. The marshalling side (handler and
//! client signatures, `make_client`, `register_handler`) is in
//! [`rpc_impl`](crate::engine::rpc::rpc_impl).
//!
//! Layout and lifetime rules a maintainer must know:
//! - [`Connection`] starts with a pointer to its [`ConnectionVtable`] (one function,
//!   `peer_address`); [`Client`] and [`ServerConnection`] are `#[repr(C)]` and start with
//!   their `Connection`, so a `*mut Connection` is cast to them, and they dereference to it.
//! - Every continuation captures `this` as a raw pointer: a connection, a client and a
//!   server do not move once their background loops run, and `stop()` waits for those loops
//!   (`_stopped`, `_ss_stopped`). The constructors build the value; `start(self: Pin<&mut
//!   Self>)` starts the loops at the final place of the object.
//! - The outgoing queue is an intrusive list whose entries are `HostBox`es; each entry's
//!   `done` promise lives in the entry, so queuing a frame allocates once.

use std::cell::{Cell, UnsafeCell};
use std::collections::{BTreeMap, HashMap};

// The maps on the path of each call (`_outstanding`, `_handlers`) and the other
// integer-keyed ones use a one-multiply hash, not SipHash: their keys are message ids and
// verbs chosen by this process, so no keyed hash is needed.
use crate::engine::net::hash::FxBuildHasher;
use crate::engine::util::noncopyable_function::NoncopyableFunction;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::{PhantomData, PhantomPinned};
use std::pin::Pin;

use crate::engine::core::api::intrusive::{List, ListHook};
use crate::engine::core::api::{listen_with, make_socket};
use crate::engine::core::byteorder::{read_le, write_le};
use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::coroutine::make_coroutine;
use crate::engine::core::deleter::make_object_deleter;
use crate::engine::core::future::{
    ExceptionPtr, Future, HostBox, HostedPromise, LogicError, Promise, PromiseHost, RuntimeError,
    futurize_invoke, make_exception_future, make_ready_future,
};
use crate::engine::core::gate::{Gate, GateClosedException, Holder, with_gate};
use crate::engine::core::iostream::{InputStream, OutputStream};
use crate::engine::core::loop_::{do_for_each, do_until, keep_doing, parallel_for_each};
use crate::engine::core::metrics::{self as sm, Description, Label};
use crate::engine::core::metrics_registration::MetricGroups;
use crate::engine::core::queue::Queue;
use crate::engine::core::reactor::yield_;
use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::core::semaphore::{
    BasicSemaphore, Semaphore, SemaphoreDefaultExceptionFactory, SemaphoreUnits, get_units,
    get_units_until,
};
use crate::engine::core::sharded::{ForeignPtr, make_foreign, this_shard_id};
use crate::engine::core::shared_future::SharedPromise;
use crate::engine::core::shared_ptr::{
    EnableSharedFromThis, SharedFromThis, SharedPtr, make_lw_shared, make_shared, make_shared_esft,
    static_pointer_cast,
};
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::core::timer::Timer;
use crate::engine::core::weak_ptr::{WeakPtr, WeaklyReferencable, weak_from_this};
use crate::engine::core::with_scheduling_group::with_scheduling_group;
use crate::engine::net::api::{
    AcceptResult, ConnectedSocket, KeepaliveParams, ListenOptions, LoadBalancingAlgorithm,
    ServerSocket, Socket, TcpKeepaliveParams, Transport,
};
use crate::engine::net::packet::Packet;
use crate::engine::net::packet_data_source::as_input_stream;
use crate::engine::net::socket_defs::SocketAddress;
use crate::engine::rpc::rpc_impl::{
    ExceptionType, RESPONSE_FRAME_HEADROOM, RpcMsgType, deserialize_connection_id,
    relative_timeout_to_absolute, serialize_connection_id, unmarshal_exception,
};
use crate::engine::rpc::rpc_types::{
    Bufs, ClientInfo, Compressor, CompressorFactory, ConnectionId, ErrorKind,
    INVALID_CONNECTION_ID, MAX_QUEUED_STREAM_BUFFERS, MAX_STREAM_BUFFERS_MEMORY, RcvBuf,
    RpcClockType, Sink, SinkImplBase, SndBuf, SourceImplBase, Stats, XshardConnectionPtr,
    canceled_error, closed_error, stream_closed, timeout_error,
};
use crate::engine::util::log::{LogLevel, Logger as EngineLogger};
use crate::engine_assert;
use crate::intrusive_adapter;

/// A point in time of the RPC clock (timeouts).
pub type RpcTimePoint = crate::engine::core::lowres_clock::LowresTimePoint;

/// A duration of the RPC clock, in nanoseconds (a signed count).
pub type RpcDurationNs = i64;

/// The id of an RPC message, which matches a response to its request.
///
/// The RPC framework defines client-server communication protocols; its entry point is
/// [`Protocol`].
pub type IdType = i64;

/// The semaphore that bounds the memory of a server's requests, timed by the RPC clock.
pub type RpcSemaphore = BasicSemaphore<SemaphoreDefaultExceptionFactory, RpcClockType>;
/// Units taken from an [`RpcSemaphore`], returned when dropped.
pub type ResourcePermit = SemaphoreUnits<SemaphoreDefaultExceptionFactory, RpcClockType>;

/// The eight bytes that open the negotiation frame of every RPC connection; a peer that
/// does not send them is rejected.
pub const RPC_MAGIC: &[u8; 8] = b"TSLDBRPC";

/// Specifies resource isolation for a connection.
#[derive(Clone, Copy, Debug)]
pub struct IsolationConfig {
    /// Specifies a scheduling group under which the connection (and all its
    /// verb handlers) will execute.
    pub sched_group: SchedulingGroup,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        IsolationConfig { sched_group: current_scheduling_group() }
    }
}

/// Default isolation configuration - run everything in the default scheduling group.
///
/// In the scheduling_group that the protocol::server was created in.
pub fn default_isolate_connection(_isolation_cookie: Sstring) -> IsolationConfig {
    IsolationConfig::default()
}

/// A function that can be copied: copying it copies the callable (one allocation per
/// copy). Every `Fn(A) -> R + Clone` closure is one.
pub trait CopyableFn<A, R>: Fn(A) -> R {
    /// A boxed copy of the callable.
    fn clone_box(&self) -> Box<dyn CopyableFn<A, R>>;
}

impl<A, R, F: Fn(A) -> R + Clone + 'static> CopyableFn<A, R> for F {
    #[inline]
    fn clone_box(&self) -> Box<dyn CopyableFn<A, R>> {
        Box::new(self.clone())
    }
}

impl<A: 'static, R: 'static> Clone for Box<dyn CopyableFn<A, R>> {
    #[inline]
    fn clone(&self) -> Self {
        (**self).clone_box()
    }
}

/// A copyable filter of peer addresses; see [`CopyableFn`].
pub trait ConnectionFilterFn: Fn(&SocketAddress) -> bool {
    /// A boxed copy of the callable.
    fn clone_box(&self) -> Box<dyn ConnectionFilterFn>;
}

impl<F: Fn(&SocketAddress) -> bool + Clone + 'static> ConnectionFilterFn for F {
    #[inline]
    fn clone_box(&self) -> Box<dyn ConnectionFilterFn> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn ConnectionFilterFn> {
    #[inline]
    fn clone(&self) -> Self {
        (**self).clone_box()
    }
}

/// Configures isolation for a connection based on its isolation cookie. May throw,
/// in which case the connection will be terminated.
pub type SyncronousIsolationFunction =
    Box<dyn CopyableFn<Sstring, Result<IsolationConfig, ExceptionPtr>>>;
/// The asynchronous form of `SyncronousIsolationFunction`.
pub type AsyncronousIsolationFunction = Box<dyn CopyableFn<Sstring, Future<IsolationConfig>>>;

/// The isolation function of a server: synchronous or asynchronous.
#[derive(Clone)]
pub enum IsolationFunctionAlternatives {
    /// Returns the configuration.
    Syncronous(SyncronousIsolationFunction),
    /// Resolves to the configuration.
    Asyncronous(AsyncronousIsolationFunction),
}

/// Resource limits for an RPC server.
///
/// A request's memory use is estimated as
///
/// ```text
/// req_mem = basic_request_size * serialized_request_size * bloat_factor
/// ```
///
/// Concurrent requests are limited so that
///
/// ```text
/// sum(req_mem) <= max_memory
/// ```
///
/// See [`Server`].
#[derive(Clone)]
pub struct ResourceLimits {
    /// Minimum request footprint in memory
    pub basic_request_size: usize,
    /// Serialized size multiplied by this to estimate memory used by request
    pub bloat_factor: u32,
    /// Maximum amount of memory that may be consumed by all requests
    pub max_memory: usize,
    /// How a connection is isolated, from its cookie.
    pub isolate_connection: IsolationFunctionAlternatives,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            basic_request_size: 0,
            bloat_factor: 1,
            max_memory: RpcSemaphore::max_counter(),
            isolate_connection: IsolationFunctionAlternatives::Syncronous(Box::new(|c| {
                Ok(default_isolate_connection(c))
            })),
        }
    }
}

/// The options of a client.
#[derive(Clone)]
pub struct ClientOptions {
    /// The tcp keepalive, if any.
    pub keepalive: Option<TcpKeepaliveParams>,
    /// Whether to disable Nagle's algorithm.
    pub tcp_nodelay: bool,
    /// Whether to reuse the local address.
    pub reuseaddr: bool,
    /// The compressors offered to the server; the factory outlives the client.
    pub compressor_factory: Option<&'static dyn CompressorFactory>,
    /// Whether to send the timeouts of the calls.
    pub send_timeout_data: bool,
    /// The parent connection of a stream.
    pub stream_parent: ConnectionId,
    /// Configures how this connection is isolated from the other connections of the same
    /// server; the server maps it to an [`IsolationConfig`] with
    /// [`ResourceLimits::isolate_connection`].
    pub isolation_cookie: Sstring,
    /// The domain of the metrics of the client.
    pub metrics_domain: Sstring,
    /// Whether to ask the server for the duration of the handlers.
    pub send_handler_duration: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            keepalive: None,
            tcp_nodelay: true,
            reuseaddr: false,
            compressor_factory: None,
            send_timeout_data: true,
            stream_parent: INVALID_CONNECTION_ID,
            isolation_cookie: Sstring::new(),
            metrics_domain: Sstring::from("default"),
            send_handler_duration: true,
        }
    }
}

// RPC call that passes stream connection id as a parameter
// may arrive to a different shard from where the stream connection
// was opened, so the connection id is not known to a server that handles
// the RPC call. The shard that the stream connection belong to is know
// since it is a part of connection id, but this is not enough to locate
// a server instance the connection belongs to if there are more than one
// server on the shard. Stream domain parameter is here to help with that.
// Different servers on all shards logically belonging to the same service should
// belong to the same streaming domain. Only one server on each shard can belong to
// a particulr streaming domain.
/// The streaming domain of a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamingDomainType {
    pub(crate) _id: u64,
}

impl StreamingDomainType {
    /// The domain `id`.
    #[inline]
    pub const fn new(id: u64) -> Self {
        StreamingDomainType { _id: id }
    }
}

/// Hashes the id combined with the golden-ratio constant `0x9e3779b9`.
impl Hash for StreamingDomainType {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self._id.wrapping_add(0x9e3779b9));
    }
}

/// Writes the domain id.
impl fmt::Display for StreamingDomainType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self._id)
    }
}

/// The options of a server.
#[derive(Clone)]
pub struct ServerOptions {
    /// The compressors the server accepts; the factory outlives the server.
    pub compressor_factory: Option<&'static dyn CompressorFactory>,
    /// Whether to disable Nagle's algorithm.
    pub tcp_nodelay: bool,
    /// The streaming domain of the server.
    pub streaming_domain: Option<StreamingDomainType>,
    /// How the connections are spread over the shards.
    pub load_balancing_algorithm: LoadBalancingAlgorithm,
    // optional filter function. If set, will be called with remote
    // (connecting) address.
    // Returning false will refuse the incoming connection.
    // Returning true will allow the mechanism to proceed.
    /// The filter of the incoming connections.
    pub filter_connection: Option<Box<dyn ConnectionFilterFn>>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            compressor_factory: None,
            tcp_nodelay: true,
            streaming_domain: None,
            load_balancing_algorithm: LoadBalancingAlgorithm::default(),
            filter_connection: None,
        }
    }
}

/// The memory a request of `serialized_size` bytes is accounted for.
#[inline]
pub fn estimate_request_size(lim: &ResourceLimits, serialized_size: usize) -> usize {
    lim.basic_request_size + serialized_size * lim.bloat_factor as usize
}

/// The fixed part of the negotiation frame.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct NegotiationFrame {
    /// The protocol magic, [`RPC_MAGIC`].
    pub magic: [u8; RPC_MAGIC.len()],
    /// additional negotiation data length; multiple negotiation_frame_feature_record structs
    pub len: u32,
}

/// The features negotiated by a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ProtocolFeatures(pub u32);

impl ProtocolFeatures {
    /// COMPRESS
    pub const COMPRESS: ProtocolFeatures = ProtocolFeatures(0);
    /// TIMEOUT
    pub const TIMEOUT: ProtocolFeatures = ProtocolFeatures(1);
    /// CONNECTION_ID
    pub const CONNECTION_ID: ProtocolFeatures = ProtocolFeatures(2);
    /// STREAM_PARENT
    pub const STREAM_PARENT: ProtocolFeatures = ProtocolFeatures(3);
    /// ISOLATION
    pub const ISOLATION: ProtocolFeatures = ProtocolFeatures(4);
    /// HANDLER_DURATION
    pub const HANDLER_DURATION: ProtocolFeatures = ProtocolFeatures(5);
}

// internal representation of feature data
/// The negotiated features and their data, ordered by feature.
pub type FeatureMap = BTreeMap<ProtocolFeatures, Sstring>;

// An rpc signature, in the form signature<Ret (In0, In1, In2)>.
/// The return type and the tuple of argument types of a verb.
pub struct Signature<Ret, In>(std::marker::PhantomData<(Ret, In)>);

/// The logger of a protocol: forwards to a engine logger when one is set.
#[derive(Clone, Copy, Default)]
pub struct Logger {
    _engine_logger: Option<&'static EngineLogger>,
}

impl Logger {
    fn log(&self, str: &Sstring) {
        if let Some(l) = self._engine_logger {
            // default level for log messages is `info`
            crate::log_info!(l, "{}", str);
        }
    }

    // _engine_logger will always be used first if it's available
    fn log_level(&self, level: LogLevel, args: fmt::Arguments<'_>) {
        if let Some(l) = self._engine_logger {
            l.log(level, crate::engine::util::log::FormatInfo::new("{}"), args);
        }
    }

    /// Sets the engine logger.
    pub fn set(&mut self, logger: Option<&'static EngineLogger>) {
        self._engine_logger = logger;
    }

    /// Logs `str` about message `msg_id` of a client.
    pub fn client_msg(&self, info: &ClientInfo, msg_id: IdType, str: &Sstring) {
        self.log(&Sstring::from(format!("client {} msg_id {}:  {}", info.addr, msg_id, str)));
    }

    /// Logs `str` at `level` about message `msg_id` of a client.
    pub fn client_msg_level(&self, info: &ClientInfo, msg_id: IdType, level: LogLevel, str: &str) {
        self.log_level(level, format_args!("client {} msg_id {}:  {}", info.addr, msg_id, str));
    }

    /// Logs `str` about a client.
    pub fn client(&self, info: &ClientInfo, str: &Sstring) {
        self.addr(&info.addr, str);
    }

    /// Logs `str` at `level` about a client.
    pub fn client_level(&self, info: &ClientInfo, level: LogLevel, str: &str) {
        self.addr_level(&info.addr, level, str);
    }

    /// Logs `str` about the peer at `addr`.
    pub fn addr(&self, addr: &SocketAddress, str: &Sstring) {
        self.log(&Sstring::from(format!("client {}: {}", addr, str)));
    }

    /// Logs `str` at `level` about the peer at `addr`.
    pub fn addr_level(&self, addr: &SocketAddress, level: LogLevel, str: &str) {
        self.log_level(level, format_args!("client {}: {}", addr, str));
    }
}

/// The functions that differ between the kinds of [`Connection`]: `peer_address`.
pub struct ConnectionVtable {
    /// The address of the peer of the connection.
    pub peer_address: unsafe fn(*const Connection) -> SocketAddress,
}

/// A frame waiting in the outgoing queue of a connection.
pub(crate) struct OutgoingEntry {
    /// The hook that links the entry into the queue; it unlinks itself when dropped.
    hook: ListHook,
    pub(crate) t: Timer<RpcClockType>,
    pub(crate) buf: SndBuf,
    pub(crate) done: HostedPromise<()>,
    pub(crate) pcancel: *mut crate::engine::rpc::rpc_types::Cancellable,
}

intrusive_adapter!(pub(crate) OutgoingEntryAdapter = OutgoingEntry { hook });

/// The outgoing queue of a connection: an intrusive list of entries.
pub(crate) type OutgoingEntryContainer = List<OutgoingEntryAdapter>;

impl OutgoingEntry {
    /// An entry for frame `b`; the entry must not move once in its `HostBox` (the queue
    /// links point at it).
    #[inline]
    pub(crate) fn new(b: SndBuf) -> Self {
        OutgoingEntry {
            hook: ListHook::new(),
            t: Timer::new(),
            buf: b,
            done: HostedPromise::new(),
            pcancel: std::ptr::null_mut(),
        }
    }

    /// Stops the timer and the cancellation of the send.
    pub(crate) fn uncancellable(&mut self) {
        // SAFETY: the entry lives in its HostBox and does not move.
        unsafe { Pin::new_unchecked(&mut self.t) }.cancel();
        if !self.pcancel.is_null() {
            // SAFETY: a set pcancel is a live cancellable (it clears the pointer through
            // send_back_pointer before going away).
            unsafe { (*self.pcancel).cancel_send = NoncopyableFunction::<fn()>::empty() };
        }
    }

    /// `is_linked()` of the base hook.
    #[inline]
    pub(crate) fn is_linked(&self) -> bool {
        self.hook.is_linked()
    }

    /// `unlink()` of the base hook.
    #[inline]
    pub(crate) fn unlink(&self) {
        self.hook.unlink();
    }
}

impl Drop for OutgoingEntry {
    fn drop(&mut self) {
        if !self.pcancel.is_null() {
            // SAFETY: as in uncancellable().
            unsafe {
                (*self.pcancel).cancel_send = NoncopyableFunction::<fn()>::empty();
                (*self.pcancel).send_back_pointer = std::ptr::null_mut();
            }
        }
    }
}

// SAFETY: `done` is a field of the entry, so it lives and stays in place with it.
unsafe impl PromiseHost for OutgoingEntry {
    type Value = ();
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<()> {
        // SAFETY: the caller's contract: `this` points at a live entry.
        unsafe { &raw mut (*this).done }
    }
}

/// The state shared by the two ends of an rpc connection.
///
/// It does not move once it has entries in its outgoing queue or background loops: it
/// lives in a `SharedPtr` (server connections, stream clients) or in a pinned
/// `Client`.
#[repr(C)]
pub struct Connection {
    _vtable: &'static ConnectionVtable,
    pub(crate) _fd: ConnectedSocket,
    pub(crate) _read_buf: InputStream<u8>,
    pub(crate) _write_buf: OutputStream<u8>,
    pub(crate) _error: bool,
    pub(crate) _connected: bool,
    pub(crate) _negotiated: Option<SharedPromise<()>>,
    pub(crate) _stopped: Promise<()>,
    pub(crate) _stats: Stats,
    pub(crate) _logger: *const Logger,
    // The owner of the pointer below is the serializer of a `Protocol<Serializer>`.
    // The type of the pointer is erased here, but the actual type is Serializer
    pub(crate) _serializer: *mut (),
    pub(crate) _outgoing_queue_ready: Future<()>,
    pub(crate) _outgoing_queue: OutgoingEntryContainer,
    pub(crate) _outgoing_queue_size: usize,
    pub(crate) _compressor: Option<Box<dyn Compressor>>,
    pub(crate) _propagate_timeout: bool,
    pub(crate) _timeout_negotiated: bool,
    pub(crate) _handler_duration_negotiated: bool,
    // stream related fields
    pub(crate) _is_stream: bool,
    pub(crate) _id: ConnectionId,

    pub(crate) _streams: HashMap<ConnectionId, XshardConnectionPtr, FxBuildHasher>,
    pub(crate) _stream_queue: Queue<RcvBuf>,
    pub(crate) _stream_sem: Semaphore,
    pub(crate) _sink_closed: bool,
    pub(crate) _source_closed: bool,
    /// Whether the peer's end-of-stream frame has been queued: the connection's end after
    /// it is the end of the stream, not a loss, so the queue is not aborted then.
    pub(crate) _stream_eos_received: bool,
    // the future holds if sink is already closed
    // if it is not ready it means the sink is been closed
    pub(crate) _sink_closed_future: Future<bool>,
    /// Set by stream_close() before it closes the write buffer: from then on send() refuses
    /// new frames (see stream_close()).
    pub(crate) _stream_closing: bool,
}

impl Connection {
    /// A connection over `fd`, logging to `l`, with the type-erased serializer `s` and id `id`.
    ///
    /// # Safety
    /// `l` and the serializer `s` outlive the connection.
    pub unsafe fn with_socket(
        vtable: &'static ConnectionVtable,
        fd: ConnectedSocket,
        l: &Logger,
        s: *mut (),
        id: ConnectionId,
    ) -> Self {
        // SAFETY: the caller's contract.
        let mut c = unsafe { Self::new(vtable, l, s, id) };
        c.set_socket_unconnected(fd);
        c
    }

    /// A connection without a socket yet; `l` outlives the
    /// connection (it belongs to the protocol).
    ///
    /// # Safety
    /// `l` and the serializer `s` outlive the connection.
    pub unsafe fn new(
        vtable: &'static ConnectionVtable,
        l: &Logger,
        s: *mut (),
        id: ConnectionId,
    ) -> Self {
        let negotiated = SharedPromise::new();
        let outgoing_queue_ready = negotiated.get_shared_future();
        Connection {
            _vtable: vtable,
            _fd: ConnectedSocket::new(),
            _read_buf: InputStream::default(),
            _write_buf: OutputStream::default(),
            _error: false,
            _connected: false,
            _negotiated: Some(negotiated),
            _stopped: Promise::new(),
            _stats: Stats::default(),
            _logger: l,
            _serializer: s,
            _outgoing_queue_ready: outgoing_queue_ready,
            _outgoing_queue: List::new(),
            _outgoing_queue_size: 0,
            _compressor: None,
            _propagate_timeout: false,
            _timeout_negotiated: false,
            _handler_duration_negotiated: false,
            _is_stream: false,
            _id: id,
            _streams: HashMap::default(),
            _stream_queue: Queue::new(MAX_QUEUED_STREAM_BUFFERS),
            _stream_sem: Semaphore::new(MAX_STREAM_BUFFERS_MEMORY),
            _sink_closed: true,
            _source_closed: true,
            _stream_eos_received: false,
            _sink_closed_future: make_ready_future(false),
            _stream_closing: false,
        }
    }

    #[inline]
    pub(crate) fn is_stream(&self) -> bool {
        self._is_stream
    }

    #[inline]
    pub(crate) fn stream_check_twoway_closed(&self) -> bool {
        self._sink_closed && self._source_closed
    }

    /// The frames waiting to be sent.
    #[inline]
    pub fn outgoing_queue_length(&self) -> usize {
        self._outgoing_queue_size
    }

    /// Whether the connection failed or was stopped.
    #[inline]
    pub fn error(&self) -> bool {
        self._error
    }

    /// Closes the sink side of a stream connection.
    pub fn close_sink(&mut self) -> Future<()> {
        self._sink_closed = true;
        if self.stream_check_twoway_closed() {
            return self.stream_close();
        }
        make_ready_future(())
    }

    /// Whether the sink side is closed.
    #[inline]
    pub fn sink_closed(&self) -> bool {
        self._sink_closed
    }

    /// Closes the source side of a stream connection.
    pub fn close_source(&mut self) -> Future<()> {
        self._source_closed = true;
        if self.stream_check_twoway_closed() {
            return self.stream_close();
        }
        make_ready_future(())
    }

    /// The id of the connection.
    #[inline]
    pub fn get_connection_id(&self) -> ConnectionId {
        self._id
    }

    /// The counters, for the rpc code.
    #[inline]
    pub fn get_stats_internal(&mut self) -> &mut Stats {
        &mut self._stats
    }

    /// The address of the peer, through the vtable.
    #[inline]
    pub fn peer_address(&self) -> SocketAddress {
        // SAFETY: the vtable is the one of the object this connection begins.
        unsafe { (self._vtable.peer_address)(self) }
    }

    /// The logger of the protocol.
    #[inline]
    pub fn get_logger(&self) -> &Logger {
        // SAFETY: the logger belongs to the protocol, which outlives its connections.
        unsafe { &*self._logger }
    }

    /// The serializer of the protocol, type-erased: it is an `S` only when `S` is the
    /// serializer type of the protocol that made the connection, and it lives as long as
    /// that protocol, which outlives its connections.
    #[inline]
    pub fn serializer<S>(&self) -> *mut S {
        self._serializer as *mut S
    }

    /// Holds the outgoing queue until `p` is set (for tests). Runs in a engine thread:
    /// `_outgoing_queue_ready.get()` waits for the queue to drain, and its exception is thrown.
    pub fn suspend_for_testing(&mut self, p: &mut Promise<()>) -> Result<(), ExceptionPtr> {
        let ready = std::mem::replace(&mut self._outgoing_queue_ready, make_ready_future(()));
        ready.get()?;
        let dummy = HostBox::new(OutgoingEntry::new(SndBuf::default()));
        let d = dummy.as_ptr();
        // SAFETY: the entry stays in its HostBox, alive until the test sets `p`, and the
        // connection does not move while its queue is not empty.
        unsafe {
            self._outgoing_queue.push_back(d);
            self._outgoing_queue_ready =
                HostedPromise::get_future(OutgoingEntry::hosted_promise(d));
        }
        let _ = p.get_future().then(move |()| {
            // SAFETY: `dummy`, moved here, keeps the entry alive.
            unsafe { HostedPromise::set_value(OutgoingEntry::hosted_promise(dummy.as_ptr()), ()) };
        });
        Ok(())
    }

    /// Logs `log: <what eptr>` about the peer, at `level`.
    pub(crate) fn log_exception(&self, level: LogLevel, log: &str, eptr: &ExceptionPtr) {
        log_exception(self.get_logger(), &self.peer_address(), level, log, eptr);
    }
}

/// A shared pointer to a connection, as a foreign pointer holds it: it dereferences to the
/// cell of the connection, so that the shards that reach it through an
/// `XshardConnectionPtr` can mutate it through `get()`.
pub struct ConnectionPtr(pub SharedPtr<Connection>);

impl std::ops::Deref for ConnectionPtr {
    type Target = UnsafeCell<Connection>;
    #[inline]
    fn deref(&self) -> &UnsafeCell<Connection> {
        match self.0.get() {
            // SAFETY: UnsafeCell<T> has the layout of T; the pointer comes from the
            // allocation of the shared object, not from a shared reference.
            Some(p) => unsafe { &*(p.as_ptr() as *const UnsafeCell<Connection>) },
            None => unreachable!("an xshard connection pointer is never null"),
        }
    }
}

/// The connection behind the cross-shard pointer `c`.
#[inline]
pub(crate) fn xshard_get(c: &XshardConnectionPtr) -> *mut Connection {
    // SAFETY: only the address of the connection's cell is taken, and the reference to the
    // cell ends at once; the connection is not read here.
    match unsafe { c.get_unchecked() } {
        Some(cell) => cell.get(),
        None => unreachable!("an xshard connection pointer is never null"),
    }
}

/// A pointer that a closure given to `smp::submit_to` carries to another shard (a `this`
/// or a reference of the calling shard).
pub(crate) struct XshardPtr<T>(*mut T);

impl<T> Clone for XshardPtr<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for XshardPtr<T> {}

impl<T> XshardPtr<T> {
    /// Wraps `p` for a cross-shard call.
    ///
    /// # Safety
    /// The pointee outlives every cross-shard call that uses the pointer, and while such a
    /// call runs, no other shard writes what it reaches through it (the other shards only
    /// read it, or wait for the call).
    #[inline]
    pub(crate) unsafe fn new(p: *mut T) -> Self {
        XshardPtr(p)
    }

    /// The pointer (a method, so that closures capture the whole wrapper).
    #[inline]
    pub(crate) fn get(self) -> *mut T {
        self.0
    }
}

// SAFETY: only the address crosses shards, and every wrapper is built under the contract of
// `XshardPtr::new`, which rules out concurrent access to the pointee.
unsafe impl<T> Send for XshardPtr<T> {}

/// A frame to send whose content is ready later (a pending promise and its data).
///
/// It lives in a [`HostBox`], which does not move, so its
/// promise's node is part of the entry and `get_future()` allocates nothing. `data` is
/// taken when the buffer is sent.
pub(crate) struct DeferredSndBuf {
    pub(crate) pr: HostedPromise<()>,
    pub(crate) data: Option<SndBuf>,
}

// SAFETY: hosted_promise returns the `pr` field, derived from `this`.
unsafe impl PromiseHost for DeferredSndBuf {
    type Value = ();
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<()> {
        // SAFETY: the caller's contract: `this` points at a live buffer.
        unsafe { &raw mut (*this).pr }
    }
}

/// The `_next_seq_num` of a sink, alone in its cache line (`alignas (cache_line_size)`).
#[repr(C, align(64))]
pub(crate) struct SinkSeqNum(pub(crate) u64);

/// The state of a sink used on the shard of its connection, alone in its cache line.
#[repr(C, align(64))]
pub(crate) struct SinkRemoteState {
    pub(crate) last_seq_num: u64,
    pub(crate) out_of_order_bufs: BTreeMap<u64, HostBox<DeferredSndBuf>>,
}

// send data Out...
/// The sink implementation of rpc streams: sends `Out` values over the stream connection.
#[repr(C)]
pub struct RpcSinkImpl<S: 'static, Out: 'static> {
    pub(crate) base: SinkImplBase,
    // Used on the shard *this lives on.
    pub(crate) _next_seq_num: SinkSeqNum,

    // Used on the shard the _conn lives on.
    pub(crate) _remote_state: SinkRemoteState,
    pub(crate) _marker: PhantomData<fn(&S, &Out)>,
}

impl<S: 'static, Out: 'static> RpcSinkImpl<S, Out> {
    /// A sink over the stream connection `con`.
    pub fn new(con: XshardConnectionPtr) -> Self {
        let base = SinkImplBase::new(con);
        // SAFETY: the connection is alive while the pointer to it is.
        unsafe { (*xshard_get(&base._con))._sink_closed = false };
        RpcSinkImpl {
            base,
            _next_seq_num: SinkSeqNum(1),
            _remote_state: SinkRemoteState { last_seq_num: 0, out_of_order_bufs: BTreeMap::new() },
            _marker: PhantomData,
        }
    }
}

// receive data In...
/// The source implementation of rpc streams: receives `In` values from the stream connection.
pub struct RpcSourceImpl<S: 'static, In: 'static> {
    pub(crate) base: SourceImplBase,
    pub(crate) _marker: PhantomData<fn(&S) -> In>,
}

impl<S: 'static, In: 'static> RpcSourceImpl<S, In> {
    /// A source over the stream connection `con`.
    pub fn new(con: XshardConnectionPtr) -> Self {
        let base = SourceImplBase::new(con);
        // SAFETY: the connection is alive while the pointer to it is.
        unsafe { (*xshard_get(&base._con))._source_closed = false };
        RpcSourceImpl { base, _marker: PhantomData }
    }
}

/// The data members of `client::reply_handler_base`.
pub struct ReplyHandlerData {
    /// The timeout of the call.
    pub t: Timer<RpcClockType>,
    /// The cancellable of the call, if any.
    pub pcancel: *mut crate::engine::rpc::rpc_types::Cancellable,
    /// When the call was sent.
    pub start: RpcTimePoint,
}

impl Default for ReplyHandlerData {
    fn default() -> Self {
        ReplyHandlerData {
            t: Timer::new(),
            pcancel: std::ptr::null_mut(),
            start: RpcTimePoint::from_nanos(0),
        }
    }
}

impl Drop for ReplyHandlerData {
    fn drop(&mut self) {
        if !self.pcancel.is_null() {
            // SAFETY: a set pcancel is a live cancellable (it clears the pointer through
            // wait_back_pointer before going away).
            unsafe {
                (*self.pcancel).cancel_wait = NoncopyableFunction::<fn()>::empty();
                (*self.pcancel).wait_back_pointer = std::ptr::null_mut();
            }
        }
    }
}

/// A call waiting for its reply.
pub trait ReplyHandlerBase {
    /// The data members.
    fn data(&mut self) -> &mut ReplyHandlerData;
    /// The reply arrived with `data`. A failure to read it ends the connection loop.
    fn call(
        &mut self,
        client: &mut Client,
        msg_id: IdType,
        data: RcvBuf,
    ) -> Result<(), ExceptionPtr>;
    /// The call timed out.
    fn timeout(&mut self) {}
    /// The call was canceled.
    fn cancel(&mut self) {}
}

/// The promise side of a reply (`rcv_reply_base`), which `ReplyHandler` fails.
pub trait ReplyPromise {
    /// Marks the call done and fails its promise with `ex`.
    fn fail(&mut self, ex: ExceptionPtr);
}

/// A call waiting for its reply: `func` reads the reply into `reply` when it arrives.
pub struct ReplyHandler<Reply, Func> {
    /// The base data.
    pub base: ReplyHandlerData,
    /// Handles the reply.
    pub func: Func,
    /// The promise of the call.
    pub reply: Reply,
}

impl<Reply, Func> ReplyHandler<Reply, Func> {
    /// A handler that runs `f` on the reply, with the promise state `reply`.
    #[inline]
    pub fn new(f: Func, reply: Reply) -> Self {
        ReplyHandler { base: ReplyHandlerData::default(), func: f, reply }
    }
}

impl<Reply, Func> ReplyHandlerBase for ReplyHandler<Reply, Func>
where
    Reply: ReplyPromise,
    Func: FnMut(&mut Reply, &mut Client, IdType, RcvBuf) -> Result<(), ExceptionPtr>,
{
    #[inline]
    fn data(&mut self) -> &mut ReplyHandlerData {
        &mut self.base
    }
    #[inline]
    fn call(
        &mut self,
        client: &mut Client,
        msg_id: IdType,
        data: RcvBuf,
    ) -> Result<(), ExceptionPtr> {
        (self.func)(&mut self.reply, client, msg_id, data)
    }
    fn timeout(&mut self) {
        self.reply.fail(ExceptionPtr::new(timeout_error()));
    }
    fn cancel(&mut self) {
        self.reply.fail(ExceptionPtr::new(canceled_error()));
    }
}

/// Links a client into the metrics of its domain.
pub(crate) struct ClientMetrics {
    _c: *const Client,
    _domain: *mut ClientMetricsDomain,
    _link: ListHook,
}

intrusive_adapter!(pub(crate) ClientMetricsAdapter = ClientMetrics { _link });

/// The clients whose metrics are aggregated in one domain.
pub(crate) type DomainList = List<ClientMetricsAdapter>;

/// The client end of an rpc connection: sends requests and waits for their replies.
/// It starts with its [`Connection`] and can be weakly referenced.
#[repr(C)]
pub struct Client {
    _conn: Connection,
    _weak: WeaklyReferencable<Client>,
    _socket: Socket,
    _message_id: IdType,
    _outstanding: HashMap<IdType, Box<dyn ReplyHandlerBase>, FxBuildHasher>,
    _server_addr: SocketAddress,
    _local_addr: SocketAddress,
    _options: ClientOptions,
    _parent: WeakPtr<Client>, // for stream clients

    _metrics: ClientMetrics,
    /// Keeps the protocol from being dropped while the client uses it.
    _proto_use: ProtocolUse,
}

crate::weakly_referencable!(Client { _weak });

impl std::ops::Deref for Client {
    type Target = Connection;
    #[inline]
    fn deref(&self) -> &Connection {
        &self._conn
    }
}

impl std::ops::DerefMut for Client {
    #[inline]
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self._conn
    }
}

/// The vtable of `Client`.
static CLIENT_VTABLE: ConnectionVtable = ConnectionVtable { peer_address: Client::vt_peer_address };

impl Client {
    unsafe fn vt_peer_address(c: *const Connection) -> SocketAddress {
        // SAFETY: the vtable is only installed in the Connection that begins a Client.
        unsafe { (*(c as *const Client)).peer_address() }
    }

    /// The requests waiting for their reply.
    #[inline]
    pub fn incoming_queue_length(&self) -> usize {
        self._outstanding.len()
    }

    /// The id of the next request (ids increase by one per request).
    #[inline]
    pub fn next_message_id(&mut self) -> IdType {
        let id = self._message_id;
        self._message_id += 1;
        id
    }

    /// The address of the peer.
    #[inline]
    pub fn peer_address(&self) -> SocketAddress {
        self._server_addr
    }

    /// Resolves once the connection is negotiated.
    pub fn await_connection(&mut self) -> Future<()> {
        match &self._conn._negotiated {
            None => make_ready_future(()),
            Some(n) => n.get_shared_future(),
        }
    }

    /// A sink of a new stream over `socket`, whose values are `Out` serialized by `S`.
    pub fn make_stream_sink_with<S: 'static, Out: 'static>(
        &mut self,
        socket: Socket,
    ) -> Future<Sink<Out>>
    where
        RpcSinkImpl<S, Out>: crate::engine::rpc::rpc_types::SinkImpl<Out>,
    {
        let this: *mut Client = self;
        let mut socket = Some(socket);
        self.await_connection().then(move |()| -> Future<Sink<Out>> {
            // SAFETY: the client outlives its streams; stop() waits for them.
            let me = unsafe { &mut *this };
            if !me.get_connection_id().is_valid() {
                return make_exception_future(ExceptionPtr::new(RuntimeError(
                    "Streaming is not supported by the server".to_string(),
                )));
            }
            let mut o = me._options.clone();
            o.stream_parent = me.get_connection_id();
            o.send_timeout_data = false;
            o.metrics_domain.push_str("_stream");
            let Some(socket) = socket.take() else {
                unreachable!("the continuation runs once");
            };
            // SAFETY: the stream client uses the parent's logger and serializer, which belong
            // to the protocol; it counts as one more use of it, so they outlive it.
            let c = make_shared(unsafe {
                Client::with_options_socket(
                    me.get_logger(),
                    me._serializer,
                    o,
                    socket,
                    &me._server_addr,
                    &me._local_addr,
                )
            });
            // SAFETY: `c` was just made; nothing else refers to it.
            let cm = unsafe { c.get_mut_unchecked() };
            cm._proto_use = me._proto_use.another();
            // SAFETY: the parent is started, so pinned where it lives (a started client never
            // moves), and the new client lives in its shared object and never moves from it.
            unsafe {
                weak_from_this(Pin::new_unchecked(&mut *me), Pin::new_unchecked(&mut cm._parent))
            };
            cm._conn._is_stream = true;
            // SAFETY: the client lives in the shared object and never moves from it.
            if let Err(e) = unsafe { Pin::new_unchecked(cm) }.start() {
                return make_exception_future(e);
            }
            let c2 = c.clone();
            let c3 = c.clone();
            // SAFETY: as above.
            unsafe { c.get_mut_unchecked() }
                .await_connection()
                .then(move |()| -> Result<Sink<Out>, ExceptionPtr> {
                    // SAFETY: the parent client outlives the continuation (see above).
                    let me = unsafe { &mut *this };
                    if me._conn._error {
                        return Err(ExceptionPtr::new(closed_error()));
                    }
                    // SAFETY: a Client begins with its Connection (repr(C)).
                    let conn =
                        unsafe { static_pointer_cast(&c2, |p: *mut Client| p as *mut Connection) };
                    let s: XshardConnectionPtr = make_lw_shared(make_foreign(ConnectionPtr(conn)));
                    me._conn.register_stream(c2.get_connection_id(), s.clone());
                    let imp: SharedPtr<dyn crate::engine::rpc::rpc_types::SinkImpl<Out>> = {
                        let p = make_shared(RpcSinkImpl::<S, Out>::new(s));
                        // SAFETY: the cast is the unsizing of the same object.
                        unsafe {
                            static_pointer_cast(&p, |q: *mut RpcSinkImpl<S, Out>| {
                                q as *mut dyn crate::engine::rpc::rpc_types::SinkImpl<Out>
                            })
                        }
                    };
                    Ok(Sink::new(imp))
                })
                .handle_exception(move |eptr| {
                    // If await_connection fails we need to stop the client
                    // before destroying it.
                    let c4 = c3.clone();
                    // SAFETY: the stream client is alive while `c3` holds it.
                    unsafe { c3.get_mut_unchecked() }.stop().then(move |()| {
                        let _keep = c4;
                        make_exception_future::<Sink<Out>>(eptr)
                    })
                })
        })
    }

    /// A sink of a new stream over a new socket.
    pub fn make_stream_sink<S: 'static, Out: 'static>(&mut self) -> Future<Sink<Out>>
    where
        RpcSinkImpl<S, Out>: crate::engine::rpc::rpc_types::SinkImpl<Out>,
    {
        self.make_stream_sink_with::<S, Out>(make_socket())
    }
}

/// A connection accepted by a [`Server`]: runs the handlers of its requests. It starts
/// with its [`Connection`] and lives in a shared pointer.
#[repr(C)]
pub struct ServerConnection {
    _conn: Connection,
    _esft: EnableSharedFromThis,
    _info: ClientInfo,
    _parent_id: ConnectionId,
    _isolation_config: Option<IsolationConfig>,
}

impl std::ops::Deref for ServerConnection {
    type Target = Connection;
    #[inline]
    fn deref(&self) -> &Connection {
        &self._conn
    }
}

impl std::ops::DerefMut for ServerConnection {
    #[inline]
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self._conn
    }
}

crate::shared_from_this_impl!(ServerConnection { _esft });

/// The vtable of `ServerConnection`.
static SERVER_CONNECTION_VTABLE: ConnectionVtable =
    ConnectionVtable { peer_address: ServerConnection::vt_peer_address };

impl ServerConnection {
    unsafe fn vt_peer_address(c: *const Connection) -> SocketAddress {
        // SAFETY: the vtable is only installed in the Connection that begins a
        // ServerConnection.
        unsafe { (*(c as *const ServerConnection)).peer_address() }
    }

    /// A new shared pointer to this connection, sharing the count of the existing ones.
    #[inline]
    pub fn shared_from_this(&self) -> SharedPtr<ServerConnection> {
        // SAFETY: server connections are made by make_shared_esft
        // (protocol::make_server_connection).
        unsafe { SharedFromThis::shared_from_this(self) }
    }

    /// The client of the connection.
    #[inline]
    pub fn info(&mut self) -> &mut ClientInfo {
        &mut self._info
    }

    /// The client of the connection.
    #[inline]
    pub fn info_ref(&self) -> &ClientInfo {
        &self._info
    }

    /// The counters.
    pub fn get_stats(&self) -> Stats {
        let mut res = self._conn._stats;
        res.pending = self.outgoing_queue_length() as u64;
        res
    }

    /// The address of the peer.
    #[inline]
    pub fn peer_address(&self) -> SocketAddress {
        self._info.addr
    }

    // Resources will be released when this goes out of scope
    /// Waits for `memory_consumed` bytes of the memory of the server.
    pub fn wait_for_resources(
        &mut self,
        memory_consumed: usize,
        timeout: Option<RpcTimePoint>,
    ) -> Future<ResourcePermit> {
        // SAFETY: the server outlives its connections and the permits of their requests (its
        // stop() waits for them) and stays pinned once started; only its semaphore field is
        // borrowed, for this call, and nothing else borrows that field meanwhile (the reactor
        // runs one task at a time).
        let sem = unsafe { &mut (*self.get_server())._resources_available };
        // SAFETY: as above; the units return to the same semaphore, which outlives them.
        unsafe {
            match timeout {
                Some(t) => get_units_until(sem, memory_consumed, t),
                None => get_units(sem, memory_consumed),
            }
        }
    }

    /// The memory a request of `serialized_size` bytes is accounted for.
    #[inline]
    pub fn estimate_request_size(&self, serialized_size: usize) -> usize {
        // SAFETY: the server outlives its connections; its limits are not written once it has
        // started, so a shared borrow of that field alone is sound.
        estimate_request_size(unsafe { &(*self.get_server())._limits }, serialized_size)
    }

    /// The largest request the server accepts.
    #[inline]
    pub fn max_request_size(&self) -> usize {
        // SAFETY: as in estimate_request_size().
        unsafe { (*self.get_server())._limits.max_memory }
    }

    /// The server of the connection.
    ///
    /// All the connections of a server share it, so this gives the pointer and not a
    /// reference: dereferencing it is `unsafe`. The server outlives its connections (its
    /// `stop()` waits for them) and stays pinned once started; a caller borrows only the
    /// fields it uses, and only while no other borrow of those fields is live.
    #[inline]
    pub fn get_server(&self) -> *mut Server {
        self._info.server
    }
}

thread_local! {
    /// The servers of this shard by streaming domain. Only this
    /// shard touches it, and never re-entrantly.
    static SERVERS: UnsafeCell<HashMap<StreamingDomainType, *mut Server, FxBuildHasher>> = const { UnsafeCell::new(HashMap::with_hasher(FxBuildHasher::new())) };
}

/// Runs `f` on the servers of this shard.
#[inline]
fn with_servers<R>(
    f: impl FnOnce(&mut HashMap<StreamingDomainType, *mut Server, FxBuildHasher>) -> R,
) -> R {
    // SAFETY: the map is thread-local and `f` does not reach it again.
    SERVERS.with(|s| f(unsafe { &mut *s.get() }))
}

/// An rpc server: the listening port and all accepted connections.
pub struct Server {
    _proto: *mut dyn ProtocolBase,
    _ss: ServerSocket,
    _limits: ResourceLimits,
    _resources_available: RpcSemaphore,
    _conns: HashMap<ConnectionId, SharedPtr<ServerConnection>, FxBuildHasher>,
    _ss_stopped: Promise<()>,
    _reply_gate: Gate,
    _options: ServerOptions,
    _shutdown: bool,
    _next_client_id: u64,
    /// Keeps the protocol from being dropped while the server uses it.
    _proto_use: ProtocolUse,
}

impl Server {
    /// Calls `f` with each connection.
    ///
    /// Each call gets its own copy of the
    /// connection's pointer (a non-atomic increment), so `f` never holds a reference into the
    /// map. `f` reaches the server only through `ServerConnection::get_server()`, whose
    /// dereference is `unsafe`: it must not add or remove connections while the loop runs.
    pub fn foreach_connection<F: FnMut(SharedPtr<ServerConnection>)>(&self, mut f: F) {
        for c in self._conns.values() {
            f(c.clone());
        }
    }

    /// The gate of the replies in flight.
    #[inline]
    pub fn reply_gate(&self) -> Pin<&Gate> {
        // SAFETY: the gate is reached through the server of a connection, and connections exist
        // only once the server has started, pinned (`start(self: Pin<&mut Self>)`); it is never
        // moved out of the server.
        unsafe { Pin::new_unchecked(&self._reply_gate) }
    }

    /// The gate of the replies in flight, reached through the server pointer that the
    /// connections share, without borrowing the rest of the server.
    ///
    /// # Safety
    /// `this` points to a started server that stays alive and in place for `'a`. The gate
    /// keeps its state in cells, so other shared borrows of it may coexist.
    #[inline]
    pub unsafe fn reply_gate_of<'a>(this: *const Server) -> Pin<&'a Gate> {
        // SAFETY: the caller's contract; the borrow covers the gate field only, which is never
        // moved out of a started (pinned) server.
        unsafe { Pin::new_unchecked(&*core::ptr::addr_of!((*this)._reply_gate)) }
    }
}

/// The server function of a verb: it gets the connection, the optional deadline, the
/// message id, the request frame and the gate holder that keeps the handler registered.
pub type RpcHandlerFunc = Box<
    dyn FnMut(SharedPtr<ServerConnection>, Option<RpcTimePoint>, i64, RcvBuf, Holder) -> Future<()>,
>;

/// A registered verb handler: its scheduling group, function and the gate that tracks
/// its running calls.
pub struct RpcHandler {
    /// The scheduling group of the handler.
    pub sg: SchedulingGroup,
    /// The handler.
    pub func: RpcHandlerFunc,
    /// Closed to unregister the handler.
    pub use_gate: Gate,
}

/// A handler together with the gate holder that keeps it registered while it runs.
pub struct HandlerWithHolder {
    /// The handler, which stays registered while `holder` is held.
    pub handler: *mut RpcHandler,
    /// Holds the gate of the handler.
    pub holder: Holder,
}

/// The serializer-independent interface of a [`Protocol`], as servers and connections use it.
pub trait ProtocolBase {
    /// Makes the connection of an accepted socket.
    ///
    /// # Safety
    /// `server` is the live server that accepted `fd`; it stops its connections before it
    /// goes away.
    unsafe fn make_server_connection(
        &mut self,
        server: *mut Server,
        fd: ConnectedSocket,
        addr: SocketAddress,
        id: ConnectionId,
    ) -> SharedPtr<ServerConnection>;
    /// The handler of verb `msg_id`, if any (used by the server).
    fn get_handler(&mut self, msg_id: u64) -> Option<HandlerWithHolder>;
}

/// Defines a protocol for communication between a server and a client.
///
/// A protocol is defined by a `Serializer` and a `MsgType`. The `Serializer` is
/// responsible for serializing and unserializing all types used as arguments and
/// return types used in the protocol. The `Serializer` is expected to define a
/// [`Read`](crate::engine::rpc::rpc_impl::Read) and
/// [`Write`](crate::engine::rpc::rpc_impl::Write) implementation for each such type `T`,
/// over a [`SerializerInput`](crate::engine::rpc::rpc_impl::SerializerInput) or a
/// [`SerializerOutput`](crate::engine::rpc::rpc_impl::SerializerOutput); the [`Type`](crate::engine::rpc::rpc_types::Type) tag
/// passed to `read` selects the type.
/// `MsgType` defines the type to be used as the message id, the id which is
/// used to identify different messages used in the protocol. These are also
/// often referred to as "verbs". The client will use the message id, to
/// specify the remote method (verb) to invoke on the server. The server uses
/// the message id to dispatch the incoming call to the right handler.
/// `MsgType` should be hashable and serializable. It is preferable to use enum
/// for message types, but do not forget to provide hash function for it.
///
/// Use register_handler() on the server to define the available verbs and the
/// code to be executed when they are invoked by clients. Use make_client() on
/// the client to create a matching callable that can be used to invoke the
/// verb on the server and wait for its result. Note that register_handler()
/// also returns a client, that can be used to invoke the registered verb on
/// another node (given that the other node has the same verb). This is useful
/// for symmetric protocols, where two or more nodes all have servers as well as
/// connect to the other nodes as clients.
///
/// Use protocol::server to listen for and accept incoming connections on the
/// server and protocol::client to establish connections to the server.
/// Note that registering the available verbs can be done before/after
/// listening for connections, but best to ensure that by the time incoming
/// requests are to be expected, all the verbs are set-up.
///
/// ## Configuration
///
/// TODO
///
/// ## Isolation
///
/// RPC supports isolating verb handlers from each other. There are two ways to
/// achieve this: per-handler isolation (the old way) and per-connection
/// isolation (the new way). If no isolation is configured, all handlers will be
/// executed in the context of the scheduling_group in which the
/// protocol::server was created.
///
/// Per-handler isolation (the old way) can be configured by using the
/// register_handler() overload which takes a scheduling_group. When invoked,
/// the body of the handler will be executed from the context of the configured
/// scheduling_group.
///
/// Per-connection isolation (the new way) is a more flexible mechanism that
/// requires user application provided logic to determine how connections are
/// isolated. This mechanism has two parts, the server and the client part.
/// The client configures isolation by setting client_options::isolation_cookie.
/// This cookie is an opaque (to the RPC layer) string that is to be interpreted
/// on the server using user application provided logic. The application
/// provides this logic to the server by setting
/// resource_limits::isolate_connection to an appropriate handler function, that
/// interprets the opaque cookie and resolves it to an isolation_config. The
/// scheduling_group in the former will be used not just to execute all verb
/// handlers, but also the connection loop itself, hence providing better
/// isolation.
///
/// There a few gotchas related to mixing the two isolation mechanisms. This can
/// happen when the application is updated and one of the client/server is
/// still using the old/new mechanism. In general per-connection isolation
/// overrides the per-handler one. If both are set up, the former will determine
/// the scheduling_group context for the handlers. If the client is not
/// configured to send an isolation cookie, the server's
/// resource_limits::isolate_connection will not be invoked and the server will
/// fall back to per-handler isolation if configured. If the client is
/// configured to send an isolation cookie but the server doesn't have a
/// resource_limits::isolate_connection configured, it will use
/// default_isolate_connection() to interpret the cookie. Note that this still
/// overrides the per-handler isolation if any is configured. If the server is
/// so old that it doesn't have the per-connection isolation feature at all, it
/// will of course just use the per-handler one, if configured.
///
/// ## Compatibility
///
/// TODO
///
/// `S` is the serializer for the protocol; `M` is the type to be used as the message id
/// or verb id.
///
/// The handlers are boxed, so that their gates stay in place while the map grows. Servers
/// and clients are made by `Protocol::new_server` and `Protocol::new_client`.
///
/// The servers, the clients and their connections keep pointers to the protocol (its
/// handlers, logger and serializer), so it does not move: it is `!Unpin` and the
/// constructors of servers and clients take it pinned. The protocol must outlive them: it
/// counts them and aborts if it is dropped while one is alive.
pub struct Protocol<S: 'static, M: RpcMsgType = u32> {
    pub(crate) _handlers: HashMap<M, Box<RpcHandler>, FxBuildHasher>,
    pub(crate) _serializer: S,
    pub(crate) _logger: Logger,
    _users: Cell<usize>,
    _pinned: PhantomPinned,
}

/// A server's or client's use of its protocol: counted while alive (see `Protocol`).
pub(crate) struct ProtocolUse(*const Cell<usize>);

impl ProtocolUse {
    /// Not made by a protocol (the low-level constructors).
    #[inline]
    pub(crate) const fn none() -> Self {
        ProtocolUse(std::ptr::null())
    }

    /// Counts one more user of the protocol whose counter is `users`.
    ///
    /// # Safety
    /// `users` is the counter of a pinned protocol, which aborts on drop while it is not zero.
    #[inline]
    unsafe fn new(users: &Cell<usize>) -> Self {
        users.set(users.get() + 1);
        ProtocolUse(users)
    }

    /// Another use of the same protocol (a stream client of a client).
    #[inline]
    pub(crate) fn another(&self) -> Self {
        if !self.0.is_null() {
            // SAFETY: a set counter belongs to a protocol that outlives every use (it aborts
            // otherwise), and this use is alive.
            unsafe { (*self.0).set((*self.0).get() + 1) };
        }
        ProtocolUse(self.0)
    }
}

impl Drop for ProtocolUse {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: as in another().
            unsafe { (*self.0).set((*self.0).get() - 1) };
        }
    }
}

impl<S: 'static, M: RpcMsgType> Protocol<S, M> {
    /// A protocol with `serializer` and no handlers.
    pub fn new(serializer: S) -> Self {
        Protocol {
            _handlers: HashMap::default(),
            _serializer: serializer,
            _logger: Logger::default(),
            _users: Cell::new(0),
            _pinned: PhantomPinned,
        }
    }

    /// The protocol's own pointers, from its pinned place.
    #[inline]
    fn pinned_parts(
        self: Pin<&mut Self>,
    ) -> (*mut dyn ProtocolBase, *const Logger, *mut (), ProtocolUse) {
        // SAFETY: the protocol is not moved out of the pin; it stays at this address until it
        // is dropped, and its drop aborts while the returned use (a server or a client) is
        // alive, so the pointers stay valid for as long as their user.
        unsafe {
            let me = self.get_unchecked_mut();
            let users = ProtocolUse::new(&me._users);
            let s: *mut S = &mut me._serializer;
            (me as *mut Self as *mut dyn ProtocolBase, &me._logger, s as *mut (), users)
        }
    }

    /// A server of this protocol listening on `addr`, with `memory_limit`.
    pub fn new_server(
        self: Pin<&mut Self>,
        addr: &SocketAddress,
        memory_limit: ResourceLimits,
    ) -> Result<Server, ExceptionPtr> {
        let (p, _, _, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the server (see pinned_parts()).
        let mut server = unsafe { Server::new(p, addr, memory_limit) }?;
        server._proto_use = users;
        Ok(server)
    }

    /// A server of this protocol listening on `addr`, with options `opts` and `memory_limit`.
    pub fn new_server_with_options(
        self: Pin<&mut Self>,
        opts: ServerOptions,
        addr: &SocketAddress,
        memory_limit: ResourceLimits,
    ) -> Result<Server, ExceptionPtr> {
        let (p, _, _, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the server (see pinned_parts()).
        let mut server = unsafe { Server::with_options(p, opts, addr, memory_limit) }?;
        server._proto_use = users;
        Ok(server)
    }

    /// A server over an already listening `socket`, with `memory_limit`; the options of
    /// this constructor are not used.
    pub fn new_server_from_socket(
        self: Pin<&mut Self>,
        socket: ServerSocket,
        memory_limit: ResourceLimits,
        _opts: ServerOptions,
    ) -> Server {
        let (p, _, _, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the server (see pinned_parts()).
        let mut server =
            unsafe { Server::from_socket(p, socket, memory_limit, ServerOptions::default()) };
        server._proto_use = users;
        server
    }

    /// A server of this protocol over the listening `socket`, with options `opts` and
    /// `memory_limit`.
    pub fn new_server_with_options_socket(
        self: Pin<&mut Self>,
        opts: ServerOptions,
        socket: ServerSocket,
        memory_limit: ResourceLimits,
    ) -> Server {
        let (p, _, _, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the server (see pinned_parts()).
        let mut server = unsafe { Server::with_options_socket(p, opts, socket, memory_limit) };
        server._proto_use = users;
        server
    }

    /// Creates a client of this protocol that connects to the remote address `addr` from
    /// the local address `local`.
    pub fn new_client(self: Pin<&mut Self>, addr: &SocketAddress, local: &SocketAddress) -> Client {
        let (_, l, s, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the client (see pinned_parts()).
        let mut client = unsafe { Client::new(&*l, s, addr, local) };
        client._proto_use = users;
        client
    }

    /// A client of this protocol connecting to `addr` from `local`, with `options`.
    pub fn new_client_with_options(
        self: Pin<&mut Self>,
        options: ClientOptions,
        addr: &SocketAddress,
        local: &SocketAddress,
    ) -> Client {
        let (_, l, s, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the client (see pinned_parts()).
        let mut client = unsafe { Client::with_options(&*l, s, options, addr, local) };
        client._proto_use = users;
        client
    }

    /// Creates a client of this protocol that connects to the remote address `addr` from
    /// the local address `local` through `socket`.
    pub fn new_client_with_socket(
        self: Pin<&mut Self>,
        socket: Socket,
        addr: &SocketAddress,
        local: &SocketAddress,
    ) -> Client {
        let (_, l, s, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the client (see pinned_parts()).
        let mut client = unsafe { Client::with_socket(&*l, s, socket, addr, local) };
        client._proto_use = users;
        client
    }

    /// A client of this protocol connecting to `addr` from `local` over `socket`, with
    /// `options`.
    pub fn new_client_with_options_socket(
        self: Pin<&mut Self>,
        options: ClientOptions,
        socket: Socket,
        addr: &SocketAddress,
        local: &SocketAddress,
    ) -> Client {
        let (_, l, s, users) = self.pinned_parts();
        // SAFETY: the protocol is pinned and outlives the client (see pinned_parts()).
        let mut client =
            unsafe { Client::with_options_socket(&*l, s, options, socket, addr, local) };
        client._proto_use = users;
        client
    }

    /// Set a logger to be used to log messages.
    pub fn set_logger(self: Pin<&mut Self>, logger: Option<&'static EngineLogger>) {
        // SAFETY: only the logger's target changes; the protocol is not moved.
        unsafe { self.get_unchecked_mut() }._logger.set(logger);
    }

    /// The logger of the protocol.
    #[inline]
    pub fn get_logger(&self) -> &Logger {
        &self._logger
    }

    /// Returns whether any handler is registered.
    ///
    /// A debugging helper: use it only for debugging and do not rely on it.
    #[inline]
    pub fn has_handlers(&self) -> bool {
        !self._handlers.is_empty()
    }

    pub(crate) fn register_receiver(
        &mut self,
        t: M,
        handler: RpcHandler,
    ) -> Result<(), ExceptionPtr> {
        match self._handlers.entry(t) {
            std::collections::hash_map::Entry::Occupied(_) => Err(ExceptionPtr::new(RuntimeError(
                "registered handler already exists".to_string(),
            ))),
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(Box::new(handler));
                Ok(())
            }
        }
    }
}

impl<S: 'static, M: RpcMsgType> Drop for Protocol<S, M> {
    fn drop(&mut self) {
        if self._users.get() != 0 {
            // The protocol must outlive its servers and clients; dropping it earlier would
            // leave them with dangling pointers.
            eprintln!(
                "rpc::protocol destroyed while {} servers or clients use it",
                self._users.get()
            );
            std::process::abort();
        }
    }
}

impl<S: 'static, M: RpcMsgType> ProtocolBase for Protocol<S, M> {
    unsafe fn make_server_connection(
        &mut self,
        server: *mut Server,
        fd: ConnectedSocket,
        addr: SocketAddress,
        id: ConnectionId,
    ) -> SharedPtr<ServerConnection> {
        let s: *mut S = &mut self._serializer;
        // SAFETY: the protocol is pinned and outlives its servers (it aborts otherwise), and
        // the caller's contract covers `server`.
        make_shared_esft(unsafe {
            ServerConnection::new(server, fd, addr, &self._logger, s as *mut (), id)
        })
    }

    fn get_handler(&mut self, msg_id: u64) -> Option<HandlerWithHolder> {
        self.get_handler_impl(msg_id)
    }
}

// ---- snd_buf ----

impl SndBuf {
    /// A frame of `size` bytes, in one buffer or in chunks of `SndBuf::CHUNK_SIZE`.
    pub fn with_size(size: usize) -> Self {
        let mut size_ = size;
        let bufs = if size <= Self::CHUNK_SIZE {
            Bufs::One(TemporaryBuffer::new(size))
        } else {
            let mut v = Vec::with_capacity(size.div_ceil(Self::CHUNK_SIZE));
            while size_ != 0 {
                let b = TemporaryBuffer::<u8>::new(Self::CHUNK_SIZE.min(size_));
                size_ -= b.size();
                v.push(b);
            }
            Bufs::Many(v)
        };
        SndBuf { size: size as u32, bufs, su: None }
    }

    /// The first buffer.
    pub fn front(&mut self) -> &mut TemporaryBuffer<u8> {
        match &mut self.bufs {
            Bufs::One(one) => one,
            Bufs::Many(v) => &mut v[0],
        }
    }
}

/// Frames that `make_shard_local_buffer_copy` copies.
pub trait ShardLocalBuffer: Sized + 'static {
    /// An empty frame of `size` bytes.
    fn sized(size: u32) -> Self;
    /// The size of the frame.
    fn size_of(&self) -> u32;
    /// The buffers of the frame.
    fn bufs(&self) -> &Bufs;
    /// The buffers of the frame, mutable.
    fn bufs_mut(&mut self) -> &mut Bufs;
}

impl ShardLocalBuffer for SndBuf {
    #[inline]
    fn sized(size: u32) -> Self {
        SndBuf { size, bufs: Bufs::default(), su: None }
    }
    #[inline]
    fn size_of(&self) -> u32 {
        self.size
    }
    #[inline]
    fn bufs(&self) -> &Bufs {
        &self.bufs
    }
    #[inline]
    fn bufs_mut(&mut self) -> &mut Bufs {
        &mut self.bufs
    }
}

impl ShardLocalBuffer for RcvBuf {
    #[inline]
    fn sized(size: u32) -> Self {
        RcvBuf::with_size(size as usize)
    }
    #[inline]
    fn size_of(&self) -> u32 {
        self.size
    }
    #[inline]
    fn bufs(&self) -> &Bufs {
        &self.bufs
    }
    #[inline]
    fn bufs_mut(&mut self) -> &mut Bufs {
        &mut self.bufs
    }
}

// Make a copy of a remote buffer. No data is actually copied, only pointers and
// a deleter of a new buffer takes care of deleting the source buffer
/// A frame of this shard sharing the memory of a frame of another shard.
pub fn make_shard_local_buffer_copy<T: ShardLocalBuffer>(mut org: ForeignPtr<Box<T>>) -> T {
    if org.get_owner_shard() == this_shard_id() {
        // The frames handed between shards are never null.
        // SAFETY: on the owner shard (checked above), which keeps the frame.
        let Some(frame) = (unsafe { org.release() }) else {
            unreachable!("null frame handed to another shard")
        };
        return *frame;
    }
    // SAFETY: the frame of another shard is only read here (its size and the address of its
    // buffers); its owner does not touch it while it is handed over.
    let Some(frame) = (unsafe { org.get_unchecked() }) else {
        unreachable!("null frame handed to another shard")
    };
    let mut buf = T::sized(frame.size_of());
    // The frame lives in a Box, so its buffers stay in place when `org` moves into the
    // deleter, which keeps them alive as long as the new buffers.
    let bufs: *const Bufs = frame.bufs();
    // SAFETY: `bufs` points into the boxed frame owned by the deleter. The
    // data pointers are taken from the buffers as writable pointers; the bytes they
    // point at are not part of the `Bufs` borrowed here.
    match unsafe { &*bufs } {
        Bufs::One(one) => {
            let (p, n) = (one.get() as *mut u8, one.size());
            // SAFETY: `p` and `n` are the bytes of the source buffer, which the deleter keeps
            // alive with the frame.
            *buf.bufs_mut() =
                Bufs::One(unsafe { TemporaryBuffer::with_deleter(p, n, make_object_deleter(org)) });
        }
        Bufs::Many(orgbufs) => {
            let mut newbufs = Vec::with_capacity(orgbufs.len());
            let mut d = make_object_deleter(org);
            for b in orgbufs.iter() {
                // SAFETY: as above, for each buffer of the frame.
                newbufs.push(unsafe {
                    TemporaryBuffer::with_deleter(b.get() as *mut u8, b.size(), d.share())
                });
            }
            *buf.bufs_mut() = Bufs::Many(newbufs);
        }
    }
    buf
}

/// Logs `log: <what>` about the peer of a connection.
pub(crate) fn log_exception(
    logger: &Logger,
    peer: &SocketAddress,
    level: LogLevel,
    log: &str,
    eptr: &ExceptionPtr,
) {
    let s = match eptr.get() {
        Some(ex) => ex.to_string(),
        None => "unknown exception".to_string(),
    };
    let formatted = format!("{}: {}", log, s);
    logger.addr_level(peer, level, &formatted);
}

// ---- connection ----

impl Connection {
    /// Compresses `buf` and writes its compressed size in front of it.
    pub(crate) fn compress(&mut self, buf: SndBuf) -> Result<SndBuf, ExceptionPtr> {
        if let Some(c) = self._compressor.as_mut() {
            let mut buf = c.compress(4, buf)?;
            const _: () = assert!(SndBuf::CHUNK_SIZE >= 4, "send buffer chunk size is too small");
            let size = buf.size - 4;
            // SAFETY: the compressor left 4 bytes of head space in the first buffer.
            unsafe { write_le::<u32>(buf.front().get_write(), size) };
            return Ok(buf);
        }
        Ok(buf)
    }

    /// Writes the buffers of `buf` to the socket.
    pub(crate) fn send_buffer(&mut self, buf: SndBuf) -> Future<()> {
        let this: *mut Connection = self;
        match buf.bufs {
            Bufs::One(b) => self._write_buf.write_buffer(b),
            // The vector moves into the loop, which keeps it as `do_with` does.
            Bufs::Many(ar) => do_for_each(ar, move |b: TemporaryBuffer<u8>| {
                // SAFETY: the connection outlives its send loop.
                unsafe { (*this)._write_buf.write_buffer(b) }
            }),
        }
    }

    /// Sends the frame of `d`, which stays in its HostBox until the send is done.
    pub(crate) fn send_entry(&mut self, d: *mut OutgoingEntry) -> Future<()> {
        // `futurize_invoke`: a failure of compress() becomes the failed future.
        // SAFETY: the caller keeps the entry alive until the returned future resolves.
        let d = unsafe { &mut *d };
        // left_ms is 0 when no timer is set (the server takes 0 as "no timeout"). When a
        // timer is set, an entry whose deadline already passed is dropped without being sent
        // (as the timer would have withdrawn it if it were not at the front of the queue);
        // otherwise the remaining time goes with it so that the server honours the deadline
        // too. Sending the negative remainder wrapped to a timeout ~2^63 ms away.
        let expire = d.t.get_timeout();
        let mut left_ms: u64 = 0;
        if expire != RpcTimePoint::from_nanos(0) {
            let ns = expire.nanos_since_epoch() - RpcClockType::now().nanos_since_epoch();
            let ms = ns / 1_000_000;
            if ms <= 0 {
                return make_ready_future(());
            }
            left_ms = ms as u64;
        }
        if d.buf.size != 0 && self._propagate_timeout {
            const _: () = assert!(SndBuf::CHUNK_SIZE >= 8, "send buffer chunk size is too small");
            if self._timeout_negotiated {
                // SAFETY: the request frame has room for the timeout in its first chunk.
                unsafe { write_le::<u64>(d.buf.front().get_write(), left_ms) };
            } else {
                d.buf.front().trim_front(8);
                d.buf.size -= 8;
            }
        }
        let buf = match self.compress(std::mem::take(&mut d.buf)) {
            Ok(b) => b,
            Err(e) => return make_exception_future(e),
        };
        let this: *mut Connection = self;
        self.send_buffer(buf).then(move |()| {
            // SAFETY: the connection outlives its send loop.
            let me = unsafe { &mut *this };
            me._stats.sent_messages += 1;
            me._write_buf.flush()
        })
    }

    /// Marks the connection negotiated: the queued frames may go.
    pub(crate) fn set_negotiated(&mut self) {
        if let Some(n) = self._negotiated.as_mut() {
            n.set_value(());
        }
        self._negotiated = None;
    }

    /// Fails the queued frames and closes the output.
    pub(crate) fn stop_send_loop(&mut self, ex: Option<ExceptionPtr>) -> Future<()> {
        self._error = true;
        if self._connected {
            self._fd.shutdown_output();
        }
        let ex = match ex {
            Some(e) => e,
            None => ExceptionPtr::new(closed_error()),
        };
        while let Some(back) = self._outgoing_queue.back() {
            let it = back.as_ptr();
            // Cancel all but front entry normally. The front entry is sitting in the
            // send_entry() and cannot be withdrawn, except when _negotiated is still
            // engaged. In the latter case when it will be aborted below the entry's
            // continuation will not be called and its done promise will not resolve
            // the _outgoing_queue_ready, so do it here
            if self._outgoing_queue.front().map(|f| f.as_ptr()) != Some(it) {
                self.withdraw(it, Some(ex.clone()));
            } else {
                if self._negotiated.is_some() {
                    // SAFETY: a linked entry is alive in its HostBox.
                    unsafe {
                        HostedPromise::set_exception(OutgoingEntry::hosted_promise(it), ex.clone())
                    };
                }
                break;
            }
        }
        if let Some(n) = self._negotiated.as_mut() {
            n.set_exception(ex);
        }
        let this: *mut Connection = self;
        let ready = std::mem::replace(&mut self._outgoing_queue_ready, make_ready_future(()));
        let sink_closed =
            std::mem::replace(&mut self._sink_closed_future, make_ready_future(false));
        crate::engine::core::when_all::when_all((ready, sink_closed)).then(
            move |(mut ready, sink_closed): (Future<()>, Future<bool>)| {
                // _outgoing_queue_ready might be exceptional if queue drain or
                // _negotiated abortion set it such
                ready.ignore_ready_future();
                // _sink_closed_future is never exceptional
                let sink_closed = matches!(sink_closed.get(), Ok(true));
                // SAFETY: the connection outlives stop_send_loop().
                let me = unsafe { &mut *this };
                if me._connected && !sink_closed {
                    me._write_buf.close()
                } else {
                    make_ready_future(())
                }
            },
        )
    }

    /// Takes over a connected socket.
    pub fn set_socket(&mut self, fd: ConnectedSocket) -> Result<(), ExceptionPtr> {
        if self._connected {
            return Err(ExceptionPtr::new(RuntimeError("already connected".to_string())));
        }
        self.set_socket_unconnected(fd);
        Ok(())
    }

    /// The body of set_socket() once `_connected` is known to be false.
    fn set_socket_unconnected(&mut self, mut fd: ConnectedSocket) {
        self._read_buf =
            fd.input(crate::engine::net::api::ConnectedSocketInputStreamConfig::default());
        self._write_buf = fd.output(8192);
        self._fd = fd;
        self._connected = true;
    }

    /// Sends the negotiation frame with `features`.
    pub fn send_negotiation_frame(&mut self, features: FeatureMap) -> Future<()> {
        let negotiation_frame_feature_record_size =
            |e: (&ProtocolFeatures, &Sstring)| 8 + e.1.size() as u32;
        let extra_len: u32 = features.iter().map(negotiation_frame_feature_record_size).sum();
        let mut reply = TemporaryBuffer::<u8>::new(
            std::mem::size_of::<NegotiationFrame>() + extra_len as usize,
        );
        let mut p = reply.get_write();
        // SAFETY: the buffer holds the magic, the length and every feature record, which
        // is what is written below.
        unsafe {
            std::ptr::copy_nonoverlapping(RPC_MAGIC.as_ptr(), p, 8);
            p = p.add(8);
            write_le::<u32>(p, extra_len);
            p = p.add(4);
            for (k, v) in &features {
                write_le::<u32>(p, k.0);
                p = p.add(4);
                write_le::<u32>(p, v.size() as u32);
                p = p.add(4);
                std::ptr::copy_nonoverlapping(v.as_bytes().as_ptr(), p, v.size());
                p = p.add(v.size());
            }
        }
        let this: *mut Connection = self;
        self._write_buf.write_buffer(reply).then(move |()| {
            // SAFETY: the connection outlives its negotiation.
            let me = unsafe { &mut *this };
            me._stats.sent_messages += 1;
            me._write_buf.flush()
        })
    }

    /// Takes `it` out of the queue, resolving its continuation now.
    pub(crate) fn withdraw(&mut self, it: *mut OutgoingEntry, ex: Option<ExceptionPtr>) {
        // SAFETY: `it` is a linked entry of this queue, alive in its HostBox.
        engine_assert!(unsafe { (*it).is_linked() });

        // SAFETY: as above; an entry that is not the front one has a previous entry.
        let pit = match unsafe { self._outgoing_queue.prev_of(it) } {
            Some(p) => p.as_ptr(),
            None => unreachable!("only entries behind the front one are withdrawn"),
        };
        // Previous entry's (pit's) done future will schedule current entry (it)
        // continuation. Similarly, it.done will schedule next entry continuation
        // or will resolve _outgoing_queue_ready future.
        //
        // To withdraw "it" we need to do two things:
        // - make pit.done resolve it->next (some time later)
        // - resolve "it"'s continuation right now
        //
        // The latter is achieved by resolving pit.done immediatelly, the former
        // by moving it.done into pit.done. For simplicity (verging on obscurity?)
        // both done's are just swapped and "it" resolves its new promise

        // SAFETY: `it` and `pit` are distinct live entries of the queue, reached only
        // through these pointers here.
        unsafe {
            HostedPromise::swap_with(
                OutgoingEntry::hosted_promise(it),
                OutgoingEntry::hosted_promise(pit),
            );
            (*it).uncancellable();
            (*it).unlink();
            match ex {
                None => HostedPromise::set_value(OutgoingEntry::hosted_promise(it), ()),
                Some(e) => HostedPromise::set_exception(OutgoingEntry::hosted_promise(it), e),
            }
        }
    }

    /// Queues `buf`, sent after the frames before it unless it times out or is canceled.
    pub fn send(
        &mut self,
        buf: SndBuf,
        timeout: Option<RpcTimePoint>,
        cancel: Option<Pin<&mut crate::engine::rpc::rpc_types::Cancellable>>,
    ) -> Future<()> {
        // SAFETY: the pointer is stored only in the entry's pcancel, which the cancellable
        // reaches through send_back_pointer (set below).
        let cancel = cancel.map_or(std::ptr::null_mut(), |c| unsafe { c.as_registered() });
        if !self._error && !self._stream_closing {
            if let Some(t) = timeout
                && t <= RpcClockType::now()
            {
                return make_ready_future(());
            }

            let p = HostBox::new(OutgoingEntry::new(buf));
            let d: *mut OutgoingEntry = p.as_ptr();
            // SAFETY: the entry stays in its HostBox until its continuation ends, and the
            // connection does not move while its queue is not empty.
            unsafe { self._outgoing_queue.push_back(d) };
            self._outgoing_queue_size += 1;
            let this: *mut Connection = self;
            let deleter = move || {
                // SAFETY: the timer and the cancellable are disarmed (uncancellable(),
                // ~outgoing_entry) before the entry or the connection go away.
                let me = unsafe { &mut *this };
                // Front entry is most likely (unless _negotiated is unresolved, check enqueue_zero_frame()) sitting
                // inside send_entry() continuations and thus it cannot be cancelled.
                if me._outgoing_queue.front().map(|f| f.as_ptr()) != Some(d) {
                    me.withdraw(d, None);
                }
            };

            if let Some(t) = timeout {
                // SAFETY: the entry, and so its timer, does not move.
                unsafe {
                    Pin::new_unchecked(&mut (*d).t).set_callback(deleter);
                    Pin::new_unchecked(&mut (*d).t).arm(t, None);
                }
            }
            if !cancel.is_null() {
                // SAFETY: the caller's cancellable is pinned and alive; it clears these
                // pointers when it is dropped or reassigned (cancellable::assign_from).
                unsafe {
                    (*cancel).cancel_send = NoncopyableFunction::<fn()>::new(deleter);
                    (*cancel).send_back_pointer = &mut (*d).pcancel;
                    (*d).pcancel = cancel;
                }
            }

            // New entry should continue (run its .then() continuation) after _outgoing_queue_ready
            // resolves. Next entry will need to do the same after this entry's done resolves.
            // Thus -- replace _outgoing_queue_ready with d's future and chain its continuation
            // on ..._ready's old value.
            // SAFETY: the entry is alive in its HostBox.
            let next = unsafe { HostedPromise::get_future(OutgoingEntry::hosted_promise(d)) };
            std::mem::replace(&mut self._outgoing_queue_ready, next).then(move |()| -> Future<()> {
                // SAFETY: the connection outlives its send loop.
                let me = unsafe { &mut *this };
                me._outgoing_queue_size -= 1;
                if !p.is_linked() {
                    // If withdrawn the entry is unlinked and this closure is fired right at once
                    return make_ready_future(());
                }

                let d: *mut OutgoingEntry = p.as_ptr();
                // SAFETY: `p`, moved into this continuation, keeps the entry alive.
                unsafe { (*d).uncancellable() };
                me.send_entry(d).then_wrapped(move |mut f: Future<()>| {
                    if f.failed() {
                        f.ignore_ready_future();
                        // SAFETY: as above.
                        unsafe { (*this).abort() };
                    }
                    // SAFETY: `p`, moved here, keeps the entry alive.
                    unsafe {
                        HostedPromise::set_value(OutgoingEntry::hosted_promise(p.as_ptr()), ())
                    };
                })
            })
        } else {
            make_exception_future(ExceptionPtr::new(closed_error()))
        }
    }

    /// Stops receiving: the connection loop ends.
    pub fn abort(&mut self) {
        if !self._error {
            self._error = true;
            self._fd.shutdown_input();
        }
    }

    /// Aborts the connection and resolves when its loop is done (`shutdown_input` cannot
    /// fail, so there is no shutdown failure to log).
    pub fn stop(&mut self) -> Future<()> {
        self.abort();
        self._stopped.get_future()
    }
}

/// Whether `buf` has `expected` bytes; logs `log` when it is short but not empty.
fn verify_frame(c: &Connection, buf: &TemporaryBuffer<u8>, expected: usize, log: &str) -> bool {
    if buf.size() != expected {
        if buf.size() != 0 {
            c.get_logger().addr(&c.peer_address(), &Sstring::from(log));
        }
        return false;
    }
    true
}

/// Reads the negotiation frame of the peer.
fn receive_negotiation_frame(c: *mut Connection, in_: *mut InputStream<u8>) -> Future<FeatureMap> {
    // SAFETY: the connection and its input stream outlive
    // the negotiation.
    unsafe { &mut *in_ }.read_exactly(std::mem::size_of::<NegotiationFrame>()).then(
        move |neg: TemporaryBuffer<u8>| -> Future<FeatureMap> {
            // SAFETY: `c` points at the connection, which outlives its negotiation; the continuation only reads it.
            let cr = unsafe { &*c };
            if !verify_frame(
                cr,
                &neg,
                std::mem::size_of::<NegotiationFrame>(),
                "unexpected eof during negotiation frame",
            ) {
                return make_exception_future(ExceptionPtr::new(closed_error()));
            }
            let mut frame = NegotiationFrame { magic: [0; 8], len: 0 };
            // SAFETY: the buffer has the size of a negotiation frame.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    neg.get(),
                    frame.magic.as_mut_ptr(),
                    frame.magic.len(),
                );
                frame.len = read_le::<u32>(neg.get().add(8));
            }
            if &frame.magic != RPC_MAGIC {
                let m = &frame.magic;
                cr.get_logger().addr(
                    &cr.peer_address(),
                    &Sstring::from(format!(
                        "wrong protocol magic: {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                        m[0], m[1], m[2], m[3], m[4], m[5], m[6], m[7]
                    )),
                );
                return make_exception_future(ExceptionPtr::new(closed_error()));
            }
            let len = frame.len;
            // SAFETY: `in_` is the connection's input stream, which outlives the negotiation; the previous read has resolved, so no other reference to the stream is live.
            unsafe { &mut *in_ }.read_exactly(len as usize).then(
                move |extra: TemporaryBuffer<u8>| -> Result<FeatureMap, ExceptionPtr> {
                    // SAFETY: `c` points at the connection, which outlives its negotiation; the continuation only reads it.
                    let cr = unsafe { &*c };
                    if extra.size() != len as usize {
                        cr.get_logger().addr(
                            &cr.peer_address(),
                            &Sstring::from("unexpected eof during negotiation frame"),
                        );
                        return Err(ExceptionPtr::new(closed_error()));
                    }
                    let mut map = FeatureMap::new();
                    let mut p = extra.get();
                    // SAFETY: `end` is the end of the buffer; every read below is bounds
                    // checked against it first.
                    let end = unsafe { p.add(extra.size()) };
                    while p != end {
                        // SAFETY: p and end point into the same buffer.
                        let left = unsafe { end.offset_from(p) } as usize;
                        if left < 8 {
                            cr.get_logger().addr(
                                &cr.peer_address(),
                                &Sstring::from("bad feature data format in negotiation frame"),
                            );
                            return Err(ExceptionPtr::new(closed_error()));
                        }
                        // SAFETY: at least 8 bytes are left.
                        let (feature, f_len) = unsafe {
                            (ProtocolFeatures(read_le::<u32>(p)), read_le::<u32>(p.add(4)))
                        };
                        // SAFETY: as above.
                        p = unsafe { p.add(8) };
                        if f_len as usize > left - 8 {
                            cr.get_logger().addr(
                                &cr.peer_address(),
                                &Sstring::from(
                                    "buffer underflow in feature data in negotiation frame",
                                ),
                            );
                            return Err(ExceptionPtr::new(closed_error()));
                        }
                        // SAFETY: f_len bytes are left.
                        let data = Sstring::from_bytes(unsafe {
                            std::slice::from_raw_parts(p, f_len as usize)
                        });
                        // SAFETY: as above.
                        p = unsafe { p.add(f_len as usize) };
                        map.entry(feature).or_insert(data);
                    }
                    Ok(map)
                },
            )
        },
    )
}

/// Reads a frame payload of `size` bytes, in as many buffers as the stream gives.
#[inline]
fn read_rcv_buf(in_: *mut InputStream<u8>, size: u32) -> Future<RcvBuf> {
    // SAFETY: the stream outlives the read (it is the connection's, or the one kept by
    // read_frame_compressed's frame).
    unsafe { &mut *in_ }.read_up_to(size as usize).then(
        move |data: TemporaryBuffer<u8>| -> Future<RcvBuf> {
            let mut rb = RcvBuf::with_size(size as usize);
            if data.size() == 0 {
                make_ready_future(RcvBuf::default())
            } else if data.size() == size as usize {
                rb.bufs = Bufs::One(data);
                make_ready_future(rb)
            } else {
                let left = size - data.size() as u32;
                rb.bufs = Bufs::Many(vec![data]);
                // do_with(rb, left, repeat(...)): the coroutine frame holds rb and left.
                make_coroutine(async move {
                    let (mut rb, mut left) = (rb, left);
                    loop {
                        // SAFETY: as above.
                        let data = unsafe { &mut *in_ }.read_up_to(left as usize).await?;
                        if data.size() == 0 {
                            rb.size -= left;
                            break;
                        }
                        left -= data.size() as u32;
                        if let Bufs::Many(v) = &mut rb.bufs {
                            v.push(data);
                        }
                        if left == 0 {
                            break;
                        }
                    }
                    Ok(rb)
                })
            }
        },
    )
}

/// The layout of a frame, for `Connection::read_frame`.
pub(crate) trait FrameType: 'static {
    /// What a read gives.
    type ReturnType: 'static;
    /// The decoded header.
    type HeaderType: 'static;
    /// The size of the header.
    fn header_size() -> usize;
    /// The reader, for the logs.
    fn role() -> &'static str;
    /// The value of a short read.
    fn empty_value() -> Self::ReturnType;
    /// The payload size and the header.
    ///
    /// # Safety
    /// `ptr` must point to `header_size()` readable bytes.
    unsafe fn decode_header(ptr: *const u8) -> (u32, Self::HeaderType);
    /// The value of a complete frame.
    fn make_value(h: &Self::HeaderType, data: RcvBuf) -> Self::ReturnType;
}

impl Connection {
    /// Reads a frame of type `F` from `in_`.
    pub(crate) fn read_frame<F: FrameType>(
        &mut self,
        info: SocketAddress,
        in_: *mut InputStream<u8>,
    ) -> Future<F::ReturnType> {
        let header_size = F::header_size();
        let this: *const Connection = self;
        // SAFETY: the connection and the stream outlive the read.
        unsafe { &mut *in_ }.read_exactly(header_size).then(
            move |header: TemporaryBuffer<u8>| -> Future<F::ReturnType> {
                // SAFETY: as above.
                let me = unsafe { &*this };
                if header.size() != header_size {
                    if header.size() != 0 {
                        me.get_logger().addr(
                            &info,
                            &Sstring::from(format!(
                                "unexpected eof on a {} while reading header: expected {} got {}",
                                F::role(),
                                header_size,
                                header.size()
                            )),
                        );
                    }
                    return make_ready_future(F::empty_value());
                }
                // SAFETY: the header has header_size bytes.
                let (size, h) = unsafe { F::decode_header(header.get()) };
                if size == 0 {
                    make_ready_future(F::make_value(&h, RcvBuf::default()))
                } else {
                    read_rcv_buf(in_, size).then(
                        move |rb: RcvBuf| -> Result<F::ReturnType, ExceptionPtr> {
                            // SAFETY: as above.
                            let me = unsafe { &*this };
                            if rb.size != size {
                                me.get_logger().addr(
                            &info,
                            &Sstring::from(format!(
                                "unexpected eof on a {} while reading data: expected {} got {}",
                                F::role(),
                                size,
                                rb.size
                            )),
                        );
                                Ok(F::empty_value())
                            } else {
                                Ok(F::make_value(&h, rb))
                            }
                        },
                    )
                }
            },
        )
    }

    /// Reads a frame of type `F`, decompressing it first when there is a compressor.
    pub(crate) fn read_frame_compressed<F: FrameType>(
        &mut self,
        info: SocketAddress,
        compressor: *mut Option<Box<dyn Compressor>>,
        in_: *mut InputStream<u8>,
    ) -> Future<F::ReturnType> {
        // SAFETY: the compressor is a member of the connection, which outlives its reads.
        if unsafe { (*compressor).is_some() } {
            let this: *mut Connection = self;
            // SAFETY: the stream outlives the read.
            unsafe { &mut *in_ }.read_exactly(4).then(move |compress_header: TemporaryBuffer<u8>| -> Future<F::ReturnType> {
                // SAFETY: as above.
                let me = unsafe { &mut *this };
                if compress_header.size() != 4 {
                    if compress_header.size() != 0 {
                        me.get_logger().addr(
                            &info,
                            &Sstring::from(format!(
                                "unexpected eof on a {} while reading compression header: expected 4 got {}",
                                F::role(),
                                compress_header.size()
                            )),
                        );
                    }
                    return make_ready_future(F::empty_value());
                }
                let ptr = compress_header.get();
                // SAFETY: the header has 4 bytes.
                let size = unsafe { read_le::<u32>(ptr) };
                read_rcv_buf(in_, size).then(move |compressed_data: RcvBuf| -> Future<F::ReturnType> {
                    // SAFETY: as above.
                    let me = unsafe { &mut *this };
                    if compressed_data.size != size {
                        me.get_logger().addr(
                            &info,
                            &Sstring::from(format!(
                                "unexpected eof on a {} while reading compressed data: expected {} got {}",
                                F::role(),
                                size,
                                compressed_data.size
                            )),
                        );
                        return make_ready_future(F::empty_value());
                    }
                    // SAFETY: as above.
                    let eb = match unsafe { (*compressor).as_mut() } {
                        Some(c) => match c.decompress(compressed_data) {
                            Ok(eb) => eb,
                            Err(e) => return make_exception_future(e),
                        },
                        None => unreachable!("the compressor is set for the life of the connection"),
                    };
                    if eb.size == 0 {
                        // Empty frames might be sent as means of communication between the compressors, and should be skipped by the RPC layer.
                        // We skip the empty frame here. We recursively restart the function, as if the empty frame didn't happen.
                        // The yield() is here to limit the stack depth of the recursion to 1.
                        return yield_().then(move |()| {
                            // SAFETY: as above.
                            unsafe { (*this).read_frame_compressed::<F>(info, compressor, in_) }
                        });
                    }
                    let mut p = Packet::new();
                    match eb.bufs {
                        Bufs::One(one) => p = Packet::append_buffer(p, one),
                        Bufs::Many(bufs) => {
                            p.reserve(bufs.len() as i32);
                            for b in bufs {
                                p = Packet::append_buffer(p, b);
                            }
                        }
                    }
                    // do_with(as_input_stream(p), ...): the coroutine frame holds the stream.
                    make_coroutine(async move {
                        let mut in2 = as_input_stream(p);
                        // SAFETY: as above; `in2` lives in the frame until the read ends.
                        unsafe { (*this).read_frame::<F>(info, &mut in2) }.await
                    })
                })
            })
        } else {
            self.read_frame::<F>(info, in_)
        }
    }
}

/// The frames of a stream connection.
pub(crate) struct StreamFrame;

/// The header of a stream frame: end of stream, and the payload size.
pub(crate) struct StreamFrameHeader {
    eos: bool,
}

impl FrameType for StreamFrame {
    type ReturnType = Option<RcvBuf>;
    type HeaderType = StreamFrameHeader;
    #[inline]
    fn header_size() -> usize {
        4
    }
    #[inline]
    fn role() -> &'static str {
        "stream"
    }
    #[inline]
    fn empty_value() -> Option<RcvBuf> {
        None
    }
    #[inline]
    unsafe fn decode_header(ptr: *const u8) -> (u32, StreamFrameHeader) {
        // SAFETY: guaranteed by the caller.
        let size = unsafe { read_le::<u32>(ptr) };
        if size != u32::MAX {
            (size, StreamFrameHeader { eos: false })
        } else {
            (0, StreamFrameHeader { eos: true })
        }
    }
    #[inline]
    fn make_value(t: &StreamFrameHeader, mut data: RcvBuf) -> Option<RcvBuf> {
        if t.eos {
            data.size = u32::MAX;
        }
        Some(data)
    }
}

impl Connection {
    /// Reads a stream frame from `in_`.
    pub(crate) fn read_stream_frame_compressed(
        &mut self,
        in_: *mut InputStream<u8>,
    ) -> Future<Option<RcvBuf>> {
        let compressor: *mut Option<Box<dyn Compressor>> = &mut self._compressor;
        self.read_frame_compressed::<StreamFrame>(self.peer_address(), compressor, in_)
    }

    /// Closes a stream connection whose two sides are closed.
    pub(crate) fn stream_close(&mut self) -> Future<()> {
        let mut f = make_ready_future(());
        let this: *mut Connection = self;
        if !self.error() {
            // Nothing may be sent on this connection from here on. The write buffer is
            // closed below while _error is still false, and _error is only set by the stop()
            // that follows; a frame accepted meanwhile (a compressor may send an empty frame
            // at any time) would be written into the closed stream.
            self._stream_closing = true;
            let mut p = Promise::<bool>::new();
            self._sink_closed_future = p.get_future();
            // Entries that send() accepted before the flag was set may still be queued: wait
            // for them to be written before closing the write buffer. Nothing else waits for
            // _outgoing_queue_ready from here on: send() is refused, and stop_send_loop()
            // waits for _sink_closed_future.
            let queue_drained =
                std::mem::replace(&mut self._outgoing_queue_ready, make_ready_future(()));
            // stop_send_loop(), which also calls _write_buf.close(), and this code can run in parallel.
            // Use _sink_closed_future to serialize them and skip second call to close()
            f = queue_drained
                .then_wrapped(move |mut fut: Future<()>| {
                    // _outgoing_queue_ready is exceptional if an entry failed; the write
                    // buffer still has to be closed.
                    fut.ignore_ready_future();
                    // SAFETY: the connection outlives its streams.
                    unsafe { (*this)._write_buf.close() }
                })
                .finally(move || {
                    p.set_value(true);
                });
        }
        // SAFETY: the connection outlives its streams.
        f.finally(move || unsafe { (*this).stop() })
    }

    /// Queues a received stream frame, holding memory units while it waits.
    pub(crate) fn stream_process_incoming(&mut self, mut buf: RcvBuf) -> Future<()> {
        // we do not want to dead lock on huge packets, so let them in
        // but only one at a time
        let size = (buf.size as usize).min(MAX_STREAM_BUFFERS_MEMORY);
        if buf.size == u32::MAX {
            // max fragment length marks an end of a stream
            self._stream_eos_received = true;
        }
        let this: *mut Connection = self;
        // SAFETY: the connection, and so its stream semaphore, outlives its stream loop and the
        // units of the frames it queues (stop() waits for the loop and drains the queue).
        unsafe { get_units(&mut self._stream_sem, size) }.then(move |su| {
            buf.su = Some(su);
            // SAFETY: the connection outlives its stream loop.
            unsafe { (*this)._stream_queue.push_eventually(buf) }
        })
    }

    /// Reads and queues one stream frame.
    pub(crate) fn handle_stream_frame(&mut self) -> Future<()> {
        let this: *mut Connection = self;
        let in_: *mut InputStream<u8> = &mut self._read_buf;
        self.read_stream_frame_compressed(in_).then(move |data: Option<RcvBuf>| -> Future<()> {
            // SAFETY: the connection outlives its stream loop.
            let me = unsafe { &mut *this };
            match data {
                None => {
                    me._error = true;
                    make_ready_future(())
                }
                Some(d) => me.stream_process_incoming(d),
            }
        })
    }

    /// Moves the queued stream frames to `bufs`.
    ///
    /// # Safety
    /// `bufs` stays alive and unaliased until the returned future resolves.
    pub unsafe fn stream_receive(
        &mut self,
        bufs: *mut CircularBuffer<ForeignPtr<Box<RcvBuf>>>,
    ) -> Future<()> {
        let this: *mut Connection = self;
        self._stream_queue.not_empty().then(move |()| -> Result<(), ExceptionPtr> {
            // SAFETY: the connection and the source's buffers outlive the receive.
            let (me, bufs) = unsafe { (&mut *this, &mut *bufs) };
            let eof = !me._stream_queue.consume(|b: RcvBuf| {
                if b.size == u32::MAX {
                    // max fragment length marks an end of a stream
                    false
                } else {
                    bufs.push_back(make_foreign(Box::new(b)));
                    true
                }
            })?;
            if eof && !bufs.empty() {
                engine_assert!(me._stream_queue.empty());
                // push eof marker back for next read to notice it; the queue was just
                // drained, so there is room.
                let _ = me._stream_queue.push(RcvBuf::with_size(u32::MAX as usize));
            }
            Ok(())
        })
    }

    /// Registers the stream connection `c` of id `id`.
    pub fn register_stream(&mut self, id: ConnectionId, c: XshardConnectionPtr) {
        self._streams.entry(id).or_insert(c);
    }

    /// The stream connection of id `id`.
    pub fn get_stream(&self, id: ConnectionId) -> Result<XshardConnectionPtr, ExceptionPtr> {
        match self._streams.get(&id) {
            None => Err(ExceptionPtr::new(LogicError(format!("rpc stream id {} not found", id)))),
            Some(c) => Ok(c.clone()),
        }
    }
}

// The request frame is
//   le64 optional timeout (see request_frame_with_timeout below)
//   le64 message type a.k.a. verb ID
//   le64 message ID
//   le32 payload length
//   ...  payload
/// The request frames of a connection without timeouts.
pub(crate) struct RequestFrame;

/// A parsed request frame: optional timeout, verb, message id and payload.
pub(crate) type RequestFrameReturn = (Option<u64>, u64, i64, Option<RcvBuf>);
/// The header of a request frame: optional timeout, verb and message id.
pub(crate) type RequestFrameHeader = (Option<u64>, u64, i64);

impl RequestFrame {
    pub(crate) const RAW_HEADER_SIZE: usize = 8 + 8 + 4;

    /// Writes the header of a request in `buf` at `off`.
    #[inline]
    pub(crate) fn encode_header(type_: u64, msg_id: i64, buf: &mut SndBuf, off: usize) {
        let len = buf.size - Self::RAW_HEADER_SIZE as u32 - off as u32;
        // SAFETY: the frame was made with request_frame_headroom bytes of head space in
        // its first chunk.
        unsafe {
            let p = buf.front().get_write().add(off);
            write_le::<u64>(p, type_);
            write_le::<i64>(p.add(8), msg_id);
            write_le::<u32>(p.add(16), len);
        }
    }

    #[inline]
    unsafe fn decode(ptr: *const u8) -> (u32, RequestFrameHeader) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let type_ = read_le::<u64>(ptr);
            let msgid = read_le::<i64>(ptr.add(8));
            let size = read_le::<u32>(ptr.add(16));
            (size, (None, type_, msgid))
        }
    }
}

const _: () =
    assert!(crate::engine::rpc::rpc_impl::REQUEST_FRAME_HEADROOM >= RequestFrame::RAW_HEADER_SIZE);

impl FrameType for RequestFrame {
    type ReturnType = RequestFrameReturn;
    type HeaderType = RequestFrameHeader;
    #[inline]
    fn header_size() -> usize {
        Self::RAW_HEADER_SIZE
    }
    #[inline]
    fn role() -> &'static str {
        "server"
    }
    #[inline]
    fn empty_value() -> RequestFrameReturn {
        (None, 0, 0, None)
    }
    #[inline]
    unsafe fn decode_header(ptr: *const u8) -> (u32, RequestFrameHeader) {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::decode(ptr) }
    }
    #[inline]
    fn make_value(t: &RequestFrameHeader, data: RcvBuf) -> RequestFrameReturn {
        (t.0, t.1, t.2, Some(data))
    }
}

// This frame is used if protocol_features.TIMEOUT was negotiated
/// The request frames of a connection that negotiated timeouts (a timeout field first).
pub(crate) struct RequestFrameWithTimeout;

impl RequestFrameWithTimeout {
    pub(crate) const RAW_HEADER_SIZE: usize = 8 + RequestFrame::RAW_HEADER_SIZE;

    /// Writes the header of a request; the expiration timer is encoded later.
    #[inline]
    pub(crate) fn encode_header(type_: u64, msg_id: i64, buf: &mut SndBuf) {
        const _: () = assert!(
            SndBuf::CHUNK_SIZE >= RequestFrameWithTimeout::RAW_HEADER_SIZE,
            "send buffer chunk size is too small"
        );
        // expiration timer is encoded later
        RequestFrame::encode_header(type_, msg_id, buf, 8);
    }
}

const _: () = assert!(
    crate::engine::rpc::rpc_impl::REQUEST_FRAME_HEADROOM
        >= RequestFrameWithTimeout::RAW_HEADER_SIZE
);

impl FrameType for RequestFrameWithTimeout {
    type ReturnType = RequestFrameReturn;
    type HeaderType = RequestFrameHeader;
    #[inline]
    fn header_size() -> usize {
        Self::RAW_HEADER_SIZE
    }
    #[inline]
    fn role() -> &'static str {
        RequestFrame::role()
    }
    #[inline]
    fn empty_value() -> RequestFrameReturn {
        RequestFrame::empty_value()
    }
    #[inline]
    unsafe fn decode_header(ptr: *const u8) -> (u32, RequestFrameHeader) {
        // SAFETY: guaranteed by the caller (the header has 8 more bytes in front).
        unsafe {
            let mut h = RequestFrame::decode(ptr.add(8));
            h.1.0 = Some(read_le::<u64>(ptr));
            h
        }
    }
    #[inline]
    fn make_value(t: &RequestFrameHeader, data: RcvBuf) -> RequestFrameReturn {
        RequestFrame::make_value(t, data)
    }
}

// ---- client ----

impl Client {
    /// Sends request `msg_id` of verb `type_` with payload `buf`.
    pub fn request(
        &mut self,
        type_: u64,
        msg_id: i64,
        mut buf: SndBuf,
        timeout: Option<RpcTimePoint>,
        cancel: Option<Pin<&mut crate::engine::rpc::rpc_types::Cancellable>>,
    ) -> Future<()> {
        RequestFrameWithTimeout::encode_header(type_, msg_id, &mut buf);
        self._conn.send(buf, timeout, cancel)
    }

    /// Records the features the server returned.
    fn negotiate(&mut self, provided: FeatureMap) -> Result<(), ExceptionPtr> {
        let this: *mut Client = self;
        // record features returned here
        for (id, v) in provided {
            // supported features go here
            match id {
                ProtocolFeatures::COMPRESS => {
                    if let Some(f) = self._options.compressor_factory {
                        self._conn._compressor = f.negotiate_with(
                            v.clone(),
                            false,
                            // SAFETY: the compressor belongs to the connection, which
                            // outlives it.
                            Box::new(move || unsafe {
                                (*this)._conn.send(SndBuf::default(), None, None)
                            }),
                        );
                    }
                    if self._conn._compressor.is_none() {
                        return Err(ExceptionPtr::new(RuntimeError(format!(
                            "RPC server responded with compression {} - unsupported",
                            v
                        ))));
                    }
                }
                ProtocolFeatures::TIMEOUT => {
                    self._conn._timeout_negotiated = true;
                }
                ProtocolFeatures::HANDLER_DURATION => {
                    self._conn._handler_duration_negotiated = true;
                }
                ProtocolFeatures::CONNECTION_ID => {
                    self._conn._id = deserialize_connection_id(&v);
                }
                _ => {
                    // nothing to do
                }
            }
        }
        Ok(())
    }

    fn negotiate_protocol(&mut self, features: FeatureMap) -> Future<()> {
        let this: *mut Client = self;
        self._conn.send_negotiation_frame(features).then(move |()| {
            // SAFETY: the client outlives its connection loop.
            let me = unsafe { &mut *this };
            let conn: *mut Connection = &mut me._conn;
            let in_: *mut InputStream<u8> = &mut me._conn._read_buf;
            receive_negotiation_frame(conn, in_).then(move |features: FeatureMap| {
                // SAFETY: as above.
                unsafe { (*this).negotiate(features) }
            })
        })
    }
}

// The response frame is
//   le64 message ID
//   le32 payload size
//   ...  payload
/// The response frames of a connection.
pub(crate) struct ResponseFrame;

/// A parsed response frame: message id, optional payload size and payload.
pub(crate) type ResponseFrameReturn = (i64, Option<u32>, Option<RcvBuf>);
/// The header of a response frame: message id and optional payload size.
pub(crate) type ResponseFrameHeader = (i64, Option<u32>);

impl ResponseFrame {
    pub(crate) const RAW_HEADER_SIZE: usize = 8 + 4;

    /// Writes the header of a response whose header has `header_size` bytes.
    #[inline]
    pub(crate) fn encode_header(msg_id: i64, data: &mut SndBuf, header_size: usize) {
        const _: () = assert!(
            SndBuf::CHUNK_SIZE >= ResponseFrame::RAW_HEADER_SIZE,
            "send buffer chunk size is too small"
        );
        let len = data.size - header_size as u32;
        // SAFETY: the response was made with response_frame_headroom bytes of head space
        // in its first chunk.
        unsafe {
            let p = data.front().get_write();
            write_le::<i64>(p, msg_id);
            write_le::<u32>(p.add(8), len);
        }
    }

    #[inline]
    unsafe fn decode(ptr: *const u8) -> (u32, ResponseFrameHeader) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let msgid = read_le::<i64>(ptr);
            let size = read_le::<u32>(ptr.add(8));
            (size, (msgid, None))
        }
    }
}

const _: () = assert!(RESPONSE_FRAME_HEADROOM >= ResponseFrame::RAW_HEADER_SIZE);

impl FrameType for ResponseFrame {
    type ReturnType = ResponseFrameReturn;
    type HeaderType = ResponseFrameHeader;
    #[inline]
    fn header_size() -> usize {
        Self::RAW_HEADER_SIZE
    }
    #[inline]
    fn role() -> &'static str {
        "client"
    }
    #[inline]
    fn empty_value() -> ResponseFrameReturn {
        (0, None, None)
    }
    #[inline]
    unsafe fn decode_header(ptr: *const u8) -> (u32, ResponseFrameHeader) {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::decode(ptr) }
    }
    #[inline]
    fn make_value(t: &ResponseFrameHeader, data: RcvBuf) -> ResponseFrameReturn {
        (t.0, t.1, Some(data))
    }
}

// The response frame is
//   le64 message ID
//   le32 payload size
//   le32 handler duration
//   ...  payload
/// The response frames of a connection that negotiated the handler duration field.
pub(crate) struct ResponseFrameWithHandlerTime;

impl ResponseFrameWithHandlerTime {
    pub(crate) const RAW_HEADER_SIZE: usize = ResponseFrame::RAW_HEADER_SIZE + 4;

    /// The handler duration in microseconds, or `-1U` when unknown or too long.
    #[inline]
    pub(crate) fn encode_handler_duration(ht: Option<RpcDurationNs>) -> u32 {
        if let Some(ht) = ht {
            let us = ht / 1_000;
            if us < u32::MAX as i64 {
                return us as u32;
            }
        }
        u32::MAX
    }

    /// Writes the header of a response with the duration of its handler.
    #[inline]
    pub(crate) fn encode_header(msg_id: i64, ht: Option<RpcDurationNs>, data: &mut SndBuf) {
        const _: () = assert!(SndBuf::CHUNK_SIZE >= ResponseFrameWithHandlerTime::RAW_HEADER_SIZE);
        let p = data.front().get_write();
        ResponseFrame::encode_header(msg_id, data, Self::RAW_HEADER_SIZE);
        // SAFETY: the first chunk holds the whole header.
        unsafe { write_le::<u32>(p.add(12), Self::encode_handler_duration(ht)) };
    }
}

const _: () = assert!(RESPONSE_FRAME_HEADROOM >= ResponseFrameWithHandlerTime::RAW_HEADER_SIZE);

impl FrameType for ResponseFrameWithHandlerTime {
    type ReturnType = ResponseFrameReturn;
    type HeaderType = ResponseFrameHeader;
    #[inline]
    fn header_size() -> usize {
        Self::RAW_HEADER_SIZE
    }
    #[inline]
    fn role() -> &'static str {
        ResponseFrame::role()
    }
    #[inline]
    fn empty_value() -> ResponseFrameReturn {
        ResponseFrame::empty_value()
    }
    #[inline]
    unsafe fn decode_header(ptr: *const u8) -> (u32, ResponseFrameHeader) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let mut p = ResponseFrame::decode(ptr);
            let ht = read_le::<u32>(ptr.add(12));
            if ht != u32::MAX {
                p.1.1 = Some(ht);
            }
            p
        }
    }
    #[inline]
    fn make_value(t: &ResponseFrameHeader, data: RcvBuf) -> ResponseFrameReturn {
        ResponseFrame::make_value(t, data)
    }
}

impl Client {
    // Returned future is
    // - message id
    // - optional server-side handler duration
    // - message payload
    fn read_response_frame_compressed(
        &mut self,
        in_: *mut InputStream<u8>,
    ) -> Future<ResponseFrameReturn> {
        let addr = self._server_addr;
        let compressor: *mut Option<Box<dyn Compressor>> = &mut self._conn._compressor;
        if self._conn._handler_duration_negotiated {
            self._conn.read_frame_compressed::<ResponseFrameWithHandlerTime>(addr, compressor, in_)
        } else {
            self._conn.read_frame_compressed::<ResponseFrame>(addr, compressor, in_)
        }
    }

    /// The counters, with the queue lengths.
    pub fn get_stats(&self) -> Stats {
        let mut res = self._conn._stats;
        res.wait_reply = self.incoming_queue_length() as u64;
        res.pending = self.outgoing_queue_length() as u64;
        res
    }

    /// Waits for the reply of message `id` with `h`.
    pub fn wait_for_reply(
        &mut self,
        id: IdType,
        mut h: Box<dyn ReplyHandlerBase>,
        timeout: Option<RpcTimePoint>,
        cancel: Option<Pin<&mut crate::engine::rpc::rpc_types::Cancellable>>,
    ) {
        // SAFETY: the pointer is stored only in the handler's pcancel, which the cancellable
        // reaches through wait_back_pointer (set below).
        let cancel = cancel.map_or(std::ptr::null_mut(), |c| unsafe { c.as_registered() });
        let this: *mut Client = self;
        if let Some(t) = timeout {
            // SAFETY: the handler is boxed, so its timer does not move; the timer is
            // dropped with the handler, before the client goes away.
            let mut timer = unsafe { Pin::new_unchecked(&mut h.data().t) };
            // SAFETY: the timer is dropped with the handler, before the client goes away, so `this` is alive whenever the callback runs.
            timer.as_mut().set_callback(move || unsafe { (*this).wait_timed_out(id) });
            timer.arm(t, None);
        }
        if !cancel.is_null() {
            // SAFETY: the caller's cancellable is pinned and alive; it clears these pointers
            // when it is dropped or reassigned.
            unsafe {
                (*cancel).cancel_wait = NoncopyableFunction::<fn()>::new(move || {
                    // SAFETY: the cancellable is disarmed by ~reply_handler_base before the
                    // handler or the client go away.
                    let me = &mut *this;
                    if let Some(mut h) = me._outstanding.remove(&id) {
                        h.cancel();
                    }
                });
                h.data().pcancel = cancel;
                (*cancel).wait_back_pointer = &mut h.data().pcancel;
            }
        }
        self._outstanding.entry(id).or_insert(h);
    }

    /// The call `id` timed out.
    pub fn wait_timed_out(&mut self, id: IdType) {
        self._conn._stats.timeout += 1;
        if let Some(mut h) = self._outstanding.remove(&id) {
            h.timeout();
        }
    }

    /// Stops the client: shuts the socket down and resolves when the connection loop ends.
    pub fn stop(&mut self) -> Future<()> {
        self._conn._error = true;
        if let Err(e) = self._socket.shutdown() {
            self._conn.log_exception(
                LogLevel::Error,
                "fail to shutdown connection while stopping",
                &e,
            );
        }
        self._conn._stopped.get_future()
    }

    /// Aborts the streams of this client.
    pub fn abort_all_streams(&mut self) {
        while let Some(&k) = self._conn._streams.keys().next() {
            let s = &self._conn._streams[&k];
            engine_assert!(s.get_owner_shard() == this_shard_id()); // abort can be called only locally
            // SAFETY: the stream connection is local and alive while registered.
            unsafe { (*xshard_get(s)).abort() };
            self._conn._streams.remove(&k);
        }
    }

    /// Removes this stream client from its parent.
    pub fn deregister_this_stream(&mut self) {
        let p = self._parent.get();
        if !p.is_null() {
            // SAFETY: a live weak pointer points to a live client.
            unsafe { (*p)._conn._streams.remove(&self._conn._id) };
        }
    }

    // This is the enlightened copy of the connection::send() method. Its intention is to
    // keep a dummy entry in front of the queue while connect+negotiate is happenning so
    // that all subsequent entries could abort on timeout or explicit cancellation.
    fn enqueue_zero_frame(&mut self) {
        if self._conn._error {
            return;
        }

        let p = HostBox::new(OutgoingEntry::new(SndBuf::with_size(0)));
        // SAFETY: the entry stays in its HostBox until its continuation ends, and the
        // client does not move while its queue is not empty.
        unsafe { self._conn._outgoing_queue.push_back(p.as_ptr()) };

        // Make it in the background. Even if the client is stopped it will pick
        // up all the entries hanging around
        // SAFETY: the entry is alive in its HostBox.
        let next = unsafe { HostedPromise::get_future(OutgoingEntry::hosted_promise(p.as_ptr())) };
        let _ = std::mem::replace(&mut self._conn._outgoing_queue_ready, next).then_wrapped(
            move |mut f: Future<()>| {
                if f.failed() {
                    f.ignore_ready_future();
                } else {
                    // SAFETY: `p`, moved here, keeps the entry alive.
                    unsafe {
                        HostedPromise::set_value(OutgoingEntry::hosted_promise(p.as_ptr()), ())
                    };
                }
            },
        );
    }
}

/// The metrics of the clients of one domain.
pub(crate) struct ClientMetricsDomain {
    list: DomainList,
    dead: Stats,
    metric_groups: MetricGroups,
}

thread_local! {
    /// `client::metrics::domain::all`; the domains are boxed (`unordered_map` nodes)
    /// because their lists and metric callbacks point at them. Only this shard touches
    /// it, never re-entrantly.
    static DOMAINS: UnsafeCell<HashMap<Sstring, Box<ClientMetricsDomain>>> = UnsafeCell::new(HashMap::new());
}

impl ClientMetricsDomain {
    /// The sum of `field` over the live and the dead clients.
    fn count_all(&self, field: fn(&Stats) -> u64) -> u64 {
        let mut res = field(&self.dead);
        for m in self.list.iter() {
            // SAFETY: a linked metrics object belongs to a live client.
            res += field(unsafe { &(*m.as_ref()._c)._conn._stats });
        }
        res
    }

    /// The sum of `fn_` over the live clients.
    fn count_all_fn(&self, fn_: fn(&Client) -> usize) -> usize {
        let mut res = 0;
        for m in self.list.iter() {
            // SAFETY: as in count_all().
            res += fn_(unsafe { &*m.as_ref()._c });
        }
        res
    }

    /// The metrics domain `name`: boxed first, since the metric callbacks point at it.
    fn new(name: &Sstring) -> Box<ClientMetricsDomain> {
        let mut d = Box::new(ClientMetricsDomain {
            list: List::new(),
            dead: Stats::default(),
            metric_groups: MetricGroups::new(),
        });
        let this: *const ClientMetricsDomain = &*d;
        let domain_l = Label::new("domain").call(name);
        macro_rules! count_all {
            ($f:ident) => {
                // SAFETY: the domain outlives its metric groups, which it owns.
                move || unsafe { (*this).count_all(|s| s.$f) }
            };
        }
        // Registration fails only on a double registration, which cannot happen for a
        // new domain; this constructor cannot fail, so the result is ignored.
        let _ = d.metric_groups.add_group(
            "rpc_client",
            vec![
                sm::make_gauge(
                    "count",
                    // SAFETY: as in count_all!.
                    move || unsafe { (*this).list.size() },
                    Description::new("Total number of clients"),
                    vec![domain_l.clone()],
                ),
                sm::make_counter(
                    "sent_messages",
                    count_all!(sent_messages),
                    Description::new("Total number of messages sent"),
                    vec![domain_l.clone()],
                ),
                sm::make_counter(
                    "replied",
                    count_all!(replied),
                    Description::new("Total number of responses received"),
                    vec![domain_l.clone()],
                ),
                sm::make_counter(
                    "exception_received",
                    count_all!(exception_received),
                    Description::new("Total number of exceptional responses received"),
                    vec![domain_l.clone()],
                )
                .set_skip_when_empty(true),
                sm::make_counter(
                    "timeout",
                    count_all!(timeout),
                    Description::new("Total number of timeout responses"),
                    vec![domain_l.clone()],
                )
                .set_skip_when_empty(true),
                sm::make_counter(
                    "delay_samples",
                    count_all!(delay_samples),
                    Description::new("Total number of delay samples"),
                    vec![domain_l.clone()],
                ),
                sm::make_counter(
                    "delay_total",
                    move || -> f64 {
                        let mut res = 0.0;
                        // SAFETY: as in count_all!.
                        for m in unsafe { (*this).list.iter() } {
                            // SAFETY: as in count_all().
                            res += unsafe { (*m.as_ref()._c)._conn._stats.delay_total };
                        }
                        res
                    },
                    Description::new("Total delay in seconds"),
                    vec![domain_l.clone()],
                ),
                sm::make_gauge(
                    "pending",
                    // SAFETY: the gauge is registered in the client's own metric group, removed before the client is dropped, so `this` is alive whenever the gauge is read.
                    move || unsafe { (*this).count_all_fn(|c| c.outgoing_queue_length()) },
                    Description::new("Number of queued outbound messages"),
                    vec![domain_l.clone()],
                ),
                sm::make_gauge(
                    "wait_reply",
                    // SAFETY: the gauge is registered in the client's own metric group, removed before the client is dropped, so `this` is alive whenever the gauge is read.
                    move || unsafe { (*this).count_all_fn(|c| c.incoming_queue_length()) },
                    Description::new("Number of replies waiting for"),
                    vec![domain_l],
                ),
            ],
        );
        d
    }

    /// The domain `name`, made on first use.
    fn find_or_create(name: &Sstring) -> *mut ClientMetricsDomain {
        DOMAINS.with(|all| {
            // SAFETY: the map is thread-local, and ClientMetricsDomain::new does not reach it.
            let all = unsafe { &mut *all.get() };
            let d = all.entry(name.clone()).or_insert_with(|| ClientMetricsDomain::new(name));
            &mut **d as *mut ClientMetricsDomain
        })
    }
}

impl ClientMetrics {
    /// Metrics not linked to a domain yet.
    fn unlinked() -> Self {
        ClientMetrics {
            _c: std::ptr::null(),
            _domain: std::ptr::null_mut(),
            _link: ListHook::new(),
        }
    }

    /// Links the client, at its final place, into its domain.
    fn attach(&mut self, c: *const Client) {
        self._c = c;
        // SAFETY: `c` is the client that owns these metrics.
        self._domain =
            ClientMetricsDomain::find_or_create(unsafe { &(*c)._options.metrics_domain });
        // SAFETY: the domain is boxed and lives as long as the shard; the client does not
        // move once attached, and the hook unlinks itself when dropped.
        unsafe { (*self._domain).list.push_back(self) };
    }

    /// Adds the counters of the client to the dead ones of its domain.
    fn detach(&mut self, stats: &Stats) {
        if self._domain.is_null() {
            return;
        }
        // SAFETY: the domain lives as long as the shard.
        let d = unsafe { &mut (*self._domain).dead };
        d.replied += stats.replied;
        d.exception_received += stats.exception_received;
        d.sent_messages += stats.sent_messages;
        d.timeout += stats.timeout;
        d.delay_samples += stats.delay_samples;
        d.delay_total += stats.delay_total;
        self._link.unlink();
    }
}

impl Drop for Client {
    /// The metrics are unregistered before the fields drop: they read the counters of the
    /// connection, which would otherwise be dropped first.
    fn drop(&mut self) {
        let stats = self._conn._stats;
        self._metrics.detach(&stats);
    }
}

impl Client {
    /// A client of `addr` from `local` over `socket`, with options `ops`, logging to `l`
    /// with the type-erased serializer `s`; [`Client::start`] connects it.
    ///
    /// # Safety
    /// `l` and the serializer `s` outlive the client and its connection.
    pub unsafe fn with_options_socket(
        l: &Logger,
        s: *mut (),
        ops: ClientOptions,
        socket: Socket,
        addr: &SocketAddress,
        local: &SocketAddress,
    ) -> Client {
        Client {
            // SAFETY: the caller's contract covers the connection too.
            _conn: unsafe { Connection::new(&CLIENT_VTABLE, l, s, INVALID_CONNECTION_ID) },
            _weak: WeaklyReferencable::new(),
            _socket: socket,
            _message_id: 1,
            _outstanding: HashMap::default(),
            _server_addr: *addr,
            _local_addr: *local,
            _options: ops,
            _parent: WeakPtr::default(),
            _metrics: ClientMetrics::unlinked(),
            _proto_use: ProtocolUse::none(),
        }
    }

    /// Starts the client at its final place: links its
    /// metrics and connects in the background. The caller has to call
    /// `Client::stop` to synchronize.
    pub fn start(self: Pin<&mut Self>) -> Result<(), ExceptionPtr> {
        // SAFETY: the client is not moved out of the pin.
        let me = unsafe { self.get_unchecked_mut() };
        let this: *mut Client = me;
        me._metrics.attach(this);
        me._socket.set_reuseaddr(me._options.reuseaddr)?;
        // Run client in the background.
        // Communicate result via _stopped.
        // The caller has to call client::stop() to synchronize.
        let ops = me._options.clone();
        let _ = me
            ._socket
            .connect(me._server_addr, me._local_addr, Transport::TCP)
            .then(move |fd: ConnectedSocket| -> Future<()> {
                // SAFETY: the client outlives its connection loop (stop() waits for it).
                match unsafe { (*this).on_connect(fd, &ops) } {
                    Ok(f) => f,
                    Err(e) => make_exception_future(e),
                }
            })
            .then_wrapped(move |f: Future<()>| {
                // SAFETY: as above.
                unsafe { (*this).on_connection_done(f) }
            });
        me.enqueue_zero_frame();
        Ok(())
    }

    /// The continuation of the connect: negotiates, then reads the replies.
    fn on_connect(
        &mut self,
        mut fd: ConnectedSocket,
        ops: &ClientOptions,
    ) -> Result<Future<()>, ExceptionPtr> {
        fd.set_nodelay(ops.tcp_nodelay)?;
        if let Some(k) = &ops.keepalive {
            fd.set_keepalive(true)?;
            fd.set_keepalive_parameters(&KeepaliveParams::Tcp(*k))?;
        }
        self._conn.set_socket(fd)?;

        let mut features = FeatureMap::new();
        if let Some(f) = self._options.compressor_factory {
            features.insert(ProtocolFeatures::COMPRESS, f.supported().clone());
        }
        if self._options.send_timeout_data {
            features.insert(ProtocolFeatures::TIMEOUT, Sstring::new());
        }
        if self._options.send_handler_duration {
            features.insert(ProtocolFeatures::HANDLER_DURATION, Sstring::new());
        }
        if self._options.stream_parent.is_valid() {
            features.insert(
                ProtocolFeatures::STREAM_PARENT,
                serialize_connection_id(&self._options.stream_parent),
            );
        }
        if !self._options.isolation_cookie.empty() {
            features.insert(ProtocolFeatures::ISOLATION, self._options.isolation_cookie.clone());
        }

        let this: *mut Client = self;
        Ok(self.negotiate_protocol(features).then(move |()| {
            // SAFETY: the client outlives its connection loop.
            let me = unsafe { &mut *this };
            me._conn._propagate_timeout = !me.is_stream();
            me._conn.set_negotiated();
            do_until(
                move || {
                    // SAFETY: the client outlives its connection loop; the loop condition only reads it, between the steps of the body.
                    let me = unsafe { &*this };
                    me._conn._read_buf.eof() || me._conn._error
                },
                move || -> Future<()> {
                    // SAFETY: the client outlives its connection loop; the body runs after the condition has returned, so no other reference to the client is live.
                    let me = unsafe { &mut *this };
                    if me.is_stream() {
                        return me._conn.handle_stream_frame();
                    }
                    let in_: *mut InputStream<u8> = &mut me._conn._read_buf;
                    me.read_response_frame_compressed(in_).then(
                        move |msg_id_and_data: ResponseFrameReturn| {
                            // SAFETY: the client outlives its connection loop, and the response is handled after the read that produced it has resolved.
                            unsafe { (*this).handle_response(msg_id_and_data) }
                        },
                    )
                },
            )
        }))
    }

    /// Handles one response frame.
    fn handle_response(
        &mut self,
        msg_id_and_data: ResponseFrameReturn,
    ) -> Result<(), ExceptionPtr> {
        let (msg_id, ht, data) = msg_id_and_data;
        let Some(mut data) = data else {
            self._conn._error = true;
            return Ok(());
        };
        if let Some(mut handler) = self._outstanding.remove(&msg_id.abs()) {
            handler.call(self, msg_id, data)?;
            if let Some(ht) = ht {
                self._conn._stats.delay_samples += 1;
                let elapsed_ns = RpcClockType::now().nanos_since_epoch()
                    - handler.data().start.nanos_since_epoch();
                self._conn._stats.delay_total += elapsed_ns as f64 / 1e9 - ht as f64 / 1e6;
            }
        } else if msg_id < 0 {
            // A frame too short to read is logged as the other errors (`catch(...)`).
            let ex = match unmarshal_exception(&mut data) {
                Ok(e) | Err(e) => e,
            };
            match ex.downcast_ref::<crate::engine::rpc::rpc_types::Error>().map(|e| e.kind()) {
                Some(ErrorKind::UnknownVerb(type_)) => {
                    // if this is unknown verb exception with unknown id ignore it
                    // can happen if unknown verb was used by no_wait client
                    self.get_logger().addr(
                        &self.peer_address(),
                        &Sstring::from(format!("unknown verb exception {} ignored", type_)),
                    );
                }
                _ => {
                    // We've got error response but handler is no longer waiting, could be timed out.
                    self._conn.log_exception(LogLevel::Info, "ignoring error response", &ex);
                }
            }
        } else {
            // we get a reply for a message id not in _outstanding
            // this can happened if the message id is timed out already
            self.get_logger().addr_level(
                &self.peer_address(),
                LogLevel::Debug,
                "got a reply for an expired message id",
            );
        }
        Ok(())
    }

    /// The end of the connection loop: logs, fails what waits, and resolves `_stopped`.
    fn on_connection_done(&mut self, f: Future<()>) -> Future<()> {
        let mut ep: Option<ExceptionPtr> = None;
        if f.failed() {
            let e = f.get_exception();
            let stream = self.is_stream();
            if self._conn._connected {
                let log = if stream {
                    "client stream connection dropped"
                } else {
                    "client connection dropped"
                };
                self._conn.log_exception(LogLevel::Error, log, &e);
            } else {
                let log = if stream { "stream fail to connect" } else { "fail to connect" };
                self._conn.log_exception(LogLevel::Debug, log, &e);
            }
            ep = Some(e);
        }
        // A connection that ends cleanly after the peer's end of stream leaves the queued
        // frames and the end to the source.
        let clean_end = ep.is_none() && self._conn._stream_eos_received;
        if self.is_stream() && (ep.is_some() || self._conn._error) && !clean_end {
            self._conn._stream_queue.abort(ExceptionPtr::new(stream_closed()));
        }
        self._conn._error = true;
        let this: *mut Client = self;
        // SAFETY: the client outlives its stop (stop() waits for _stopped).
        self._conn
            .stop_send_loop(ep)
            .then_wrapped(move |mut f: Future<()>| {
                f.ignore_ready_future();
                // SAFETY: the client outlives its stop (stop() waits for `_stopped`), and each step of the chain runs alone on this shard.
                let me = unsafe { &mut *this };
                me._outstanding.clear();
                if me.is_stream() {
                    me.deregister_this_stream();
                } else {
                    me.abort_all_streams();
                }
            })
            .finally(move || {
                // SAFETY: the client outlives its stop (stop() waits for `_stopped`), and each step of the chain runs alone on this shard.
                let me = unsafe { &mut *this };
                match me._conn._compressor.as_mut() {
                    Some(c) => c.close(),
                    None => make_ready_future(()),
                }
            })
            .finally(move || {
                // SAFETY: the client outlives its stop: stop() waits for this very promise.
                unsafe { (*this)._conn._stopped.set_value(()) };
            })
    }

    /// A client of `addr` from `local`, with default options and a new socket.
    ///
    /// # Safety
    /// `l` and the serializer `s` outlive the client and its connection.
    pub unsafe fn new(
        l: &Logger,
        s: *mut (),
        addr: &SocketAddress,
        local: &SocketAddress,
    ) -> Client {
        // SAFETY: the caller's contract.
        unsafe {
            Self::with_options_socket(l, s, ClientOptions::default(), make_socket(), addr, local)
        }
    }

    /// A client of `addr` from `local`, with `options` and a new socket.
    ///
    /// # Safety
    /// `l` and the serializer `s` outlive the client and its connection.
    pub unsafe fn with_options(
        l: &Logger,
        s: *mut (),
        options: ClientOptions,
        addr: &SocketAddress,
        local: &SocketAddress,
    ) -> Client {
        // SAFETY: the caller's contract.
        unsafe { Self::with_options_socket(l, s, options, make_socket(), addr, local) }
    }

    /// A client of `addr` from `local` over `socket`, with default options.
    ///
    /// # Safety
    /// `l` and the serializer `s` outlive the client and its connection.
    pub unsafe fn with_socket(
        l: &Logger,
        s: *mut (),
        socket: Socket,
        addr: &SocketAddress,
        local: &SocketAddress,
    ) -> Client {
        // SAFETY: the caller's contract.
        unsafe { Self::with_options_socket(l, s, ClientOptions::default(), socket, addr, local) }
    }
}

// ---- server connection ----

impl ServerConnection {
    /// Answers the features the client requested.
    fn negotiate(&mut self, requested: FeatureMap) -> Future<FeatureMap> {
        let this: *mut ServerConnection = self;
        let mut ret = FeatureMap::new();
        let mut f = make_ready_future(());
        for (id, v) in &requested {
            // supported features go here
            match *id {
                ProtocolFeatures::COMPRESS => {
                    // SAFETY: the server outlives its connections and its options are not
                    // written once it has started.
                    if let Some(cf) = unsafe { (*self.get_server())._options.compressor_factory } {
                        self._conn._compressor = cf.negotiate_with(
                            v.clone(),
                            true,
                            // SAFETY: the compressor belongs to the connection, which
                            // outlives it.
                            Box::new(move || unsafe {
                                (*this)._conn.send(SndBuf::default(), None, None)
                            }),
                        );
                        if let Some(c) = &self._conn._compressor {
                            ret.insert(ProtocolFeatures::COMPRESS, c.name());
                        }
                    }
                }
                ProtocolFeatures::TIMEOUT => {
                    self._conn._timeout_negotiated = true;
                    ret.insert(ProtocolFeatures::TIMEOUT, Sstring::new());
                }
                ProtocolFeatures::HANDLER_DURATION => {
                    self._conn._handler_duration_negotiated = true;
                    ret.insert(ProtocolFeatures::HANDLER_DURATION, Sstring::new());
                }
                ProtocolFeatures::STREAM_PARENT => {
                    // SAFETY: as for the compressor factory above.
                    if unsafe { (*self.get_server())._options.streaming_domain }.is_none() {
                        f = f.then(|()| {
                            make_exception_future::<()>(ExceptionPtr::new(RuntimeError(
                                "streaming is not configured for the server".to_string(),
                            )))
                        });
                    } else {
                        self._parent_id = deserialize_connection_id(v);
                        self._conn._is_stream = true;
                        // remove stream connection from rpc connection list
                        let my_id = self.get_connection_id();
                        // SAFETY: the server outlives its connections; only its map is
                        // borrowed, for this call, and no loop over it is running (negotiation
                        // runs in its own task, never inside foreach_connection or stop).
                        unsafe { (*self.get_server())._conns.remove(&my_id) };
                        let c = self.shared_from_this();
                        f = f.then(move |()| {
                            // SAFETY: the connection is kept alive by `c` while negotiating.
                            let me = unsafe { &mut *this };
                            // SAFETY: a ServerConnection begins with its Connection (repr(C)).
                            let conn = unsafe { static_pointer_cast(&c, |p: *mut ServerConnection| p as *mut Connection) };
                            let mut c = Some(make_foreign(ConnectionPtr(conn)));
                            // SAFETY: the connection stays alive (held by `c`) until the call
                            // returns, and the other shard only reads its parent id and its
                            // server's options, which nothing writes once negotiated.
                            let xthis = unsafe { XshardPtr::new(this) };
                            Smp::submit_to(me._parent_id.shard() as u32, move || -> Result<(), ExceptionPtr> {
                                // SAFETY: as above.
                                let me = unsafe { &*xthis.get() };
                                // SAFETY: as above; the options are not written once the server
                                // has started.
                                let Some(domain) = (unsafe { (*me.get_server())._options.streaming_domain }) else {
                                    unreachable!("streaming_domain was checked above");
                                };
                                let Some(s) = with_servers(|m| m.get(&domain).copied()) else {
                                    return Err(ExceptionPtr::new(LogicError(format!(
                                        "Shard {} does not have server with streaming domain {}",
                                        this_shard_id(),
                                        domain
                                    ))));
                                };
                                // SAFETY: a registered server is alive.
                                let s = unsafe { &mut *s };
                                let Some(parent) = s._conns.get(&me._parent_id) else {
                                    return Err(ExceptionPtr::new(LogicError(format!(
                                        "Unknown parent connection {} on shard {}",
                                        me._parent_id,
                                        this_shard_id()
                                    ))));
                                };
                                // SAFETY: the server holds its connections alive.
                                let parent = unsafe { parent.get_mut_unchecked() };
                                if parent._conn._error {
                                    return Err(ExceptionPtr::new(RuntimeError(format!(
                                        "Parent connection {} is aborting on shard {}",
                                        me._parent_id,
                                        this_shard_id()
                                    ))));
                                }
                                let Some(c) = c.take() else {
                                    unreachable!("the call runs once");
                                };
                                // SAFETY: the foreign pointer holds the connection; its id is
                                // read through the foreign pointer.
                                let id = match unsafe { c.get_unchecked() } {
                                    // SAFETY: `cell` is the non-null connection the foreign pointer holds; it lives on this shard, which is the only one that reads it.
                                    Some(cell) => unsafe { (*cell.get()).get_connection_id() },
                                    None => unreachable!("an xshard connection pointer is never null"),
                                };
                                parent._conn.register_stream(id, make_lw_shared(c));
                                Ok(())
                            })
                        });
                    }
                }
                ProtocolFeatures::ISOLATION => {
                    let isolation_cookie = v.clone();
                    // isolation_function_visitor: calls the isolation function with the cookie.
                    f = f.then(move |()| {
                        // SAFETY: the connection outlives its negotiation.
                        let me = unsafe { &mut *this };
                        // SAFETY: the server outlives its connections and its limits are not
                        // written once it has started.
                        let fut = match unsafe { &(*me.get_server())._limits.isolate_connection } {
                            IsolationFunctionAlternatives::Syncronous(f) => {
                                futurize_invoke(|| f(isolation_cookie))
                            }
                            IsolationFunctionAlternatives::Asyncronous(f) => f(isolation_cookie),
                        };
                        fut.then(move |conf: IsolationConfig| {
                            // SAFETY: as above.
                            unsafe { (*this)._isolation_config = Some(conf) };
                        })
                    });
                    ret.entry(*id).or_insert_with(|| v.clone());
                }
                _ => {
                    // nothing to do
                }
            }
        }
        // SAFETY: as for the compressor factory above.
        if unsafe { (*self.get_server())._options.streaming_domain }.is_some() {
            ret.insert(ProtocolFeatures::CONNECTION_ID, serialize_connection_id(&self._conn._id));
        }
        f.then(move |()| -> Result<FeatureMap, ExceptionPtr> { Ok(ret) })
    }

    fn negotiate_protocol(&mut self) -> Future<()> {
        let this: *mut ServerConnection = self;
        let conn: *mut Connection = &mut self._conn;
        let in_: *mut InputStream<u8> = &mut self._conn._read_buf;
        receive_negotiation_frame(conn, in_).then(move |requested_features: FeatureMap| {
            // SAFETY: the connection outlives its negotiation.
            unsafe { &mut *this }.negotiate(requested_features).then(
                move |returned_features: FeatureMap| {
                    // SAFETY: the connection outlives its negotiation, and this continuation runs after negotiate() has returned.
                    unsafe { (*this)._conn.send_negotiation_frame(returned_features) }
                },
            )
        })
    }

    fn read_request_frame_compressed(
        &mut self,
        in_: *mut InputStream<u8>,
    ) -> Future<RequestFrameReturn> {
        let addr = self._info.addr;
        let compressor: *mut Option<Box<dyn Compressor>> = &mut self._conn._compressor;
        if self._conn._timeout_negotiated {
            self._conn.read_frame_compressed::<RequestFrameWithTimeout>(addr, compressor, in_)
        } else {
            self._conn.read_frame_compressed::<RequestFrame>(addr, compressor, in_)
        }
    }

    /// Sends the response `data` of message `msg_id`.
    pub fn respond(
        &mut self,
        msg_id: i64,
        mut data: SndBuf,
        timeout: Option<RpcTimePoint>,
        handler_duration: Option<RpcDurationNs>,
    ) -> Future<()> {
        if self._conn._handler_duration_negotiated {
            ResponseFrameWithHandlerTime::encode_header(msg_id, handler_duration, &mut data);
        } else {
            data.front().trim_front(4);
            data.size -= 4;
            ResponseFrame::encode_header(msg_id, &mut data, ResponseFrame::RAW_HEADER_SIZE);
        }
        self._conn.send(data, timeout, None)
    }

    fn send_unknown_verb_reply(
        &mut self,
        timeout: Option<RpcTimePoint>,
        msg_id: i64,
        type_: u64,
    ) -> Future<()> {
        let this: *mut ServerConnection = self;
        self.wait_for_resources(28, timeout).then(
            move |permit: ResourcePermit| -> Result<(), ExceptionPtr> {
                // send unknown_verb exception back
                const UNKNOWN_VERB_MESSAGE_SIZE: usize = RESPONSE_FRAME_HEADROOM + 2 * 4 + 8;
                let mut data = SndBuf::with_size(UNKNOWN_VERB_MESSAGE_SIZE);
                const _: () = assert!(
                    SndBuf::CHUNK_SIZE >= UNKNOWN_VERB_MESSAGE_SIZE,
                    "send buffer chunk size is too small"
                );
                // SAFETY: the frame is one chunk of UNKNOWN_VERB_MESSAGE_SIZE bytes.
                unsafe {
                    let p = data.front().get_write().add(RESPONSE_FRAME_HEADROOM);
                    write_le::<u32>(p, ExceptionType::UnknownVerb as u32);
                    write_le::<u32>(p.add(4), 8);
                    write_le::<u64>(p.add(8), type_);
                }
                // Send asynchronously.
                // This is safe since connection::stop() will wait for background work.
                // SAFETY: the connection outlives its requests.
                let me = unsafe { &mut *this };
                // SAFETY: the server outlives its connections and is pinned once started.
                let r = with_gate(unsafe { Server::reply_gate_of(me.get_server()) }, move || {
                    // workaround for https://gcc.gnu.org/bugzilla/show_bug.cgi?id=83268
                    // SAFETY: as above.
                    let me = unsafe { &mut *this };
                    let c = me.shared_from_this();
                    me.respond(-msg_id, data, timeout, None).then(move |()| {
                        let _keep = (c, permit);
                    })
                });
                match r {
                    Ok(f) => {
                        let _ = f;
                        Ok(())
                    }
                    Err(e) if e.is::<GateClosedException>() => Ok(()), /* ignore */
                    Err(e) => Err(e),
                }
            },
        )
    }

    /// Runs the connection: negotiates, then reads and dispatches the requests.
    pub fn process(&mut self) -> Future<()> {
        let this: *mut ServerConnection = self;
        let conn_ptr = self.shared_from_this();
        self.negotiate_protocol()
            .then(move |()| {
                // SAFETY: `conn_ptr` keeps the connection alive until the loop
                // ends ("hold onto connection pointer until do_until() exists").
                let me = unsafe { &mut *this };
                let sg = match &me._isolation_config {
                    Some(c) => c.sched_group,
                    None => current_scheduling_group(),
                };
                with_scheduling_group(sg, move || {
                    // SAFETY: `conn_ptr`, held until the loop ends, keeps the connection alive; the closure runs after the previous step has returned.
                    let me = unsafe { &mut *this };
                    me._conn.set_negotiated();
                    do_until(
                        move || {
                            // SAFETY: `conn_ptr` keeps the connection alive until the loop ends; the condition only reads it, between the steps of the body.
                            let me = unsafe { &*this };
                            me._conn._read_buf.eof() || me._conn._error
                        },
                        move || -> Future<()> {
                            // SAFETY: `conn_ptr` keeps the connection alive until the loop ends; the body runs after the condition has returned.
                            let me = unsafe { &mut *this };
                            if me.is_stream() {
                                return me._conn.handle_stream_frame();
                            }
                            let in_: *mut InputStream<u8> = &mut me._conn._read_buf;
                            me.read_request_frame_compressed(in_).then(
                                move |header_and_buffer: RequestFrameReturn| {
                                    // SAFETY: `conn_ptr` keeps the connection alive until the loop ends, and the request is handled after the read that produced it has resolved.
                                    unsafe { (*this).handle_request(header_and_buffer) }
                                },
                            )
                        },
                    )
                })
            })
            // SAFETY: `conn_ptr`, held by the step below, keeps the connection alive here.
            .then_wrapped(move |f: Future<()>| unsafe { (*this).on_process_done(f) })
            .finally(move || {
                // hold onto connection pointer until do_until() exists
                let _keep = conn_ptr;
            })
    }

    /// Dispatches one request to its handler.
    fn handle_request(&mut self, header_and_buffer: RequestFrameReturn) -> Future<()> {
        let (expire, type_, msg_id, data) = header_and_buffer;
        let Some(data) = data else {
            self._conn._error = true;
            return make_ready_future(());
        };
        let mut timeout: Option<RpcTimePoint> = None;
        if let Some(e) = expire
            && e != 0
        {
            timeout = Some(relative_timeout_to_absolute((e as i64).saturating_mul(1_000_000)));
        }
        // SAFETY: the server outlives its connections and its protocol pointer is not written
        // once it has started.
        let proto = unsafe { (*self.get_server())._proto };
        // SAFETY: the protocol outlives its servers.
        let Some(h) = (unsafe { (*proto).get_handler(type_) }) else {
            return self.send_unknown_verb_reply(timeout, msg_id, type_);
        };

        // If the new method of per-connection scheduling group was used, honor it.
        // Otherwise, use the old per-handler scheduling group.
        let handler = h.handler;
        let sg = match &self._isolation_config {
            Some(c) => c.sched_group,
            // SAFETY: the handler stays registered while its holder is held.
            None => unsafe { (*handler).sg },
        };
        let guard = h.holder;
        let this: *mut ServerConnection = self;
        with_scheduling_group(sg, move || {
            // SAFETY: as above; the connection outlives its requests.
            unsafe { ((*handler).func)((*this).shared_from_this(), timeout, msg_id, data, guard) }
        })
    }

    /// The end of the connection loop: logs, fails what waits, and resolves `_stopped`.
    fn on_process_done(&mut self, f: Future<()>) -> Future<()> {
        let mut ep: Option<ExceptionPtr> = None;
        if f.failed() {
            let e = f.get_exception();
            let log = format!(
                "server{} connection dropped",
                if self.is_stream() { " stream" } else { "" }
            );
            self._conn.log_exception(LogLevel::Error, &log, &e);
            ep = Some(e);
        }
        self._conn._fd.shutdown_input();
        // A connection that ends cleanly after the peer's end of stream leaves the queued
        // frames and the end to the source.
        let clean_end = ep.is_none() && self._conn._stream_eos_received;
        if self.is_stream() && (ep.is_some() || self._conn._error) && !clean_end {
            self._conn._stream_queue.abort(ExceptionPtr::new(stream_closed()));
        }
        self._conn._error = true;
        let this: *mut ServerConnection = self;
        self._conn
            .stop_send_loop(ep)
            .then_wrapped(move |mut f: Future<()>| {
                f.ignore_ready_future();
                // SAFETY: the connection is held by process() until this ends.
                let me = unsafe { &mut *this };
                let id = me.get_connection_id();
                // SAFETY: the server outlives its connections (stop() waits for this loop);
                // only its map is borrowed, for this call, outside any loop over it.
                unsafe { (*me.get_server())._conns.remove(&id) };
                if me.is_stream() { me.deregister_this_stream() } else { me.abort_all_streams() }
            })
            .finally(move || {
                // SAFETY: the connection is held by process() until this chain ends, and each step runs alone on this shard.
                let me = unsafe { &mut *this };
                match me._conn._compressor.as_mut() {
                    Some(c) => c.close(),
                    None => make_ready_future(()),
                }
            })
            .finally(move || {
                // SAFETY: the connection is held by process() until this chain ends.
                unsafe { (*this)._conn._stopped.set_value(()) };
            })
    }

    /// A connection of server `s` over `fd` from peer `addr`, logging to `l`, with the
    /// type-erased `serializer` and id `id`.
    ///
    /// # Safety
    /// The server `s`, `l` and `serializer` outlive the connection (the server stops its
    /// connections before it goes away; the logger and serializer are its protocol's).
    pub unsafe fn new(
        s: *mut Server,
        fd: ConnectedSocket,
        addr: SocketAddress,
        l: &Logger,
        serializer: *mut (),
        id: ConnectionId,
    ) -> Self {
        ServerConnection {
            // SAFETY: the caller's contract.
            _conn: unsafe {
                Connection::with_socket(&SERVER_CONNECTION_VTABLE, fd, l, serializer, id)
            },
            _esft: EnableSharedFromThis::new(),
            _info: ClientInfo { addr, server: s, conn_id: id, user_data: HashMap::new() },
            _parent_id: INVALID_CONNECTION_ID,
            _isolation_config: None,
        }
    }

    /// Removes this stream connection from its parent.
    pub fn deregister_this_stream(&mut self) -> Future<()> {
        // SAFETY: the server outlives its connections and its options are not written once it
        // has started.
        if unsafe { (*self.get_server())._options.streaming_domain }.is_none() {
            return make_ready_future(());
        }
        // SAFETY: the connection outlives its stop, which waits for this call, and the other
        // shard only reads its parent id, connection id and server options, which nothing
        // writes meanwhile.
        let xthis = unsafe { XshardPtr::new(self as *mut ServerConnection) };
        Smp::submit_to(self._parent_id.shard() as u32, move || {
            // SAFETY: as above.
            let me = unsafe { &*xthis.get() };
            // SAFETY: as above.
            let Some(domain) = (unsafe { (*me.get_server())._options.streaming_domain }) else {
                return;
            };
            if let Some(s) = with_servers(|m| m.get(&domain).copied()) {
                // SAFETY: a registered server is alive.
                let s = unsafe { &mut *s };
                if let Some(parent) = s._conns.get(&me._parent_id) {
                    // SAFETY: the server holds its connections alive.
                    unsafe { parent.get_mut_unchecked() }
                        ._conn
                        ._streams
                        .remove(&me.get_connection_id());
                }
            }
        })
    }

    /// Aborts the streams of this connection on their shards.
    pub fn abort_all_streams(&mut self) -> Future<()> {
        let this: *mut ServerConnection = self;
        // SAFETY: the map stays unchanged until the loop has started every call.
        let streams = unsafe { (*this)._conn._streams.values() };
        parallel_for_each(streams, |s: &XshardConnectionPtr| {
            // The shared pointer copy stays on this shard until the call completes, and only
            // the connection crosses.
            let s = s.clone();
            // SAFETY: `s` keeps the connection alive until the call completes, and only the
            // shard that owns the connection reaches it.
            let con = unsafe { XshardPtr::new(xshard_get(&s)) };
            Smp::submit_to(s.get_owner_shard(), move || {
                // SAFETY: as above; the call runs on the shard that owns the connection.
                unsafe { (*con.get()).abort() };
            })
            .finally(move || drop(s))
        })
        .then(move |()| {
            // SAFETY: the connection outlives its stop.
            unsafe { (*this)._conn._streams.clear() };
        })
    }
}

// ---- server ----

impl Server {
    /// A server of `proto` listening on `addr`, with `limits` and default options.
    ///
    /// # Safety
    /// `proto` points at a protocol that does not move and outlives the server.
    pub unsafe fn new(
        proto: *mut dyn ProtocolBase,
        addr: &SocketAddress,
        limits: ResourceLimits,
    ) -> Result<Server, ExceptionPtr> {
        let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let socket = listen_with(*addr, lo)?;
        // SAFETY: the caller's contract.
        Ok(unsafe { Self::from_socket(proto, socket, limits, ServerOptions::default()) })
    }

    /// A server of `proto` listening on `addr`, with options `opts` and `limits`.
    ///
    /// # Safety
    /// `proto` points at a protocol that does not move and outlives the server.
    pub unsafe fn with_options(
        proto: *mut dyn ProtocolBase,
        opts: ServerOptions,
        addr: &SocketAddress,
        limits: ResourceLimits,
    ) -> Result<Server, ExceptionPtr> {
        let lo = ListenOptions {
            reuse_address: true,
            lba: opts.load_balancing_algorithm,
            ..ListenOptions::default()
        };
        let socket = listen_with(*addr, lo)?;
        // SAFETY: the caller's contract.
        Ok(unsafe { Self::from_socket(proto, socket, limits, opts) })
    }

    /// A server of `proto` over the listening socket `ss`, with `limits` and options `opts`;
    /// [`Server::start`] starts it.
    ///
    /// # Safety
    /// `proto` points at a protocol that does not move and outlives the server.
    pub unsafe fn from_socket(
        proto: *mut dyn ProtocolBase,
        ss: ServerSocket,
        limits: ResourceLimits,
        opts: ServerOptions,
    ) -> Server {
        let max_memory = limits.max_memory;
        Server {
            _proto: proto,
            _ss: ss,
            _limits: limits,
            _resources_available: RpcSemaphore::new(max_memory),
            _conns: HashMap::default(),
            _ss_stopped: Promise::new(),
            _reply_gate: Gate::new(),
            _options: opts,
            _shutdown: false,
            _next_client_id: 1,
            _proto_use: ProtocolUse::none(),
        }
    }

    /// A server of `proto` over the listening socket `ss`, with options `opts` and `limits`.
    ///
    /// # Safety
    /// `proto` points at a protocol that does not move and outlives the server.
    pub unsafe fn with_options_socket(
        proto: *mut dyn ProtocolBase,
        opts: ServerOptions,
        ss: ServerSocket,
        limits: ResourceLimits,
    ) -> Server {
        // SAFETY: the caller's contract.
        unsafe { Self::from_socket(proto, ss, limits, opts) }
    }

    /// Starts the server at its final place: registers its
    /// streaming domain and starts accepting.
    pub fn start(self: Pin<&mut Self>) -> Result<(), ExceptionPtr> {
        // SAFETY: the server is not moved out of the pin.
        let me = unsafe { self.get_unchecked_mut() };
        if let Some(d) = me._options.streaming_domain {
            let this: *mut Server = me;
            let exists = with_servers(|m| {
                if let std::collections::hash_map::Entry::Vacant(e) = m.entry(d) {
                    e.insert(this);
                    false
                } else {
                    true
                }
            });
            if exists {
                return Err(ExceptionPtr::new(RuntimeError(format!(
                    "An RPC server with the streaming domain {} is already exist",
                    d
                ))));
            }
        }
        me.accept();
        Ok(())
    }

    /// Accepts connections in the background until the socket is aborted.
    pub fn accept(&mut self) {
        let this: *mut Server = self;
        // Run asynchronously in background.
        // Communicate result via __ss_stopped.
        // The caller has to call server::stop() to synchronize.
        let _ = keep_doing(move || {
            // SAFETY: the server outlives its accept loop (stop() waits for
            // _ss_stopped).
            let me = unsafe { &mut *this };
            me._ss.accept().then(move |ar: AcceptResult| -> Result<(), ExceptionPtr> {
                // SAFETY: the server outlives its accept loop (stop() waits for `_ss_stopped`); `me` above is not used once the accept is started.
                let me = unsafe { &mut *this };
                if let Some(fc) = &me._options.filter_connection
                    && !fc(&ar.remote_address)
                {
                    return Ok(());
                }
                let AcceptResult { connection: mut fd, remote_address: addr } = ar;
                fd.set_nodelay(me._options.tcp_nodelay)?;
                let n = me._next_client_id;
                me._next_client_id += 1;
                let id = if me._options.streaming_domain.is_some() {
                    ConnectionId::make_id(n, this_shard_id() as u16)
                } else {
                    ConnectionId::make_invalid_id(n)
                };
                // SAFETY: the protocol outlives its servers; `this` is the server that
                // accepted `fd`, and it stops its connections before it goes away.
                let conn = unsafe { (*me._proto).make_server_connection(this, fd, addr, id) };
                let inserted = me._conns.insert(id, conn.clone()).is_none();
                engine_assert!(inserted);
                // Process asynchronously in background.
                // SAFETY: the connection is held by the server and by process() itself.
                let _ = unsafe { conn.get_mut_unchecked() }.process();
                Ok(())
            })
        })
        .then_wrapped(move |f: Future<()>| {
            let r = f.get();
            engine_assert!(r.is_err());
            // SAFETY: the server outlives its accept loop: stop() waits for this very promise.
            unsafe { (*this)._ss_stopped.set_value(()) };
        });
    }

    /// Shuts down the server.
    ///
    /// Light version of the stop, that just makes sure the server is not visible
    /// by remote clients, i.e. -- no new rpcs are admitted and no replies on the
    /// previously running handlers will be sent. Currently running handlers may
    /// still run.
    ///
    /// Caller of shutdown() mush wait for it to resolve before calling stop.
    pub fn shutdown(&mut self) -> Future<()> {
        if self._shutdown {
            return make_ready_future(());
        }

        self._ss.abort_accept();
        self._resources_available.broken();
        if let Some(d) = self._options.streaming_domain {
            with_servers(|m| m.remove(&d));
        }
        let this: *mut Server = self;
        self._ss_stopped
            .get_future()
            .then(move |()| {
                // SAFETY: the caller keeps the server until shutdown resolves.
                let conns = unsafe { (*this)._conns.values() };
                parallel_for_each(conns, |conn: &SharedPtr<ServerConnection>| {
                    // SAFETY: the server holds the connection alive.
                    unsafe { conn.get_mut_unchecked() }.stop()
                })
            })
            .finally(move || {
                // SAFETY: the caller keeps the server until shutdown resolves, and this is its last step.
                unsafe { (*this)._shutdown = true };
            })
    }

    /// Stops the server.
    ///
    /// It makes sure that no new rpcs are admitted, no rpc handlers issued on this
    /// connection are running any longer and no replies on the previously running
    /// handlers will be sent.
    pub fn stop(&mut self) -> Future<()> {
        let shutdown = self.shutdown();
        let closed = self._reply_gate.close();
        crate::engine::core::when_all::when_all((shutdown, closed)).discard_result()
    }

    /// Aborts the connection with id `id`, so that it receives no further messages.
    ///
    /// It is safe to call from an RPC handler running on that connection. Does nothing if
    /// the server has no connection with that id. Shutting down the input cannot fail, so
    /// there is no failure to log.
    pub fn abort_connection(&self, id: ConnectionId) {
        let Some(c) = self._conns.get(&id) else {
            return;
        };
        let Some(p) = c.get() else {
            return;
        };
        // SAFETY: the server holds the connection alive. Only its `Connection` part is borrowed,
        // and only for this synchronous call: a handler running on this connection may hold
        // the `ClientInfo` (`_info`, a separate field), and the connection's own loop borrows
        // `_conn` only between suspension points, never across one.
        unsafe { (*core::ptr::addr_of_mut!((*p.as_ptr())._conn)).abort() };
    }
}
