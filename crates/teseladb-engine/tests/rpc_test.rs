//! Tests of the RPC layer over loopback sockets: calls with and without replies, timeouts and
//! cancellation, errors and handler exceptions, compression and its negotiation, streams,
//! connection glitches and error injection, scheduling-group isolation, metadata, and
//! client metrics. A client for `fn(In...) -> Future<Ret>` is `make_client::<fn(In...) ->
//! Future<Ret>>`, and a call is `.call(&mut c, &(args...,))`; a handler fails by returning
//! `Err`. Objects shared by reference are captured as raw pointers, valid because the owner
//! outlives the thread or continuation that uses them, and closures copied to every shard are
//! wrapped in `AssertSend`.

#![cfg(target_os = "linux")]
#![allow(clippy::type_complexity)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/loopback_socket.rs"]
mod loopback_socket;
#[path = "common/minstd.rs"]
mod minstd;

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use loopback_socket::{
    LoopbackConnectionFactory, LoopbackError, LoopbackErrorInjector, LoopbackSocketImpl,
};
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::Futurize;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, Promise, RuntimeError, make_ready_future,
};
use teseladb::engine::core::loop_::{do_until, parallel_for_each};
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::sharded::{Sharded, ShardedService};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::when_all::{when_all, when_all_succeed};
use teseladb::engine::net::api::Socket;
use teseladb::engine::net::api::{ConnectedSocket, Transport, make_ipv4_address};
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use teseladb::engine::net::stack::SocketImpl;
use teseladb::engine::rpc::lz4_compressor::Lz4Compressor;
use teseladb::engine::rpc::multi_algo_compressor_factory::MultiAlgoCompressorFactory;
use teseladb::engine::rpc::rpc::{
    Client, ClientOptions, Protocol, ResourceLimits, Server, ServerOptions, StreamingDomainType,
};
use teseladb::engine::rpc::rpc_impl::{
    self as rpc_impl, Read, RemoveOptionalTuple, RpcHandlerFn, RpcMsgType, SerializerInput,
    SerializerOutput, UnmarshallArgs, Write,
};
use teseladb::engine::rpc::rpc_types::{
    self as rpc_types, Cancellable, Compressor, CompressorFactory, RcvBuf, Sink, SndBuf, Source,
    Type,
};
use teseladb::engine::util::closeable::{DeferredStop, Stoppable};

/// The serializer of the test protocol.
#[derive(Clone, Copy, Default, Debug)]
struct Serializer;

/// Writes the bytes of an arithmetic value.
#[inline]
fn write_arithmetic_type<O: SerializerOutput, const N: usize>(
    out: &mut O,
    v: [u8; N],
) -> Result<(), ExceptionPtr> {
    out.write(&v)
}

/// Reads the bytes of an arithmetic value.
#[inline]
fn read_arithmetic_type<I: SerializerInput, const N: usize>(
    in_: &mut I,
) -> Result<[u8; N], ExceptionPtr> {
    let mut v = [0u8; N];
    in_.read(&mut v)?;
    Ok(v)
}

macro_rules! arithmetic_serializer {
    ($($t:ty),*) => {$(
        impl Write<$t> for Serializer {
            #[inline]
            fn write<O: SerializerOutput>(&self, output: &mut O, v: &$t) -> Result<(), ExceptionPtr> {
                write_arithmetic_type(output, v.to_ne_bytes())
            }
        }
        impl Read<$t> for Serializer {
            #[inline]
            fn read<I: SerializerInput>(&self, input: &mut I, _: Type<$t>) -> Result<$t, ExceptionPtr> {
                Ok(<$t>::from_ne_bytes(read_arithmetic_type(input)?))
            }
        }
    )*};
}

arithmetic_serializer!(i32, u32, i64, u64, f64);

impl Write<Sstring> for Serializer {
    #[inline]
    fn write<O: SerializerOutput>(&self, out: &mut O, v: &Sstring) -> Result<(), ExceptionPtr> {
        write_arithmetic_type(out, (v.size() as u32).to_ne_bytes())?;
        out.write(v.as_bytes())
    }
}

impl Read<Sstring> for Serializer {
    #[inline]
    fn read<I: SerializerInput>(
        &self,
        in_: &mut I,
        _: Type<Sstring>,
    ) -> Result<Sstring, ExceptionPtr> {
        let size = u32::from_ne_bytes(read_arithmetic_type(in_)?);
        let mut ret = uninitialized_string(size as usize);
        in_.read(ret.as_bytes_mut())?;
        Ok(ret)
    }
}

type TestRpcProto<M = u32> = Protocol<Serializer, M>;

/// The message of the exception `e` holds.
fn exc_what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

/// A value copied to another shard without a thread-safety requirement on its type.
struct AssertSend<T>(T);

impl<T: Clone> Clone for AssertSend<T> {
    fn clone(&self) -> Self {
        AssertSend(self.0.clone())
    }
}

// SAFETY: the test hands each shard its own copy or a pointer to an object that
// outlives the cross-shard call and is used by one shard at a time.
unsafe impl<T> Send for AssertSend<T> {}

impl<T> AssertSend<T> {
    /// The value, taken through the whole wrapper so that closures capture it whole.
    fn into_inner(self) -> T {
        self.0
    }
}

/// The IPv4 wildcard address with port 0.
fn any_addr() -> SocketAddress {
    make_ipv4_address(&Ipv4Addr::default())
}

/// One injection point of `rpc_loopback_error_injector::config`.
#[derive(Clone, Copy, Debug)]
struct InjectPoint {
    limit: i32,
    kind: LoopbackError,
    _x: i32,
}

impl Default for InjectPoint {
    fn default() -> Self {
        InjectPoint { limit: 0, kind: LoopbackError::None, _x: 0 }
    }
}

impl InjectPoint {
    fn inject(&mut self) -> LoopbackError {
        let x = self._x;
        self._x += 1;
        if x >= self.limit { self.kind } else { LoopbackError::None }
    }
}

/// Where the error injector fails a connection: per direction and end, and on connect.
#[derive(Clone, Copy, Debug)]
struct ErrorInjectorConfig {
    server_rcv: InjectPoint,
    server_snd: InjectPoint,
    client_rcv: InjectPoint,
    client_snd: InjectPoint,
    connect_kind: LoopbackError,
    connect_delay: Duration,
}

impl Default for ErrorInjectorConfig {
    fn default() -> Self {
        ErrorInjectorConfig {
            server_rcv: InjectPoint::default(),
            server_snd: InjectPoint::default(),
            client_rcv: InjectPoint::default(),
            client_snd: InjectPoint::default(),
            connect_kind: LoopbackError::None,
            connect_delay: Duration::from_micros(0),
        }
    }
}

/// A loopback error injector driven by an [`ErrorInjectorConfig`].
struct RpcLoopbackErrorInjector {
    _cfg: ErrorInjectorConfig,
}

impl RpcLoopbackErrorInjector {
    fn new(cfg: ErrorInjectorConfig) -> Self {
        RpcLoopbackErrorInjector { _cfg: cfg }
    }
}

impl LoopbackErrorInjector for RpcLoopbackErrorInjector {
    fn server_rcv_error(&mut self) -> LoopbackError {
        self._cfg.server_rcv.inject()
    }

    fn server_snd_error(&mut self) -> LoopbackError {
        self._cfg.server_snd.inject()
    }

    fn client_rcv_error(&mut self) -> LoopbackError {
        self._cfg.client_rcv.inject()
    }

    fn client_snd_error(&mut self) -> LoopbackError {
        self._cfg.client_snd.inject()
    }

    fn connect_error(&mut self) -> LoopbackError {
        self._cfg.connect_kind
    }

    fn connect_delay(&mut self) -> Duration {
        self._cfg.connect_delay
    }
}

/// A client socket over the loopback factory with an error injector. The injector is boxed so the
/// loopback socket can point at it.
struct RpcSocketImpl {
    _error_injector: Box<RpcLoopbackErrorInjector>,
    _socket: LoopbackSocketImpl,
}

impl RpcSocketImpl {
    fn new(
        factory: &mut LoopbackConnectionFactory,
        inject_error: Option<ErrorInjectorConfig>,
    ) -> Self {
        let mut ei = Box::new(RpcLoopbackErrorInjector::new(inject_error.unwrap_or_default()));
        let eip: *mut dyn LoopbackErrorInjector = &mut *ei;
        let socket =
            LoopbackSocketImpl::new(factory, if inject_error.is_some() { Some(eip) } else { None });
        RpcSocketImpl { _error_injector: ei, _socket: socket }
    }
}

impl SocketImpl for RpcSocketImpl {
    fn connect(
        &mut self,
        sa: SocketAddress,
        local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket> {
        self._socket.connect(sa, local, proto)
    }
    fn set_reuseaddr(&mut self, _reuseaddr: bool) -> Result<(), ExceptionPtr> {
        Ok(())
    }
    fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr> {
        Ok(false)
    }
    fn shutdown(&mut self) -> Result<(), ExceptionPtr> {
        self._socket.shutdown()
    }
}

/// The configuration of a test environment: resource limits, server options, error injection.
#[derive(Clone, Default)]
struct RpcTestConfig {
    resource_limits: ResourceLimits,
    server_options: ServerOptions,
    inject_error: Option<ErrorInjectorConfig>,
}

/// The per-shard RPC service of a test environment. The protocol is pinned in its box: the server
/// points at it.
/// The members are declared so that the handlers drop first, then the server, then the
/// protocol, which must outlive its server.
struct RpcTestService<M: RpcMsgType + Send> {
    _handlers: Vec<M>,
    _server: Pin<Box<Server>>,
    _proto: Pin<Box<TestRpcProto<M>>>,
}

impl<M: RpcMsgType + Send> RpcTestService<M> {
    fn new(cfg: &RpcTestConfig, lcf: &mut LoopbackConnectionFactory) -> Self {
        let mut proto = Box::pin(TestRpcProto::<M>::new(Serializer));
        let server = proto.as_mut().new_server_with_options_socket(
            cfg.server_options.clone(),
            lcf.get_server_socket(),
            cfg.resource_limits.clone(),
        );
        let mut server = Box::pin(server);
        // A constructor failure is a panic here.
        server.as_mut().start().unwrap_or_else(|e| panic!("{}", e));
        RpcTestService { _proto: proto, _server: server, _handlers: Vec::new() }
    }

    fn proto(&mut self) -> Pin<&mut TestRpcProto<M>> {
        self._proto.as_mut()
    }

    fn server(&mut self) -> &mut Server {
        // SAFETY: the server is not moved out of its pin.
        unsafe { self._server.as_mut().get_unchecked_mut() }
    }

    fn register_handler<F, Mk>(
        &mut self,
        t: M,
        sg: SchedulingGroup,
        func: F,
    ) -> Result<(), ExceptionPtr>
    where
        Serializer: Write<<F::Ret as Futurize>::Output>,
        F: RpcHandlerFn<Mk>,
        F::Args: UnmarshallArgs<Serializer> + RemoveOptionalTuple,
        Mk: 'static,
    {
        self._handlers.push(t);
        self.proto().register_handler_in(t, sg, func).map(|_| ())
    }

    fn unregister_handler(&mut self, t: M) -> Future<()> {
        let it = self._handlers.iter().position(|h| *h == t);
        assert!(it.is_some());
        if let Some(i) = it {
            self._handlers.remove(i);
        }
        self.proto().unregister_handler(t)
    }
}

impl<M: RpcMsgType + Send> ShardedService for RpcTestService<M> {
    fn stop(&mut self) -> Future<()> {
        let this: *mut RpcTestService<M> = self;
        let handlers = self._handlers.clone();
        // SAFETY: sharded::stop() keeps the service alive until this resolves.
        parallel_for_each(handlers, move |t: M| unsafe { (*this).proto().unregister_handler(t) })
            .finally(move || unsafe { (*this).server().stop() })
    }
}

/// A sharded RPC test service with a loopback connection factory, for one test.
struct RpcTestEnv<M: RpcMsgType + Send = u32> {
    _cfg: RpcTestConfig,
    _lcf: LoopbackConnectionFactory,
    _service: Pin<Box<Sharded<RpcTestService<M>>>>,
}

impl<M: RpcMsgType + Send> RpcTestEnv<M> {
    fn new(cfg: RpcTestConfig) -> Self {
        RpcTestEnv {
            _cfg: cfg,
            _lcf: LoopbackConnectionFactory::new_default(),
            _service: Box::pin(Sharded::new()),
        }
    }

    /// Runs `func` on a started environment and stops it afterwards.
    fn do_with<F>(cfg: RpcTestConfig, func: F) -> Future<()>
    where
        F: FnOnce(&mut RpcTestEnv<M>) -> Future<()> + 'static,
    {
        do_with(RpcTestEnv::<M>::new(cfg), move |env: &mut RpcTestEnv<M>| {
            let envp: *mut RpcTestEnv<M> = env;
            // SAFETY: do_with keeps the environment alive for the chain.
            env.start()
                .then(move |()| func(unsafe { &mut *envp }))
                .finally(move || unsafe { (*envp).stop() })
        })
    }

    /// As [`RpcTestEnv::do_with`], running `func` in an engine thread.
    fn do_with_thread<F>(cfg: RpcTestConfig, func: F) -> Future<()>
    where
        F: FnOnce(&mut RpcTestEnv<M>) -> Result<(), ExceptionPtr> + 'static,
    {
        Self::do_with(cfg, move |env: &mut RpcTestEnv<M>| {
            let envp: *mut RpcTestEnv<M> = env;
            // SAFETY: the environment outlives the thread, joined by do_with.
            async_(move || func(unsafe { &mut *envp }))
        })
    }

    /// As [`RpcTestEnv::do_with_thread`], also handing `func` a client made with options `co`.
    fn do_with_thread_client_opts<F>(cfg: RpcTestConfig, co: ClientOptions, func: F) -> Future<()>
    where
        F: FnOnce(&mut RpcTestEnv<M>, &mut Client) -> Result<(), ExceptionPtr> + 'static,
    {
        Self::do_with(cfg, move |env: &mut RpcTestEnv<M>| {
            let envp: *mut RpcTestEnv<M> = env;
            async_(move || -> Result<(), ExceptionPtr> {
                // SAFETY: the environment outlives the thread, joined by do_with.
                let env = unsafe { &mut *envp };
                let socket = env.make_socket();
                let mut cl = new_client(env.proto(), co, socket)?;
                let clp: *mut Client = client_mut(&mut cl);
                let mut stopper = ClientStopper(clp);
                let _stop = DeferredStop::new(&mut stopper);
                // SAFETY: the client lives in this frame, after the guard.
                func(env, unsafe { &mut *clp })
            })
        })
    }

    /// As [`RpcTestEnv::do_with_thread_client_opts`] with default client options.
    fn do_with_thread_client<F>(cfg: RpcTestConfig, func: F) -> Future<()>
    where
        F: FnOnce(&mut RpcTestEnv<M>, &mut Client) -> Result<(), ExceptionPtr> + 'static,
    {
        Self::do_with_thread_client_opts(cfg, ClientOptions::default(), func)
    }

    fn make_socket(&mut self) -> Socket {
        Socket::from_impl(Box::new(RpcSocketImpl::new(&mut self._lcf, self._cfg.inject_error)))
    }

    fn proto(&mut self) -> Pin<&mut TestRpcProto<M>> {
        self.local_service().proto()
    }

    fn server(&mut self) -> &mut Server {
        self.local_service().server()
    }

    fn register_handler_sg<F, Mk>(&mut self, t: M, sg: SchedulingGroup, func: F) -> Future<()>
    where
        Serializer: Write<<F::Ret as Futurize>::Output>,
        F: RpcHandlerFn<Mk> + Clone,
        F::Args: UnmarshallArgs<Serializer> + RemoveOptionalTuple,
        Mk: 'static,
    {
        let func = AssertSend(func);
        self._service.as_mut().invoke_on_all(move |s: &mut RpcTestService<M>| {
            let func = func.clone();
            s.register_handler(t, sg, func.0)
        })
    }

    fn register_handler<F, Mk>(&mut self, t: M, func: F) -> Future<()>
    where
        Serializer: Write<<F::Ret as Futurize>::Output>,
        F: RpcHandlerFn<Mk> + Clone,
        F::Args: UnmarshallArgs<Serializer> + RemoveOptionalTuple,
        Mk: 'static,
    {
        self.register_handler_sg(t, SchedulingGroup::default(), func)
    }

    fn unregister_handler(&mut self, t: M) -> Future<()> {
        self._service
            .as_mut()
            .invoke_on_all(move |s: &mut RpcTestService<M>| s.unregister_handler(t))
    }

    fn local_service(&mut self) -> &mut RpcTestService<M> {
        // SAFETY: the service is started on this shard and no other reference to it is live.
        unsafe { self._service.as_mut().local() }
    }

    fn start(&mut self) -> Future<()> {
        let cfg = AssertSend(&self._cfg as *const RpcTestConfig);
        let lcf = AssertSend(&mut self._lcf as *mut LoopbackConnectionFactory);
        // SAFETY: the environment outlives its service, which reads `cfg` and `lcf`.
        self._service.as_mut().start(move || unsafe {
            RpcTestService::new(&*cfg.into_inner(), &mut *lcf.into_inner())
        })
    }

    fn stop(&mut self) -> Future<()> {
        let lcf: *mut LoopbackConnectionFactory = &mut self._lcf;
        // SAFETY: the environment outlives its stop().
        self._service.as_mut().stop().then(move |()| unsafe { (*lcf).destroy_all_shards() })
    }
}

/// `deferred_stop` of a client: its `stop()` through the pointer the test keeps using.
struct ClientStopper(*mut Client);

impl Stoppable for ClientStopper {
    fn stop(&mut self) -> Future<()> {
        // SAFETY: the client outlives the guard that stops it (it is declared before it).
        unsafe { (*self.0).stop() }
    }
}

/// A client of `proto` over `socket` with options `co`, built and started in place.
fn new_client<M: RpcMsgType + Send>(
    proto: Pin<&mut TestRpcProto<M>>,
    co: ClientOptions,
    socket: Socket,
) -> Result<Pin<Box<Client>>, ExceptionPtr> {
    let mut cl = Box::pin(proto.new_client_with_options_socket(
        co,
        socket,
        &any_addr(),
        &SocketAddress::default(),
    ));
    cl.as_mut().start()?;
    Ok(cl)
}

/// The client inside its pin.
fn client_mut(cl: &mut Pin<Box<Client>>) -> &mut Client {
    // SAFETY: the client is never moved out of its pin.
    unsafe { cl.as_mut().get_unchecked_mut() }
}

/// A client of the environment's protocol over a new socket, with default options.
fn env_client<M: RpcMsgType + Send>(
    env: &mut RpcTestEnv<M>,
) -> Result<Pin<Box<Client>>, ExceptionPtr> {
    let socket = env.make_socket();
    new_client(env.proto(), ClientOptions::default(), socket)
}

/// An LZ4 compressor with its own name, for the negotiation tests.
struct Mylz4 {
    inner: Lz4Compressor,
    _name: Sstring,
}

impl Compressor for Mylz4 {
    fn compress(&mut self, head_space: usize, data: SndBuf) -> Result<SndBuf, ExceptionPtr> {
        self.inner.compress(head_space, data)
    }
    fn decompress(&mut self, data: RcvBuf) -> Result<RcvBuf, ExceptionPtr> {
        self.inner.decompress(data)
    }
    fn name(&self) -> Sstring {
        self._name.clone()
    }
}

/// A compressor factory under a given name that counts how often compression is used.
struct Cfactory {
    use_compression: Cell<i32>,
    name: Sstring,
}

impl Cfactory {
    fn new(name_: &str) -> Self {
        Cfactory { use_compression: Cell::new(0), name: Sstring::from(name_) }
    }
    fn new_default() -> Self {
        Self::new("LZ4")
    }
}

impl CompressorFactory for Cfactory {
    fn supported(&self) -> &Sstring {
        &self.name
    }
    fn negotiate(&self, feature: Sstring, _is_server: bool) -> Option<Box<dyn Compressor>> {
        if feature == self.name {
            self.use_compression.set(self.use_compression.get() + 1);
            Some(Box::new(Mylz4 { inner: Lz4Compressor, _name: self.name.clone() }))
        } else {
            None
        }
    }
}

/// The factory as a leaked box, as the
/// options take a `'static` factory; the test reads it after the run.
fn leak_factory(f: Cfactory) -> &'static Cfactory {
    Box::leak(Box::new(f))
}

teseladb::engine_test_case!(test_rpc_connect, {
    let mut fs: Vec<Future<()>> = Vec::new();

    for i in 0..2 {
        for j in 0..4 {
            for with_delay in [true, false] {
                let factory = leak_factory(Cfactory::new_default());
                let mut so = ServerOptions::default();
                let mut co = ClientOptions::default();
                if i == 1 {
                    so.compressor_factory = Some(factory);
                }
                if j & 1 != 0 {
                    co.compressor_factory = Some(factory);
                }
                co.send_timeout_data = j & 2 != 0;
                co.send_handler_duration = with_delay;
                let cfg = RpcTestConfig { server_options: so, ..Default::default() };
                let f = RpcTestEnv::<u32>::do_with_thread_client_opts(cfg, co, |env, c1| {
                    env.register_handler(1, |a: i32, b: i32| make_ready_future::<i32>(a + b))
                        .get()?;
                    let sum = env.proto().make_client::<fn(i32, i32) -> Future<i32>>(1);
                    let result = sum.call(c1, &(2, 3)).get()?;
                    assert_eq!(result, 2 + 3);
                    Ok(())
                })
                .handle_exception(|_ep: ExceptionPtr| -> () {
                    panic!("No exception expected");
                })
                .finally(move || {
                    let j = j & 1;
                    if i == 1 && j == 1 {
                        assert_eq!(factory.use_compression.get(), 2);
                    } else {
                        assert_eq!(factory.use_compression.get(), 0);
                    }
                });
                fs.push(f);
            }
        }
    }
    teseladb::engine::core::when_all::when_all_iter(fs).discard_result()
});

teseladb::engine_test_case!(test_rpc_connect_multi_compression_algo, {
    let factory1 = leak_factory(Cfactory::new_default());
    let factory2 = leak_factory(Cfactory::new("LZ4NEW"));
    let mut so = ServerOptions::default();
    let mut co = ClientOptions::default();
    let server: &'static MultiAlgoCompressorFactory =
        Box::leak(Box::new(MultiAlgoCompressorFactory::new(vec![factory1, factory2])));
    let client: &'static MultiAlgoCompressorFactory =
        Box::leak(Box::new(MultiAlgoCompressorFactory::new(vec![factory2, factory1])));
    so.compressor_factory = Some(server);
    co.compressor_factory = Some(client);
    let cfg = RpcTestConfig { server_options: so, ..Default::default() };
    RpcTestEnv::<u32>::do_with_thread_client_opts(cfg, co, |env, c1| {
        env.register_handler(1, |a: i32, b: i32| make_ready_future::<i32>(a + b)).get()?;
        let sum = env.proto().make_client::<fn(i32, i32) -> Future<i32>>(1);
        let result = sum.call(c1, &(2, 3)).get()?;
        assert_eq!(result, 2 + 3);
        Ok(())
    })
    .finally(move || {
        assert_eq!(factory1.use_compression.get(), 0);
        assert_eq!(factory2.use_compression.get(), 2);
    })
});

teseladb::engine_test_case!(test_rpc_connect_abort, {
    let ecfg = ErrorInjectorConfig {
        connect_kind: LoopbackError::Abort,
        ..ErrorInjectorConfig::default()
    };
    let cfg = RpcTestConfig { inject_error: Some(ecfg), ..Default::default() };
    RpcTestEnv::<u32>::do_with_thread(cfg, |env| {
        let mut c1 = env_client(env)?;
        env.register_handler(1, || make_ready_future(())).get()?;
        let f = env.proto().make_client::<fn() -> Future<()>>(1);
        let fut = f.call(client_mut(&mut c1), &());
        client_mut(&mut c1).stop().get()?;
        assert!(fut.get().is_err());
        assert!(f.call(client_mut(&mut c1), &()).get().is_err());
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_cancel, {
    RpcTestEnv::<u32>::do_with_thread_client(RpcTestConfig::default(), |env, c1| {
        let rpc_executed = Rc::new(Cell::new(false));
        let mut good = 0;
        let handler_called = Rc::new(RefCell::new(Promise::<()>::new()));
        let f_handler_called = handler_called.borrow_mut().get_future();
        let (re, hc) = (rpc_executed.clone(), handler_called.clone());
        env.register_handler(1, move || {
            hc.borrow_mut().set_value(());
            re.set(true);
            sleep(Duration::from_millis(1))
        })
        .get()?;
        let call = env.proto().make_client::<fn() -> Future<()>>(1);
        let mut cont = Promise::<()>::new();
        let mut cancel = std::pin::pin!(Cancellable::default());
        c1.suspend_for_testing(&mut cont)?;
        let mut f = call.call_cancellable(c1, cancel.as_mut(), &());
        // cancel send side
        cancel.as_mut().cancel();
        cont.set_value(());
        if let Err(e) = f.get() {
            if is_rpc_error(&e, rpc_types::ErrorKind::Canceled) {
                good += i32::from(!rpc_executed.get());
            } else {
                return Err(e);
            }
        }
        f = call.call_cancellable(c1, cancel.as_mut(), &());
        // cancel wait side
        // SAFETY: the cancellable stays pinned; only its address is taken.
        let cp: *mut Cancellable = unsafe { cancel.as_mut().get_unchecked_mut() };
        // SAFETY: the pinned cancellable lives in this thread's frame, which waits here.
        f_handler_called.then(move |()| unsafe { Pin::new_unchecked(&mut *cp).cancel() }).get()?;
        if let Err(e) = f.get() {
            if is_rpc_error(&e, rpc_types::ErrorKind::Canceled) {
                good += 10 * i32::from(rpc_executed.get());
            } else {
                return Err(e);
            }
        }
        assert_eq!(good, 11);
        Ok(())
    })
});

/// Whether `e` is an RPC error of the given kind.
fn is_rpc_error(e: &ExceptionPtr, kind: rpc_types::ErrorKind) -> bool {
    e.downcast_ref::<rpc_types::Error>().is_some_and(|err| err.kind() == kind)
}

teseladb::engine_test_case!(test_message_to_big, {
    let cfg = RpcTestConfig {
        resource_limits: ResourceLimits {
            basic_request_size: 0,
            bloat_factor: 1,
            max_memory: 100,
            ..Default::default()
        },
        ..Default::default()
    };
    RpcTestEnv::<u32>::do_with_thread_client(cfg, |env, c| {
        let good = Rc::new(Cell::new(true));
        let g = good.clone();
        env.register_handler(1, move |_payload: Sstring| {
            g.set(false);
        })
        .get()?;
        let call = env.proto().make_client::<fn(Sstring) -> Future<()>>(1);
        match call.call(c, &(uninitialized_string(101),)).get() {
            Ok(()) => good.set(false),
            // runtime errors and the errors built on them
            Err(err) if err.is::<RuntimeError>() || err.is::<rpc_types::Error>() => {}
            Err(_) => good.set(false),
        }
        assert!(good.get());
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_remote_verb_error, {
    let cfg = RpcTestConfig::default();
    RpcTestEnv::<u32>::do_with_thread(cfg, |env| {
        let mut c1 = env_client(env)?;
        env.register_handler(1, || -> Result<(), ExceptionPtr> {
            Err(ExceptionPtr::new(teseladb::engine::core::future::RuntimeError::new(
                "error".to_string(),
            )))
        })
        .get()?;
        let f = env.proto().make_client::<fn() -> Future<()>>(1);
        let r_ = f.call(client_mut(&mut c1), &()).get();
        assert!(r_.is_err());
        let Err(e) = r_ else { unreachable!() };
        assert!(is_rpc_error(&e, rpc_types::ErrorKind::RemoteVerb));
        client_mut(&mut c1).stop().get()?;
        Ok(())
    })
});

/// What a stream test observed: which ends closed and which operations failed.
#[derive(Clone, Copy, Default, Debug)]
struct StreamTestResult {
    client_source_closed: bool,
    server_source_closed: bool,
    sink_exception: bool,
    sink_close_exception: bool,
    source_done_exception: bool,
    server_done_exception: bool,
    client_stop_exception: bool,
    server_stop_exception: bool,
    server_sum: i32,
    exception_while_creating_sink: bool,
}

fn stream_test_func(
    env: *mut RpcTestEnv,
    stop_client: bool,
    stop_server: bool,
    expect_connection_error: bool,
) -> Future<StreamTestResult> {
    async_(move || -> Result<StreamTestResult, ExceptionPtr> {
        // SAFETY: the environment outlives the thread, joined by its do_with.
        let env = unsafe { &mut *env };
        let r = Rc::new(RefCell::new(StreamTestResult::default()));
        let mut c = env_client(env)?;
        let server_done: Rc<RefCell<Future<()>>> = Rc::new(RefCell::new(make_ready_future(())));
        let (r1, sd) = (r.clone(), server_done.clone());
        env.register_handler(
            1,
            move |i: i32, source: Source<(i32,)>| -> Future<Sink<(Sstring,)>> {
                assert_eq!(i, 666);
                let sink = source.make_sink::<Serializer, (Sstring,)>();
                let (mut s1, mut s2, mut s3, s4) =
                    (sink.clone(), sink.clone(), sink.clone(), sink.clone());
                let sink_loop = async_(move || -> Result<(), ExceptionPtr> {
                    for _ in 0..100 {
                        // SAFETY: `s1` lives until the call resolves (waited for here).
                        unsafe { s1.call(&(Sstring::from("engine"),)) }.get()?;
                        sleep(Duration::from_millis(1)).get()?;
                    }
                    Ok(())
                })
                // SAFETY: `s4`, a copy of the sink, lives until the last finally() below.
                .finally(move || unsafe { s2.flush() })
                // SAFETY: as above.
                .finally(move || unsafe { s3.close() })
                .finally(move || {
                    // `[copy] {}`: the closure owns the copy until it runs (`let _ = x` would not capture it).
                    drop(s4);
                });

                let r2 = r1.clone();
                let mut source = source;
                let source_loop = async_(move || -> Result<(), ExceptionPtr> {
                    while !r2.borrow().server_source_closed {
                        // SAFETY: `source` lives until the call resolves (waited for here).
                        let data = unsafe { source.call() }.get()?;
                        match data {
                            Some(d) => r2.borrow_mut().server_sum += d.0,
                            None => {
                                r2.borrow_mut().server_source_closed = true;
                                // check that reading after eos does not crash
                                // and fails with the right exception
                                // SAFETY: as above.
                                match unsafe { source.call() }.get() {
                                    Err(ex)
                                        if is_rpc_error(
                                            &ex,
                                            rpc_types::ErrorKind::StreamClosed,
                                        ) =>
                                    {
                                        // expected
                                    }
                                    _ => {
                                        panic!("wrong exception on reading from a stream after eos")
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                });
                *sd.borrow_mut() = when_all_succeed((sink_loop, source_loop)).discard_result();
                make_ready_future(sink)
            },
        )
        .get()?;
        let call =
            env.proto().make_client::<fn(i32, Sink<(i32,)>) -> Future<Source<(Sstring,)>>>(1);
        // The error of x() below.
        let mut x = || -> Result<Option<Sink<(i32,)>>, ExceptionPtr> {
            let socket = env.make_socket();
            match client_mut(&mut c).make_stream_sink_with::<Serializer, (i32,)>(socket).get() {
                Ok(s) => Ok(Some(s)),
                Err(_) => {
                    client_mut(&mut c).stop().get()?;
                    Ok(None) // throw failed_to_create_sync{}
                }
            }
        };
        let Some(mut sink) = x()? else {
            r.borrow_mut().exception_while_creating_sink = true;
            return Ok(*r.borrow());
        };
        let mut source = call.call(client_mut(&mut c), &(666, sink.clone())).get()?;
        let r3 = r.clone();
        let source_done = async_(move || -> Result<(), ExceptionPtr> {
            while !r3.borrow().client_source_closed {
                // SAFETY: `source` lives until the call resolves (waited for here).
                let data = unsafe { source.call() }.get()?;
                match data {
                    Some(d) => assert_eq!(d.0, "engine"),
                    None => r3.borrow_mut().client_source_closed = true,
                }
            }
            Ok(())
        });
        let check_exception = |f: Future<()>| -> bool { f.get().is_err() };
        let mut stop_client_future: Future<()> = make_ready_future(());
        let mut stop_server_future: Future<()> = make_ready_future(());
        // With a connection error sink() will eventually fail, but we
        // cannot guarantee when.
        let max = if expect_connection_error { i32::MAX } else { 101 };
        for i in 1..max {
            if i == 50 {
                if stop_client {
                    // stop client while stream is in use
                    stop_client_future = client_mut(&mut c).stop();
                }
                if stop_server {
                    // stop server while stream is in use
                    stop_server_future = env.server().shutdown();
                }
            }
            sleep(Duration::from_millis(1)).get()?;
            // SAFETY: `sink` lives until the call resolves (waited for by check_exception).
            let ex = check_exception(unsafe { sink.call(&(i,)) });
            r.borrow_mut().sink_exception = ex;
            if ex {
                break;
            }
        }
        // SAFETY: as above.
        let sce = check_exception(unsafe { sink.close() });
        r.borrow_mut().sink_close_exception = sce;
        r.borrow_mut().source_done_exception = check_exception(source_done);
        let sdf = std::mem::replace(&mut *server_done.borrow_mut(), make_ready_future(()));
        r.borrow_mut().server_done_exception = check_exception(sdf);
        let cse = check_exception(if !stop_client {
            client_mut(&mut c).stop()
        } else {
            stop_client_future
        });
        r.borrow_mut().client_stop_exception = cse;
        let sse =
            check_exception(if !stop_server { make_ready_future(()) } else { stop_server_future });
        r.borrow_mut().server_stop_exception = sse;
        let res = *r.borrow();
        Ok(res)
    })
}

fn streaming_cfg() -> RpcTestConfig {
    let so = ServerOptions {
        streaming_domain: Some(StreamingDomainType::new(1)),
        ..ServerOptions::default()
    };
    RpcTestConfig { server_options: so, ..Default::default() }
}

teseladb::engine_test_case!(test_stream_simple, {
    RpcTestEnv::<u32>::do_with(streaming_cfg(), |env| {
        stream_test_func(env, false, false, false).then(|r: StreamTestResult| {
            assert!(r.client_source_closed);
            assert!(r.server_source_closed);
            assert!(r.server_sum == 5050);
            assert!(!r.sink_exception);
            assert!(!r.sink_close_exception);
            assert!(!r.source_done_exception);
            assert!(!r.server_done_exception);
            assert!(!r.client_stop_exception);
        })
    })
});

teseladb::engine_test_case!(test_stream_stop_client, {
    RpcTestEnv::<u32>::do_with(streaming_cfg(), |env| {
        stream_test_func(env, true, false, false).then(|r: StreamTestResult| {
            assert!(!r.client_source_closed);
            assert!(!r.server_source_closed);
            assert!(r.sink_exception);
            assert!(r.sink_close_exception);
            assert!(r.source_done_exception);
            assert!(r.server_done_exception);
            assert!(!r.client_stop_exception);
        })
    })
});

teseladb::engine_test_case!(test_stream_stop_server, {
    RpcTestEnv::<u32>::do_with(streaming_cfg(), |env| {
        stream_test_func(env, false, true, false).then(|r: StreamTestResult| {
            assert!(!r.client_source_closed);
            assert!(!r.server_source_closed);
            assert!(r.sink_exception);
            assert!(r.sink_close_exception);
            assert!(r.source_done_exception);
            assert!(r.server_done_exception);
            assert!(!r.client_stop_exception);
            assert!(!r.server_stop_exception);
        })
    })
});

teseladb::engine_test_case!(test_stream_connection_error, {
    let mut cfg = streaming_cfg();
    let mut ecfg = ErrorInjectorConfig::default();
    ecfg.server_rcv.limit = 50;
    ecfg.server_rcv.kind = LoopbackError::Abort;
    cfg.inject_error = Some(ecfg);
    RpcTestEnv::<u32>::do_with(cfg, |env| {
        stream_test_func(env, false, false, true).then(|r: StreamTestResult| {
            assert!(!r.client_source_closed);
            assert!(!r.server_source_closed);
            assert!(r.sink_exception);
            assert!(r.sink_close_exception);
            assert!(r.source_done_exception);
            assert!(r.server_done_exception);
            assert!(!r.client_stop_exception);
        })
    })
});

teseladb::engine_test_case!(test_stream_negotiation_error, {
    let mut cfg = streaming_cfg();
    let mut ecfg = ErrorInjectorConfig::default();
    ecfg.server_rcv.limit = 0;
    ecfg.server_rcv.kind = LoopbackError::Abort;
    cfg.inject_error = Some(ecfg);
    RpcTestEnv::<u32>::do_with(cfg, |env| {
        stream_test_func(env, false, false, true).then(|r: StreamTestResult| {
            assert!(r.exception_while_creating_sink);
        })
    })
});

/// The shared state of the send-glitch test.
struct GlitchContext {
    limit: i32,
    no_failures: bool,
    on_client: bool,
}

fn test_rpc_connection_send_glitch(on_client: bool) -> Future<()> {
    do_with(GlitchContext { limit: 0, no_failures: false, on_client }, |ctx: &mut GlitchContext| {
        let ctxp: *mut GlitchContext = ctx;
        do_until(
            // SAFETY: do_with keeps the context alive for the whole loop; the condition only reads it.
            move || unsafe { (*ctxp).no_failures },
            move || {
                // SAFETY: do_with keeps the context alive for the whole loop.
                let ctx = unsafe { &mut *ctxp };
                println!("======== 8< ========");
                println!("  Checking {} limit", ctx.limit);
                let mut cfg = RpcTestConfig::default();
                let mut ecfg = ErrorInjectorConfig::default();
                if ctx.on_client {
                    ecfg.client_snd.limit = ctx.limit;
                    ecfg.client_snd.kind = LoopbackError::OneShot;
                } else {
                    ecfg.server_snd.limit = ctx.limit;
                    ecfg.server_snd.kind = LoopbackError::OneShot;
                }
                cfg.inject_error = Some(ecfg);
                RpcTestEnv::<u32>::do_with_thread_client(cfg, move |env, c1| {
                    // SAFETY: do_with keeps the context alive for the whole loop; the step above has returned, so nothing else references it.
                    let ctx = unsafe { &mut *ctxp };
                    env.register_handler(1, || {
                        println!("  reply");
                        sleep(Duration::from_millis(100)).then(|()| make_ready_future::<u32>(13))
                    })
                    .get()?;

                    ctx.no_failures = true;

                    for i in 0..4 {
                        let call = env.proto().make_client::<fn() -> Future<u32>>(1);
                        println!("  call {}", i);
                        match call.call(c1, &()).get() {
                            Ok(id) => println!("    response: {}", id),
                            Err(e) => {
                                println!("    responce: ex {}", exc_what(&e));
                                ctx.no_failures = false;
                                ctx.limit += 1;
                                break;
                            }
                        }
                        teseladb::engine::core::reactor::yield_().get()?;
                    }
                    Ok(())
                })
            },
        )
    })
}

teseladb::engine_test_case!(test_rpc_client_send_glitch, { test_rpc_connection_send_glitch(true) });

teseladb::engine_test_case!(test_rpc_server_send_glitch, {
    test_rpc_connection_send_glitch(false)
});

use teseladb::engine::core::reactor::{
    create_scheduling_group_with_shares, destroy_scheduling_group,
};
use teseladb::engine::core::scheduling::{
    current_scheduling_group, default_scheduling_group, internal::scheduling_group_index,
};
use teseladb::engine::rpc::rpc::{IsolationConfig, IsolationFunctionAlternatives};
use teseladb::engine::util::defer::defer;

/// Creates a scheduling group with the given name and shares.
fn create_scheduling_group(name: &str, shares: f32) -> Future<SchedulingGroup> {
    create_scheduling_group_with_shares(Sstring::from(name), shares)
}

/// A handler that returns the index of the scheduling group it runs in.
fn sg_index_handler() -> Future<u32> {
    make_ready_future::<u32>(scheduling_group_index(current_scheduling_group()))
}

teseladb::engine_test_case!(test_rpc_scheduling, {
    RpcTestEnv::<u32>::do_with_thread_client(RpcTestConfig::default(), |env, c1| {
        let sg = create_scheduling_group("rpc", 100.0).get()?;
        env.register_handler_sg(1, sg, sg_index_handler).get()?;
        let call_sg_id = env.proto().make_client::<fn() -> Future<u32>>(1);
        let id = call_sg_id.call(c1, &()).get()?;
        assert!(id == scheduling_group_index(sg));
        Ok(())
    })
});

/// The synchronous `isolate_connection` of the connection-based tests.
fn sync_isolation(sg1: SchedulingGroup, sg2: SchedulingGroup) -> IsolationFunctionAlternatives {
    IsolationFunctionAlternatives::Syncronous(Box::new(
        move |cookie: Sstring| -> Result<IsolationConfig, ExceptionPtr> {
            let mut sg = current_scheduling_group();
            if cookie == "sg1" {
                sg = sg1;
            } else if cookie == "sg2" {
                sg = sg2;
            }
            let cfg = IsolationConfig { sched_group: sg };
            Ok(cfg)
        },
    ))
}

/// Client options with the isolation cookie `cookie`.
fn cookie_options(cookie: &str) -> ClientOptions {
    ClientOptions { isolation_cookie: Sstring::from(cookie), ..ClientOptions::default() }
}

teseladb::engine_test_case!(test_rpc_scheduling_connection_based, {
    teseladb::engine::core::thread::async_(|| {
        let sg1 = create_scheduling_group("sg1", 100.0).get()?;
        let s1 = sg1;
        let _sg1_kill = defer(move || {
            let _ = destroy_scheduling_group(s1).get();
        });
        let sg2 = create_scheduling_group("sg2", 100.0).get()?;
        let s2 = sg2;
        let _sg2_kill = defer(move || {
            let _ = destroy_scheduling_group(s2).get();
        });
        let limits = ResourceLimits {
            isolate_connection: sync_isolation(sg1, sg2),
            ..ResourceLimits::default()
        };
        let cfg = RpcTestConfig { resource_limits: limits, ..Default::default() };
        RpcTestEnv::<u32>::do_with_thread(cfg, move |env| {
            let co1 = cookie_options("sg1");
            let s = env.make_socket();
            let mut c1 = new_client(env.proto(), co1, s)?;
            let co2 = cookie_options("sg2");
            let s = env.make_socket();
            let mut c2 = new_client(env.proto(), co2, s)?;
            env.register_handler(1, sg_index_handler).get()?;
            let call_sg_id = env.proto().make_client::<fn() -> Future<u32>>(1);
            let mut id: u32;
            id = call_sg_id.call(client_mut(&mut c1), &()).get()?;
            assert!(id == scheduling_group_index(sg1));
            id = call_sg_id.call(client_mut(&mut c2), &()).get()?;
            assert!(id == scheduling_group_index(sg2));
            client_mut(&mut c1).stop().get()?;
            client_mut(&mut c2).stop().get()?;
            Ok(())
        })
        .get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_scheduling_connection_based_compatibility, {
    teseladb::engine::core::thread::async_(|| {
        let sg1 = create_scheduling_group("sg1", 100.0).get()?;
        let s1 = sg1;
        let _sg1_kill = defer(move || {
            let _ = destroy_scheduling_group(s1).get();
        });
        let sg2 = create_scheduling_group("sg2", 100.0).get()?;
        let s2 = sg2;
        let _sg2_kill = defer(move || {
            let _ = destroy_scheduling_group(s2).get();
        });
        let limits = ResourceLimits {
            isolate_connection: sync_isolation(sg1, sg2),
            ..ResourceLimits::default()
        };
        let cfg = RpcTestConfig { resource_limits: limits, ..Default::default() };
        RpcTestEnv::<u32>::do_with_thread(cfg, move |env| {
            let co1 = cookie_options("sg1");
            let s = env.make_socket();
            let mut c1 = new_client(env.proto(), co1, s)?;
            let co2 = cookie_options("sg2");
            let s = env.make_socket();
            let mut c2 = new_client(env.proto(), co2, s)?;
            // An old client, that doesn't have an isolation cookie
            let co3 = ClientOptions::default();
            let s = env.make_socket();
            let mut c3 = new_client(env.proto(), co3, s)?;
            // A server that uses sg1 if the client is old
            env.register_handler_sg(1, sg1, sg_index_handler).get()?;
            let call_sg_id = env.proto().make_client::<fn() -> Future<u32>>(1);
            let mut id: u32;
            id = call_sg_id.call(client_mut(&mut c1), &()).get()?;
            assert!(id == scheduling_group_index(sg1));
            id = call_sg_id.call(client_mut(&mut c2), &()).get()?;
            assert!(id == scheduling_group_index(sg2));
            id = call_sg_id.call(client_mut(&mut c3), &()).get()?;
            assert!(id == scheduling_group_index(sg1));
            client_mut(&mut c1).stop().get()?;
            client_mut(&mut c2).stop().get()?;
            client_mut(&mut c3).stop().get()?;
            Ok(())
        })
        .get()?;
        Ok(())
    })
});

/// The asynchronous `isolate_connection` of the async connection-based tests: creates `sg1`
/// and `sg2` on first use, through the pointers the test passes.
fn async_isolation(
    sg1: *mut SchedulingGroup,
    sg2: *mut SchedulingGroup,
) -> IsolationFunctionAlternatives {
    IsolationFunctionAlternatives::Asyncronous(Box::new(
        move |cookie: Sstring| -> Future<IsolationConfig> {
            let get_scheduling_group: Future<SchedulingGroup> =
                make_ready_future(()).then(move |()| -> Future<SchedulingGroup> {
                    if cookie == "sg1" {
                        // SAFETY: the scheduling groups live in the test's frame, which
                        // outlives the rpc environment.
                        if unsafe { *sg1 == default_scheduling_group() } {
                            return create_scheduling_group("sg1", 100.0).then(
                                move |sg: SchedulingGroup| {
                                    // SAFETY: the scheduling groups live in the test's frame, which outlives the rpc environment.
                                    unsafe { *sg1 = sg };
                                    make_ready_future(sg)
                                },
                            );
                        } else {
                            // SAFETY: the scheduling groups live in the test's frame, which outlives the rpc environment.
                            return make_ready_future(unsafe { *sg1 });
                        }
                    } else if cookie == "sg2" {
                        // SAFETY: the scheduling groups live in the test's frame, which outlives the rpc environment.
                        if unsafe { *sg2 == default_scheduling_group() } {
                            return create_scheduling_group("sg2", 100.0).then(
                                move |sg: SchedulingGroup| {
                                    // SAFETY: the scheduling groups live in the test's frame, which outlives the rpc environment.
                                    unsafe { *sg2 = sg };
                                    make_ready_future(sg)
                                },
                            );
                        } else {
                            // SAFETY: the scheduling groups live in the test's frame, which outlives the rpc environment.
                            return make_ready_future(unsafe { *sg2 });
                        }
                    }
                    make_ready_future(current_scheduling_group())
                });

            get_scheduling_group.then(|sg: SchedulingGroup| {
                let cfg = IsolationConfig { sched_group: sg };
                make_ready_future(cfg)
            })
        },
    ))
}

/// A guard that destroys `sg` when it was created (it is not the default group).
fn destroy_if_created(sg: *mut SchedulingGroup) -> impl Drop {
    defer(move || {
        // SAFETY: the scheduling group lives in the test's frame, declared before the guard.
        let sg = unsafe { *sg };
        if sg != default_scheduling_group() {
            let _ = destroy_scheduling_group(sg).get();
        }
    })
}

teseladb::engine_test_case!(test_rpc_scheduling_connection_based_async, {
    teseladb::engine::core::thread::async_(|| {
        let mut sg1: SchedulingGroup = default_scheduling_group();
        let mut sg2: SchedulingGroup = default_scheduling_group();
        let (sg1p, sg2p): (*mut SchedulingGroup, *mut SchedulingGroup) = (&mut sg1, &mut sg2);
        let _sg1_kill = destroy_if_created(sg1p);
        let _sg2_kill = destroy_if_created(sg2p);
        let limits = ResourceLimits {
            isolate_connection: async_isolation(sg1p, sg2p),
            ..ResourceLimits::default()
        };
        let cfg = RpcTestConfig { resource_limits: limits, ..Default::default() };
        RpcTestEnv::<u32>::do_with_thread(cfg, move |env| {
            let co1 = cookie_options("sg1");
            let s = env.make_socket();
            let mut c1 = new_client(env.proto(), co1, s)?;
            let co2 = cookie_options("sg2");
            let s = env.make_socket();
            let mut c2 = new_client(env.proto(), co2, s)?;
            env.register_handler(1, sg_index_handler).get()?;
            let call_sg_id = env.proto().make_client::<fn() -> Future<u32>>(1);
            let mut id: u32;
            id = call_sg_id.call(client_mut(&mut c1), &()).get()?;
            // SAFETY: `sg1p` points at a scheduling group in this frame, which outlives the rpc environment.
            assert!(id == scheduling_group_index(unsafe { *sg1p }));
            id = call_sg_id.call(client_mut(&mut c2), &()).get()?;
            // SAFETY: `sg2p` points at a scheduling group in this frame, which outlives the rpc environment.
            assert!(id == scheduling_group_index(unsafe { *sg2p }));
            client_mut(&mut c1).stop().get()?;
            client_mut(&mut c2).stop().get()?;
            Ok(())
        })
        .get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_scheduling_connection_based_compatibility_async, {
    teseladb::engine::core::thread::async_(|| {
        let mut sg1: SchedulingGroup = default_scheduling_group();
        let mut sg2: SchedulingGroup = default_scheduling_group();
        let sg3: SchedulingGroup = create_scheduling_group("sg3", 100.0).get()?;
        let (sg1p, sg2p): (*mut SchedulingGroup, *mut SchedulingGroup) = (&mut sg1, &mut sg2);
        let _sg1_kill = destroy_if_created(sg1p);
        let _sg2_kill = destroy_if_created(sg2p);
        let s3 = sg3;
        let _sg3_kill = defer(move || {
            let _ = destroy_scheduling_group(s3).get();
        });
        let limits = ResourceLimits {
            isolate_connection: async_isolation(sg1p, sg2p),
            ..ResourceLimits::default()
        };
        let cfg = RpcTestConfig { resource_limits: limits, ..Default::default() };
        RpcTestEnv::<u32>::do_with_thread(cfg, move |env| {
            let co1 = cookie_options("sg1");
            let s = env.make_socket();
            let mut c1 = new_client(env.proto(), co1, s)?;
            let co2 = cookie_options("sg2");
            let s = env.make_socket();
            let mut c2 = new_client(env.proto(), co2, s)?;
            // An old client, that doesn't have an isolation cookie
            let co3 = ClientOptions::default();
            let s = env.make_socket();
            let mut c3 = new_client(env.proto(), co3, s)?;
            // A server that uses sg3 if the client is old
            env.register_handler_sg(1, sg3, sg_index_handler).get()?;
            let call_sg_id = env.proto().make_client::<fn() -> Future<u32>>(1);
            let mut id: u32;
            id = call_sg_id.call(client_mut(&mut c1), &()).get()?;
            // SAFETY: `sg1p` points at a scheduling group in this frame, which outlives the rpc environment.
            assert!(id == scheduling_group_index(unsafe { *sg1p }));
            id = call_sg_id.call(client_mut(&mut c2), &()).get()?;
            // SAFETY: `sg2p` points at a scheduling group in this frame, which outlives the rpc environment.
            assert!(id == scheduling_group_index(unsafe { *sg2p }));
            id = call_sg_id.call(client_mut(&mut c3), &()).get()?;
            assert!(id == scheduling_group_index(sg3));
            client_mut(&mut c1).stop().get()?;
            client_mut(&mut c2).stop().get()?;
            client_mut(&mut c3).stop().get()?;
            Ok(())
        })
        .get()?;
        Ok(())
    })
});

use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::rpc::lz4_fragmented_compressor::Lz4FragmentedCompressor;
use teseladb::engine::rpc::rpc_types::Bufs;
use teseladb::engine::testing::random::testing::local_random;

/// The first `size` bytes of `bufs` in one buffer.
fn linearize(bufs: &Bufs, size: u32) -> TemporaryBuffer<u8> {
    match bufs {
        Bufs::One(buf) => buf.clone_buffer(),
        Bufs::Many(bufs) => {
            let mut buf = TemporaryBuffer::<u8>::new(size as usize);
            let mut dst = 0usize;
            // SAFETY: `buf` holds `size` bytes, the sum of the pieces.
            let out = unsafe { std::slice::from_raw_parts_mut(buf.get_write(), size as usize) };
            for b in bufs {
                out[dst..dst + b.size()].copy_from_slice(b.as_slice());
                dst += b.size();
            }
            buf
        }
    }
}

/// Splits `b` into buffers of at most `chunk_size` bytes.
fn split_buffer(b: TemporaryBuffer<u8>, chunk_size: usize) -> Vec<TemporaryBuffer<u8>> {
    let mut bufs = Vec::new();
    let mut src = b.as_slice();
    while !src.is_empty() {
        let this_chunk = chunk_size.min(src.len());
        bufs.push(TemporaryBuffer::copy_from(&src[..this_chunk]));
        src = &src[this_chunk..];
    }
    bufs
}

/// A deep copy of `bufs`.
fn clone_bufs(bufs: &Bufs) -> Bufs {
    match bufs {
        Bufs::One(buf) => Bufs::One(buf.clone_buffer()),
        Bufs::Many(bufs) => Bufs::Many(bufs.iter().map(|b| b.clone_buffer()).collect()),
    }
}

fn clone_snd(buffer: &SndBuf) -> SndBuf {
    SndBuf { size: buffer.size, bufs: clone_bufs(&buffer.bufs) }
}

/// `snd_buf` of `buf`, whole or split.
fn make_snd(buf: TemporaryBuffer<u8>, s: usize, split: Option<usize>) -> SndBuf {
    SndBuf {
        size: s as u32,
        bufs: match split {
            Some(split) => Bufs::Many(split_buffer(buf.clone_buffer(), split)),
            None => Bufs::One(buf.clone_buffer()),
        },
    }
}

type CompressorInput = Box<dyn FnOnce() -> (Sstring, usize, SndBuf)>;

fn gen_fill(s: usize, msg: String, split: Option<usize>) -> (Sstring, usize, SndBuf) {
    let mut buf = TemporaryBuffer::<u8>::new(s);
    // SAFETY: `buf` holds `s` bytes.
    unsafe { std::slice::from_raw_parts_mut(buf.get_write(), s) }.fill(b'a');
    (Sstring::from(msg.as_str()), 0, make_snd(buf, s, split))
}

fn gen_rand(s: usize, msg: String, split: Option<usize>) -> (Sstring, usize, SndBuf) {
    let dist = minstd::UniformIntDistribution::new(0, i8::MAX as u64);
    let mut buf = TemporaryBuffer::<u8>::new(s);
    // SAFETY: `buf` holds `s` bytes.
    for c in unsafe { std::slice::from_raw_parts_mut(buf.get_write(), s) } {
        *c = dist.sample_fn(&mut || u64::from(local_random())) as u8;
    }
    (Sstring::from(msg.as_str()), 0, make_snd(buf, s, split))
}

fn gen_text(s: usize, msg: String, split: Option<usize>) -> (Sstring, usize, SndBuf) {
    const TEXT: &[u8] = b"The quick brown fox wants bananas for his long term health but sneaks bacon behind his wife's back. ";

    let mut buf = TemporaryBuffer::<u8>::new(s);
    // SAFETY: `buf` holds `s` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(buf.get_write(), s) };
    let mut n = 0;
    while n < s {
        let rem = (s - n).min(TEXT.len());
        out[n..n + rem].copy_from_slice(&TEXT[..rem]);
        n += rem;
    }
    (Sstring::from(msg.as_str()), 0, make_snd(buf, s, split))
}

/// Checks that `bufs` holds `size` bytes.
fn sanity_check(bufs: &Bufs, size: u32) {
    let actual_size = match bufs {
        Bufs::One(buf) => buf.size(),
        Bufs::Many(bufs) => bufs.iter().fold(0usize, |sz, buf| sz + buf.size()),
    };
    assert_eq!(actual_size, size as usize);
}

fn test_compressor(
    compressor_factory: impl Fn() -> Box<dyn Compressor>,
) -> Result<(), ExceptionPtr> {
    let mut compressor = compressor_factory();

    let mut inputs: Vec<CompressorInput> = Vec::new();

    let mut add_input = |f: CompressorInput| inputs.push(f);

    let mut snd = SndBuf::with_size(1);
    // SAFETY: the buffer holds one byte.
    unsafe { *snd.front().get_write() = b'a' };
    add_input(Box::new(move || (Sstring::from("one byte, no headroom"), 0usize, snd)));

    let mut snd = SndBuf::with_size(1);
    // SAFETY: the buffer holds one byte.
    unsafe { *snd.front().get_write() = b'a' };
    add_input(Box::new(move || {
        (Sstring::from("one byte, 128k of headroom"), 128 * 1024usize, snd)
    }));

    add_input(Box::new(|| gen_fill(16 * 1024, "single 16 kB buffer of 'a'".to_string(), None)));

    add_input(Box::new(|| gen_rand(16 * 1024, "single 16 kB buffer of random".to_string(), None)));

    // ENGINE_DEBUG
    let buffer_sizes: &[usize] = if cfg!(debug_assertions) { &[1] } else { &[1, 4] };

    for &s in buffer_sizes {
        for ss in [32usize, 64, 128, 48, 56, 246, 511] {
            add_input(Box::new(move || {
                gen_fill(
                    s * 1024 * 1024,
                    format!("{} MB buffer of 'a' split into {} kB - {}", s, ss, ss),
                    Some(ss * 1024 - ss),
                )
            }));
            add_input(Box::new(move || {
                gen_fill(
                    s * 1024 * 1024,
                    format!("{} MB buffer of 'a' split into {} kB", s, ss),
                    Some(ss * 1024),
                )
            }));
            add_input(Box::new(move || {
                gen_rand(
                    s * 1024 * 1024,
                    format!("{} MB buffer of random split into {} kB", s, ss),
                    Some(ss * 1024),
                )
            }));

            add_input(Box::new(move || {
                gen_fill(
                    s * 1024 * 1024 + 1,
                    format!("{} MB + 1B buffer of 'a' split into {} kB", s, ss),
                    Some(ss * 1024),
                )
            }));
            add_input(Box::new(move || {
                gen_rand(
                    s * 1024 * 1024 + 1,
                    format!("{} MB + 1B buffer of random split into {} kB", s, ss),
                    Some(ss * 1024),
                )
            }));
        }

        for ss in [128usize, 246, 511, 3567, 2 * 1024, 8 * 1024] {
            add_input(Box::new(move || {
                gen_fill(
                    s * 1024 * 1024,
                    format!("{} MB buffer of 'a' split into {} B", s, ss),
                    Some(ss),
                )
            }));
            add_input(Box::new(move || {
                gen_rand(
                    s * 1024 * 1024,
                    format!("{} MB buffer of random split into {} B", s, ss),
                    Some(ss),
                )
            }));
            add_input(Box::new(move || {
                gen_text(
                    s * 1024 * 1024,
                    format!("{} MB buffer of text split into {} B", s, ss),
                    Some(ss),
                )
            }));
            add_input(Box::new(move || {
                gen_fill(
                    s * 1024 * 1024 - ss,
                    format!("{} MB - {}B buffer of 'a' split into {} B", s, ss, ss),
                    Some(ss),
                )
            }));
            add_input(Box::new(move || {
                gen_rand(
                    s * 1024 * 1024 - ss,
                    format!("{} MB - {}B buffer of random split into {} B", s, ss, ss),
                    Some(ss),
                )
            }));
            add_input(Box::new(move || {
                gen_text(
                    s * 1024 * 1024 - ss,
                    format!("{} MB - {}B buffer of random split into {} B", s, ss, ss),
                    Some(ss),
                )
            }));
        }
    }

    for s in [64 * 1024 + 5670usize, 16 * 1024 + 3421, 32 * 1024 - 321] {
        add_input(Box::new(move || gen_fill(s, format!("{} bytes buffer of 'a'", s), None)));
        add_input(Box::new(move || gen_rand(s, format!("{} bytes buffer of random", s), None)));
        add_input(Box::new(move || gen_text(s, format!("{} bytes buffer of text", s), None)));
    }

    let split_rcv = |snd: SndBuf, chunk: usize| -> RcvBuf {
        RcvBuf {
            size: snd.size,
            bufs: Bufs::Many(split_buffer(linearize(&snd.bufs, snd.size), chunk)),
            ..RcvBuf::default()
        }
    };
    let transforms: Vec<(Sstring, Box<dyn Fn(SndBuf) -> RcvBuf>)> = vec![
        (
            Sstring::from("identity"),
            Box::new(|snd: SndBuf| RcvBuf { size: snd.size, bufs: snd.bufs, ..RcvBuf::default() }),
        ),
        (
            Sstring::from("linearized"),
            Box::new(|snd: SndBuf| RcvBuf {
                size: snd.size,
                bufs: Bufs::One(linearize(&snd.bufs, snd.size)),
                ..RcvBuf::default()
            }),
        ),
        (Sstring::from("split 1 B"), Box::new(move |snd: SndBuf| split_rcv(snd, 1))),
        (Sstring::from("split 129 B"), Box::new(move |snd: SndBuf| split_rcv(snd, 129))),
        (Sstring::from("split 4 kB"), Box::new(move |snd: SndBuf| split_rcv(snd, 4096))),
        (
            Sstring::from("split 4 kB - 128"),
            Box::new(move |snd: SndBuf| split_rcv(snd, 4096 - 128)),
        ),
    ];

    for in_func in inputs {
        let in_ = in_func();
        eprintln!("Input: {}", in_.0);
        let mut headroom = in_.1;
        let mut compressed = compressor.compress(headroom, clone_snd(&in_.2))?;
        sanity_check(&compressed.bufs, compressed.size);

        // Remove headroom
        assert!(compressed.size as usize >= headroom);
        compressed.size -= headroom as u32;
        match &mut compressed.bufs {
            Bufs::One(buf) => {
                assert!(buf.size() >= headroom);
                buf.trim_front(headroom);
            }
            Bufs::Many(bufs) => {
                while headroom != 0 {
                    assert!(!bufs.is_empty());
                    let to_remove = bufs[0].size().min(headroom);
                    bufs[0].trim_front(to_remove);
                    if bufs[0].empty() && bufs.len() > 1 {
                        bufs.remove(0);
                    }
                    headroom -= to_remove;
                }
            }
        }

        let in_l = linearize(&in_.2.bufs, in_.2.size);

        for t in &transforms {
            eprintln!("  Transform: {}", t.0);
            let received = (t.1)(clone_snd(&compressed));

            let decompressed = compressor.decompress(received)?;
            sanity_check(&decompressed.bufs, decompressed.size);

            assert_eq!(decompressed.size, in_.2.size);

            let out_l = linearize(&decompressed.bufs, decompressed.size);

            assert_eq!(in_l.size(), out_l.size());
            assert!(in_l.as_slice() == out_l.as_slice());
        }
    }
    Ok(())
}

teseladb::engine_test_case!(test_lz4_compressor, {
    teseladb::engine::core::thread::async_(|| test_compressor(|| Box::new(Lz4Compressor)))
});

teseladb::engine_test_case!(test_lz4_fragmented_compressor, {
    teseladb::engine::core::thread::async_(|| test_compressor(|| Box::new(Lz4FragmentedCompressor)))
});

use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::future_util::sleep_abortable_with;
use teseladb::engine::core::lowres_clock::LowresClock;
use teseladb::engine::core::semaphore::Semaphore;
use teseladb::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use teseladb::engine::rpc::rpc::{RpcDurationNs, RpcTimePoint};
use teseladb::engine::rpc::rpc_types::{ClientInfo, ConnectionId, Tuple};

/// Mutable access through a lightweight shared pointer.
// A mutable reference from a shared handle.
#[allow(clippy::mut_from_ref)]
fn lw_mut<T>(p: &LwSharedPtr<T>) -> &mut T {
    // SAFETY: one shard, and no other reference to the object is alive across this call.
    unsafe { p.get_mut_unchecked() }
}

/// The watchdog of the max-timeout tests: `done` is signalled after 3 s unless aborted.
fn max_timeout_test<F>(body: F) -> Future<()>
where
    F: FnOnce(&mut RpcTestEnv, &mut Client) -> Result<(), ExceptionPtr> + 'static,
{
    // The typical failure of this test is a hang. So we use semaphore to
    // stop the test either when it succeeds, or after a long enough hang.
    let success = make_lw_shared(false);
    let done = make_lw_shared(Semaphore::new(0));
    let abrt = make_lw_shared(AbortSource::new());
    let (d1, s1) = (done.clone(), success.clone());
    // SAFETY: the abort source lives in its shared pointer, which the continuations keep.
    let as_pin = unsafe { Pin::new_unchecked(lw_mut(&abrt)) };
    let _ = sleep_abortable_with::<LowresClock>(Duration::from_secs(3), as_pin)
        .then(move |()| {
            let _ = &s1;
            lw_mut(&d1).signal(1);
        })
        .handle_exception(|_: ExceptionPtr| {});
    let co = ClientOptions { send_timeout_data: true, ..ClientOptions::default() };
    let (s2, d2, a2) = (success.clone(), done.clone(), abrt.clone());
    let _ = RpcTestEnv::<u32>::do_with_thread_client_opts(RpcTestConfig::default(), co, body).then(
        move |()| {
            *lw_mut(&s2) = true;
            // SAFETY: as above.
            unsafe { Pin::new_unchecked(lw_mut(&a2)) }.request_abort();
            lw_mut(&d2).signal(1);
        },
    );
    let d3 = done.clone();
    lw_mut(&done).wait(1).then(move |()| {
        // The continuation owns the copy until it runs (`let _ = x` would not capture it).
        drop(d3);
        assert!(*success);
    })
}

// Test reproducing a bug: If timeout is time_point::max(), converting
// it to relative timeout in the sender and then back in the receiver, when
// these calculations happen across a millisecond boundary, overflowed the
// integer and mislead the receiver to think the requested timeout was
// negative, and cause it drop its response, so the RPC call never completed.
teseladb::engine_test_case!(test_max_absolute_timeout, {
    max_timeout_test(|env, c1| {
        env.register_handler(1, |a: i32, b: i32| make_ready_future::<i32>(a + b)).get()?;
        let sum = env.proto().make_client::<fn(i32, i32) -> Future<i32>>(1);
        // The bug only reproduces if the calculation done on the sender
        // and receiver sides, happened across a millisecond boundary.
        // We can't control when it happens, so we just need to loop many
        // times, at least many milliseconds, to increase the probability
        // that we catch the bug. Experimentally, if we loop for 200ms, we
        // catch the bug virtually every time.
        let until = LowresClock::now() + Duration::from_millis(200);
        while LowresClock::now() <= until {
            let result = sum.call_until(c1, RpcTimePoint::max(), &(2, 3)).get()?;
            assert_eq!(result, 2 + 3);
        }
        Ok(())
    })
});

// Similar to the above test: Test that a relative timeout duration::max()
// also works, and again doesn't cause the timeout wrapping around to the
// past and causing dropped responses.
teseladb::engine_test_case!(test_max_relative_timeout, {
    max_timeout_test(|env, c1| {
        env.register_handler(1, |a: i32, b: i32| make_ready_future::<i32>(a + b)).get()?;
        let sum = env.proto().make_client::<fn(i32, i32) -> Future<i32>>(1);
        // The following call used to always hang, when max()+now()
        // overflowed and appeared to be a negative timeout.
        let result = sum.call_for(c1, RpcDurationNs::MAX, &(2, 3)).get()?;
        assert_eq!(result, 2 + 3);
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_tuple, {
    RpcTestEnv::<u32>::do_with_thread_client(RpcTestConfig::default(), |env, c1| {
        env.register_handler(1, || {
            make_ready_future::<Tuple<(i32, i64)>>(Tuple((1, 0x7_0000_0000i64)))
        })
        .get()?;
        let f1 = env.proto().make_client::<fn() -> Future<Tuple<(i32, i64)>>>(1);
        let result = f1.call(c1, &()).get()?;
        assert_eq!(result.0.0, 1);
        assert_eq!(result.0.1, 0x7_0000_0000i64);
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_nonvariadic_client_variadic_server, {
    RpcTestEnv::<u32>::do_with_thread_client(RpcTestConfig::default(), |env, c1| {
        // Server is variadic
        env.register_handler(1, || {
            make_ready_future::<Tuple<(i32, i64)>>(Tuple((1, 0x7_0000_0000i64)))
        })
        .get()?;
        // Client is non-variadic
        let f1 = env.proto().make_client::<fn() -> Future<Tuple<(i32, i64)>>>(1);
        let result = f1.call(c1, &()).get()?;
        assert_eq!(result.0.0, 1);
        assert_eq!(result.0.1, 0x7_0000_0000i64);
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_variadic_client_nonvariadic_server, {
    RpcTestEnv::<u32>::do_with_thread_client(RpcTestConfig::default(), |env, c1| {
        // Server is nonvariadic
        env.register_handler(1, || {
            make_ready_future::<Tuple<(i32, i64)>>(Tuple((1, 0x7_0000_0000i64)))
        })
        .get()?;
        // Client is variadic
        let f1 = env.proto().make_client::<fn() -> Future<Tuple<(i32, i64)>>>(1);
        let result = f1.call(c1, &()).get()?;
        assert_eq!(result.0.0, 1);
        assert_eq!(result.0.1, 0x7_0000_0000i64);
        Ok(())
    })
});

teseladb::engine_test_case!(test_handler_registration, {
    let ecfg = ErrorInjectorConfig {
        connect_kind: LoopbackError::Abort,
        ..ErrorInjectorConfig::default()
    };
    let cfg = RpcTestConfig { inject_error: Some(ecfg), ..Default::default() };
    RpcTestEnv::<u32>::do_with_thread(cfg, |env| {
        let mut proto = env.proto();

        // new proto must be empty
        assert!(!proto.has_handlers());

        // non-existing handler should not be found
        assert!(!proto.has_handler(1));

        // unregistered non-existing handler should return ready future
        let fut = proto.as_mut().unregister_handler(1);
        assert!(fut.available() && !fut.failed());
        fut.get()?;

        // existing handler should be found
        let handler = || make_ready_future(());
        proto.as_mut().register_handler(1, handler)?;
        assert!(proto.has_handler(1));

        // cannot register handler if already registered
        let r = proto.as_mut().register_handler(1, handler).map(|_| ());
        assert!(r.is_err_and(|e| e.is::<RuntimeError>()));

        // unregistered handler should not be found
        proto.as_mut().unregister_handler(1).get()?;
        assert!(!proto.has_handler(1));

        // re-registering a handler should succeed
        proto.as_mut().register_handler(1, handler)?;
        assert!(proto.has_handler(1));

        // proto with handlers must not be empty
        assert!(proto.has_handlers());
        Ok(())
    })
});

teseladb::engine_test_case!(test_unregister_handler, {
    RpcTestEnv::<u32>::do_with_thread_client(RpcTestConfig::default(), |env, c1| {
        let handler_called = Rc::new(RefCell::new(Promise::<()>::new()));
        // `None` is the future after `get()` moved its state out, which is not available.
        let mut f_handler_called: Option<Future<()>> =
            Some(handler_called.borrow_mut().get_future());
        let rpc_executed = Rc::new(Cell::new(false));
        let rpc_completed = Rc::new(Cell::new(false));

        let fhc: *mut Option<Future<()>> = &mut f_handler_called;
        let (re0, rc0) = (rpc_executed.clone(), rpc_completed.clone());
        let reset_state = move || -> Result<(), ExceptionPtr> {
            // SAFETY: the future lives in this thread's frame.
            let f = unsafe { &mut *fhc };
            if f.as_ref().is_some_and(|f| f.available())
                && let Some(f) = f.take()
            {
                f.get()?;
            }
            re0.set(false);
            rc0.set(false);
            Ok(())
        };

        let (hc1, re1, rc1) = (handler_called.clone(), rpc_executed.clone(), rpc_completed.clone());
        let get_handler = move || {
            let (hc, re, rc) = (hc1.clone(), re1.clone(), rc1.clone());
            move || {
                hc.borrow_mut().set_value(());
                re.set(true);
                let rc = rc.clone();
                sleep(Duration::from_millis(1)).then(move |()| {
                    rc.set(true);
                })
            }
        };

        // handler should not run if unregistered before being called
        env.register_handler(1, get_handler()).get()?;
        env.unregister_handler(1).get()?;
        assert!(!f_handler_called.as_ref().is_some_and(|f| f.available()));
        assert!(!rpc_executed.get());
        assert!(!rpc_completed.get());

        // verify normal execution path
        env.register_handler(1, get_handler()).get()?;
        let call = env.proto().make_client::<fn() -> Future<()>>(1);
        call.call(c1, &()).get()?;
        assert!(f_handler_called.as_ref().is_some_and(|f| f.available()));
        assert!(rpc_executed.get());
        assert!(rpc_completed.get());
        reset_state()?;

        // call should fail after handler is unregistered
        env.unregister_handler(1).get()?;
        match call.call(c1, &()).get() {
            Ok(()) => panic!("call should have failed"),
            Err(e)
                if matches!(
                    e.downcast_ref::<rpc_types::Error>().map(|e| e.kind()),
                    Some(rpc_types::ErrorKind::UnknownVerb(_))
                ) =>
            {
                // expected
            }
            Err(e) => {
                eprintln!("call failed in an unexpected way: {}", exc_what(&e));
                panic!("call failed in an unexpected way");
            }
        }
        assert!(!f_handler_called.as_ref().is_some_and(|f| f.available()));
        assert!(!rpc_executed.get());
        assert!(!rpc_completed.get());

        // unregistered is allowed while call is in flight
        let envp: *mut RpcTestEnv = env;
        let delayed_unregister = move || {
            // SAFETY: the environment outlives the thread.
            sleep(Duration::from_micros(500))
                .then(move |()| unsafe { (*envp).unregister_handler(1) })
        };

        env.register_handler(1, get_handler()).get()?;
        match when_all_succeed((call.call(c1, &()), delayed_unregister())).get() {
            Ok(_) => reset_state()?,
            Err(e)
                if matches!(
                    e.downcast_ref::<rpc_types::Error>().map(|e| e.kind()),
                    Some(rpc_types::ErrorKind::UnknownVerb(_))
                ) =>
            {
                // expected
            }
            Err(e) => {
                eprintln!("call failed in an unexpected way: {}", exc_what(&e));
                panic!("call failed in an unexpected way");
            }
        }

        // verify that unregister_handler waits for pending requests to finish
        {
            let handler_reached_promise = Rc::new(RefCell::new(Promise::<()>::new()));
            let handler_go_promise = Rc::new(RefCell::new(Promise::<()>::new()));
            let value_to_return = Rc::new(RefCell::new(Sstring::from("before_unregister")));
            let (hr, hg, v) = (
                handler_reached_promise.clone(),
                handler_go_promise.clone(),
                value_to_return.clone(),
            );
            env.register_handler(1, move || -> Future<Sstring> {
                hr.borrow_mut().set_value(());
                let v = v.clone();
                let f = hg.borrow_mut().get_future();
                f.then(move |()| make_ready_future(v.borrow().clone()))
            })
            .get()?;
            let f = env.proto().make_client::<fn() -> Future<Sstring>>(1);
            let response_future = f.call(c1, &());
            let reached = handler_reached_promise.borrow_mut().get_future();
            reached.get()?;
            let v2 = value_to_return.clone();
            let unregister_future = env.unregister_handler(1).then(move |()| {
                *v2.borrow_mut() = Sstring::from("after_unregister");
            });
            assert!(!unregister_future.available());
            sleep(Duration::from_millis(1)).get()?;
            assert!(!unregister_future.available());
            handler_go_promise.borrow_mut().set_value(());
            unregister_future.get()?;
            assert_eq!(response_future.get()?, "before_unregister");
        }
        Ok(())
    })
});

static DUMMY_LOG: std::sync::LazyLock<Pin<Box<teseladb::engine::util::log::Logger>>> =
    std::sync::LazyLock::new(|| {
        teseladb::engine::util::log::Logger::new("dummy")
            .unwrap_or_else(|e| panic!("{}", exc_what(&e)))
    });

teseladb::engine_test_case!(test_loggers, {
    DUMMY_LOG.set_level(teseladb::engine::util::log::LogLevel::Debug);
    RpcTestEnv::<u32>::do_with_thread_client(RpcTestConfig::default(), |env, _c1| {
        let dummy_addr = SocketAddress::default();
        let mut proto = env.proto();
        let logger: *const teseladb::engine::rpc::rpc::Logger = proto.get_logger();
        // SAFETY: the protocol owns its logger and outlives this block; set_logger()
        // changes it in place, which this block then sees.
        let logger = || unsafe { &*logger };
        logger().addr(&dummy_addr, &Sstring::from("Hello0"));
        logger().addr_level(&dummy_addr, teseladb::engine::util::log::LogLevel::Debug, "Hello1");
        proto.as_mut().set_logger(Some(&**DUMMY_LOG));
        logger().addr(&dummy_addr, &Sstring::from("Hello2"));
        logger().addr_level(&dummy_addr, teseladb::engine::util::log::LogLevel::Debug, "Hello3");
        proto.as_mut().set_logger(None);
        logger().addr(&dummy_addr, &Sstring::from("Hello6"));
        logger().addr_level(&dummy_addr, teseladb::engine::util::log::LogLevel::Debug, "Hello7");
        Ok(())
    })
});

teseladb::engine_test_case!(test_connection_id_format, {
    let cid = ConnectionId::make_id(0x123, 1);
    let res = format!("{}", cid);
    assert_eq!(res, "1230001");
    make_ready_future(())
});

// An RPC tuple of a `u32` and an `i64` has the type `Tuple<(u32, i64)>`.
const _: fn() -> Tuple<(u32, i64)> = || Tuple((1u32, 1i64));

teseladb::engine_test_case!(test_client_info, {
    RpcTestEnv::<u32>::do_with(RpcTestConfig::default(), |env| {
        let mut info = ClientInfo {
            addr: SocketAddress::default(),
            server: env.server(),
            conn_id: ConnectionId::make_id(0, 0),
            user_data: Default::default(),
        };
        let key = Sstring::from("key");
        let missing = Sstring::from("missing");

        info.attach_auxiliary(&key, 0i32);
        assert_eq!(info.retrieve_auxiliary_opt::<i32>(&key).copied(), Some(0));
        *info.retrieve_auxiliary::<i32>(&key).unwrap_or_else(|e| panic!("{}", exc_what(&e))) = 1;
        assert_eq!(info.retrieve_auxiliary_opt::<i32>(&key).copied(), Some(1));

        let p1 = info.retrieve_auxiliary_opt_mut::<i32>(&key).map(|r| r as *const i32);
        let p2 = info.retrieve_auxiliary::<i32>(&key).ok().map(|r| r as *const i32);
        assert_eq!(p1, p2);
        let const_info: &ClientInfo = &info;
        let p3 = const_info.retrieve_auxiliary_opt::<i32>(&key).map(|r| r as *const i32);
        assert_eq!(p3, p2);

        assert!(info.retrieve_auxiliary_opt_mut::<i32>(&missing).is_none());
        let const_info: &ClientInfo = &info;
        assert!(const_info.retrieve_auxiliary_opt::<i32>(&missing).is_none());

        make_ready_future(())
    })
});

fn send_messages_and_check_timeouts(
    env: &mut RpcTestEnv,
    cln: &mut Client,
) -> Result<(), ExceptionPtr> {
    env.register_handler(1, |v: i32| {
        sleep(Duration::from_secs(v as u64)).then(move |()| make_ready_future::<i32>(v))
    })
    .get()?;

    let call = env.proto().make_client::<fn(i32) -> Future<i32>>(1);
    let start = std::time::Instant::now();
    let f1 = call
        .call_for(cln, Duration::from_millis(400).as_nanos() as RpcDurationNs, &(3,))
        .finally(move || {
            let end = std::time::Instant::now();
            assert!(end - start < Duration::from_secs(1));
        });
    let f2 = call
        .call_for(cln, Duration::from_millis(600).as_nanos() as RpcDurationNs, &(3,))
        .finally(move || {
            let end = std::time::Instant::now();
            assert!(end - start < Duration::from_secs(1));
        });
    assert!(f1.get().is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Timeout)));
    assert!(f2.get().is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Timeout)));
    Ok(())
}

teseladb::engine_test_case!(test_rpc_send_timeout, {
    let cfg = RpcTestConfig::default();
    RpcTestEnv::<u32>::do_with_thread_client(cfg, send_messages_and_check_timeouts)
});

teseladb::engine_test_case!(test_rpc_send_timeout_on_connect, {
    let ecfg = ErrorInjectorConfig {
        connect_delay: Duration::from_secs(5),
        ..ErrorInjectorConfig::default()
    };
    let cfg = RpcTestConfig { inject_error: Some(ecfg), ..Default::default() };
    RpcTestEnv::<u32>::do_with_thread_client(cfg, send_messages_and_check_timeouts)
});

teseladb::engine_test_case!(test_rpc_abort_connection, {
    RpcTestEnv::<u32>::do_with_thread(RpcTestConfig::default(), |env| {
        let mut c1 = env_client(env)?;
        let arrived = Rc::new(Cell::new(0i32));
        let a = arrived.clone();
        // `rpc::client_info&`: the handler form that takes the client info (spelled out, as
        // a closure over a reference also fits the general form).
        env.register_handler::<_, rpc_impl::HandlerSignature<
            rpc_impl::DoWantClientInfoMut,
            rpc_impl::DontWantTimePoint,
            Result<i32, ExceptionPtr>,
            (i32,),
        >>(1, move |cinfo: &mut ClientInfo, x: i32| -> Result<i32, ExceptionPtr> {
            let n = a.get();
            a.set(n + 1);
            assert_eq!(x, n);
            if a.get() == 2 {
                // SAFETY: the server outlives the requests it serves.
                unsafe { (*cinfo.server).abort_connection(cinfo.conn_id) };
            }
            // The third message won't arrive because we abort the connection.

            Ok(0)
        })
        .get()?;
        let f = env.proto().make_client::<fn(i32) -> Future<i32>>(1);
        assert_eq!(f.call(client_mut(&mut c1), &(0,)).get()?, 0);
        assert!(
            f.call(client_mut(&mut c1), &(1,))
                .get()
                .is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Closed))
        );
        assert!(
            f.call(client_mut(&mut c1), &(2,))
                .get()
                .is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Closed))
        );
        assert_eq!(arrived.get(), 2);
        client_mut(&mut c1).stop().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_rpc_metric_domains, {
    teseladb::engine::core::thread::async_(|| {
        let do_one_echo =
            |env: &mut RpcTestEnv, cln: &mut Client, nr_calls: i32| -> Result<(), ExceptionPtr> {
                env.register_handler(1, |v: i32| make_ready_future::<i32>(v)).get()?;
                let call = env.proto().make_client::<fn(i32) -> Future<i32>>(1);
                for i in 0..nr_calls {
                    call.call(cln, &(i,)).get()?;
                }
                Ok(())
            };

        let mut client_opt =
            ClientOptions { metrics_domain: Sstring::from("dom1"), ..ClientOptions::default() };
        let p1 = RpcTestEnv::<u32>::do_with_thread_client_opts(
            RpcTestConfig::default(),
            client_opt.clone(),
            move |env, cln| do_one_echo(env, cln, 3),
        );
        client_opt.metrics_domain = Sstring::from("dom2");
        let p2 = RpcTestEnv::<u32>::do_with_thread_client_opts(
            RpcTestConfig::default(),
            client_opt.clone(),
            move |env, cln| do_one_echo(env, cln, 2),
        );
        let p3 = RpcTestEnv::<u32>::do_with_thread_client_opts(
            RpcTestConfig::default(),
            client_opt.clone(),
            move |env, cln| do_one_echo(env, cln, 5),
        );
        when_all((p1, p2, p3)).discard_result().get()?;

        let get_metrics = |name: &str, domain: &str| -> i32 {
            let registry = teseladb::engine::core::metrics::impl_fns::get_value_map();
            // SAFETY: this shard's registry, with no other reference alive across the lookup.
            let values = unsafe { registry.get_mut_unchecked() }.get_value_map();
            let mf = values.get(&Sstring::from(name));
            assert!(mf.is_some());
            if let Some(mf) = mf {
                for (labels, mi) in mf.iter() {
                    for (k, v) in labels.labels() {
                        if *k == "domain"
                            && *v == domain
                            && let Some(m) = mi
                        {
                            return (m.get_function())()
                                .i()
                                .unwrap_or_else(|e| panic!("{}", exc_what(&e)))
                                as i32;
                        }
                    }
                }
            }
            panic!("cannot find requested metrics");
        };

        // Negotiation messages also count, so +1 for default domain and +2 for "dom" one
        assert_eq!(get_metrics("rpc_client_sent_messages", "dom1"), 4);
        assert_eq!(get_metrics("rpc_client_sent_messages", "dom2"), 9);
        Ok(())
    })
});

// Extract a piece of contiguous data from the front of the buffer (and trim the extracted front away).
fn read_from_rcv_buf<const N: usize>(data: &mut RcvBuf) -> Result<[u8; N], ExceptionPtr> {
    if (data.size as usize) < N {
        return Err(ExceptionPtr::new(teseladb::engine::core::future::RuntimeError::new(
            "Truncated compressed RPC frame".to_string(),
        )));
    }
    let bufs: &mut [TemporaryBuffer<u8>] = match &mut data.bufs {
        Bufs::One(b) => std::slice::from_mut(b),
        Bufs::Many(v) => v.as_mut_slice(),
    };
    let mut out = [0u8; N];
    let mut done = 0;
    let mut it = 0;
    while done < N {
        let n = (N - done).min(bufs[it].size());
        out[done..done + n].copy_from_slice(&bufs[it].as_slice()[..n]);
        done += n;
        bufs[it].trim_front(n);
        it += 1;
        data.size -= n as u32;
    }
    Ok(out)
}

static COMPRESSOR_NAME: std::sync::LazyLock<Sstring> =
    std::sync::LazyLock::new(|| Sstring::from("TEST"));

/// Points to the compressor of a connection, so the test can make it send metadata.
struct Tracker {
    _compressor: Cell<*mut TrackerCompressor>,
}

type Metadata = u64;

/// A compressor that sends metadata to its peer.
///
/// When `send_metadata(x)` is called, this compressor sends a piece of metadata (x) to its peer,
/// by prepending it to an empty compressed frame.
/// It can be read from the peer with `receive_metadata()`.
struct TrackerCompressor {
    _tracker: &'static Tracker,
    _delegate: Box<dyn Compressor>,
    _send_queue: std::collections::VecDeque<Metadata>,
    _recv_queue: std::collections::VecDeque<Metadata>,
    _metadata_received: Semaphore,
    _send_empty_frame: rpc_types::SendEmptyFrame,
    _needs_progress: teseladb::engine::core::condition_variable::ConditionVariable,
    _progress_fiber: Option<Future<()>>,
}

impl TrackerCompressor {
    /// The constructor; boxed first, as the progress fiber and the tracker point at it.
    fn new(tracker: &'static Tracker, send_empty_frame: rpc_types::SendEmptyFrame) -> Box<Self> {
        let mut c = Box::new(TrackerCompressor {
            _tracker: tracker,
            _delegate: Box::new(Lz4FragmentedCompressor),
            _send_queue: Default::default(),
            _recv_queue: Default::default(),
            _metadata_received: Semaphore::new(0),
            _send_empty_frame: send_empty_frame,
            _needs_progress: Default::default(),
            _progress_fiber: None,
        });
        let this: *mut TrackerCompressor = &mut *c;
        c._progress_fiber = Some(Self::start_progress_fiber(this));
        tracker._compressor.set(this);
        c
    }

    fn start_progress_fiber(this: *mut TrackerCompressor) -> Future<()> {
        teseladb::engine::core::coroutine::make_coroutine(async move {
            loop {
                // The compressor is boxed: its condition variable does not move.
                // SAFETY: the compressor is boxed and closes the fiber before it
                // is destroyed.
                unsafe {
                    Pin::new_unchecked(&(*this)._needs_progress)
                        .when_pred(move || !(*this)._send_queue.is_empty())
                }
                .await?;
                // SAFETY: the compressor is boxed and closes the fiber before it is destroyed.
                unsafe { ((*this)._send_empty_frame)() }.await?;
            }
        })
    }

    fn send_metadata(&mut self, x: Metadata) {
        self._send_queue.push_back(x);
        self._needs_progress.signal();
    }

    fn receive_metadata(&mut self) -> Future<Metadata> {
        let this: *mut TrackerCompressor = self;
        teseladb::engine::core::coroutine::make_coroutine(async move {
            // SAFETY: the compressor outlives the test's wait.
            unsafe { (*this)._metadata_received.wait(1) }.await?;
            // SAFETY: the compressor outlives the test's wait.
            let x = unsafe { (*this)._recv_queue.pop_front() };
            Ok(x.unwrap_or_default())
        })
    }
}

/// Writes `bytes` (already big-endian) at `offset` of `dst`.
fn write_be(dst: &mut TemporaryBuffer<u8>, offset: usize, bytes: &[u8]) {
    // SAFETY: the delegate left `offset + bytes.len()` bytes of head space in the first buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.get_write().add(offset), bytes.len())
    };
}

impl Compressor for TrackerCompressor {
    fn close(&mut self) -> Future<()> {
        self._needs_progress.broken();
        self._progress_fiber
            .take()
            .unwrap_or_else(|| make_ready_future(()))
            .handle_exception(|_: ExceptionPtr| {})
    }

    fn compress(&mut self, head_space: usize, data: SndBuf) -> Result<SndBuf, ExceptionPtr> {
        let data =
            if data.size != 0 { data } else { SndBuf::from_buffer(TemporaryBuffer::default()) };
        if let Some(&front) = self._send_queue.front() {
            let mut x =
                self._delegate.compress(head_space + 1 + std::mem::size_of::<Metadata>(), data)?;
            write_be(x.front(), head_space, &1u8.to_be_bytes());
            write_be(x.front(), head_space + 1, &front.to_be_bytes());
            self._send_queue.pop_front();
            Ok(x)
        } else {
            let mut x = self._delegate.compress(head_space + 1, data)?;
            write_be(x.front(), head_space, &0u8.to_be_bytes());
            Ok(x)
        }
    }
    fn decompress(&mut self, mut data: RcvBuf) -> Result<RcvBuf, ExceptionPtr> {
        if u8::from_be_bytes(read_from_rcv_buf::<1>(&mut data)?) != 0 {
            self._recv_queue.push_back(u64::from_be_bytes(read_from_rcv_buf::<8>(&mut data)?));
            self._metadata_received.signal(1);
        }
        self._delegate.decompress(data)
    }
    fn name(&self) -> Sstring {
        COMPRESSOR_NAME.clone()
    }
}

impl Drop for TrackerCompressor {
    fn drop(&mut self) {
        self._tracker._compressor.set(std::ptr::null_mut());
    }
}

/// The factory of [`TrackerCompressor`]s.
struct TrackerFactory {
    _tracker: &'static Tracker,
}

impl CompressorFactory for TrackerFactory {
    fn supported(&self) -> &Sstring {
        &COMPRESSOR_NAME
    }
    fn negotiate_with(
        &self,
        feature: Sstring,
        _is_server: bool,
        send_empty_frame: rpc_types::SendEmptyFrame,
    ) -> Option<Box<dyn Compressor>> {
        if feature == *self.supported() {
            return Some(TrackerCompressor::new(self._tracker, send_empty_frame));
        }
        None
    }
    fn negotiate(&self, _feature: Sstring, _is_server: bool) -> Option<Box<dyn Compressor>> {
        std::process::abort();
    }
}

// Test the use of empty compressed frames as a method of communication between compressors.
teseladb::engine_test_case!(test_compressor_empty_frames, {
    teseladb::engine::core::thread::async_(|| {
        // The options take `'static` factories: the trackers and factories are leaked.
        let server_tracker: &'static Tracker =
            Box::leak(Box::new(Tracker { _compressor: Cell::new(std::ptr::null_mut()) }));
        let client_tracker: &'static Tracker =
            Box::leak(Box::new(Tracker { _compressor: Cell::new(std::ptr::null_mut()) }));
        let server_factory: &'static TrackerFactory =
            Box::leak(Box::new(TrackerFactory { _tracker: server_tracker }));
        let client_factory: &'static TrackerFactory =
            Box::leak(Box::new(TrackerFactory { _tracker: client_tracker }));
        let so = ServerOptions { compressor_factory: Some(server_factory), ..Default::default() };
        let co = ClientOptions { compressor_factory: Some(client_factory), ..Default::default() };
        let cfg = RpcTestConfig { server_options: so, ..Default::default() };

        RpcTestEnv::<u32>::do_with_thread_client_opts(cfg, co, move |env, c| {
            // Perform an RPC once to initialize the connection and compressors.
            env.register_handler(1, || Ok::<i32, ExceptionPtr>(42)).get()?;
            let proto_client = env.proto().make_client::<fn() -> Future<i32>>(1);
            assert_eq!(proto_client.call(c, &()).get()?, 42);
            assert!(!client_tracker._compressor.get().is_null());
            assert!(!server_tracker._compressor.get().is_null());
            // Check that both compressors can send metadata to each other.
            for i in 0..3u64 {
                // SAFETY: each tracker points at its live compressor.
                unsafe { (*client_tracker._compressor.get()).send_metadata(2 * i) };
                assert_eq!(
                    // SAFETY: each tracker points at its live compressor.
                    unsafe { (*server_tracker._compressor.get()).receive_metadata() }.get()?,
                    2 * i
                );
                // SAFETY: each tracker points at its live compressor.
                unsafe { (*server_tracker._compressor.get()).send_metadata(2 * i + 1) };
                assert_eq!(
                    // SAFETY: each tracker points at its live compressor.
                    unsafe { (*client_tracker._compressor.get()).receive_metadata() }.get()?,
                    2 * i + 1
                );
            }
            Ok(())
        })
        .get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_timeout_cancel, {
    let co = ClientOptions { send_timeout_data: true, ..ClientOptions::default() };
    RpcTestEnv::<u32>::do_with_thread_client_opts(RpcTestConfig::default(), co, |env, cl| {
        let mut abort_handler = Box::pin(AbortSource::new());
        // SAFETY: the source is pinned in its box and outlives the handler.
        let ahp =
            AssertSend(unsafe { abort_handler.as_mut().get_unchecked_mut() } as *mut AbortSource);
        let id: u32 = 1;
        let mut sent = 0i32;
        let received = Rc::new(Cell::new(0i32));
        let cond = Rc::new(RefCell::new(
            teseladb::engine::core::condition_variable::ConditionVariable::default(),
        ));
        let (r, cv) = (received.clone(), cond.clone());
        env.register_handler(id, move |x: i32| -> Future<i32> {
            eprintln!("received value={}", x);
            r.set(x);
            cv.borrow_mut().signal();
            let ahp = ahp.clone();
            teseladb::engine::core::coroutine::make_coroutine(async move {
                sleep_abortable_with::<teseladb::engine::core::timer::SteadyClockType>(
                    Duration::from_secs(10),
                    // SAFETY: see above.
                    unsafe { Pin::new_unchecked(&mut *ahp.0) },
                )
                .await?;
                Ok(x)
            })
        })
        .get()?;
        let echo = env.proto().make_client::<fn(i32) -> Future<i32>>(1);
        let wait_received = |sent: i32| -> Result<(), ExceptionPtr> {
            while received.get() != sent {
                let c = cond.borrow();
                // SAFETY: the condition variable lives in its `Rc` and does not move.
                let f = unsafe { Pin::new_unchecked(&*c) }.wait_for(Duration::from_millis(10));
                drop(c);
                f.get()?;
            }
            Ok(())
        };
        let ten_ms = Duration::from_millis(10).as_nanos() as RpcDurationNs;
        {
            // Relative timeout
            sent += 1;
            let f = echo.call_for(cl, ten_ms, &(sent,));
            assert!(f.get().is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Timeout)));
            wait_received(sent)?;
        }
        {
            // Absolute timeout
            sent += 1;
            let f = echo.call_until(cl, LowresClock::now() + Duration::from_millis(10), &(sent,));
            assert!(f.get().is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Timeout)));
            wait_received(sent)?;
        }
        {
            // Synchronous cancel before relative timeout
            let mut cancel_rpc = std::pin::pin!(Cancellable::default());
            sent += 1;
            let f = echo.call_for_cancellable(cl, ten_ms, cancel_rpc.as_mut(), &(sent,));
            assert!(!f.available());
            cancel_rpc.as_mut().cancel();
            assert!(f.get().is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Canceled)));
            wait_received(sent)?;
        }
        {
            // Synchronous cancel before absolute timeout
            let mut cancel_rpc = std::pin::pin!(Cancellable::default());
            sent += 1;
            let f = echo.call_until_cancellable(
                cl,
                LowresClock::now() + Duration::from_millis(10),
                cancel_rpc.as_mut(),
                &(sent,),
            );
            assert!(!f.available());
            cancel_rpc.as_mut().cancel();
            assert!(f.get().is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Canceled)));
            wait_received(sent)?;
        }
        {
            // Cancel before timeout while rpc is in flight
            let mut cancel_rpc = std::pin::pin!(Cancellable::default());
            // The value is sent as is, not incremented.
            let f = echo.call_until_cancellable(
                cl,
                LowresClock::now() + Duration::from_millis(10),
                cancel_rpc.as_mut(),
                &(sent,),
            );
            assert!(!f.available());
            // Wait until the rpc is received, then cancel
            wait_received(sent)?;
            cancel_rpc.as_mut().cancel();
            assert!(f.get().is_err_and(|e| is_rpc_error(&e, rpc_types::ErrorKind::Canceled)));
        }
        abort_handler.as_mut().request_abort();
        env.unregister_handler(id).get()?;
        Ok(())
    })
});
