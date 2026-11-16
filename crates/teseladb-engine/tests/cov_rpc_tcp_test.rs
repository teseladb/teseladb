#![cfg(target_os = "linux")]
//! Tests of RPC servers and clients over real TCP connections on the loopback interface:
//! every way a protocol makes servers and clients, the handler forms with client info and
//! deadlines, optional arguments, no-wait verbs, values that fail to serialize or to
//! deserialize, reply handlers driven by hand, the client gauges, compressors that fail,
//! connection filters, keepalive, streaming domains, server connections and their limits,
//! and cancellation and handler removal while calls are in flight.
//!
//! The reactor runs two shards (set before `main`); the servers listen on shard 0 only, so
//! their listening sockets send every connection to shard 0.

// The tests reach pinned engine objects through raw pointers; every `unsafe` block states
// why it is sound.
#![allow(unsafe_code)]
#![allow(clippy::type_complexity)]

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use teseladb::engine::core::future::{
    ExceptionPtr, Future, Promise, RuntimeError, make_ready_future,
};
use teseladb::engine::core::lowres_clock::LowresClock;
use teseladb::engine::core::reactor::{listen_with, make_socket};
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::when_all::when_all_succeed;
use teseladb::engine::net::api::{
    ListenOptions, LoadBalancingAlgorithm, ServerSocket, TcpKeepaliveParams,
    make_ipv4_address_ip_port,
};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::rpc::rpc::{
    Client, ClientOptions, Protocol, ReplyHandlerBase, ReplyHandlerData, ResourceLimits,
    Server, ServerOptions, StreamingDomainType,
};
use teseladb::engine::rpc::rpc_impl::{
    DoWantClientInfo, DoWantClientInfoMut, DoWantTimePoint, DontWantClientInfo,
    DontWantTimePoint, HandlerSignature, REQUEST_FRAME_HEADROOM, Read, SerializerInput,
    SerializerOutput, Write, marshall,
};
use teseladb::engine::rpc::rpc_types::{
    Bufs, Cancellable, ClientInfo, Compressor, CompressorFactory, ConnectionId, ErrorKind,
    NoWaitType, OptTimePoint, Optional, RcvBuf, SndBuf, Type,
};
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine::util::log::{LogLevel, Logger, Ostream};

/// Starts the test reactor with two shards before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov_rpc_tcp_test".to_string(), "--smp=2".to_string()];
        global_test_runner().start(&args);
    }
    start
};

// ---- serializer ----

#[derive(Clone, Copy, Default, Debug)]
struct Serializer;

macro_rules! arithmetic_serializer {
    ($($t:ty),*) => {$(
        impl Write<$t> for Serializer {
            fn write<O: SerializerOutput>(&self, out: &mut O, v: &$t) -> Result<(), ExceptionPtr> {
                out.write(&v.to_ne_bytes())
            }
        }
        impl Read<$t> for Serializer {
            fn read<I: SerializerInput>(&self, in_: &mut I, _: Type<$t>) -> Result<$t, ExceptionPtr> {
                let mut b = [0u8; std::mem::size_of::<$t>()];
                in_.read(&mut b)?;
                Ok(<$t>::from_ne_bytes(b))
            }
        }
    )*};
}

arithmetic_serializer!(i32, u32, i64, u64);

impl Write<Sstring> for Serializer {
    fn write<O: SerializerOutput>(&self, out: &mut O, v: &Sstring) -> Result<(), ExceptionPtr> {
        out.write(&(v.size() as u32).to_ne_bytes())?;
        out.write(v.as_bytes())
    }
}

impl Read<Sstring> for Serializer {
    fn read<I: SerializerInput>(&self, in_: &mut I, _: Type<Sstring>) -> Result<Sstring, ExceptionPtr> {
        let mut l = [0u8; 4];
        in_.read(&mut l)?;
        let mut s = uninitialized_string(u32::from_ne_bytes(l) as usize);
        in_.read(s.as_bytes_mut())?;
        Ok(s)
    }
}

/// A value the serializer refuses to write.
#[derive(Clone, Copy, Debug)]
struct Unserializable;

teseladb::rpc_plain_type!(Unserializable);

impl Write<Unserializable> for Serializer {
    fn write<O: SerializerOutput>(&self, _: &mut O, _: &Unserializable) -> Result<(), ExceptionPtr> {
        Err(ExceptionPtr::new(RuntimeError("cannot serialize".to_string())))
    }
}

impl Read<Unserializable> for Serializer {
    fn read<I: SerializerInput>(
        &self,
        _: &mut I,
        _: Type<Unserializable>,
    ) -> Result<Unserializable, ExceptionPtr> {
        Ok(Unserializable)
    }
}

type Proto = Protocol<Serializer, u32>;

// ---- logs ----

static LOG_BUF: LazyLock<Arc<Mutex<Vec<u8>>>> = LazyLock::new(|| Arc::new(Mutex::new(Vec::new())));

struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut v) => v.extend_from_slice(b),
            Err(p) => p.into_inner().extend_from_slice(b),
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Sends every log line to the buffer the tests search.
fn capture_logs() {
    RPC_LOG.set_level(LogLevel::Debug);
    Logger::set_ostream(Ostream::Custom(Box::new(SharedBuf(LOG_BUF.clone()))));
}

fn logged(s: &str) -> bool {
    let v = match LOG_BUF.lock() {
        Ok(v) => v.clone(),
        Err(p) => p.into_inner().clone(),
    };
    String::from_utf8_lossy(&v).contains(s)
}

static RPC_LOG: LazyLock<Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("cov_rpc_tcp").unwrap_or_else(|e| panic!("{}", exc_what(&e))));

// ---- helpers ----

fn exc_what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

fn is_rpc_error(e: &ExceptionPtr, kind: ErrorKind) -> bool {
    e.downcast_ref::<teseladb::engine::rpc::rpc_types::Error>().is_some_and(|x| x.kind() == kind)
}

fn bufs_bytes(bufs: &Bufs) -> Vec<u8> {
    match bufs {
        Bufs::One(b) => b.as_slice().to_vec(),
        Bufs::Many(v) => v.iter().flat_map(|b| b.as_slice().to_vec()).collect(),
    }
}

/// Polls `cond` every 10 ms for up to 5 s, from an engine thread.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) -> Result<(), ExceptionPtr> {
    for _ in 0..500 {
        if cond() {
            return Ok(());
        }
        sleep(Duration::from_millis(10)).get()?;
    }
    panic!("timed out waiting for {}", what);
}

const LOCALHOST: u32 = 0x7f00_0001;

/// A listening socket on a free loopback port that hands every connection to this shard.
fn fixed_listen() -> Result<(ServerSocket, SocketAddress), ExceptionPtr> {
    let mut lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    lo.set_fixed_cpu(this_shard_id());
    let ss = listen_with(make_ipv4_address_ip_port(LOCALHOST, 0), lo)?;
    let addr = ss.local_address()?;
    Ok((ss, addr))
}

/// A loopback port that was free a moment ago.
fn free_port() -> Result<u16, ExceptionPtr> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(ExceptionPtr::from)?;
    let p = l.local_addr().map_err(ExceptionPtr::from)?.port();
    Ok(p)
}

/// The client inside its pin.
fn cm(c: &mut Pin<Box<Client>>) -> &mut Client {
    // SAFETY: the client is never moved out of its pin.
    unsafe { c.as_mut().get_unchecked_mut() }
}

/// A protocol with a started server on shard 0. The server is declared first, so it is
/// dropped before the protocol it points at.
struct Env {
    server: Pin<Box<Server>>,
    proto: Pin<Box<Proto>>,
    addr: SocketAddress,
}

impl Env {
    fn new(so: ServerOptions, limits: ResourceLimits) -> Result<Env, ExceptionPtr> {
        let mut proto = Box::pin(Proto::new(Serializer));
        proto.as_mut().set_logger(Some(&**RPC_LOG));
        let (ss, addr) = fixed_listen()?;
        let mut server =
            Box::pin(proto.as_mut().new_server_with_options_socket(so, ss, limits));
        server.as_mut().start()?;
        Ok(Env { server, proto, addr })
    }

    fn plain() -> Result<Env, ExceptionPtr> {
        Self::new(ServerOptions::default(), ResourceLimits::default())
    }

    fn client(&mut self, co: ClientOptions) -> Result<Pin<Box<Client>>, ExceptionPtr> {
        let mut c = Box::pin(self.proto.as_mut().new_client_with_options(
            co,
            &self.addr,
            &SocketAddress::default(),
        ));
        c.as_mut().start()?;
        Ok(c)
    }

    fn server(&mut self) -> &mut Server {
        // SAFETY: the server is not moved out of its pin.
        unsafe { self.server.as_mut().get_unchecked_mut() }
    }

    /// Stops the server, then drops it and the protocol, in that order.
    fn finish(mut self) -> Result<(), ExceptionPtr> {
        self.server().stop().get()?;
        let Env { server, proto, .. } = self;
        drop(server);
        drop(proto);
        Ok(())
    }
}

/// Stops a client and drops it.
fn stop_client(mut c: Pin<Box<Client>>) -> Result<(), ExceptionPtr> {
    cm(&mut c).stop().get()?;
    drop(c);
    Ok(())
}

// ---- constructors ----

teseladb::engine_test_case!(protocols_make_servers_and_clients_every_way, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut proto = Box::pin(Proto::new(Serializer));
        proto.as_mut().register_handler(1, |a: i32, b: i32| make_ready_future::<i32>(a + b))?;
        let sum = proto.make_client::<fn(i32, i32) -> Future<i32>>(1);

        // No server can listen on an address of no local interface (192.0.2.1).
        let foreign = make_ipv4_address_ip_port(0xC000_0201, free_port()?);
        assert!(proto.as_mut().new_server(&foreign, ResourceLimits::default()).is_err());
        assert!(
            proto
                .as_mut()
                .new_server_with_options(ServerOptions::default(), &foreign, ResourceLimits::default())
                .is_err()
        );

        // A server on an address of its own; a second shutdown has nothing left to do.
        {
            let any = make_ipv4_address_ip_port(LOCALHOST, 0);
            let mut s = Box::pin(proto.as_mut().new_server(&any, ResourceLimits::default())?);
            s.as_mut().start()?;
            // SAFETY: the server is not moved out of its pin.
            let sp = unsafe { s.as_mut().get_unchecked_mut() };
            sp.shutdown().get()?;
            let again = sp.shutdown();
            assert!(again.available());
            again.get()?;
            sp.stop().get()?;
        }

        // A server over a socket listening already, and the three ways to make a client.
        let (ss, addr) = fixed_listen()?;
        let mut s = Box::pin(proto.as_mut().new_server_from_socket(
            ss,
            ResourceLimits::default(),
            ServerOptions::default(),
        ));
        s.as_mut().start()?;
        let local = SocketAddress::default();
        let mut c1 = Box::pin(proto.as_mut().new_client(&addr, &local));
        c1.as_mut().start()?;
        let keepalive = ClientOptions {
            keepalive: Some(TcpKeepaliveParams {
                idle: Duration::from_secs(30),
                interval: Duration::from_secs(5),
                count: 3,
            }),
            ..ClientOptions::default()
        };
        let mut c2 = Box::pin(proto.as_mut().new_client_with_options(keepalive, &addr, &local));
        c2.as_mut().start()?;
        let mut c3 = Box::pin(proto.as_mut().new_client_with_socket(make_socket(), &addr, &local));
        c3.as_mut().start()?;
        assert_eq!(sum.call(cm(&mut c1), &(1, 2)).get()?, 3);
        assert_eq!(sum.call(cm(&mut c2), &(3, 4)).get()?, 7);
        assert_eq!(sum.call(cm(&mut c3), &(5, 6)).get()?, 11);
        assert!(cm(&mut c1).peer_address() == addr);
        stop_client(c1)?;
        stop_client(c2)?;
        stop_client(c3)?;
        // SAFETY: as above.
        unsafe { s.as_mut().get_unchecked_mut() }.stop().get()?;
        drop(s);

        // A server listening on a given address with options: every connection to this shard.
        let addr2 = make_ipv4_address_ip_port(LOCALHOST, free_port()?);
        let so = ServerOptions {
            load_balancing_algorithm: LoadBalancingAlgorithm::Fixed,
            ..ServerOptions::default()
        };
        let mut s2 =
            Box::pin(proto.as_mut().new_server_with_options(so, &addr2, ResourceLimits::default())?);
        s2.as_mut().start()?;
        let mut c4 = Box::pin(proto.as_mut().new_client(&addr2, &local));
        c4.as_mut().start()?;
        assert_eq!(sum.call(cm(&mut c4), &(10, 20)).get()?, 30);
        stop_client(c4)?;
        // SAFETY: as above.
        unsafe { s2.as_mut().get_unchecked_mut() }.stop().get()?;
        drop(s2);
        drop(proto);
        Ok(())
    })
});

// ---- handler forms ----

teseladb::engine_test_case!(handlers_take_client_info_deadlines_and_optional_arguments, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut env = Env::plain()?;
        let p = env.proto.as_mut();
        p.register_handler::<_, HandlerSignature<
            DoWantClientInfo,
            DontWantTimePoint,
            Result<i32, ExceptionPtr>,
            (i32,),
        >>(1, |ci: &ClientInfo, x: i32| -> Result<i32, ExceptionPtr> {
            Ok(if ci.addr.is_unspecified() { -1 } else { x + 1 })
        })?;
        let p = env.proto.as_mut();
        p.register_handler::<_, HandlerSignature<
            DoWantClientInfo,
            DoWantTimePoint,
            Result<i32, ExceptionPtr>,
            (i32,),
        >>(2, |ci: &ClientInfo, tp: OptTimePoint, x: i32| -> Result<i32, ExceptionPtr> {
            assert!(!ci.addr.is_unspecified());
            Ok(if tp.0.is_some() { x } else { -x })
        })?;
        let p = env.proto.as_mut();
        p.register_handler::<_, HandlerSignature<
            DoWantClientInfoMut,
            DoWantTimePoint,
            Result<i32, ExceptionPtr>,
            (i32,),
        >>(3, |ci: &mut ClientInfo, tp: OptTimePoint, x: i32| -> Result<i32, ExceptionPtr> {
            let key = Sstring::from("calls");
            let n = ci.retrieve_auxiliary_opt::<i32>(&key).copied().unwrap_or(0);
            ci.attach_auxiliary(&key, n + 1);
            Ok(x * 10 + n + if tp.0.is_some() { 100 } else { 0 })
        })?;
        let p = env.proto.as_mut();
        p.register_handler::<_, HandlerSignature<
            DontWantClientInfo,
            DoWantTimePoint,
            Result<i32, ExceptionPtr>,
            (i32,),
        >>(4, |tp: OptTimePoint, x: i32| -> Result<i32, ExceptionPtr> {
            Ok(if tp.0.is_some() { x } else { -x })
        })?;
        env.proto.as_mut().register_handler(
            5,
            |x: i32, y: Optional<i32>| -> Result<i32, ExceptionPtr> { Ok(x + y.0.unwrap_or(100)) },
        )?;

        let mut c = env.client(ClientOptions::default())?;
        let second = Duration::from_secs(1).as_nanos() as i64;
        let v1 = env.proto.make_client::<fn(i32) -> Future<i32>>(1);
        assert_eq!(v1.call(cm(&mut c), &(4,)).get()?, 5);
        let v2 = env.proto.make_client::<fn(i32) -> Future<i32>>(2);
        assert_eq!(v2.call_for(cm(&mut c), second, &(6,)).get()?, 6);
        assert_eq!(v2.call(cm(&mut c), &(6,)).get()?, -6);
        let v3 = env.proto.make_client::<fn(i32) -> Future<i32>>(3);
        assert_eq!(v3.call(cm(&mut c), &(2,)).get()?, 20);
        assert_eq!(v3.call_for(cm(&mut c), second, &(2,)).get()?, 121);
        let v4 = env.proto.make_client::<fn(i32) -> Future<i32>>(4);
        assert_eq!(v4.call_until(cm(&mut c), LowresClock::now() + Duration::from_secs(1), &(8,)).get()?, 8);
        assert_eq!(v4.call(cm(&mut c), &(8,)).get()?, -8);

        // An older client that does not send the optional argument, and a newer one that does.
        let old = env.proto.make_client::<fn(i32) -> Future<i32>>(5);
        assert_eq!(old.call(cm(&mut c), &(5,)).get()?, 105);
        let new = env.proto.make_client::<fn(i32, Optional<i32>) -> Future<i32>>(5);
        assert_eq!(new.call(cm(&mut c), &(5, 7)).get()?, 12);

        let st = cm(&mut c).get_stats();
        assert_eq!(st.replied, 9);
        assert_eq!(st.wait_reply, 0);
        assert_eq!(st.pending, 0);
        assert_eq!(cm(&mut c).get_stats_internal().replied, 9);
        assert_eq!(cm(&mut c).incoming_queue_length(), 0);
        assert_eq!(cm(&mut c).outgoing_queue_length(), 0);
        stop_client(c)?;
        env.finish()
    })
});

// ---- server connections ----

teseladb::engine_test_case!(server_connections_expose_their_client_limits_and_counters, {
    async_(|| -> Result<(), ExceptionPtr> {
        let limits = ResourceLimits {
            basic_request_size: 5,
            bloat_factor: 2,
            max_memory: 1 << 20,
            ..ResourceLimits::default()
        };
        let mut env = Env::new(ServerOptions::default(), limits)?;
        env.proto.as_mut().register_handler(1, |x: i32| make_ready_future::<i32>(x))?;
        let mut c = env.client(ClientOptions::default())?;
        let echo = env.proto.make_client::<fn(i32) -> Future<i32>>(1);
        assert_eq!(echo.call(cm(&mut c), &(3,)).get()?, 3);

        let server_ptr: *mut Server = env.server();
        let mut conns = Vec::new();
        env.server().foreach_connection(|sc| conns.push(sc));
        assert_eq!(conns.len(), 1);
        let sc = conns.remove(0);
        assert_eq!(sc.max_request_size(), 1 << 20);
        assert_eq!(sc.estimate_request_size(10), 25);
        assert!(sc.get_server() == server_ptr);
        assert!(sc.peer_address() == sc.info_ref().addr);
        assert!(!sc.peer_address().is_unspecified());
        let st = sc.get_stats();
        // The negotiation frame and the reply.
        assert!(st.sent_messages >= 2);
        assert_eq!(st.pending, 0);
        let again = sc.shared_from_this();
        assert!(again.get().map(|p| p.as_ptr()) == sc.get().map(|p| p.as_ptr()));
        let key = Sstring::from("tag");
        // SAFETY: the connection is idle (no request in flight) and only this thread runs.
        unsafe { sc.get_mut_unchecked() }.info().attach_auxiliary(&key, 42i32);
        assert_eq!(sc.info_ref().retrieve_auxiliary_opt::<i32>(&key).copied(), Some(42));
        assert!(!env.server().reply_gate().is_closed());
        drop(again);
        drop(sc);

        // Aborting a connection the server does not have changes nothing.
        env.server().abort_connection(ConnectionId::make_id(999_999, 0));
        assert_eq!(echo.call(cm(&mut c), &(4,)).get()?, 4);

        // A connected client takes no second socket.
        let extra = teseladb::engine::core::reactor::connect(env.addr).get()?;
        let r = cm(&mut c).set_socket(extra);
        assert!(r.is_err_and(|e| exc_what(&e) == "already connected"));
        assert_eq!(echo.call(cm(&mut c), &(5,)).get()?, 5);
        stop_client(c)?;
        env.finish()
    })
});

// ---- no-wait verbs ----

teseladb::engine_test_case!(no_wait_verbs_send_no_reply_and_log_handler_errors, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::plain()?;
        let got = Rc::new(Cell::new(0));
        let g = got.clone();
        env.proto.as_mut().register_handler(10, move |x: i32| -> Future<NoWaitType> {
            g.set(x);
            make_ready_future(NoWaitType)
        })?;
        env.proto.as_mut().register_handler(11, |_x: i32| -> Result<NoWaitType, ExceptionPtr> {
            Err(ExceptionPtr::new(RuntimeError("nowait boom".to_string())))
        })?;
        let mut c = env.client(ClientOptions::default())?;
        let fire = env.proto.make_client::<fn(i32) -> Future<NoWaitType>>(10);
        fire.call(cm(&mut c), &(3,)).get()?;
        wait_until("the no-wait handler", || got.get() == 3)?;
        assert_eq!(cm(&mut c).incoming_queue_length(), 0);
        let failing = env.proto.make_client::<fn(i32) -> Future<NoWaitType>>(11);
        failing.call(cm(&mut c), &(1,)).get()?;
        wait_until("the no-wait error log", || {
            logged("exception \"nowait boom\" in no_wait handler ignored")
        })?;

        // A stopped client fails the call at once.
        cm(&mut c).stop().get()?;
        let r = fire.call(cm(&mut c), &(4,)).get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        drop(c);
        env.finish()
    })
});

teseladb::engine_test_case!(an_oversized_no_wait_request_is_logged_and_dropped, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let limits = ResourceLimits { max_memory: 100, ..ResourceLimits::default() };
        let mut env = Env::new(ServerOptions::default(), limits)?;
        let ran = Rc::new(Cell::new(false));
        let r = ran.clone();
        env.proto.as_mut().register_handler(12, move |_s: Sstring| -> Future<NoWaitType> {
            r.set(true);
            make_ready_future(NoWaitType)
        })?;
        let mut c = env.client(ClientOptions::default())?;
        let big = env.proto.make_client::<fn(Sstring) -> Future<NoWaitType>>(12);
        big.call(cm(&mut c), &(uninitialized_string(300),)).get()?;
        wait_until("the oversized request log", || logged("large than memory limit 100"))?;
        wait_until("the dropped reply log", || logged("in no_wait handler ignored"))?;
        assert!(!ran.get());
        stop_client(c)?;
        env.finish()
    })
});

// ---- serialization failures ----

teseladb::engine_test_case!(values_that_fail_to_serialize_or_deserialize, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::plain()?;
        env.proto
            .as_mut()
            .register_handler(22, || -> Result<Unserializable, ExceptionPtr> { Ok(Unserializable) })?;
        let ran = Rc::new(Cell::new(0));
        let r = ran.clone();
        env.proto.as_mut().register_handler(23, move |x: i64| -> Result<i64, ExceptionPtr> {
            r.set(r.get() + 1);
            Ok(x)
        })?;
        let mut c = env.client(ClientOptions::default())?;

        // The client cannot serialize the argument: the call fails before anything is sent.
        let bad_arg = env.proto.make_client::<fn(Unserializable) -> Future<i32>>(20);
        let e = bad_arg.call(cm(&mut c), &(Unserializable,)).get();
        assert!(e.is_err_and(|e| exc_what(&e) == "cannot serialize"));
        let bad_arg_nw = env.proto.make_client::<fn(Unserializable) -> Future<NoWaitType>>(21);
        let e = bad_arg_nw.call(cm(&mut c), &(Unserializable,)).get();
        assert!(e.is_err_and(|e| exc_what(&e) == "cannot serialize"));

        // The server cannot serialize the reply: the client gets the error instead.
        let bad_reply = env.proto.make_client::<fn() -> Future<Unserializable>>(22);
        let e = bad_reply.call(cm(&mut c), &()).get();
        assert!(e.is_err_and(|e| is_rpc_error(&e, ErrorKind::RemoteVerb)
            && exc_what(&e).contains("cannot serialize")));
        assert_eq!(cm(&mut c).get_stats().exception_received, 1);

        // The server cannot read the arguments (4 bytes for an i64): it logs and drops the
        // request, and the call times out.
        let short = env.proto.make_client::<fn(i32) -> Future<i64>>(23);
        let e = short
            .call_for(cm(&mut c), Duration::from_millis(300).as_nanos() as i64, &(1,))
            .get();
        assert!(e.is_err_and(|e| is_rpc_error(&e, ErrorKind::Timeout)));
        assert!(logged("caught exception while processing a message"));
        assert_eq!(ran.get(), 0);
        assert_eq!(cm(&mut c).get_stats().timeout, 1);
        stop_client(c)?;
        env.finish()
    })
});

// ---- reply handlers driven by hand ----

/// A reply handler that records the reply it gets and whether it was dropped.
struct Recording {
    base: ReplyHandlerData,
    got: Rc<RefCell<Option<(i64, Vec<u8>)>>>,
    dropped: Rc<Cell<bool>>,
}

impl ReplyHandlerBase for Recording {
    fn data(&mut self) -> &mut ReplyHandlerData {
        &mut self.base
    }
    fn call(&mut self, _client: &mut Client, msg_id: i64, data: RcvBuf) -> Result<(), ExceptionPtr> {
        *self.got.borrow_mut() = Some((msg_id, bufs_bytes(&data.bufs)));
        Ok(())
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

fn recording() -> (Box<Recording>, Rc<RefCell<Option<(i64, Vec<u8>)>>>, Rc<Cell<bool>>) {
    let got = Rc::new(RefCell::new(None));
    let dropped = Rc::new(Cell::new(false));
    let h = Box::new(Recording {
        base: ReplyHandlerData::default(),
        got: got.clone(),
        dropped: dropped.clone(),
    });
    (h, got, dropped)
}

teseladb::engine_test_case!(requests_and_reply_handlers_driven_by_hand, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut env = Env::plain()?;
        env.proto.as_mut().register_handler(30, |x: i32| -> Result<i32, ExceptionPtr> { Ok(x * 2) })?;
        let mut c = env.client(ClientOptions::default())?;
        // The first call negotiates the connection.
        let v = env.proto.make_client::<fn(i32) -> Future<i32>>(30);
        assert_eq!(v.call(cm(&mut c), &(1,)).get()?, 2);

        // A request and a wait made by hand.
        let id = cm(&mut c).next_message_id();
        assert_eq!(cm(&mut c).next_message_id(), id + 1);
        let data = marshall(&Serializer, REQUEST_FRAME_HEADROOM, &(21i32,))?;
        let req = cm(&mut c).request(30, id, data, None, None);
        let (h, got, dropped) = recording();
        cm(&mut c).wait_for_reply(id, h, None, None);
        assert_eq!(cm(&mut c).incoming_queue_length(), 1);
        req.get()?;
        wait_until("the reply", || got.borrow().is_some())?;
        assert_eq!(*got.borrow(), Some((id, 42i32.to_ne_bytes().to_vec())));
        assert!(dropped.get());

        // A wait that times out: the default timeout() of the handler runs, and it is dropped.
        let timeouts = cm(&mut c).get_stats().timeout;
        let (h, got, dropped) = recording();
        cm(&mut c).wait_for_reply(9_999, h, Some(LowresClock::now() + Duration::from_millis(20)), None);
        wait_until("the timeout", || dropped.get())?;
        assert!(got.borrow().is_none());
        assert_eq!(cm(&mut c).get_stats().timeout, timeouts + 1);
        assert_eq!(cm(&mut c).incoming_queue_length(), 0);

        // A wait that is canceled: the default cancel() of the handler runs.
        let mut cancel = std::pin::pin!(Cancellable::default());
        let (h, got, dropped) = recording();
        cm(&mut c).wait_for_reply(9_998, h, None, Some(cancel.as_mut()));
        assert_eq!(cm(&mut c).incoming_queue_length(), 1);
        cancel.as_mut().cancel();
        assert!(dropped.get());
        assert!(got.borrow().is_none());
        assert_eq!(cm(&mut c).incoming_queue_length(), 0);

        // A reply for a message nobody waits for any more is dropped.
        let id = cm(&mut c).next_message_id();
        let data = marshall(&Serializer, REQUEST_FRAME_HEADROOM, &(5i32,))?;
        cm(&mut c).request(30, id, data, None, None).get()?;
        assert_eq!(v.call(cm(&mut c), &(3,)).get()?, 6);
        stop_client(c)?;
        env.finish()
    })
});

// ---- client gauges ----

/// The value of metric `name` in the domain `domain`.
fn metric(name: &str, domain: &str) -> Option<f64> {
    let registry = teseladb::engine::core::metrics::impl_fns::get_value_map();
    // SAFETY: this shard's registry, with no other reference alive across the lookup.
    let values = unsafe { registry.get_mut_unchecked() }.get_value_map();
    let mf = values.get(&Sstring::from(name))?;
    for (labels, mi) in mf.iter() {
        for (k, v) in labels.labels() {
            if *k == "domain"
                && *v == domain
                && let Some(m) = mi
            {
                return Some((m.get_function())().d());
            }
        }
    }
    None
}

teseladb::engine_test_case!(client_gauges_count_queued_and_waiting_calls, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut env = Env::plain()?;
        let release = Rc::new(RefCell::new(Promise::<()>::new()));
        let rel = release.clone();
        env.proto.as_mut().register_handler(40, move |x: i32| -> Future<i32> {
            rel.borrow_mut().get_future().then(move |()| make_ready_future(x))
        })?;
        let co = ClientOptions { metrics_domain: Sstring::from("cov_gauges"), ..ClientOptions::default() };
        let mut c = env.client(co)?;
        let v = env.proto.make_client::<fn(i32) -> Future<i32>>(40);
        let pending = v.call(cm(&mut c), &(9,));
        wait_until("the call to be sent and wait for its reply", || {
            metric("rpc_client_wait_reply", "cov_gauges") == Some(1.0)
                && metric("rpc_client_pending", "cov_gauges") == Some(0.0)
        })?;
        assert!(metric("rpc_client_delay_total", "cov_gauges").is_some_and(|d| d >= 0.0));
        assert_eq!(metric("rpc_client_count", "cov_gauges"), Some(1.0));
        release.borrow_mut().set_value(());
        assert_eq!(pending.get()?, 9);
        assert_eq!(metric("rpc_client_wait_reply", "cov_gauges"), Some(0.0));
        // The server reports the duration of the handler, so the reply is a delay sample.
        assert_eq!(cm(&mut c).get_stats().delay_samples, 1);
        assert_eq!(metric("rpc_client_delay_samples", "cov_gauges"), Some(1.0));
        stop_client(c)?;
        // The counters of a dropped client stay in its domain.
        assert_eq!(metric("rpc_client_count", "cov_gauges"), Some(0.0));
        assert_eq!(metric("rpc_client_replied", "cov_gauges"), Some(1.0));
        env.finish()
    })
});

// ---- compressors ----

/// What a [`TestCompressor`] fails at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failing {
    /// Nothing: it copies the frames.
    Nothing,
    /// Compressing.
    Compress,
    /// Decompressing.
    Decompress,
}

/// A compressor that copies frames, or fails as told.
struct TestCompressor {
    fail: Failing,
}

impl Compressor for TestCompressor {
    fn compress(&mut self, head_space: usize, data: SndBuf) -> Result<SndBuf, ExceptionPtr> {
        if self.fail == Failing::Compress {
            return Err(ExceptionPtr::new(RuntimeError("compress failed".to_string())));
        }
        let bytes = bufs_bytes(&data.bufs);
        let mut out = TemporaryBuffer::<u8>::new(head_space + bytes.len());
        // SAFETY: the buffer holds `head_space + bytes.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.get_write().add(head_space), bytes.len())
        };
        Ok(SndBuf::from_buffer(out))
    }
    fn decompress(&mut self, data: RcvBuf) -> Result<RcvBuf, ExceptionPtr> {
        if self.fail == Failing::Decompress {
            return Err(ExceptionPtr::new(RuntimeError("decompress failed".to_string())));
        }
        Ok(data)
    }
    fn name(&self) -> Sstring {
        Sstring::from("COPY")
    }
}

/// Negotiates "COPY"; its compressors fail as `client` or `server` say, by side.
struct TestFactory {
    name: Sstring,
    client: Failing,
    server: Failing,
}

impl CompressorFactory for TestFactory {
    fn supported(&self) -> &Sstring {
        &self.name
    }
    fn negotiate(&self, feature: Sstring, is_server: bool) -> Option<Box<dyn Compressor>> {
        if feature != self.name {
            return None;
        }
        let fail = if is_server { self.server } else { self.client };
        Some(Box::new(TestCompressor { fail }))
    }
}

fn leak_factory(client: Failing, server: Failing) -> &'static TestFactory {
    Box::leak(Box::new(TestFactory { name: Sstring::from("COPY"), client, server }))
}

/// One call through client and server factories; returns its result.
fn call_with_factories(
    cf: &'static TestFactory,
    sf: &'static TestFactory,
) -> Result<Result<i32, ExceptionPtr>, ExceptionPtr> {
    let so = ServerOptions { compressor_factory: Some(sf), ..ServerOptions::default() };
    let mut env = Env::new(so, ResourceLimits::default())?;
    env.proto.as_mut().register_handler(1, |x: i32| make_ready_future::<i32>(x + 1))?;
    let co = ClientOptions { compressor_factory: Some(cf), ..ClientOptions::default() };
    let mut c = env.client(co)?;
    let v = env.proto.make_client::<fn(i32) -> Future<i32>>(1);
    let r = v.call(cm(&mut c), &(1,)).get();
    stop_client(c)?;
    env.finish()?;
    Ok(r)
}

teseladb::engine_test_case!(compressors_that_fail_end_the_connection, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        // Both ends copy the frames: the call goes through.
        let ok = call_with_factories(
            leak_factory(Failing::Nothing, Failing::Nothing),
            leak_factory(Failing::Nothing, Failing::Nothing),
        )?;
        assert_eq!(ok.ok(), Some(2));
        // The client cannot compress its request: it aborts its connection.
        let r = call_with_factories(
            leak_factory(Failing::Compress, Failing::Nothing),
            leak_factory(Failing::Nothing, Failing::Nothing),
        )?;
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        // The server cannot decompress the request: it drops the connection.
        let r = call_with_factories(
            leak_factory(Failing::Nothing, Failing::Nothing),
            leak_factory(Failing::Nothing, Failing::Decompress),
        )?;
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        assert!(logged("server connection dropped: decompress failed"));
        Ok(())
    })
});

// ---- streams, filters, domains ----

teseladb::engine_test_case!(streams_need_a_server_with_a_streaming_domain, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut env = Env::plain()?;
        env.proto.as_mut().register_handler(1, |x: i32| make_ready_future::<i32>(x))?;
        let mut c = env.client(ClientOptions::default())?;
        let v = env.proto.make_client::<fn(i32) -> Future<i32>>(1);
        assert_eq!(v.call(cm(&mut c), &(1,)).get()?, 1);
        let r = cm(&mut c).make_stream_sink::<Serializer, (i32,)>().get();
        assert!(r.is_err_and(|e| exc_what(&e) == "Streaming is not supported by the server"));
        stop_client(c)?;
        env.finish()
    })
});

teseladb::engine_test_case!(a_filter_refuses_connections, {
    async_(|| -> Result<(), ExceptionPtr> {
        let asked = Rc::new(Cell::new(0));
        let a = asked.clone();
        let so = ServerOptions {
            filter_connection: Some(Box::new(move |peer: &SocketAddress| {
                a.set(a.get() + 1);
                peer.is_unspecified()
            })),
            ..ServerOptions::default()
        };
        let mut env = Env::new(so, ResourceLimits::default())?;
        env.proto.as_mut().register_handler(1, |x: i32| make_ready_future::<i32>(x))?;
        let mut c = env.client(ClientOptions::default())?;
        let v = env.proto.make_client::<fn(i32) -> Future<i32>>(1);
        assert!(v.call(cm(&mut c), &(1,)).get().is_err());
        assert_eq!(asked.get(), 1);
        let mut n = 0;
        env.server().foreach_connection(|_| n += 1);
        assert_eq!(n, 0);
        stop_client(c)?;
        env.finish()
    })
});

teseladb::engine_test_case!(a_streaming_domain_belongs_to_one_server_per_shard, {
    async_(|| -> Result<(), ExceptionPtr> {
        let domain = StreamingDomainType::new(4242);
        let so = ServerOptions { streaming_domain: Some(domain), ..ServerOptions::default() };
        let mut env = Env::new(so.clone(), ResourceLimits::default())?;
        let (ss, _) = fixed_listen()?;
        let mut second =
            Box::pin(env.proto.as_mut().new_server_with_options_socket(so, ss, ResourceLimits::default()));
        let r = second.as_mut().start();
        assert!(r.is_err_and(|e| exc_what(&e)
            == "An RPC server with the streaming domain 4242 is already exist"));
        drop(second);
        // The first server keeps the domain until it shuts down; then the name is free.
        env.server().shutdown().get()?;
        let (ss, _) = fixed_listen()?;
        let so = ServerOptions { streaming_domain: Some(domain), ..ServerOptions::default() };
        let mut third =
            Box::pin(env.proto.as_mut().new_server_with_options_socket(so, ss, ResourceLimits::default()));
        third.as_mut().start()?;
        // SAFETY: the server is not moved out of its pin.
        unsafe { third.as_mut().get_unchecked_mut() }.stop().get()?;
        drop(third);
        env.finish()
    })
});

// ---- cancellation and handler removal (on two shards) ----

teseladb::engine_test_case!(calls_are_canceled_before_and_after_they_are_sent, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut env = Env::plain()?;
        let executed = Rc::new(Cell::new(false));
        let mut first_call = Promise::<()>::new();
        let called_f = first_call.get_future();
        // The promise of the first run of the handler; later runs find it taken.
        let called = Rc::new(RefCell::new(Some(first_call)));
        let (ex, ca) = (executed.clone(), called.clone());
        env.proto.as_mut().register_handler(1, move || {
            if let Some(mut p) = ca.borrow_mut().take() {
                p.set_value(());
            }
            ex.set(true);
            sleep(Duration::from_millis(50))
        })?;
        let mut c = env.client(ClientOptions::default())?;
        let call = env.proto.make_client::<fn() -> Future<()>>(1);
        // Hold the outgoing queue, queue the call, cancel it: it never leaves the client.
        let mut cont = Promise::<()>::new();
        let mut cancel = std::pin::pin!(Cancellable::default());
        cm(&mut c).suspend_for_testing(&mut cont)?;
        let f = call.call_cancellable(cm(&mut c), cancel.as_mut(), &());
        cancel.as_mut().cancel();
        cont.set_value(());
        assert!(f.get().is_err_and(|e| is_rpc_error(&e, ErrorKind::Canceled)));
        assert!(!executed.get());

        // Cancel while the server runs the handler: the wait is canceled.
        let f = call.call_cancellable(cm(&mut c), cancel.as_mut(), &());
        called_f.get()?;
        cancel.as_mut().cancel();
        assert!(f.get().is_err_and(|e| is_rpc_error(&e, ErrorKind::Canceled)));
        assert!(executed.get());
        // The late reply is dropped and the connection keeps working.
        sleep(Duration::from_millis(100)).get()?;
        call.call(cm(&mut c), &()).get()?;
        stop_client(c)?;
        env.finish()
    })
});

teseladb::engine_test_case!(handlers_are_removed_while_calls_are_in_flight, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut env = Env::plain()?;
        let mut c = env.client(ClientOptions::default())?;
        let ran = Rc::new(Cell::new(0));
        let make_handler = {
            let ran = ran.clone();
            move || {
                let ran = ran.clone();
                move || {
                    ran.set(ran.get() + 1);
                    sleep(Duration::from_millis(1))
                }
            }
        };
        // A handler removed before it is called never runs.
        env.proto.as_mut().register_handler(1, make_handler())?;
        assert!(env.proto.has_handler(1));
        env.proto.as_mut().unregister_handler(1).get()?;
        assert!(!env.proto.has_handler(1));
        let call = env.proto.make_client::<fn() -> Future<()>>(1);
        let r = call.call(cm(&mut c), &()).get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::UnknownVerb(1))));
        assert_eq!(ran.get(), 0);

        // Removing a handler waits for the calls it is running.
        let reached = Rc::new(RefCell::new(Promise::<()>::new()));
        let go = Rc::new(RefCell::new(Promise::<()>::new()));
        let value = Rc::new(RefCell::new(Sstring::from("before")));
        let (r2, g2, v2) = (reached.clone(), go.clone(), value.clone());
        env.proto.as_mut().register_handler(1, move || -> Future<Sstring> {
            r2.borrow_mut().set_value(());
            let v = v2.clone();
            let f = g2.borrow_mut().get_future();
            f.then(move |()| make_ready_future(v.borrow().clone()))
        })?;
        let get = env.proto.make_client::<fn() -> Future<Sstring>>(1);
        let response = get.call(cm(&mut c), &());
        let reached_f = reached.borrow_mut().get_future();
        reached_f.get()?;
        let v3 = value.clone();
        let removed = env.proto.as_mut().unregister_handler(1).then(move |()| {
            *v3.borrow_mut() = Sstring::from("after");
        });
        assert!(!removed.available());
        // A second call while the handler is being removed is refused.
        let refused = get.call(cm(&mut c), &()).get();
        assert!(refused.is_err_and(|e| is_rpc_error(&e, ErrorKind::UnknownVerb(1))));
        go.borrow_mut().set_value(());
        removed.get()?;
        assert_eq!(response.get()?, "before");
        assert_eq!(*value.borrow(), "after");

        // A removal racing a call: the call either completes or finds no handler.
        env.proto.as_mut().register_handler(1, make_handler())?;
        let protop: *mut Proto =
            // SAFETY: only the address is taken; the protocol stays pinned.
            unsafe { env.proto.as_mut().get_unchecked_mut() };
        let delayed = sleep(Duration::from_micros(500)).then(move |()| {
            // SAFETY: the protocol outlives this thread, which waits for the removal.
            unsafe { Pin::new_unchecked(&mut *protop) }.unregister_handler(1)
        });
        let plain = env.proto.make_client::<fn() -> Future<()>>(1);
        match when_all_succeed((plain.call(cm(&mut c), &()), delayed)).get() {
            Ok(_) => {}
            Err(e) => assert!(is_rpc_error(&e, ErrorKind::UnknownVerb(1))),
        }
        assert!(!env.proto.has_handler(1));
        stop_client(c)?;
        env.finish()
    })
});
