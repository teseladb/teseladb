//! Buffered input and output streams over pluggable data sources and sinks.
//!
//! Two abstract interfaces, [`DataSourceImpl`] (wrapped by [`DataSource`]) and
//! [`DataSinkImpl`] (wrapped by [`DataSink`]), acquire bulk data from, or push bulk data
//! to, some provider: a TCP connection, a disk file or a memory buffer. Two concrete types,
//! [`InputStream`] and [`OutputStream`], buffer that data and give easier means to process
//! it: exact-size reads, consumer loops ([`InputStream::consume`]), buffered and zero-copy
//! writes, and flush batching through the reactor.
//!
//! The streams are generic over a one-byte element type (`u8` by default); sources and
//! sinks always move `TemporaryBuffer<u8>`. Overloaded operations have distinct names:
//! `DataSinkImpl::put` (packet), `put_vec`, `put_buffer`; `OutputStream::write` (slice),
//! `write_raw`, `write_cstr`, `write_sstring`, `write_str`, `write_packet`,
//! `write_scattered`, `write_buffer`; `InputStream::consume` (by value) and `consume_ref`.
//! The longer method bodies live in [`crate::engine::core::iostream_impl`].
//!
//! Each stream keeps its state in its own node that never moves. Pending operations hold a
//! counted `StateRef` to it, so moving a stream is safe, and dropping a stream while one
//! of its operations is pending aborts the process instead of freeing the state under it.
//! Streams belong to one shard, and their methods must be called sequentially.

use crate::engine::net::node_ptr::NodePtr;
use std::ptr;

use std::cell::Cell;

use crate::engine::core::future::{ExceptionPtr, Future, LogicError, Promise, make_ready_future};
use crate::engine::core::reactor_backend::engine_assert;
use crate::engine::core::scattered_message::ScatteredMessage;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::{ByteType, TemporaryBuffer};
use crate::engine::net::packet::{Fragment, Packet};

pub use crate::engine::core::iostream_impl::{copy, fallback_put, internal};

/// A provider of bulk input data, behind a [`DataSource`].
pub trait DataSourceImpl: 'static {
    /// Returns the next buffer of data; an empty buffer means the end of the data.
    fn get(&mut self) -> Future<TemporaryBuffer<u8>>;
    /// Skips `n` bytes and returns the rest of the buffer where the skip ended. The default
    /// reads and discards buffers with `get`.
    fn skip(&mut self, n: u64) -> Future<TemporaryBuffer<u8>> {
        crate::engine::core::iostream_impl::data_source_impl_skip(self, n)
    }
    /// Releases the source's resources; the default does nothing.
    fn close(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

/// An owned [`DataSourceImpl`], or an empty source.
///
/// The implementation is kept in a node, not a `Box`: implementations whose continuations
/// keep a pointer to themselves (the node's root pointer) stay valid when the source
/// moves.
#[derive(Default)]
pub struct DataSource {
    _dsi: Option<NodePtr<dyn DataSourceImpl>>,
}

/// The buffer type sources produce.
pub type DataSourceTmpBuf = TemporaryBuffer<u8>;

impl DataSource {
    /// The implementation, if any. Kept for wrappers; currently unused in the crate.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn impl_(&mut self) -> Option<*mut dyn DataSourceImpl> {
        self._dsi.as_ref().map(NodePtr::as_ptr)
    }

    /// A source over `dsi`.
    #[inline]
    pub fn new(dsi: Box<dyn DataSourceImpl>) -> Self {
        // SAFETY: the pointer comes from `Box::into_raw` and the node is its only owner.
        DataSource { _dsi: Some(unsafe { NodePtr::from_raw(Box::into_raw(dsi)) }) }
    }

    /// A source over an implementation already in its node (whose root pointer the
    /// implementation may keep).
    #[inline]
    pub(crate) fn from_node(dsi: NodePtr<dyn DataSourceImpl>) -> Self {
        DataSource { _dsi: Some(dsi) }
    }

    /// The implementation.
    ///
    /// # Panics
    /// Fails an assertion on an empty source.
    #[inline]
    fn dsi(&mut self) -> &mut dyn DataSourceImpl {
        match self._dsi.as_ref() {
            // SAFETY: the implementation is live while its node is; the reference is tied
            // to the source's exclusive borrow, and the implementation's continuations
            // reach it by its root pointer only while they run, not during this call.
            Some(d) => unsafe { d.get_mut() },
            None => crate::engine::util::assert::internal::assert_fail(
                "_dsi",
                file!(),
                line!() as i32,
                "use of an empty teseladb::data_source",
            ),
        }
    }

    /// Returns the next buffer; empty at the end of the data.
    ///
    /// # Panics
    /// On an empty source.
    #[inline]
    pub fn get(&mut self) -> Future<DataSourceTmpBuf> {
        self.dsi().get()
    }
    /// Skips `n` bytes; see [`DataSourceImpl::skip`].
    ///
    /// # Panics
    /// On an empty source.
    #[inline]
    pub fn skip(&mut self, n: u64) -> Future<DataSourceTmpBuf> {
        self.dsi().skip(n)
    }
    /// Closes the implementation.
    ///
    /// # Panics
    /// On an empty source.
    #[inline]
    pub fn close(&mut self) -> Future<()> {
        self.dsi().close()
    }
}

/// A consumer of bulk output data, behind a [`DataSink`].
pub trait DataSinkImpl: 'static {
    /// Allocates a buffer of `size` bytes for the stream to fill; the default is a plain
    /// [`TemporaryBuffer::new`]. Sinks with alignment needs override it.
    fn allocate_buffer(&mut self, size: usize) -> TemporaryBuffer<u8> {
        TemporaryBuffer::<u8>::new(size)
    }
    /// Writes a packet.
    fn put(&mut self, data: Packet) -> Future<()>;
    /// Writes several buffers; the default wraps them in one packet.
    fn put_vec(&mut self, data: Vec<TemporaryBuffer<u8>>) -> Future<()> {
        let mut p = Packet::new();
        p.reserve(data.len() as i32);
        for mut buf in data {
            let frag = Fragment { base: buf.get_write(), size: buf.size() };
            p = Packet::append_zero_copy(p, frag, buf.release());
        }
        self.put(p)
    }
    /// Writes one buffer; the default wraps it in a packet.
    fn put_buffer(&mut self, mut buf: TemporaryBuffer<u8>) -> Future<()> {
        let frag = Fragment { base: buf.get_write(), size: buf.size() };
        self.put(Packet::from_fragment_deleter(frag, buf.release()))
    }
    /// Flushes written data; the default does nothing.
    fn flush(&mut self) -> Future<()> {
        make_ready_future(())
    }
    /// Closes the sink to further writes.
    fn close(&mut self) -> Future<()>;

    /// The maximum buffer size the sink accepts. An output stream constructed without a
    /// buffer size uses this value and does not put larger buffers (see
    /// [`OutputStreamOptions::trim_to_size`]).
    fn buffer_size(&self) -> usize {
        engine_assert!(false, "Data sink must have the buffer_size() method overload");
        0
    }

    /// Whether the sink supports batched flushes ([`OutputStreamOptions::batch_flushes`]).
    ///
    /// A sink that returns true must handle flush errors that happen in the background,
    /// in [`DataSinkImpl::on_batch_flush_error`]; otherwise batching has no effect.
    fn can_batch_flushes(&self) -> bool {
        false
    }

    /// Called when a background (batched) flush fails.
    fn on_batch_flush_error(&mut self) {
        engine_assert!(false, "Data sink must implement on_batch_flush_error() method");
    }

    // Sinks that only implement `put_buffer` can implement `put` with [`fallback_put`].
}

/// An owned [`DataSinkImpl`], or an empty sink.
///
/// The implementation is kept in a node, as in [`DataSource`]. Every method panics on an
/// empty sink.
#[derive(Default)]
pub struct DataSink {
    _dsi: Option<NodePtr<dyn DataSinkImpl>>,
}

impl DataSink {
    /// A sink over `dsi`.
    #[inline]
    pub fn new(dsi: Box<dyn DataSinkImpl>) -> Self {
        // SAFETY: the pointer comes from `Box::into_raw` and the node is its only owner.
        DataSink { _dsi: Some(unsafe { NodePtr::from_raw(Box::into_raw(dsi)) }) }
    }

    /// A sink over an implementation already in its node (whose root pointer the
    /// implementation may keep).
    #[inline]
    pub(crate) fn from_node(dsi: NodePtr<dyn DataSinkImpl>) -> Self {
        DataSink { _dsi: Some(dsi) }
    }

    /// The implementation.
    ///
    /// # Panics
    /// Fails an assertion on an empty sink.
    #[inline]
    fn dsi(&mut self) -> &mut dyn DataSinkImpl {
        match self._dsi.as_ref() {
            // SAFETY: as in `DataSource::dsi`.
            Some(d) => unsafe { d.get_mut() },
            None => crate::engine::util::assert::internal::assert_fail(
                "_dsi",
                file!(),
                line!() as i32,
                "use of an empty teseladb::data_sink",
            ),
        }
    }

    #[inline]
    fn dsi_ref(&self) -> &dyn DataSinkImpl {
        match self._dsi.as_ref() {
            // SAFETY: as in `DataSource::dsi`, shared.
            Some(d) => unsafe { d.get() },
            None => crate::engine::util::assert::internal::assert_fail(
                "_dsi",
                file!(),
                line!() as i32,
                "use of an empty teseladb::data_sink",
            ),
        }
    }

    /// See [`DataSinkImpl::allocate_buffer`].
    #[inline]
    pub fn allocate_buffer(&mut self, size: usize) -> TemporaryBuffer<u8> {
        self.dsi().allocate_buffer(size)
    }
    /// See [`DataSinkImpl::put_vec`].
    #[inline]
    pub fn put_vec(&mut self, data: Vec<TemporaryBuffer<u8>>) -> Future<()> {
        self.dsi().put_vec(data)
    }
    /// See [`DataSinkImpl::put_buffer`].
    #[inline]
    pub fn put_buffer(&mut self, data: TemporaryBuffer<u8>) -> Future<()> {
        self.dsi().put_buffer(data)
    }
    /// See [`DataSinkImpl::put`].
    #[inline]
    pub fn put(&mut self, p: Packet) -> Future<()> {
        self.dsi().put(p)
    }
    /// See [`DataSinkImpl::flush`].
    #[inline]
    pub fn flush(&mut self) -> Future<()> {
        self.dsi().flush()
    }
    /// Closes the implementation.
    ///
    /// # Panics
    /// On an empty source.
    #[inline]
    pub fn close(&mut self) -> Future<()> {
        self.dsi().close()
    }

    /// See [`DataSinkImpl::buffer_size`].
    #[inline]
    pub fn buffer_size(&self) -> usize {
        self.dsi_ref().buffer_size()
    }
    /// See [`DataSinkImpl::can_batch_flushes`].
    #[inline]
    pub fn can_batch_flushes(&self) -> bool {
        self.dsi_ref().can_batch_flushes()
    }
    /// See [`DataSinkImpl::on_batch_flush_error`].
    #[inline]
    pub fn on_batch_flush_error(&mut self) {
        self.dsi().on_batch_flush_error();
    }
}

/// Consumer result: all the input was consumed and the consumer is ready for more.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContinueConsuming;

/// Consumer result: the consumer is done; carries the unconsumed part of the last buffer
/// (possibly empty), which the stream keeps for the next read.
pub struct StopConsuming<CharType: ByteType = u8> {
    _buf: TemporaryBuffer<CharType>,
}

impl<CharType: ByteType> StopConsuming<CharType> {
    /// A stop result carrying the unconsumed bytes `buf`.
    #[inline]
    pub fn new(buf: TemporaryBuffer<CharType>) -> Self {
        StopConsuming { _buf: buf }
    }

    /// The unconsumed bytes.
    #[inline]
    pub fn get_buffer(&mut self) -> &mut TemporaryBuffer<CharType> {
        &mut self._buf
    }
    /// The unconsumed bytes, shared.
    #[inline]
    pub fn get_buffer_ref(&self) -> &TemporaryBuffer<CharType> {
        &self._buf
    }
}

/// Consumer result: all the input was consumed and the next bytes should be skipped
/// before the next buffer is delivered.
#[derive(Clone, Copy, Debug)]
pub struct SkipBytes {
    _value: u64,
}

impl SkipBytes {
    /// A skip of `v` bytes.
    #[inline]
    pub const fn new(v: u64) -> Self {
        SkipBytes { _value: v }
    }
    /// The number of bytes to skip.
    #[inline]
    pub const fn get_value(&self) -> u64 {
        self._value
    }
}

/// The three possible consumer results.
pub enum ConsumptionVariant<CharType: ByteType = u8> {
    /// See [`ContinueConsuming`].
    Continue(ContinueConsuming),
    /// See [`StopConsuming`].
    Stop(StopConsuming<CharType>),
    /// See [`SkipBytes`].
    Skip(SkipBytes),
}

/// What a consumer returns for each buffer: one of [`ConsumptionVariant`].
pub struct ConsumptionResult<CharType: ByteType = u8> {
    _result: ConsumptionVariant<CharType>,
}

impl<CharType: ByteType> ConsumptionResult<CharType> {
    /// The result.
    #[inline]
    pub fn get(&mut self) -> &mut ConsumptionVariant<CharType> {
        &mut self._result
    }
    /// The result, shared.
    #[inline]
    pub fn get_ref(&self) -> &ConsumptionVariant<CharType> {
        &self._result
    }
}

impl<CharType: ByteType> From<Option<TemporaryBuffer<CharType>>> for ConsumptionResult<CharType> {
    /// Converts the deprecated "unconsumed remainder" form: `None` continues, `Some(buf)`
    /// stops with `buf` unconsumed.
    #[inline]
    fn from(opt_buf: Option<TemporaryBuffer<CharType>>) -> Self {
        match opt_buf {
            Some(b) => {
                ConsumptionResult { _result: ConsumptionVariant::Stop(StopConsuming::new(b)) }
            }
            None => ConsumptionResult { _result: ConsumptionVariant::Continue(ContinueConsuming) },
        }
    }
}

impl<CharType: ByteType> From<ContinueConsuming> for ConsumptionResult<CharType> {
    /// A continue result.
    #[inline]
    fn from(c: ContinueConsuming) -> Self {
        ConsumptionResult { _result: ConsumptionVariant::Continue(c) }
    }
}

impl<CharType: ByteType> From<StopConsuming<CharType>> for ConsumptionResult<CharType> {
    /// A stop result.
    #[inline]
    fn from(stop: StopConsuming<CharType>) -> Self {
        ConsumptionResult { _result: ConsumptionVariant::Stop(stop) }
    }
}

impl<CharType: ByteType> From<SkipBytes> for ConsumptionResult<CharType> {
    /// A skip result.
    #[inline]
    fn from(skip: SkipBytes) -> Self {
        ConsumptionResult { _result: ConsumptionVariant::Skip(skip) }
    }
}

/// A continue [`ConsumptionResult`].
#[inline]
pub fn continue_consuming<CharType: ByteType>() -> ConsumptionResult<CharType> {
    ContinueConsuming.into()
}

/// A stop [`ConsumptionResult`] with `buf` unconsumed.
#[inline]
pub fn stop_consuming<CharType: ByteType>(
    buf: TemporaryBuffer<CharType>,
) -> ConsumptionResult<CharType> {
    StopConsuming::new(buf).into()
}

/// A skip [`ConsumptionResult`] of `v` bytes.
#[inline]
pub fn skip_bytes<CharType: ByteType>(v: u64) -> ConsumptionResult<CharType> {
    SkipBytes::new(v).into()
}

/// A consumer for [`InputStream::consume`].
///
/// It operates on each buffer given to it and returns a future [`ConsumptionResult`]:
///  - continue, when it consumed all the input and is ready for more;
///  - stop, when it is done; the result carries the unconsumed part of the last buffer,
///    which may be empty;
///  - skip, when it consumed all the input and wants bytes skipped before the next buffer.
///
/// Closures `FnMut(TemporaryBuffer) -> Future<ConsumptionResult>` implement it. A consumer
/// in the deprecated "unconsumed remainder" form (`None` to continue, `Some(rest)` to
/// stop) is wrapped in [`ObsoleteConsumer`].
pub trait InputStreamConsumer<CharType: ByteType> {
    /// Consumes `buf`; an empty buffer signals the end of the stream.
    fn call(&mut self, buf: TemporaryBuffer<CharType>) -> Future<ConsumptionResult<CharType>>;
}

impl<CharType: ByteType, F> InputStreamConsumer<CharType> for F
where
    F: FnMut(TemporaryBuffer<CharType>) -> Future<ConsumptionResult<CharType>>,
{
    #[inline]
    fn call(&mut self, buf: TemporaryBuffer<CharType>) -> Future<ConsumptionResult<CharType>> {
        self(buf)
    }
}

/// Adapts a consumer in the deprecated form, which returns the unconsumed remainder, to
/// [`InputStreamConsumer`].
pub struct ObsoleteConsumer<F>(pub F);

impl<CharType: ByteType, F> InputStreamConsumer<CharType> for ObsoleteConsumer<F>
where
    F: FnMut(TemporaryBuffer<CharType>) -> Future<Option<TemporaryBuffer<CharType>>>,
{
    #[inline]
    fn call(&mut self, buf: TemporaryBuffer<CharType>) -> Future<ConsumptionResult<CharType>> {
        (self.0)(buf).then(|r: Option<TemporaryBuffer<CharType>>| {
            make_ready_future(ConsumptionResult::from(r))
        })
    }
}

/// Buffers data from a [`DataSource`] and gives the user a stream interface.
///
/// All methods must be called sequentially: No method may be called before the future of the previous one has resolved.
///
/// The state lives in its own allocation ([`InputStreamState`], in a node that is never
/// moved), which the continuations of the pending operations reach through the state's root
/// pointer (`StateRef`): moving the stream does not move it, and dropping the stream
/// while a continuation of it exists aborts, so the stream must outlive its futures.
pub struct InputStream<CharType: ByteType = u8> {
    s: NodePtr<InputStreamState<CharType>>,
}

/// The state of an [`InputStream`]: its source, its current buffer and its end flag.
pub struct InputStreamState<CharType: ByteType = u8> {
    /// The state's own address with the provenance of its node, written once by the
    /// stream; the continuations keep this pointer.
    pub(crate) _this: *mut InputStreamState<CharType>,
    pub(crate) _fd: DataSource,
    pub(crate) _buf: TemporaryBuffer<CharType>,
    pub(crate) _eof: bool,
    /// Continuations that still refer to this state (each holds a `StateRef`).
    pub(crate) _pending: Cell<u32>,
}

/// The deprecated consumer result: `None` to continue, `Some(rest)` to stop. Kept for
/// compatibility only; new code should use [`ConsumptionResult`].
pub type UnconsumedRemainder<CharType = u8> = Option<TemporaryBuffer<CharType>>;

impl<CharType: ByteType> InputStreamState<CharType> {
    const _SIZE_CHECK: () =
        assert!(std::mem::size_of::<CharType>() == 1, "must buffer stream of bytes");

    /// Bytes buffered and not yet read.
    #[inline]
    pub(crate) fn available(&self) -> usize {
        self._buf.size()
    }
    /// Drops the buffered bytes. Kept for wrappers; currently unused in the crate.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn reset(&mut self) {
        self._buf = TemporaryBuffer::default();
    }
    /// The underlying source. Kept for wrappers; currently unused in the crate.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn fd(&mut self) -> *mut DataSource {
        &mut self._fd
    }

    /// The state of a stream over `fd`, before [`InputStream::new`] places it in its node.
    #[inline]
    pub(crate) fn new(fd: DataSource) -> Self {
        #[allow(clippy::let_unit_value)]
        let () = Self::_SIZE_CHECK;
        InputStreamState {
            _this: ptr::null_mut(),
            _fd: fd,
            _buf: TemporaryBuffer::default(),
            _eof: false,
            _pending: Cell::new(0),
        }
    }

    /// The state's root pointer, counted, for a continuation (see `StateRef`).
    #[inline]
    pub(crate) fn hold(&self) -> StateRef<Self> {
        debug_assert!(!self._this.is_null());
        // SAFETY: `_this` is the root pointer of this live state, set by its stream, which
        // checks the counter in its destructor.
        unsafe { StateRef::new(self._this) }
    }

    /// Whether the end-of-file flag is set.
    ///
    /// The flag is only set after a read noticed the end of the stream, so `eof()` may
    /// return false while the next `read()` returns an empty buffer; after that read it
    /// returns true.
    #[inline]
    pub fn eof(&self) -> bool {
        self._eof
    }

    /// Closes the stream's data source.
    ///
    /// Waits for any background operations (for example read-ahead) to complete, so that
    /// any resources the stream uses can be safely destroyed, such as the file used by a
    /// file input stream. The future resolves when the stream no longer needs the data
    /// source.
    #[inline]
    pub fn close(&mut self) -> Future<()> {
        self._fd.close()
    }

    // `read_exactly`, `consume`, `read`, `read_up_to`, `skip` and `read_exactly_part` are
    // implemented, with their documentation, in iostream_impl.rs.
}

/// A stream state with the counter of the continuations that refer to it.
pub(crate) trait StreamState {
    /// The pending-operation counter of the state at `this`, as a place reached through the
    /// state's root pointer.
    ///
    /// # Safety
    /// `this` is the root pointer of a live stream state.
    unsafe fn pending_of(this: *mut Self) -> *const Cell<u32>;
}

impl<CharType: ByteType> StreamState for InputStreamState<CharType> {
    #[inline]
    unsafe fn pending_of(this: *mut Self) -> *const Cell<u32> {
        // SAFETY: the contract of the trait; a place projection, no reference.
        unsafe { ptr::addr_of!((*this)._pending) }
    }
}

impl<CharType: ByteType> StreamState for OutputStreamState<CharType> {
    #[inline]
    unsafe fn pending_of(this: *mut Self) -> *const Cell<u32> {
        // SAFETY: the contract of the trait; a place projection, no reference.
        unsafe { ptr::addr_of!((*this)._pending) }
    }
}

/// The handle a continuation of a stream operation uses to reach the stream's state: the
/// state's root pointer, counted in the state's `_pending` for as long as the continuation
/// (the closure that holds it) exists, so that dropping the stream under it aborts instead
/// of freeing the state. This checks that a stream outlives its futures. The count costs
/// one non-atomic increment when the continuation is made and one decrement when it is
/// dropped; there is no continuation or task of its own.
pub(crate) struct StateRef<S: StreamState> {
    p: *mut S,
}

impl<S: StreamState> StateRef<S> {
    /// Counts a new continuation of the state `p`.
    ///
    /// # Safety
    /// `p` is the root pointer of a live stream state, whose stream checks the counter
    /// before it frees the state.
    #[inline]
    pub(crate) unsafe fn new(p: *mut S) -> Self {
        // SAFETY: the contract above; the counter is a `Cell`, reached by a shared place.
        unsafe {
            let c = S::pending_of(p);
            (*c).set((*c).get() + 1);
        }
        StateRef { p }
    }

    /// The state's root pointer, valid while `self` exists.
    #[inline]
    pub(crate) fn ptr(&self) -> *mut S {
        self.p
    }
}

impl<S: StreamState> Drop for StateRef<S> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: the stream aborts instead of freeing the state while the counter is not
        // zero, so the state is live while `self` exists.
        unsafe {
            let c = S::pending_of(self.p);
            (*c).set((*c).get() - 1);
        }
    }
}

/// Called when a stream is dropped while an operation on it is pending: aborts with an
/// assertion failure instead of freeing state that a continuation still uses.
#[cold]
fn destroyed_while_pending(what: &str) -> ! {
    crate::engine::util::assert::internal::assert_fail(
        what,
        file!(),
        line!() as i32,
        "the stream must outlive its futures",
    )
}

impl<CharType: ByteType> Default for InputStream<CharType> {
    /// A stream over an empty source.
    #[inline]
    fn default() -> Self {
        Self::from_state(InputStreamState::new(DataSource::default()))
    }
}

impl<CharType: ByteType> std::ops::Deref for InputStream<CharType> {
    type Target = InputStreamState<CharType>;
    #[inline]
    fn deref(&self) -> &InputStreamState<CharType> {
        // SAFETY: the continuations reborrow the state only while they run, which is not
        // while the stream is borrowed (one thread, and operations run in sequence).
        unsafe { self.s.get() }
    }
}

impl<CharType: ByteType> std::ops::DerefMut for InputStream<CharType> {
    #[inline]
    fn deref_mut(&mut self) -> &mut InputStreamState<CharType> {
        // SAFETY: as in deref; the reference is tied to the stream's exclusive borrow.
        unsafe { self.s.get_mut() }
    }
}

impl<CharType: ByteType> Drop for InputStream<CharType> {
    #[inline]
    fn drop(&mut self) {
        if self._pending.get() != 0 {
            destroyed_while_pending("input_stream destroyed while an operation is pending");
        }
    }
}

impl<CharType: ByteType> InputStream<CharType> {
    /// A stream reading from `fd`.
    #[inline]
    pub fn new(fd: DataSource) -> Self {
        Self::from_state(InputStreamState::new(fd))
    }

    /// Places the state in its node and records its root pointer.
    #[inline]
    fn from_state(st: InputStreamState<CharType>) -> Self {
        let s = NodePtr::new(st);
        let p = s.as_ptr();
        // SAFETY: `p` is the fresh node's pointer, with no reference to it live.
        unsafe { (*p)._this = p };
        InputStream { s }
    }

    /// See [`InputStreamState::read_exactly`].
    #[inline]
    pub fn read_exactly(&mut self, n: usize) -> Future<TemporaryBuffer<CharType>> {
        (**self).read_exactly(n)
    }
    /// See [`InputStreamState::consume`].
    #[inline]
    pub fn consume<Consumer: InputStreamConsumer<CharType> + 'static>(
        &mut self,
        consumer: Consumer,
    ) -> Future<()> {
        (**self).consume(consumer)
    }
    /// See [`InputStreamState::consume_ref`].
    ///
    /// # Safety
    /// `consumer` outlives the returned future and is not used elsewhere meanwhile.
    #[inline]
    pub unsafe fn consume_ref<Consumer: InputStreamConsumer<CharType> + 'static>(
        &mut self,
        consumer: &mut Consumer,
    ) -> Future<()> {
        // SAFETY: forwarded from the caller.
        unsafe { (**self).consume_ref(consumer) }
    }
    /// See [`InputStreamState::read_up_to`].
    #[inline]
    pub fn read_up_to(&mut self, n: usize) -> Future<TemporaryBuffer<CharType>> {
        (**self).read_up_to(n)
    }
    /// See [`InputStreamState::read`].
    #[inline]
    pub fn read(&mut self) -> Future<TemporaryBuffer<CharType>> {
        (**self).read()
    }
    /// See [`InputStreamState::skip`].
    #[inline]
    pub fn skip(&mut self, n: u64) -> Future<()> {
        (**self).skip(n)
    }
    /// See [`InputStreamState::close`].
    #[inline]
    pub fn close(&mut self) -> Future<()> {
        (**self).close()
    }

    /// Detaches and returns the underlying [`DataSource`], consuming the stream.
    ///
    /// Intended for custom [`DataSourceImpl`] implementations that wrap an existing input
    /// stream, so it should not be called on a stream that was already used.
    ///
    /// # Errors
    /// A [`LogicError`] when the stream holds buffered data (it was already used).
    pub fn detach(mut self) -> Result<DataSource, ExceptionPtr> {
        if self._buf.is_nonempty() {
            return Err(ExceptionPtr::from(LogicError(
                "detach() called on a used input_stream".into(),
            )));
        }

        Ok(std::mem::take(&mut self._fd))
    }
}

/// Options of an [`OutputStream`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputStreamOptions {
    /// Never hand the sink buffers larger than the configured size, splitting large
    /// writes.
    pub trim_to_size: bool,
    /// Merge flushes with each other, deferring them to the reactor's flush poller.
    pub batch_flushes: bool,
}

/// Buffers data before handing it over to a [`DataSink`].
///
/// With [`OutputStreamOptions::trim_to_size`] the sink never receives chunks larger than
/// the configured size, which could otherwise happen when a single write is larger than
/// that size. The sink never receives empty chunks.
///
/// All methods must be called sequentially: No method may be called before the future of the previous one has resolved. The stream must
/// be closed ([`OutputStream::close`]) and the close awaited before it is dropped.
///
/// The state lives in its own allocation ([`OutputStreamState`], in a node that is never
/// moved), which the continuations of the pending operations (`StateRef`) and the
/// reactor's batch flush list reach through the state's root pointer: moving the stream
/// does not move it, and dropping the stream while a continuation of it exists aborts.
pub struct OutputStream<CharType: ByteType = u8> {
    s: NodePtr<OutputStreamState<CharType>>,
}

/// The state of an [`OutputStream`]: its sink, buffer, pending zero-copy data and flush
/// state. `repr(C)` keeps one layout for every byte type, so that the reactor's batch
/// flush list (of `u8` streams) can hold any of them.
#[repr(C)]
pub struct OutputStreamState<CharType: ByteType = u8> {
    pub(crate) _fd: DataSink,
    pub(crate) _buf: TemporaryBuffer<CharType>,
    pub(crate) _zc_bufs: Packet, //zero copy buffers
    pub(crate) _size: usize,
    pub(crate) _begin: usize,
    pub(crate) _end: usize,
    pub(crate) _trim_to_size: bool,
    pub(crate) _batch_flushes: bool,
    pub(crate) _in_batch: Option<Promise<()>>,
    pub(crate) _flush: bool,
    pub(crate) _flushing: bool,
    pub(crate) _ex: ExceptionPtr,
    /// The next stream of the batch flush list (intrusive link).
    pub(crate) _in_poller: *mut OutputStreamState<CharType>,
    /// Continuations that still refer to this state (each holds a `StateRef`).
    pub(crate) _pending: Cell<u32>,
    /// The state's own address with the provenance of its node, written once by the
    /// stream; the continuations and the batch flush list keep it.
    pub(crate) _this: *mut OutputStreamState<CharType>,
}

impl<CharType: ByteType> OutputStreamState<CharType> {
    /// The members with their default initializers, except the ones the constructors set.
    #[inline]
    fn from_parts(fd: DataSink, size: usize, trim_to_size: bool, batch_flushes: bool) -> Self {
        OutputStreamState {
            _fd: fd,
            _buf: TemporaryBuffer::default(),
            _zc_bufs: Packet::make_null_packet(),
            _size: size,
            _begin: 0,
            _end: 0,
            _trim_to_size: trim_to_size,
            _batch_flushes: batch_flushes,
            _in_batch: None,
            _flush: false,
            _flushing: false,
            _ex: ExceptionPtr::default(),
            _in_poller: ptr::null_mut(),
            _pending: Cell::new(0),
            _this: ptr::null_mut(),
        }
    }

    /// The state's root pointer, for the flush list (see the field).
    #[inline]
    pub(crate) fn this_ptr(&self) -> *mut Self {
        debug_assert!(!self._this.is_null());
        self._this
    }

    /// The state's root pointer, counted, for a continuation (see `StateRef`).
    #[inline]
    pub(crate) fn hold(&self) -> StateRef<Self> {
        // SAFETY: `this_ptr` is the root pointer of this live state, set by its stream,
        // which checks the counter in its destructor.
        unsafe { StateRef::new(self.this_ptr()) }
    }
}

impl<CharType: ByteType> OutputStreamState<CharType> {
    const _SIZE_CHECK: () =
        assert!(std::mem::size_of::<CharType>() == 1, "must buffer stream of bytes");

    /// Free bytes left in the current buffer. Currently unused.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn available(&self) -> usize {
        self._end - self._begin
    }

    /// The state of a stream over `fd` with buffer size `size`, before the stream places it
    /// in its node.
    #[inline]
    pub(crate) fn with_options(fd: DataSink, size: usize, opts: OutputStreamOptions) -> Self {
        #[allow(clippy::let_unit_value)]
        let () = Self::_SIZE_CHECK;
        let batch_flushes = opts.batch_flushes && fd.can_batch_flushes();
        Self::from_parts(fd, size, opts.trim_to_size, batch_flushes)
    }

    /// [`OutputStreamState::with_options`] with the options as booleans.
    #[deprecated(note = "use OutputStreamOptions instead of booleans")]
    #[inline]
    pub(crate) fn with_flags(
        fd: DataSink,
        size: usize,
        trim_to_size: bool,
        batch_flushes: bool,
    ) -> Self {
        let batch_flushes = batch_flushes && fd.can_batch_flushes();
        Self::from_parts(fd, size, trim_to_size, batch_flushes)
    }

    /// [`OutputStreamState::with_options`] with the sink's own buffer size and default
    /// options.
    #[inline]
    pub(crate) fn new(fd: DataSink) -> Self {
        let size = fd.buffer_size();
        Self::from_parts(fd, size, true, false)
    }

    // The writes, `flush` and `close` are implemented, with their documentation, in
    // iostream_impl.rs.
}

impl<CharType: ByteType> Default for OutputStream<CharType> {
    /// A stream over an empty sink.
    #[inline]
    fn default() -> Self {
        Self::from_state(OutputStreamState::from_parts(DataSink::default(), 0, false, false))
    }
}

impl<CharType: ByteType> std::ops::Deref for OutputStream<CharType> {
    type Target = OutputStreamState<CharType>;
    #[inline]
    fn deref(&self) -> &OutputStreamState<CharType> {
        // SAFETY: as in InputStream's deref.
        unsafe { self.s.get() }
    }
}

impl<CharType: ByteType> std::ops::DerefMut for OutputStream<CharType> {
    #[inline]
    fn deref_mut(&mut self) -> &mut OutputStreamState<CharType> {
        // SAFETY: as in InputStream's deref_mut.
        unsafe { self.s.get_mut() }
    }
}

impl<CharType: ByteType> Drop for OutputStream<CharType> {
    #[inline]
    fn drop(&mut self) {
        if self._pending.get() != 0 {
            destroyed_while_pending("output_stream destroyed while an operation is pending");
        }
    }
}

impl<CharType: ByteType> OutputStream<CharType> {
    /// A stream writing to `fd` with a buffer of `size` bytes.
    #[inline]
    pub fn with_options(fd: DataSink, size: usize, opts: OutputStreamOptions) -> Self {
        Self::from_state(OutputStreamState::with_options(fd, size, opts))
    }

    /// Places the state in its node and records its root pointer.
    #[inline]
    fn from_state(st: OutputStreamState<CharType>) -> Self {
        let s = NodePtr::new(st);
        let p = s.as_ptr();
        // SAFETY: `p` is the fresh node's pointer, with no reference to it live.
        unsafe { (*p)._this = p };
        OutputStream { s }
    }

    /// [`OutputStream::with_options`] with the options as booleans (deprecated form).
    #[deprecated(note = "use OutputStreamOptions instead of booleans")]
    #[inline]
    pub fn with_flags(fd: DataSink, size: usize, trim_to_size: bool, batch_flushes: bool) -> Self {
        #[allow(deprecated)]
        let s = OutputStreamState::with_flags(fd, size, trim_to_size, batch_flushes);
        Self::from_state(s)
    }

    /// A stream writing to `fd`, with the sink's own buffer size and default options.
    #[inline]
    pub fn new(fd: DataSink) -> Self {
        Self::from_state(OutputStreamState::new(fd))
    }

    /// See [`OutputStreamState::write`].
    #[inline]
    pub fn write(&mut self, buf: &[CharType]) -> Future<()> {
        (**self).write(buf)
    }
    /// See [`OutputStreamState::write_raw`].
    ///
    /// # Safety
    /// `buf` is valid for reads of `n` elements during the call (the bytes are copied or
    /// handed to the sink in buffers of the stream's own before it returns).
    #[inline]
    pub unsafe fn write_raw(&mut self, buf: *const CharType, n: usize) -> Future<()> {
        // SAFETY: forwarded from the caller.
        unsafe { (**self).write_raw(buf, n) }
    }
    /// See [`OutputStreamState::write_cstr`].
    ///
    /// # Safety
    /// `buf` points to a NUL-terminated string.
    #[inline]
    pub unsafe fn write_cstr(&mut self, buf: *const CharType) -> Future<()> {
        // SAFETY: forwarded from the caller.
        unsafe { (**self).write_cstr(buf) }
    }
    /// See [`OutputStreamState::write_sstring`].
    #[inline]
    pub fn write_sstring(&mut self, v: &Sstring) -> Future<()> {
        (**self).write_sstring(v)
    }
    /// See [`OutputStreamState::write_str`].
    #[inline]
    pub fn write_str(&mut self, v: &str) -> Future<()> {
        (**self).write_str(v)
    }
    /// See [`OutputStreamState::flush`].
    #[inline]
    pub fn flush(&mut self) -> Future<()> {
        (**self).flush()
    }
    /// See [`OutputStreamState::close`].
    #[inline]
    pub fn close(&mut self) -> Future<()> {
        (**self).close()
    }

    /// Detaches and returns the underlying [`DataSink`], consuming the stream.
    ///
    /// Intended for custom [`DataSinkImpl`] implementations that wrap an existing output
    /// stream, so it should not be called on a stream that was already used.
    ///
    /// # Errors
    /// A [`LogicError`] when the stream holds buffered data (it was already used).
    pub fn detach(mut self) -> Result<DataSink, ExceptionPtr> {
        if self._buf.is_nonempty() {
            return Err(ExceptionPtr::from(LogicError(
                "detach() called on a used output_stream".into(),
            )));
        }

        Ok(std::mem::take(&mut self._fd))
    }
}

impl OutputStream<u8> {
    /// See [`OutputStreamState::write_packet`].
    #[inline]
    pub fn write_packet(&mut self, p: Packet) -> Future<()> {
        (**self).write_packet(p)
    }
    /// See [`OutputStreamState::write_buffer`].
    #[inline]
    pub fn write_buffer(&mut self, p: TemporaryBuffer<u8>) -> Future<()> {
        (**self).write_buffer(p)
    }
    /// See [`OutputStreamState::write_scattered`].
    #[inline]
    pub fn write_scattered(&mut self, msg: ScatteredMessage) -> Future<()> {
        (**self).write_scattered(msg)
    }
}

impl<CharType: ByteType> Drop for OutputStreamState<CharType> {
    /// Asserts that the stream was closed: nothing buffered and no batch flush pending.
    fn drop(&mut self) {
        if self._batch_flushes {
            engine_assert!(self._in_batch.is_none(), "Was this stream properly closed?");
        } else {
            engine_assert!(
                self._end == 0 && !self._zc_bufs.is_valid(),
                "Was this stream properly closed?"
            );
        }
    }
}

/// The reactor's list of streams waiting for a batched flush: an intrusive singly linked
/// list through each state's `_in_poller` link, with a cached tail for O(1) appends.
pub struct BatchFlushList<CharType: ByteType = u8> {
    _head: *mut OutputStreamState<CharType>,
    _tail: *mut OutputStreamState<CharType>,
}

/// The batch flush list of `u8` streams, which the reactor keeps.
pub type BatchFlushListT = BatchFlushList<u8>;

impl<CharType: ByteType> Default for BatchFlushList<CharType> {
    fn default() -> Self {
        Self::new()
    }
}

impl<CharType: ByteType> BatchFlushList<CharType> {
    /// An empty list.
    #[inline]
    pub const fn new() -> Self {
        BatchFlushList { _head: ptr::null_mut(), _tail: ptr::null_mut() }
    }

    /// Whether the list is empty.
    #[inline]
    pub fn empty(&self) -> bool {
        self._head.is_null()
    }

    /// The first stream (null if the list is empty).
    #[inline]
    pub fn front(&self) -> *mut OutputStreamState<CharType> {
        self._head
    }

    /// Removes the first stream.
    #[inline]
    pub fn pop_front(&mut self) {
        if self._head.is_null() {
            return;
        }
        // SAFETY: a linked stream stays in place until it leaves the list.
        unsafe {
            let next = (*self._head)._in_poller;
            (*self._head)._in_poller = ptr::null_mut();
            self._head = next;
        }
        if self._head.is_null() {
            self._tail = ptr::null_mut();
        }
    }

    /// Appends `x` in O(1), through the cached tail.
    ///
    /// # Safety
    /// `x` is the root pointer of a live stream state that stays in place, and is not
    /// dropped, while it is in the list.
    #[inline]
    pub unsafe fn push_back(&mut self, x: *mut OutputStreamState<CharType>) {
        // SAFETY: the stream is live and does not move while it is in the list.
        unsafe { (*x)._in_poller = ptr::null_mut() };
        if self._tail.is_null() {
            self._head = x;
        } else {
            // SAFETY: the tail is a live linked stream.
            unsafe { (*self._tail)._in_poller = x };
        }
        self._tail = x;
    }
}

/// A data source can be closed by the generic close helpers.
impl crate::engine::util::closeable::Closeable for DataSource {
    #[inline]
    fn close(&mut self) -> Future<()> {
        DataSource::close(self)
    }
}

/// A data sink can be closed by the generic close helpers.
impl crate::engine::util::closeable::Closeable for DataSink {
    #[inline]
    fn close(&mut self) -> Future<()> {
        DataSink::close(self)
    }
}

/// An input stream can be closed by the generic close helpers.
impl<CharType: ByteType> crate::engine::util::closeable::Closeable for InputStream<CharType> {
    #[inline]
    fn close(&mut self) -> Future<()> {
        InputStream::close(self)
    }
}

/// An output stream can be closed by the generic close helpers.
impl<CharType: ByteType> crate::engine::util::closeable::Closeable for OutputStream<CharType> {
    #[inline]
    fn close(&mut self) -> Future<()> {
        OutputStream::close(self)
    }
}
