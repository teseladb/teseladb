#![cfg(target_os = "linux")]
//! Tests of RPC streams over real TCP connections: sources and sinks passed as arguments
//! in both directions, a sink closed twice or written after its close, values queued with
//! the end of the stream before they are read, a source that cannot read what it receives,
//! a sink used from another shard, and a server or a client stopped while a stream is open.
//!
//! The reactor runs two shards (set before `main`): the cross-shard sink and the stop cases
//! run on two shards here.

#![allow(unsafe_code)]
#![allow(clippy::type_complexity)]

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{ListenOptions, ServerSocket, make_ipv4_address_ip_port};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::rpc::rpc::{
    Client, ClientOptions, Protocol, ResourceLimits, Server, ServerOptions, StreamingDomainType,
};
use teseladb::engine::rpc::rpc_impl::{Read, SerializerInput, SerializerOutput, Write};
use teseladb::engine::rpc::rpc_types::{ErrorKind, Sink, Source, Type};
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine::util::log::{LogLevel, Logger, Ostream};

/// Starts the test reactor with two shards before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov_rpc_streams_test".to_string(), "--smp=2".to_string()];
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

arithmetic_serializer!(i32, u32, u64);

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

type Proto = Protocol<Serializer, u32>;

// ---- helpers ----

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

fn capture_logs() {
    RPC_LOG.set_level(LogLevel::Debug);
    Logger::set_ostream(Ostream::Custom(Box::new(SharedBuf(LOG_BUF.clone()))));
}

static RPC_LOG: LazyLock<Pin<Box<Logger>>> = LazyLock::new(|| {
    Logger::new("cov_rpc_streams").unwrap_or_else(|e| panic!("{}", exc_what(&e)))
});

fn exc_what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

fn is_rpc_error(e: &ExceptionPtr, kind: ErrorKind) -> bool {
    e.downcast_ref::<teseladb::engine::rpc::rpc_types::Error>().is_some_and(|x| x.kind() == kind)
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

/// A value moved to another shard and back without a thread-safety requirement on its type.
struct AssertSend<T>(T);

// SAFETY: the tests move the value to one other shard and back, and only one shard uses it
// at a time.
unsafe impl<T> Send for AssertSend<T> {}

impl<T> AssertSend<T> {
    /// The value, taken through the whole wrapper so that closures capture it whole.
    fn into_inner(self) -> T {
        self.0
    }
}

fn fixed_listen() -> Result<(ServerSocket, SocketAddress), ExceptionPtr> {
    let mut lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    lo.set_fixed_cpu(this_shard_id());
    let ss = listen_with(make_ipv4_address_ip_port(0x7f00_0001, 0), lo)?;
    let addr = ss.local_address()?;
    Ok((ss, addr))
}

/// A protocol with a started streaming server on shard 0 (server first: dropped before the
/// protocol).
struct Env {
    server: Pin<Box<Server>>,
    proto: Pin<Box<Proto>>,
    addr: SocketAddress,
}

impl Env {
    fn new(domain: u64) -> Result<Env, ExceptionPtr> {
        let so = ServerOptions {
            streaming_domain: Some(StreamingDomainType::new(domain)),
            ..ServerOptions::default()
        };
        let mut proto = Box::pin(Proto::new(Serializer));
        proto.as_mut().set_logger(Some(&**RPC_LOG));
        let (ss, addr) = fixed_listen()?;
        let mut server = Box::pin(proto.as_mut().new_server_with_options_socket(
            so,
            ss,
            ResourceLimits::default(),
        ));
        server.as_mut().start()?;
        Ok(Env { server, proto, addr })
    }

    fn client(&mut self) -> Result<Pin<Box<Client>>, ExceptionPtr> {
        let mut c = Box::pin(self.proto.as_mut().new_client_with_options(
            ClientOptions::default(),
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

    /// Stops the server (unless `stopped`), then drops it and the protocol, in that order.
    fn finish(mut self, stopped: bool) -> Result<(), ExceptionPtr> {
        if !stopped {
            self.server().stop().get()?;
        }
        let Env { server, proto, .. } = self;
        drop(server);
        drop(proto);
        Ok(())
    }
}

fn cm(c: &mut Pin<Box<Client>>) -> &mut Client {
    // SAFETY: the client is never moved out of its pin.
    unsafe { c.as_mut().get_unchecked_mut() }
}

/// What a verb handler kept of a stream: its source and the sink made from it.
type Kept<In, Out> = Rc<RefCell<Option<(Source<In>, Sink<Out>)>>>;

/// Registers verb `t`: it keeps the source it gets and returns a sink over the same stream.
fn register_keeper<In, Out>(env: &mut Env, t: u32) -> Result<Kept<In, Out>, ExceptionPtr>
where
    In: 'static,
    Out: 'static,
    Serializer: Write<Sink<Out>>,
    Source<In>: teseladb::engine::rpc::rpc_impl::RpcType,
    Out: teseladb::engine::rpc::rpc_impl::MarshallArgs<Serializer>,
    In: teseladb::engine::rpc::rpc_impl::UnmarshallArgs<Serializer>,
{
    let kept: Kept<In, Out> = Rc::new(RefCell::new(None));
    let k = kept.clone();
    env.proto.as_mut().register_handler(t, move |src: Source<In>| -> Future<Sink<Out>> {
        let sink = src.make_sink::<Serializer, Out>();
        *k.borrow_mut() = Some((src, sink.clone()));
        make_ready_future(sink)
    })?;
    Ok(kept)
}

// ---- tests ----

teseladb::engine_test_case!(sinks_and_sources_travel_as_arguments_and_close_once, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::new(31)?;
        let kept = register_keeper::<(i32,), (Sstring,)>(&mut env, 1)?;
        // Verb 2 gets a sink from the source the client passes, and closes it.
        env.proto.as_mut().register_handler(2, move |mut sink: Sink<(Sstring,)>| -> Future<u64> {
            let id = sink.get_id().id();
            let keep = sink.clone();
            // SAFETY: `keep`, a copy of the sink, lives until the close resolves.
            unsafe { sink.close() }.then(move |()| {
                drop(keep);
                make_ready_future(id)
            })
        })?;
        let mut c = env.client()?;
        let mut sink = cm(&mut c).make_stream_sink::<Serializer, (i32,)>().get()?;
        let open = env.proto.make_client::<fn(Sink<(i32,)>) -> Future<Source<(Sstring,)>>>(1);
        let mut source = open.call(cm(&mut c), &(sink.clone(),)).get()?;
        let pass = env.proto.make_client::<fn(Source<(Sstring,)>) -> Future<u64>>(2);
        let id = pass.call(cm(&mut c), &(source.clone(),)).get()?;
        // Both ends of the stream have the id the server gave the stream connection.
        assert_eq!(id, source.get_id().id());
        assert_eq!(id, sink.get_id().id());

        // The server's own sink over that stream: the stream's sink side is closed already.
        let (mut ssrc, mut ssink) = kept.borrow_mut().take().unwrap_or_else(|| panic!("no stream"));
        // SAFETY: `ssink` lives in this frame until the end of the test.
        let r = unsafe { ssink.close() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::StreamClosed)));
        // A value sent after the close is refused; flush reports it.
        // SAFETY: as above.
        unsafe { ssink.call(&(Sstring::from("late"),)) }.get()?;
        // SAFETY: as above.
        let r = unsafe { ssink.flush() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::StreamClosed)));

        // Values queued with the end of the stream before the server reads any of them.
        for i in 1..=3 {
            // SAFETY: `sink` lives in this frame until the end of the test.
            unsafe { sink.call(&(i,)) }.get()?;
        }
        // SAFETY: as above.
        unsafe { sink.flush() }.get()?;
        // SAFETY: as above.
        unsafe { sink.close() }.get()?;
        sleep(Duration::from_millis(100)).get()?;
        for i in 1..=3 {
            // SAFETY: `ssrc` lives in this frame until the end of the test.
            assert_eq!(unsafe { ssrc.call() }.get()?, Some((i,)));
        }
        // SAFETY: as above.
        assert_eq!(unsafe { ssrc.call() }.get()?, None);

        // Both sides of the server's end are closed now, so it closed the connection; the
        // client's end drops what it had queued, and its source reports the closed stream.
        sleep(Duration::from_millis(100)).get()?;
        // SAFETY: `source` lives in this frame until the end of the test.
        let r = unsafe { source.call() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::StreamClosed)));
        cm(&mut c).stop().get()?;
        drop((ssrc, ssink, source, sink));
        drop(c);
        env.finish(false)
    })
});

teseladb::engine_test_case!(a_source_that_cannot_read_its_values_fails, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::new(32)?;
        // The client sends an i32, the server reads a string: the i32 is taken as the length.
        env.proto.as_mut().register_handler(3, move |src: Source<(Sstring,)>| -> Future<bool> {
            let mut s = src.clone();
            // SAFETY: `src`, a copy of the source, lives until the read resolves.
            unsafe { s.call() }.then_wrapped(move |f: Future<Option<(Sstring,)>>| {
                drop(src);
                make_ready_future(f.get().is_err())
            })
        })?;
        let mut c = env.client()?;
        let mut sink = cm(&mut c).make_stream_sink::<Serializer, (i32,)>().get()?;
        // SAFETY: `sink` lives in this frame until the end of the test.
        unsafe { sink.call(&(1000,)) }.get()?;
        // SAFETY: as above.
        unsafe { sink.flush() }.get()?;
        let read = env.proto.make_client::<fn(Sink<(i32,)>) -> Future<bool>>(3);
        assert!(read.call(cm(&mut c), &(sink.clone(),)).get()?);
        // SAFETY: as above.
        unsafe { sink.close() }.get()?;
        cm(&mut c).stop().get()?;
        drop(sink);
        drop(c);
        env.finish(false)
    })
});

teseladb::engine_test_case!(a_sink_is_used_from_another_shard, {
    async_(|| -> Result<(), ExceptionPtr> {
        assert!(Smp::count() >= 2, "the test needs two shards");
        let mut env = Env::new(33)?;
        env.proto.as_mut().register_handler(4, move |src: Source<(i32,)>| -> Future<i32> {
            let mut s = src.clone();
            // SAFETY: `src`, a copy of the source, lives until the read resolves.
            unsafe { s.call() }.then(move |v: Option<(i32,)>| {
                drop(src);
                make_ready_future(v.map_or(-1, |t| t.0))
            })
        })?;
        let mut c = env.client()?;
        let sink = cm(&mut c).make_stream_sink::<Serializer, (i32,)>().get()?;
        // The sink goes to shard 1, sends a value and flushes there, and comes back.
        let moved = AssertSend(sink);
        let back = Smp::submit_to(1, move || {
            let mut s = moved.into_inner();
            // SAFETY: `s` moves into the continuations below, so it lives until they end.
            let sent = unsafe { s.call(&(77,)) };
            sent.then(move |()| {
                let mut s = s;
                // SAFETY: as above.
                let flushed = unsafe { s.flush() };
                flushed.then(move |()| make_ready_future(AssertSend(s)))
            })
        })
        .get()?;
        let mut sink = back.into_inner();
        let read = env.proto.make_client::<fn(Sink<(i32,)>) -> Future<i32>>(4);
        assert_eq!(read.call(cm(&mut c), &(sink.clone(),)).get()?, 77);
        // SAFETY: `sink` lives in this frame until the end of the test.
        unsafe { sink.close() }.get()?;
        cm(&mut c).stop().get()?;
        drop(sink);
        drop(c);
        env.finish(false)
    })
});

teseladb::engine_test_case!(stopping_the_server_ends_its_open_streams, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::new(34)?;
        let kept = register_keeper::<(i32,), (i32,)>(&mut env, 5)?;
        let mut c = env.client()?;
        let mut sink = cm(&mut c).make_stream_sink::<Serializer, (i32,)>().get()?;
        let open = env.proto.make_client::<fn(Sink<(i32,)>) -> Future<Source<(i32,)>>>(5);
        let mut source = open.call(cm(&mut c), &(sink.clone(),)).get()?;
        let (mut ssrc, mut ssink) = kept.borrow_mut().take().unwrap_or_else(|| panic!("no stream"));

        // The server stops with the stream open: its streaming domain goes first, then its
        // connections, and the stream connection ends with its parent.
        env.server().stop().get()?;
        // SAFETY: `ssrc` lives in this frame until the end of the test.
        let r = unsafe { ssrc.call() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::StreamClosed)));
        // SAFETY: `ssink` lives in this frame until the end of the test.
        let r = unsafe { ssink.close() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));

        // The client sees its stream end too.
        wait_until("the client to lose its connection", || cm(&mut c).error())?;
        // SAFETY: `source` lives in this frame until the end of the test.
        let r = unsafe { source.call() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::StreamClosed)));
        // SAFETY: `sink` lives in this frame until the end of the test.
        let r = unsafe { sink.close() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        cm(&mut c).stop().get()?;
        // Let the stream connections finish their removal from the stopped server.
        sleep(Duration::from_millis(100)).get()?;
        drop((ssrc, ssink, source, sink));
        drop(c);
        env.finish(true)
    })
});

teseladb::engine_test_case!(stopping_the_client_ends_its_open_streams, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::new(35)?;
        let kept = register_keeper::<(i32,), (i32,)>(&mut env, 6)?;
        let mut c = env.client()?;
        let mut sink = cm(&mut c).make_stream_sink::<Serializer, (i32,)>().get()?;
        let open = env.proto.make_client::<fn(Sink<(i32,)>) -> Future<Source<(i32,)>>>(6);
        let mut source = open.call(cm(&mut c), &(sink.clone(),)).get()?;
        let (mut ssrc, mut ssink) = kept.borrow_mut().take().unwrap_or_else(|| panic!("no stream"));

        // The client stops with the stream open: it aborts its streams, and the server's end
        // of the stream ends when its parent connection does.
        cm(&mut c).stop().get()?;
        // SAFETY: `sink` lives in this frame until the end of the test.
        let r = unsafe { sink.close() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        // SAFETY: `source` lives in this frame until the end of the test.
        let r = unsafe { source.call() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::StreamClosed)));
        // SAFETY: `ssrc` lives in this frame until the end of the test.
        let r = unsafe { ssrc.call() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::StreamClosed)));
        // SAFETY: `ssink` lives in this frame until the end of the test.
        let r = unsafe { ssink.close() }.get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        let mut n = 0;
        env.server().foreach_connection(|_| n += 1);
        wait_until("the server to drop the client", || {
            let mut m = 0;
            env.server().foreach_connection(|_| m += 1);
            m == 0
        })?;
        assert!(n <= 1);
        drop((ssrc, ssink, source, sink));
        drop(c);
        env.finish(false)
    })
});
