//! The types shared by the RPC client, server and handlers.
//!
//! Connection identity ([`ConnectionId`]) and statistics ([`Stats`]), the client info a
//! handler sees ([`ClientInfo`]), the RPC errors ([`Error`], one type whose [`ErrorKind`]
//! tells the variety), cancellation of a pending call ([`Cancellable`]), frame buffers
//! ([`SndBuf`], [`RcvBuf`], [`Bufs`]), the compressor interfaces ([`Compressor`],
//! [`CompressorFactory`]), streaming ([`Sink`], [`Source`]) and the [`Optional`] and
//! [`Tuple`] wrappers used in handler signatures. `Sink`, `Source` and `Tuple` take their
//! element types as one tuple type parameter.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomPinned;
use std::pin::Pin;

use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::semaphore::{Semaphore, SemaphoreUnits};
use crate::engine::core::sharded::ForeignPtr;
use crate::engine::core::shared_ptr::{LwSharedPtr, SharedPtr};
use crate::engine::core::simple_stream::{
    FragmentIterator, FragmentedMemoryInputStream, MemoryInputStream, SimpleMemoryInputStream,
};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::socket_defs::SocketAddress;
use crate::engine::rpc::rpc::Server;
use crate::engine::util::noncopyable_function::NoncopyableFunction;

/// The clock of RPC timeouts: the low-resolution reactor clock.
pub type RpcClockType = LowresClock;

// used to tag a type for serializers
/// A tag of `T` for the serializers.
pub struct Type<T>(std::marker::PhantomData<T>);

impl<T> Default for Type<T> {
    fn default() -> Self {
        Type(std::marker::PhantomData)
    }
}

/// The counters of a connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Replies received.
    pub replied: u64,
    /// Requests waiting.
    pub pending: u64,
    /// Exceptions received.
    pub exception_received: u64,
    /// Messages sent.
    pub sent_messages: u64,
    /// Requests waiting for a reply.
    pub wait_reply: u64,
    /// Requests timed out.
    pub timeout: u64,
    /// Delay samples taken.
    pub delay_samples: u64,
    /// The sum of the delays, in seconds.
    pub delay_total: f64,
}

/// The type of the connection counters.
pub type CounterType = u64;

/// The id of a connection: a counter and the shard of the server that made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct ConnectionId {
    _id: u64,
}

/// The shard of an invalid connection id.
const INVALID_SHARD: u16 = 0xffff;

impl ConnectionId {
    /// The raw id.
    #[inline]
    pub const fn id(&self) -> u64 {
        self._id
    }

    /// Whether the id is valid.
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.shard() != INVALID_SHARD as usize
    }

    /// The shard of the server.
    #[inline]
    pub const fn shard(&self) -> usize {
        (self._id & 0xffff) as usize
    }

    /// An invalid id with counter `id`.
    #[inline]
    pub const fn make_invalid_id(id: u64) -> ConnectionId {
        Self::make_id(id, INVALID_SHARD)
    }

    /// The id of connection `id` of shard `shard`.
    #[inline]
    pub const fn make_id(id: u64, shard: u16) -> ConnectionId {
        ConnectionId { _id: (id << 16) | shard as u64 }
    }

    /// From a raw id.
    #[inline]
    pub const fn new(id: u64) -> Self {
        ConnectionId { _id: id }
    }
}

/// The id of no connection.
pub const INVALID_CONNECTION_ID: ConnectionId = ConnectionId::make_invalid_id(0);

/// The id in hexadecimal.
impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.id())
    }
}

/// Hashes the id combined with the golden-ratio constant `0x9e3779b9`.
impl Hash for ConnectionId {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.id().wrapping_add(0x9e3779b9));
    }
}

/// The client of a request, as the server sees it.
pub struct ClientInfo {
    /// The address of the client.
    pub addr: SocketAddress,
    /// The server, which outlives its connections.
    pub server: *mut Server,
    /// The id of the connection.
    pub conn_id: ConnectionId,
    /// The data the handlers attach to the client.
    pub user_data: HashMap<Sstring, Box<dyn Any>>,
}

impl ClientInfo {
    /// Attaches `object` under `key` (kept when the key exists, as `emplace`).
    pub fn attach_auxiliary<T: 'static>(&mut self, key: &Sstring, object: T) {
        self.user_data.entry(key.clone()).or_insert_with(|| Box::new(object));
    }

    /// The object attached under `key`.
    ///
    /// # Errors
    /// When nothing is attached under `key`, aborts the connection and fails with
    /// [`missing_auxiliary_error`]; fails with a "bad_any_cast" error when the object is not
    /// a `T`.
    pub fn retrieve_auxiliary<T: 'static>(
        &mut self,
        key: &Sstring,
    ) -> Result<&mut T, ExceptionPtr> {
        if !self.user_data.contains_key(key) {
            return Err(self.report_missing_auxiliary(key));
        }
        let it = self.user_data.get_mut(key);
        it.and_then(|b| b.downcast_mut::<T>()).ok_or_else(|| {
            ExceptionPtr::new(crate::engine::core::future::RuntimeError("bad_any_cast".to_string()))
        })
    }

    /// Aborts the connection and returns the error of a handler that asked for auxiliary data
    /// that was never attached under `key` (for example because the verb that should have
    /// attached it failed first). A missing key is an outcome of the peer's behaviour, not a
    /// programming error, so it must not abort the process.
    fn report_missing_auxiliary(&self, key: &Sstring) -> ExceptionPtr {
        let msg = format!("missing auxiliary data '{}' on connection {}", key, self.conn_id);
        if !self.server.is_null() {
            // SAFETY: the server outlives its connections and their handlers.
            unsafe { (*self.server).abort_connection(self.conn_id) };
        }
        ExceptionPtr::new(missing_auxiliary_error(msg))
    }

    /// The object attached under `key`, if any.
    pub fn retrieve_auxiliary_opt<T: 'static>(&self, key: &Sstring) -> Option<&T> {
        self.user_data.get(key).and_then(|b| b.downcast_ref::<T>())
    }

    /// The object attached under `key`, if any.
    pub fn retrieve_auxiliary_opt_mut<T: 'static>(&mut self, key: &Sstring) -> Option<&mut T> {
        self.user_data.get_mut(key).and_then(|b| b.downcast_mut::<T>())
    }
}

/// The variety of an rpc error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// A generic rpc error.
    Error,
    /// The connection is closed ([`closed_error`]).
    Closed,
    /// The call timed out ([`timeout_error`]).
    Timeout,
    /// The peer does not know the verb ([`unknown_verb_error`]), with the verb.
    UnknownVerb(u64),
    /// The handler failed with an error the protocol cannot carry ([`unknown_exception_error`]).
    UnknownException,
    /// The peer broke the protocol ([`rpc_protocol_error`]).
    RpcProtocol,
    /// The call was canceled ([`canceled_error`]).
    Canceled,
    /// The peer closed the stream ([`stream_closed`]).
    StreamClosed,
    /// The remote handler failed with a message ([`remote_verb_error`]).
    RemoteVerb,
    /// No auxiliary object is attached under the key a handler asked for
    /// ([`missing_auxiliary_error`]).
    MissingAuxiliary,
}

/// An rpc error: a message and its [`ErrorKind`].
#[derive(Clone, Debug)]
pub struct Error {
    _msg: String,
    _kind: ErrorKind,
}

impl Error {
    /// A generic rpc error with message `msg`.
    pub fn new(msg: &str) -> Self {
        Error { _msg: msg.to_string(), _kind: ErrorKind::Error }
    }

    /// The message.
    pub fn what(&self) -> &str {
        &self._msg
    }

    /// The variety of the error.
    pub fn kind(&self) -> ErrorKind {
        self._kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self._msg)
    }
}

impl std::error::Error for Error {}

/// The error of a call on a closed connection.
pub fn closed_error() -> Error {
    Error { _msg: "connection is closed".to_string(), _kind: ErrorKind::Closed }
}

/// The error of a call that timed out.
pub fn timeout_error() -> Error {
    Error { _msg: "rpc call timed out".to_string(), _kind: ErrorKind::Timeout }
}

/// The error of a call to a verb the peer does not know.
pub fn unknown_verb_error(type_: u64) -> Error {
    Error { _msg: "unknown verb".to_string(), _kind: ErrorKind::UnknownVerb(type_) }
}

/// The error of a call whose handler failed with an unknown error.
pub fn unknown_exception_error() -> Error {
    Error { _msg: "unknown exception".to_string(), _kind: ErrorKind::UnknownException }
}

/// The error of a frame that breaks the protocol.
pub fn rpc_protocol_error() -> Error {
    Error { _msg: "rpc protocol exception".to_string(), _kind: ErrorKind::RpcProtocol }
}

/// The error of a canceled call.
pub fn canceled_error() -> Error {
    Error { _msg: "rpc call was canceled".to_string(), _kind: ErrorKind::Canceled }
}

/// The error of a stream the peer closed.
pub fn stream_closed() -> Error {
    Error { _msg: "rpc stream was closed by peer".to_string(), _kind: ErrorKind::StreamClosed }
}

/// The error of a remote handler that failed with `msg`.
pub fn remote_verb_error(msg: impl Into<String>) -> Error {
    Error { _msg: msg.into(), _kind: ErrorKind::RemoteVerb }
}

/// The error of [`ClientInfo::retrieve_auxiliary`] when nothing is attached under the key; the
/// connection is aborted before it is returned.
pub fn missing_auxiliary_error(msg: impl Into<String>) -> Error {
    Error { _msg: msg.into(), _kind: ErrorKind::MissingAuxiliary }
}

/// The type of `NO_WAIT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoWaitType;

// return this from a callback if client does not want to waiting for a reply
/// The value a handler returns when the client does not wait for a reply.
pub const NO_WAIT: NoWaitType = NoWaitType;

/// An optional parameter or return value: absent when the peer is older.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Optional<T>(pub Option<T>);

impl<T> std::ops::Deref for Optional<T> {
    type Target = Option<T>;
    #[inline]
    fn deref(&self) -> &Option<T> {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Optional<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Option<T> {
        &mut self.0
    }
}

/// `optional(value)` when set, `none` otherwise.
impl<T: fmt::Display> fmt::Display for Optional<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(v) => write!(f, "optional({})", v),
            None => f.write_str("none"),
        }
    }
}

/// The deadline of a call, if it has one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OptTimePoint(
    pub Option<<RpcClockType as crate::engine::core::lowres_clock::Clock>::TimePoint>,
);

/// Cancels a request (its send, its wait for the reply).
///
/// The pending send and wait keep pointers back to it (`send_back_pointer`,
/// `wait_back_pointer` point at their `pcancel`), so it must not move while registered:
/// a `Cancellable` is `!Unpin` and the calls take it as `Pin<&mut Cancellable>`. Once
/// registered it stays where it is until it is dropped, and
/// [`Cancellable::assign_from`] takes over another pinned one, fixing the back pointers.
/// Dropping it cancels, and also clears the back pointers of a send that could not be
/// withdrawn, so none is left dangling.
pub struct Cancellable {
    /// Cancels the send (the closure, a pointer and an iterator or id, lives in the inline
    /// storage, without an allocation).
    pub(crate) cancel_send: NoncopyableFunction<fn()>,
    /// Cancels the wait for the reply.
    pub(crate) cancel_wait: NoncopyableFunction<fn()>,
    /// The pointer of the send to this object.
    pub(crate) send_back_pointer: *mut *mut Cancellable,
    /// The pointer of the wait to this object.
    pub(crate) wait_back_pointer: *mut *mut Cancellable,
    _pinned: PhantomPinned,
}

impl Default for Cancellable {
    fn default() -> Self {
        Cancellable {
            cancel_send: NoncopyableFunction::<fn()>::empty(),
            cancel_wait: NoncopyableFunction::<fn()>::empty(),
            send_back_pointer: std::ptr::null_mut(),
            wait_back_pointer: std::ptr::null_mut(),
            _pinned: PhantomPinned,
        }
    }
}

impl Cancellable {
    /// Cancels this one, then takes over `x`.
    pub fn assign_from(self: Pin<&mut Self>, x: Pin<&mut Cancellable>) {
        // SAFETY: neither object is moved out of its place; both stay pinned, and the back
        // pointers are rewritten to the address of `self`, which does not change.
        let (this, x) = unsafe {
            (
                self.get_unchecked_mut() as *mut Cancellable,
                x.get_unchecked_mut() as *mut Cancellable,
            )
        };
        if std::ptr::eq(x, this) {
            return;
        }
        // SAFETY: `this` and `x` are distinct live objects; the calls below reach them only
        // through these pointers, and the back pointers, while set, point at the `pcancel` of
        // a pending send or wait, which clears them before going away.
        unsafe {
            Self::cancel_raw(this);
            Self::clear_back_pointers(this);
            (*this).cancel_send = std::mem::take(&mut (*x).cancel_send);
            (*this).cancel_wait = std::mem::take(&mut (*x).cancel_wait);
            (*this).send_back_pointer = (*x).send_back_pointer;
            (*this).wait_back_pointer = (*x).wait_back_pointer;
            if !(*this).send_back_pointer.is_null() {
                *(*this).send_back_pointer = this;
                (*x).send_back_pointer = std::ptr::null_mut();
            }
            if !(*this).wait_back_pointer.is_null() {
                *(*this).wait_back_pointer = this;
                (*x).wait_back_pointer = std::ptr::null_mut();
            }
        }
    }

    /// Cancels the send and the wait.
    pub fn cancel(self: Pin<&mut Self>) {
        // SAFETY: the object is not moved; cancel_raw reaches it only through the pointer.
        unsafe { Self::cancel_raw(self.get_unchecked_mut()) }
    }

    /// Cancels through a pointer: the cancel functions clear this object's slots through
    /// their `pcancel` (`uncancellable()`), so no reference to it may be live while they run.
    /// Each function is taken out of its slot while it runs, so clearing the slot does not
    /// drop the running closure; it goes back when the request kept it (a send at the front
    /// of the queue cannot be withdrawn, and keeps its `cancel_send` then).
    ///
    /// # Safety
    /// `this` points at a live `Cancellable` with no reference to it live.
    unsafe fn cancel_raw(this: *mut Cancellable) {
        // SAFETY: the contract above; the slots are read and written only through `this`.
        unsafe {
            let mut f = std::mem::take(&mut (*this).cancel_send);
            if f.is_some() {
                f.call();
                if !(*this).cancel_send.is_some() && !(*this).send_back_pointer.is_null() {
                    (*this).cancel_send = f;
                }
            }
            let mut f = std::mem::take(&mut (*this).cancel_wait);
            if f.is_some() {
                f.call();
                if !(*this).cancel_wait.is_some() && !(*this).wait_back_pointer.is_null() {
                    (*this).cancel_wait = f;
                }
            }
        }
    }

    /// Detaches the pending send and wait that still point at this object.
    ///
    /// # Safety
    /// As `Cancellable::cancel_raw`.
    unsafe fn clear_back_pointers(this: *mut Cancellable) {
        // SAFETY: a set back pointer points at the `pcancel` of a live send or wait.
        unsafe {
            if !(*this).send_back_pointer.is_null() {
                *(*this).send_back_pointer = std::ptr::null_mut();
                (*this).send_back_pointer = std::ptr::null_mut();
            }
            if !(*this).wait_back_pointer.is_null() {
                *(*this).wait_back_pointer = std::ptr::null_mut();
                (*this).wait_back_pointer = std::ptr::null_mut();
            }
            (*this).cancel_send = NoncopyableFunction::<fn()>::empty();
            (*this).cancel_wait = NoncopyableFunction::<fn()>::empty();
        }
    }

    /// The address a pending send or wait records, from the pinned object.
    ///
    /// # Safety
    /// The caller stores the pointer only in a send or wait whose `pcancel` the object can
    /// reach through its back pointers (so dropping or reassigning it detaches them).
    #[inline]
    pub(crate) unsafe fn as_registered(self: Pin<&mut Self>) -> *mut Cancellable {
        // SAFETY: the object stays pinned; the pointer is only kept as the contract says.
        unsafe { self.get_unchecked_mut() }
    }
}

impl Drop for Cancellable {
    fn drop(&mut self) {
        let this: *mut Cancellable = self;
        // SAFETY: `self` is not used again in this function; the pinned object is still in
        // its place, and cancel_raw and clear_back_pointers reach it only through `this`.
        unsafe {
            Self::cancel_raw(this);
            Self::clear_back_pointers(this);
        }
    }
}

/// The buffers of a frame: one, or several chunks.
pub enum Bufs {
    /// Several chunks.
    Many(Vec<TemporaryBuffer<u8>>),
    /// One contiguous buffer.
    One(TemporaryBuffer<u8>),
}

impl Default for Bufs {
    #[inline]
    fn default() -> Self {
        Bufs::Many(Vec::new())
    }
}

/// A received frame.
#[derive(Default)]
pub struct RcvBuf {
    /// Its size.
    pub size: u32,
    /// The memory units it holds.
    pub su: Option<SemaphoreUnits>,
    /// Its buffers.
    pub bufs: Bufs,
}

impl RcvBuf {
    /// An empty frame of size `size`.
    #[inline]
    pub fn with_size(size: usize) -> Self {
        RcvBuf { size: size as u32, ..Default::default() }
    }

    /// A frame of one buffer.
    #[inline]
    pub fn from_buffer(b: TemporaryBuffer<u8>) -> Self {
        RcvBuf { size: b.size() as u32, su: None, bufs: Bufs::One(b) }
    }

    /// A frame of several buffers.
    #[inline]
    pub fn from_buffers(bufs: Vec<TemporaryBuffer<u8>>, size: usize) -> Self {
        RcvBuf { size: size as u32, su: None, bufs: Bufs::Many(bufs) }
    }
}

/// A frame to send.
#[derive(Default)]
pub struct SndBuf {
    /// Its size.
    pub size: u32,
    /// Its buffers.
    pub bufs: Bufs,
}

impl SndBuf {
    // Preferred, but not required, chunk size.
    /// The preferred size of a chunk.
    pub const CHUNK_SIZE: usize = 128 * 1024;

    /// A frame of one buffer.
    #[inline]
    pub fn from_buffer(b: TemporaryBuffer<u8>) -> Self {
        SndBuf { size: b.size() as u32, bufs: Bufs::One(b) }
    }

    /// A frame of several buffers.
    #[inline]
    pub fn from_buffers(bufs: Vec<TemporaryBuffer<u8>>, size: usize) -> Self {
        SndBuf { size: size as u32, bufs: Bufs::Many(bufs) }
    }
}

/// An iterator over the buffers of a frame ([`SndBuf`] or [`RcvBuf`]): a pointer into them.
#[derive(Clone, Copy, Debug)]
pub struct BufsIterator {
    _p: *mut TemporaryBuffer<u8>,
}

impl BufsIterator {
    /// The iterator at the first buffer of `v`.
    ///
    /// # Safety
    /// `v` and its buffers stay alive and in place, and are not used elsewhere, while the
    /// iterator or a stream built on it is used; the streams read or write only within the
    /// total size of the buffers.
    #[inline]
    pub unsafe fn begin(v: &mut [TemporaryBuffer<u8>]) -> Self {
        BufsIterator { _p: v.as_mut_ptr() }
    }
}

// SAFETY: a BufsIterator is made only by the unsafe `begin()`, whose contract keeps the
// buffers valid while the iterator and its streams are used.
unsafe impl FragmentIterator for BufsIterator {
    #[inline]
    fn fragment(&self) -> (*mut u8, usize) {
        // SAFETY: the memory streams only dereference the iterator within the size they
        // were given, which the buffers of the frame cover (begin()'s contract).
        unsafe { ((*self._p).get_write(), (*self._p).size()) }
    }
    #[inline]
    fn advance(&mut self) {
        self._p = self._p.wrapping_add(1);
    }
}

/// A stream over the buffers of a received frame (`memory_input_stream<rcv_buf::iterator>`);
/// the frame outlives it.
pub type DeserializerStream = MemoryInputStream<BufsIterator>;

/// The stream the deserializers read a frame from.
///
/// # Safety
/// `input` stays alive and in place, and unused elsewhere, while the stream is used.
#[inline]
pub unsafe fn make_deserializer_stream(input: &mut RcvBuf) -> DeserializerStream {
    let size = input.size as usize;
    match &mut input.bufs {
        Bufs::One(b) => MemoryInputStream::Simple(
            // SAFETY: the caller's contract covers the buffer.
            unsafe { SimpleMemoryInputStream::new(b.get(), b.size()) },
        ),
        // SAFETY: the caller's contract covers the buffers.
        Bufs::Many(ar) => MemoryInputStream::Fragmented(FragmentedMemoryInputStream::new(
            unsafe { BufsIterator::begin(ar) },
            size,
        )),
    }
}

/// A compression algorithm of a connection.
pub trait Compressor {
    // compress data and leave head_space bytes at the beginning of returned buffer
    /// Compresses `data`, leaving `head_space` bytes at the beginning of the result.
    fn compress(&mut self, head_space: usize, data: SndBuf) -> Result<SndBuf, ExceptionPtr>;
    // decompress data
    /// Decompresses `data`.
    fn decompress(&mut self, data: RcvBuf) -> Result<RcvBuf, ExceptionPtr>;
    /// The name of the algorithm.
    fn name(&self) -> Sstring;
    /// Releases the resources of the compressor.
    fn close(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

/// The function a compressor calls to have an empty frame sent to its peer.
pub type SendEmptyFrame = Box<dyn Fn() -> Future<()>>;

// factory to create compressor for a connection
/// Creates the compressor of a connection.
pub trait CompressorFactory {
    // return feature string that will be sent as part of protocol negotiation
    /// The feature string sent in the negotiation.
    fn supported(&self) -> &Sstring;
    // negotiate compress algorithm
    // send_empty_frame() requests an empty frame to be sent to the peer compressor on the other side of the connection.
    // By attaching a header to this empty frame, the compressor can communicate somthing to the peer,
    // send_empty_frame() mustn't be called from inside compress() or decompress().
    /// Negotiates the algorithm, with a way to send an empty frame to the peer.
    fn negotiate_with(
        &self,
        feature: Sstring,
        is_server: bool,
        _send_empty_frame: SendEmptyFrame,
    ) -> Option<Box<dyn Compressor>> {
        self.negotiate(feature, is_server)
    }
    /// Negotiates the algorithm.
    fn negotiate(&self, feature: Sstring, is_server: bool) -> Option<Box<dyn Compressor>>;
}

/// A connection reached from another shard: a shared handle to a foreign pointer to the
/// connection, freed on its owner shard; see [`ConnectionPtr`](crate::engine::rpc::rpc::ConnectionPtr).
pub type XshardConnectionPtr = LwSharedPtr<ForeignPtr<crate::engine::rpc::rpc::ConnectionPtr>>;
/// The most buffers a stream queues.
pub const MAX_QUEUED_STREAM_BUFFERS: usize = 50;
/// The most memory the buffers of a stream take.
pub const MAX_STREAM_BUFFERS_MEMORY: usize = 100 * 1024;

/// The data members of `sink<Out...>::impl`.
pub struct SinkImplBase {
    /// The connection.
    pub _con: XshardConnectionPtr,
    /// The memory of the buffers in flight.
    pub _sem: Semaphore,
    /// The first error of the sink.
    pub _ex: Option<ExceptionPtr>,
}

impl SinkImplBase {
    /// The data of a sink over `con`.
    pub fn new(con: XshardConnectionPtr) -> Self {
        SinkImplBase { _con: con, _sem: Semaphore::new(MAX_STREAM_BUFFERS_MEMORY), _ex: None }
    }
}

/// The implementation behind a [`Sink`]: sends values and closes the stream.
pub trait SinkImpl<Out> {
    /// The common data.
    fn base(&self) -> &SinkImplBase;
    /// Sends `args`.
    fn call(&mut self, args: &Out) -> Future<()>;
    /// Closes the sink.
    fn close(&mut self) -> Future<()>;
    /// Flushes the sink.
    fn flush(&mut self) -> Future<()>;
}

// send data Out...
/// Sends the values `Out` (a tuple) over a stream.
pub struct Sink<Out: 'static> {
    _impl: SharedPtr<dyn SinkImpl<Out>>,
}

impl<Out: 'static> Clone for Sink<Out> {
    fn clone(&self) -> Self {
        Sink { _impl: self._impl.clone() }
    }
}

impl<Out: 'static> Sink<Out> {
    /// A sink over `impl_`.
    ///
    /// # Panics
    /// If `impl_` is null: every call goes through it.
    pub fn new(impl_: SharedPtr<dyn SinkImpl<Out>>) -> Self {
        assert!(impl_.get().is_some(), "a sink needs an impl");
        Sink { _impl: impl_ }
    }

    #[inline]
    fn imp(&self) -> *mut dyn SinkImpl<Out> {
        match self._impl.get() {
            Some(p) => p.as_ptr(),
            // `new` checks it: the impl is never null.
            None => unreachable!("a sink always has an impl"),
        }
    }

    /// Sends `args`.
    ///
    /// # Safety
    /// This sink, or a copy of it, stays alive until the returned future resolves (the
    /// impl's continuations point at it). The impl is reached by a `&mut` only for the length of the
    /// call, which returns before any continuation runs, so calls through different copies
    /// never overlap.
    pub unsafe fn call(&mut self, args: &Out) -> Future<()> {
        // SAFETY: the shared pointer keeps the impl alive, and the contract above makes this the
        // only reference to it during the call.
        unsafe { (*self.imp()).call(args) }
    }

    /// Closes the sink.
    ///
    /// # Safety
    /// This sink, or a copy of it, stays alive until the returned future resolves (the
    /// impl's continuations point at it). The impl is reached by a `&mut` only for the length of the
    /// call, which returns before any continuation runs, so calls through different copies
    /// never overlap.
    pub unsafe fn close(&mut self) -> Future<()> {
        // SAFETY: as in call().
        unsafe { (*self.imp()).close() }
    }

    // Calling this function makes sure that any data buffered
    // by the stream sink will be flushed to the network.
    // It does not mean the data was received by the corresponding
    // source.
    /// Flushes the data buffered by the sink to the network.
    ///
    /// # Safety
    /// This sink, or a copy of it, stays alive until the returned future resolves (the
    /// impl's continuations point at it). The impl is reached by a `&mut` only for the length of the
    /// call, which returns before any continuation runs, so calls through different copies
    /// never overlap.
    pub unsafe fn flush(&mut self) -> Future<()> {
        // SAFETY: as in call().
        unsafe { (*self.imp()).flush() }
    }

    /// The impl, for the rpc code.
    pub fn impl_(&self) -> &SharedPtr<dyn SinkImpl<Out>> {
        &self._impl
    }
}

/// The data members of `source<In...>::impl`.
pub struct SourceImplBase {
    /// The connection.
    pub _con: XshardConnectionPtr,
    /// The received buffers.
    pub _bufs: CircularBuffer<ForeignPtr<Box<RcvBuf>>>,
}

impl SourceImplBase {
    /// The data of a source over `con`.
    pub fn new(con: XshardConnectionPtr) -> Self {
        let mut _bufs = CircularBuffer::new();
        _bufs.reserve(MAX_QUEUED_STREAM_BUFFERS);
        SourceImplBase { _con: con, _bufs }
    }
}

/// The implementation behind a [`Source`]: receives values until the stream ends.
pub trait SourceImpl<In> {
    /// The common data.
    fn base(&self) -> &SourceImplBase;
    /// The common data, mutable.
    fn base_mut(&mut self) -> &mut SourceImplBase;
    /// The next values, or none at the end of the stream.
    fn call(&mut self) -> Future<Option<In>>;
}

// receive data In...
/// Receives the values `In` (a tuple) from a stream.
pub struct Source<In: 'static> {
    _impl: SharedPtr<dyn SourceImpl<In>>,
}

impl<In: 'static> Clone for Source<In> {
    fn clone(&self) -> Self {
        Source { _impl: self._impl.clone() }
    }
}

impl<In: 'static> Source<In> {
    /// A source over `impl_`.
    ///
    /// # Panics
    /// If `impl_` is null: every call goes through it.
    pub fn new(impl_: SharedPtr<dyn SourceImpl<In>>) -> Self {
        assert!(impl_.get().is_some(), "a source needs an impl");
        Source { _impl: impl_ }
    }

    /// The next values, or none at the end of the stream.
    ///
    /// # Safety
    /// As `Sink::call`, for this source and its copies.
    pub unsafe fn call(&mut self) -> Future<Option<In>> {
        match self._impl.get() {
            // SAFETY: the shared pointer keeps the impl alive, and the contract above makes this
            // the only reference to it during the call.
            Some(p) => unsafe { (*p.as_ptr()).call() },
            // `new` checks it: the impl is never null.
            None => unreachable!("a source always has an impl"),
        }
    }

    /// The impl, for the rpc code.
    pub fn impl_(&self) -> &SharedPtr<dyn SourceImpl<In>> {
        &self._impl
    }
}

/// Used to return multiple values in rpc without variadic futures
///
/// If you wish to return multiple values from an rpc procedure, use a
/// handler returning `Future<Tuple<(return types...)>>`. This
/// will be marshalled by rpc, so you do not need to have your Serializer
/// serialize/deserialize this tuple type. The serialization format is
/// compatible with the deprecated variadic future support, and is compatible
/// with adding new return types in a backwards compatible way provided new
/// parameters are appended only, and wrapped with [`Optional`]:
/// `Future<Tuple<(existing return types..., Optional<NewReturnType>)>>`
///
/// You may also use another tuple type, such as a plain Rust tuple. In this case,
/// your Serializer type must recognize your tuple type and provide serialization
/// and deserialization for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tuple<T>(pub T);

impl<T> From<T> for Tuple<T> {
    #[inline]
    fn from(x: T) -> Self {
        Tuple(x)
    }
}
