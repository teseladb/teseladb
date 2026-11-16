//! An in-memory socket pair for network tests: a [`LoopbackConnectionFactory`] hands out
//! server and client sockets whose data travels through per-shard [`LoopbackBuffer`] queues,
//! and a [`LoopbackErrorInjector`] can make reads, writes and connects fail on demand.

#![allow(dead_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::UnsafeCell;
use std::time::Duration;

use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, Promise, RuntimeError, SystemError, make_exception_future,
    make_ready_future,
};
use teseladb::engine::core::iostream::{DataSink, DataSinkImpl, DataSource, DataSourceImpl};
use teseladb::engine::core::loop_::{do_for_each, parallel_for_each};
use teseladb::engine::core::queue::Queue;
use teseladb::engine::core::shard_id::{ShardId, this_shard_id};
use teseladb::engine::core::sharded::{ForeignPtr, make_foreign};
use teseladb::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::net::api::{
    AcceptResult, ConnectedSocket, KeepaliveParams, ServerSocket, TcpKeepaliveParams, Transport,
};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::net::stack::{ConnectedSocketImpl, ServerSocketImpl, SocketImpl};
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;

const EPIPE: i32 = 32;
const ECONNABORTED: i32 = 103;

/// A system error with errno `e`.
fn system_error(e: i32) -> ExceptionPtr {
    ExceptionPtr::new(SystemError::from_errno(e))
}

/// What an injected failure does: nothing (`None`), fail this one call (`OneShot`), or abort the connection (`Abort`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopbackError {
    None,
    OneShot,
    Abort,
}

/// Decides, per call, whether a loopback socket operation fails.
pub trait LoopbackErrorInjector {
    fn server_rcv_error(&mut self) -> LoopbackError {
        LoopbackError::None
    }
    fn server_snd_error(&mut self) -> LoopbackError {
        LoopbackError::None
    }
    fn client_rcv_error(&mut self) -> LoopbackError {
        LoopbackError::None
    }
    fn client_snd_error(&mut self) -> LoopbackError {
        LoopbackError::None
    }
    fn connect_error(&mut self) -> LoopbackError {
        LoopbackError::None
    }
    fn connect_delay(&mut self) -> Duration {
        Duration::from_micros(0)
    }
}

/// A raw pointer to an error injector; null is `None`.
pub type ErrorInjectorPtr = Option<*mut dyn LoopbackErrorInjector>;

/// Which end of a connection a buffer feeds: the client or the server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopbackBufferType {
    ClientTx,
    ServerTx,
}

/// A queue of buffers between the two ends of a connection. Its members are reached through
/// shared and foreign handles, hence the cell.
pub struct LoopbackBuffer {
    state: UnsafeCell<LoopbackBufferState>,
}

struct LoopbackBufferState {
    aborted: bool,
    q: Queue<TemporaryBuffer<u8>>,
    error_injector: ErrorInjectorPtr,
    type_: LoopbackBufferType,
    shutdown: Option<Promise<()>>,
}

/// The mutable state of a loopback buffer reached through a shared handle (or through the
/// foreign handle that wraps one).
#[allow(clippy::mut_from_ref)]
fn lb(p: &LoopbackBuffer) -> &mut LoopbackBufferState {
    // SAFETY: a loopback buffer is only touched from its owner shard, one task at a time,
    // and no reference into its state outlives the call that takes it.
    unsafe { &mut *p.state.get() }
}

/// The buffer behind a foreign handle, on the owner shard of the loopback buffer.
fn owned(fb: &ForeignBuffer) -> &LoopbackBuffer {
    match fb.get_local() {
        Some(b) => b,
        // Dereferencing a foreign handle away from its shard, or a null one, is a bug in
        // the test.
        None => panic!("loopback buffer reached away from its shard"),
    }
}

/// A pointer that a cross-shard closure captures: the object, or the target of a foreign
/// handle, that the closure works on.
struct Remote<T>(*const T);

// SAFETY: the pointee is only dereferenced on the shard that owns it (the destination of
// the `submit_to` that carries it), and its owner keeps it alive until the call resolves.
unsafe impl<T> Send for Remote<T> {}

impl<T> Remote<T> {
    /// # Safety
    /// The pointee is alive and owned by the current shard.
    unsafe fn get(&self) -> &T {
        // SAFETY: the caller's contract.
        unsafe { &*self.0 }
    }

    /// The raw pointer, read through the whole wrapper so that closures capture it whole.
    fn ptr(&self) -> *const T {
        self.0
    }
}

/// The error injector behind a raw pointer.
fn inj(p: *mut dyn LoopbackErrorInjector) -> &'static mut dyn LoopbackErrorInjector {
    // SAFETY: the test owning the injector keeps it alive for as long as its sockets.
    unsafe { &mut *p }
}

impl LoopbackBuffer {
    pub fn new(error_injection: ErrorInjectorPtr, t: LoopbackBufferType) -> Self {
        LoopbackBuffer {
            state: UnsafeCell::new(LoopbackBufferState {
                aborted: false,
                q: Queue::new(128),
                error_injector: error_injection,
                type_: t,
                shutdown: None,
            }),
        }
    }

    pub fn push(&self, b: TemporaryBuffer<u8>) -> Future<()> {
        lb(self).push(b)
    }

    pub fn pop(&self) -> Future<TemporaryBuffer<u8>> {
        lb(self).pop()
    }

    pub fn abort(&self) {
        lb(self).abort();
    }

    pub fn shutdown(&self) {
        lb(self).shutdown();
    }

    pub fn wait_input_shutdown(&self) -> Future<()> {
        lb(self).wait_input_shutdown()
    }
}

impl LoopbackBufferState {
    pub fn push(&mut self, b: TemporaryBuffer<u8>) -> Future<()> {
        if self.aborted {
            return make_exception_future(system_error(EPIPE));
        }
        if let Some(ei) = self.error_injector {
            let error = if self.type_ == LoopbackBufferType::ClientTx {
                inj(ei).client_snd_error()
            } else {
                inj(ei).server_snd_error()
            };
            if error == LoopbackError::OneShot {
                return make_exception_future(ExceptionPtr::new(RuntimeError(
                    "test injected glitch on send".into(),
                )));
            }
            if error == LoopbackError::Abort {
                self.abort();
                return make_exception_future(ExceptionPtr::new(RuntimeError(
                    "test injected error on send".into(),
                )));
            }
        }
        // SAFETY: the queue lives in the heap block of the shared loopback buffer,
        // which the sockets at both ends keep alive while their reads and writes are pending.
        unsafe { self.q.push_eventually(b) }
    }

    pub fn pop(&mut self) -> Future<TemporaryBuffer<u8>> {
        if self.aborted {
            return make_exception_future(system_error(EPIPE));
        }
        if let Some(ei) = self.error_injector {
            let error = if self.type_ == LoopbackBufferType::ClientTx {
                inj(ei).client_rcv_error()
            } else {
                inj(ei).server_rcv_error()
            };
            if error == LoopbackError::OneShot {
                return make_exception_future(ExceptionPtr::new(RuntimeError(
                    "test injected glitch on receive".into(),
                )));
            }
            if error == LoopbackError::Abort {
                self.abort();
                return make_exception_future(ExceptionPtr::new(RuntimeError(
                    "test injected error on receive".into(),
                )));
            }
        }
        // SAFETY: as in `push`.
        unsafe { self.q.pop_eventually() }
    }

    pub fn abort(&mut self) {
        self.shutdown();
        self.aborted = true;
        self.q.abort(system_error(EPIPE));
    }

    pub fn shutdown(&mut self) {
        // it can be called by both -- reader and writer socket impls
        if let Some(mut p) = self.shutdown.take() {
            p.set_value(());
        }
    }

    pub fn wait_input_shutdown(&mut self) -> Future<()> {
        assert!(self.shutdown.is_none());
        let p = self.shutdown.insert(Promise::new());
        p.get_future()
    }
}

type ForeignBuffer = ForeignPtr<LwSharedPtr<LoopbackBuffer>>;

pub struct LoopbackDataSinkImpl {
    buffer: LwSharedPtr<ForeignBuffer>,
    batch_flush_error: NoncopyableFunction<fn()>,
}

impl LoopbackDataSinkImpl {
    pub fn new(buffer: LwSharedPtr<ForeignBuffer>, flush_error: NoncopyableFunction<fn()>) -> Self {
        LoopbackDataSinkImpl { buffer, batch_flush_error: flush_error }
    }
}

impl DataSinkImpl for LoopbackDataSinkImpl {
    fn put(&mut self, mut data: Packet) -> Future<()> {
        let this: *const Self = self;
        do_with(data.release(), move |bufs: &mut Vec<TemporaryBuffer<u8>>| {
            let bufsp: *mut Vec<TemporaryBuffer<u8>> = bufs;
            let n = bufs.len();
            do_for_each(0..n, move |i: usize| {
                // SAFETY: do_with keeps `bufs` alive, and in place, until the loop resolves.
                let buf: &mut TemporaryBuffer<u8> = unsafe { &mut (&mut *bufsp)[i] };
                let b = Remote(buf.get());
                let s = buf.size();
                // SAFETY: the sink outlives its put().
                let buffer: &ForeignBuffer = unsafe { &(*this).buffer };
                let owner = buffer.get_owner_shard();
                let target = Remote::<ForeignBuffer>(buffer);
                Smp::submit_to(owner, move || {
                    // SAFETY: `b` points at `s` bytes kept alive by `bufs` until the push resolves.
                    let tb = TemporaryBuffer::copy_from(unsafe {
                        std::slice::from_raw_parts(b.ptr(), s)
                    });
                    // SAFETY: the sink keeps its foreign_ptr alive and unchanged until the push
                    // resolves; this only reads it.
                    owned(unsafe { target.get() }).push(tb)
                })
            })
        })
    }

    fn close(&mut self) -> Future<()> {
        let target = Remote::<ForeignBuffer>(&*self.buffer);
        Smp::submit_to(self.buffer.get_owner_shard(), move || {
            // SAFETY: the sink keeps its foreign_ptr alive and unchanged until the push
            // resolves (the sink outlives its close()).
            owned(unsafe { target.get() }).push(TemporaryBuffer::default()).handle_exception_type(
                |err: &SystemError| {
                    if err.code() != EPIPE {
                        return Err(ExceptionPtr::new(err.clone()));
                    }
                    Ok(())
                },
            )
        })
    }

    fn can_batch_flushes(&self) -> bool {
        true
    }

    fn on_batch_flush_error(&mut self) {
        self.batch_flush_error.call();
    }
}

pub struct LoopbackDataSourceImpl {
    eof: bool,
    buffer: LwSharedPtr<LoopbackBuffer>,
}

impl LoopbackDataSourceImpl {
    pub fn new(buffer: LwSharedPtr<LoopbackBuffer>) -> Self {
        LoopbackDataSourceImpl { eof: false, buffer }
    }
}

impl DataSourceImpl for LoopbackDataSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        let this: *mut Self = self;
        lb(&self.buffer).pop().then_wrapped(move |b: Future<TemporaryBuffer<u8>>| {
            // SAFETY: the data source outlives the reads it issues.
            let me = unsafe { &mut *this };
            me.eof = b.failed();
            if !me.eof {
                // future::get() is destructive, so we have to play these games
                // FIXME: make future::get() non-destructive
                return match b.get() {
                    Ok(tmp) => {
                        me.eof = tmp.empty();
                        make_ready_future(tmp)
                    }
                    Err(e) => make_exception_future(e),
                };
            }
            b
        })
    }

    fn close(&mut self) -> Future<()> {
        if !self.eof {
            lb(&self.buffer).abort();
        } else {
            lb(&self.buffer).shutdown();
        }
        make_ready_future(())
    }
}

pub struct LoopbackConnectedSocketImpl {
    tx: LwSharedPtr<ForeignBuffer>,
    rx: LwSharedPtr<LoopbackBuffer>,
}

impl LoopbackConnectedSocketImpl {
    pub fn new(tx: ForeignBuffer, rx: LwSharedPtr<LoopbackBuffer>) -> Self {
        LoopbackConnectedSocketImpl { tx: make_lw_shared(tx), rx }
    }
}

impl ConnectedSocketImpl for LoopbackConnectedSocketImpl {
    fn source(&mut self) -> DataSource {
        DataSource::new(Box::new(LoopbackDataSourceImpl::new(self.rx.clone())))
    }

    fn sink(&mut self) -> DataSink {
        let this: *mut Self = self;
        DataSink::new(Box::new(LoopbackDataSinkImpl::new(
            self.tx.clone(),
            // SAFETY: the socket impl outlives its sink.
            NoncopyableFunction::<fn()>::new(move || unsafe { (*this).shutdown_input() }),
        )))
    }

    fn shutdown_input(&mut self) {
        lb(&self.rx).abort();
    }

    fn shutdown_output(&mut self) {
        // `tx` is a copy: it keeps the foreign handle alive until the call returns; it is
        // released back on this shard, since its count is not atomic.
        let tx = self.tx.clone();
        let target = Remote::<ForeignBuffer>(&*tx);
        let _ = Smp::submit_to(self.tx.get_owner_shard(), move || {
            // SAFETY: `tx` keeps the foreign_ptr alive and unchanged until this call resolves;
            // this only reads it.
            owned(unsafe { target.get() }).abort();
        })
        .finally(move || drop(tx));
    }

    fn set_nodelay(&mut self, _nodelay: bool) -> Result<(), ExceptionPtr> {
        Ok(())
    }

    fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        Ok(true)
    }

    fn set_keepalive(&mut self, _keepalive: bool) -> Result<(), ExceptionPtr> {
        Ok(())
    }

    fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        Ok(false)
    }

    fn set_keepalive_parameters(&mut self, _p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        Ok(())
    }

    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        Ok(KeepaliveParams::Tcp(TcpKeepaliveParams {
            idle: Duration::from_secs(0),
            interval: Duration::from_secs(0),
            count: 0,
        }))
    }

    unsafe fn set_sockopt(
        &mut self,
        _level: i32,
        _optname: i32,
        _data: *const u8,
        _len: usize,
    ) -> Result<(), ExceptionPtr> {
        Err(ExceptionPtr::new(RuntimeError(
            "Setting custom socket options is not supported for loopback".into(),
        )))
    }

    unsafe fn get_sockopt(
        &self,
        _level: i32,
        _optname: i32,
        _data: *mut u8,
        _len: usize,
    ) -> Result<i32, ExceptionPtr> {
        Err(ExceptionPtr::new(RuntimeError(
            "Getting custom socket options is not supported for loopback".into(),
        )))
    }

    fn local_address(&self) -> SocketAddress {
        // dummy
        SocketAddress::default()
    }

    fn remote_address(&self) -> SocketAddress {
        // dummy
        SocketAddress::default()
    }

    fn wait_input_shutdown(&mut self) -> Future<()> {
        lb(&self.rx).wait_input_shutdown()
    }
}

pub struct LoopbackServerSocketImpl {
    pending: LwSharedPtr<Queue<ConnectedSocket>>,
}

// A mutable reference from a shared handle.
#[allow(clippy::mut_from_ref)]
fn pq(p: &LwSharedPtr<Queue<ConnectedSocket>>) -> &mut Queue<ConnectedSocket> {
    // SAFETY: the queue of pending connections is only used from its shard.
    unsafe { &mut *p.get() }
}

impl LoopbackServerSocketImpl {
    pub fn new(q: LwSharedPtr<Queue<ConnectedSocket>>) -> Self {
        LoopbackServerSocketImpl { pending: q }
    }
}

impl ServerSocketImpl for LoopbackServerSocketImpl {
    fn accept(&mut self) -> Future<AcceptResult> {
        // SAFETY: the queue lives in the heap block of `_pending`, which the factory and this
        // socket keep alive while the accept is pending.
        unsafe { pq(&self.pending).pop_eventually() }.then(|cs: ConnectedSocket| {
            make_ready_future(AcceptResult {
                connection: cs,
                remote_address: SocketAddress::default(),
            })
        })
    }

    fn abort_accept(&mut self) {
        pq(&self.pending).abort(system_error(ECONNABORTED));
    }

    fn local_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        // CMH dummy
        Ok(SocketAddress::default())
    }
}

pub struct LoopbackConnectionFactory {
    shard: u32,
    shards_count: u32,
    pending_capacity: u32,
    pending: Vec<Option<LwSharedPtr<Queue<ConnectedSocket>>>>,
}

impl LoopbackConnectionFactory {
    pub fn new(shards_count: u32) -> Self {
        LoopbackConnectionFactory {
            shard: 0,
            shards_count,
            pending_capacity: 10,
            pending: (0..shards_count).map(|_| None).collect(),
        }
    }

    /// A factory with one pending-connection queue per shard of the running engine.
    pub fn new_default() -> Self {
        Self::new(Smp::count())
    }

    pub fn with_pending_capacity(pending_capacity: u32, shards_count: u32) -> Self {
        let mut lcf = LoopbackConnectionFactory::new(shards_count);
        lcf.pending_capacity = pending_capacity;
        lcf
    }

    fn pending_queue(&mut self) -> LwSharedPtr<Queue<ConnectedSocket>> {
        let id = this_shard_id() as usize;
        let cap = self.pending_capacity as usize;
        self.pending[id].get_or_insert_with(|| make_lw_shared(Queue::new(cap))).clone()
    }

    pub fn get_server_socket(&mut self) -> ServerSocket {
        assert!(this_shard_id() < self.shards_count);
        let q = self.pending_queue();
        ServerSocket::from_impl(Box::new(LoopbackServerSocketImpl::new(q)))
    }

    pub fn make_new_server_connection(
        &mut self,
        b1: ForeignBuffer,
        b2: LwSharedPtr<LoopbackBuffer>,
    ) -> Future<()> {
        assert!(this_shard_id() < self.shards_count);
        let q = self.pending_queue();
        // SAFETY: the queue lives in the heap block of the shard's pending queue, which the
        // factory keeps alive until destroy_shard().
        unsafe {
            pq(&q).push_eventually(ConnectedSocket::from_impl(Box::new(
                LoopbackConnectedSocketImpl::new(b1, b2),
            )))
        }
    }

    pub fn make_new_client_connection(
        &mut self,
        b1: LwSharedPtr<LoopbackBuffer>,
        b2: ForeignBuffer,
    ) -> ConnectedSocket {
        ConnectedSocket::from_impl(Box::new(LoopbackConnectedSocketImpl::new(b2, b1)))
    }

    pub fn next_shard(&mut self) -> u32 {
        let s = self.shard % self.shards_count;
        self.shard += 1;
        s
    }

    pub fn destroy_shard(&mut self, shard: u32) {
        assert!(shard < self.shards_count);
        self.pending[shard as usize] = None;
    }

    pub fn destroy_all_shards(&mut self) -> Future<()> {
        let this = Remote::<Self>(self);
        parallel_for_each(0..self.shards_count, move |shard: ShardId| {
            let this = Remote(this.ptr());
            Smp::submit_to(shard, move || {
                // SAFETY: the factory outlives this call; each shard
                // only touches its own slot.
                unsafe { (*this.ptr().cast_mut()).destroy_shard(this_shard_id()) };
            })
        })
    }
}

pub struct LoopbackSocketImpl {
    factory: *mut LoopbackConnectionFactory,
    error_injector: ErrorInjectorPtr,
    b1: Option<LwSharedPtr<LoopbackBuffer>>,
    b2: Option<ForeignBuffer>,
    connect_abort: Option<Promise<ConnectedSocket>>,
}

impl LoopbackSocketImpl {
    /// A client socket that connects through `factory`, failing as `error_injector` decides.
    pub fn new(factory: &mut LoopbackConnectionFactory, error_injector: ErrorInjectorPtr) -> Self {
        LoopbackSocketImpl { factory, error_injector, b1: None, b2: None, connect_abort: None }
    }

    // A mutable reference from a shared handle.
    #[allow(clippy::mut_from_ref)]
    fn factory(&self) -> &mut LoopbackConnectionFactory {
        // SAFETY: the factory outlives the socket (the test owns both and drops the socket first).
        unsafe { &mut *self.factory }
    }
}

impl SocketImpl for LoopbackSocketImpl {
    fn connect(
        &mut self,
        _sa: SocketAddress,
        _local: SocketAddress,
        _proto: Transport,
    ) -> Future<ConnectedSocket> {
        if let Some(ei) = self.error_injector {
            let error = inj(ei).connect_error();
            if error != LoopbackError::None {
                let p = self.connect_abort.insert(Promise::new());
                return p.get_future();
            }
        }
        let shard = self.factory().next_shard();
        let b1 =
            make_lw_shared(LoopbackBuffer::new(self.error_injector, LoopbackBufferType::ServerTx));
        self.b1 = Some(b1.clone());
        let this: *mut Self = self;
        let remote_this = Remote::<Self>(this);
        let fb1 = make_foreign(b1);
        Smp::submit_to(shard, move || {
            // SAFETY: the socket outlives its connect().
            let me = unsafe { &mut *remote_this.ptr().cast_mut() };
            let b2 = make_lw_shared(LoopbackBuffer::new(
                me.error_injector,
                LoopbackBufferType::ClientTx,
            ));
            me.b2 = Some(make_foreign(b2.clone()));
            me.factory()
                .make_new_server_connection(fb1, b2.clone())
                .then(move |()| make_ready_future(make_foreign(b2)))
        })
        .then(move |b2: ForeignBuffer| {
            // SAFETY: as above.
            let me = unsafe { &mut *this };
            if let Some(ei) = me.error_injector {
                let delay = inj(ei).connect_delay();
                if delay != Duration::from_micros(0) {
                    return sleep(delay).then(move |()| {
                        // SAFETY: as above.
                        let me = unsafe { &mut *this };
                        let b1 = me.b1.clone().expect("b1");
                        make_ready_future(me.factory().make_new_client_connection(b1, b2))
                    });
                }
            }
            let b1 = me.b1.clone().expect("b1");
            make_ready_future(me.factory().make_new_client_connection(b1, b2))
        })
    }

    fn set_reuseaddr(&mut self, _reuseaddr: bool) -> Result<(), ExceptionPtr> {
        Ok(())
    }

    fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr> {
        Ok(false)
    }

    fn shutdown(&mut self) -> Result<(), ExceptionPtr> {
        if let Some(mut p) = self.connect_abort.take() {
            p.set_exception(system_error(ECONNABORTED));
        } else {
            if let Some(b1) = &self.b1 {
                lb(b1).abort();
            }
            if let Some(b2) = self.b2.take() {
                let _ = Smp::submit_to(b2.get_owner_shard(), move || {
                    owned(&b2).abort();
                });
            }
        }
        Ok(())
    }
}
