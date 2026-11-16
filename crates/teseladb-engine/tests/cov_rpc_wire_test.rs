#![cfg(target_os = "linux")]
//! Tests of the RPC wire handling against peers that write the frames by hand over TCP:
//! negotiation frames with a wrong magic, cut short, with malformed features or features
//! nobody knows; request, response, compressed and stream frames cut short; requests split
//! over several segments; replies for calls nobody waits for, error replies of every kind,
//! handler durations; compression the client does not support; stream connections whose
//! parent is unknown, on another shard, or on a server without streaming.
//!
//! The raw peers run in plain threads with blocking sockets; the engine side runs in an
//! engine thread that polls for them, so the reactor never blocks. The reactor runs two
//! shards (set before `main`).

#![allow(unsafe_code)]

use std::io::Read as _;
use std::io::Write as _;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError};
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{ListenOptions, ServerSocket, make_ipv4_address_ip_port};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::rpc::lz4_compressor::Lz4CompressorFactory;
use teseladb::engine::rpc::rpc::{
    Client, ClientOptions, Protocol, ResourceLimits, Server, ServerOptions, StreamingDomainType,
};
use teseladb::engine::rpc::rpc_impl::{Read, SerializerInput, SerializerOutput, Write};
use teseladb::engine::rpc::rpc_types::{ConnectionId, ErrorKind, Type};
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine::util::log::{LogLevel, Logger, Ostream};

/// Starts the test reactor with two shards before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov_rpc_wire_test".to_string(), "--smp=2".to_string()];
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

arithmetic_serializer!(i32, i64, u32);

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
    LazyLock::new(|| Logger::new("cov_rpc_wire").unwrap_or_else(|e| panic!("{}", exc_what(&e))));

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

// ---- the raw peer ----

const MAGIC: &[u8; 8] = b"TSLDBRPC";
const COMPRESS: u32 = 0;
const TIMEOUT: u32 = 1;
const CONNECTION_ID: u32 = 2;
const STREAM_PARENT: u32 = 3;
const HANDLER_DURATION: u32 = 5;
const UNKNOWN_FEATURE: u32 = 77;

type PeerResult<T> = Result<T, String>;

fn io<T>(r: std::io::Result<T>) -> PeerResult<T> {
    r.map_err(|e| e.to_string())
}

/// A negotiation frame with `features`.
fn neg_frame(features: &[(u32, &[u8])]) -> Vec<u8> {
    let mut extra = Vec::new();
    for (f, d) in features {
        extra.extend_from_slice(&f.to_le_bytes());
        extra.extend_from_slice(&(d.len() as u32).to_le_bytes());
        extra.extend_from_slice(d);
    }
    let mut v = MAGIC.to_vec();
    v.extend_from_slice(&(extra.len() as u32).to_le_bytes());
    v.extend_from_slice(&extra);
    v
}

fn read_n(s: &mut TcpStream, n: usize) -> PeerResult<Vec<u8>> {
    let mut v = vec![0u8; n];
    io(s.read_exact(&mut v))?;
    Ok(v)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// Reads a negotiation frame and returns its features.
fn read_neg(s: &mut TcpStream) -> PeerResult<Vec<(u32, Vec<u8>)>> {
    let head = read_n(s, 12)?;
    if &head[..8] != MAGIC {
        return Err(format!("bad magic {:?}", &head[..8]));
    }
    let extra = read_n(s, u32_at(&head, 8) as usize)?;
    let mut out = Vec::new();
    let mut p = 0;
    while p < extra.len() {
        let f = u32_at(&extra, p);
        let l = u32_at(&extra, p + 4) as usize;
        out.push((f, extra[p + 8..p + 8 + l].to_vec()));
        p += 8 + l;
    }
    Ok(out)
}

fn has_feature(fs: &[(u32, Vec<u8>)], f: u32) -> bool {
    fs.iter().any(|(k, _)| *k == f)
}

/// A request frame, with the timeout field when `timeout` is set.
fn request_frame(timeout: Option<u64>, verb: u64, msg_id: i64, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if let Some(t) = timeout {
        v.extend_from_slice(&t.to_le_bytes());
    }
    v.extend_from_slice(&verb.to_le_bytes());
    v.extend_from_slice(&msg_id.to_le_bytes());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// A response frame, with the handler duration field when `ht` is set.
fn response_frame(msg_id: i64, ht: Option<u32>, payload: &[u8]) -> Vec<u8> {
    let mut v = msg_id.to_le_bytes().to_vec();
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    if let Some(h) = ht {
        v.extend_from_slice(&h.to_le_bytes());
    }
    v.extend_from_slice(payload);
    v
}

/// Reads a response: message id, handler duration (when `with_ht`), payload.
fn read_response(s: &mut TcpStream, with_ht: bool) -> PeerResult<(i64, Option<u32>, Vec<u8>)> {
    let head = read_n(s, if with_ht { 16 } else { 12 })?;
    let id = u64_at(&head, 0) as i64;
    let len = u32_at(&head, 8) as usize;
    let ht = if with_ht { Some(u32_at(&head, 12)) } else { None };
    let payload = read_n(s, len)?;
    Ok((id, ht, payload))
}

/// Reads a request without the timeout field: verb, message id, payload.
fn read_request(s: &mut TcpStream) -> PeerResult<(u64, i64, Vec<u8>)> {
    let head = read_n(s, 20)?;
    let verb = u64_at(&head, 0);
    let id = u64_at(&head, 8) as i64;
    let payload = read_n(s, u32_at(&head, 16) as usize)?;
    Ok((verb, id, payload))
}

/// Everything the peer sends until it closes its side.
fn read_to_eof(s: &mut TcpStream) -> PeerResult<Vec<u8>> {
    let mut v = Vec::new();
    io(s.read_to_end(&mut v))?;
    Ok(v)
}

fn error_payload(kind: u32, data: &[u8]) -> Vec<u8> {
    let mut v = kind.to_le_bytes().to_vec();
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    v
}

/// Connects to the engine server on `port`, with a read timeout against hangs.
fn raw_connect(port: u16) -> PeerResult<TcpStream> {
    let s = io(TcpStream::connect(("127.0.0.1", port)))?;
    io(s.set_read_timeout(Some(Duration::from_secs(10))))?;
    io(s.set_nodelay(true))?;
    Ok(s)
}

/// Runs `script` in a plain thread against the engine server on `port`.
fn raw_client<T: Send + 'static>(
    port: u16,
    script: impl FnOnce(&mut TcpStream) -> PeerResult<T> + Send + 'static,
) -> JoinHandle<PeerResult<T>> {
    std::thread::spawn(move || {
        let mut s = raw_connect(port)?;
        script(&mut s)
    })
}

/// A listener for a raw server; its port.
fn raw_listener() -> Result<(TcpListener, u16), ExceptionPtr> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(ExceptionPtr::from)?;
    let p = l.local_addr().map_err(ExceptionPtr::from)?.port();
    Ok((l, p))
}

/// Runs `script` in a plain thread on the first connection `l` accepts.
fn raw_server<T: Send + 'static>(
    l: TcpListener,
    script: impl FnOnce(&mut TcpStream) -> PeerResult<T> + Send + 'static,
) -> JoinHandle<PeerResult<T>> {
    std::thread::spawn(move || {
        let (mut s, _) = io(l.accept())?;
        io(s.set_read_timeout(Some(Duration::from_secs(10))))?;
        io(s.set_nodelay(true))?;
        script(&mut s)
    })
}

/// Waits, from an engine thread, for the peer thread `h` and returns its result.
fn join<T>(h: JoinHandle<PeerResult<T>>) -> Result<T, ExceptionPtr> {
    for _ in 0..1500 {
        if h.is_finished() {
            break;
        }
        sleep(Duration::from_millis(10)).get()?;
    }
    assert!(h.is_finished(), "the raw peer did not finish");
    match h.join() {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => panic!("raw peer failed: {}", e),
        Err(_) => panic!("raw peer panicked"),
    }
}

// ---- the engine side ----

const LOCALHOST: u32 = 0x7f00_0001;

fn port_of(a: &SocketAddress) -> u16 {
    u16::from_be(a.as_posix_sockaddr_in().sin_port)
}

fn fixed_listen() -> Result<(ServerSocket, SocketAddress), ExceptionPtr> {
    let mut lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    lo.set_fixed_cpu(this_shard_id());
    let ss = listen_with(make_ipv4_address_ip_port(LOCALHOST, 0), lo)?;
    let addr = ss.local_address()?;
    Ok((ss, addr))
}

/// A protocol with a started server on shard 0 (server first: dropped before the protocol).
struct Env {
    server: Pin<Box<Server>>,
    proto: Pin<Box<Proto>>,
    port: u16,
}

impl Env {
    fn new(so: ServerOptions) -> Result<Env, ExceptionPtr> {
        Self::with_limits(so, ResourceLimits::default())
    }

    fn with_limits(so: ServerOptions, limits: ResourceLimits) -> Result<Env, ExceptionPtr> {
        let mut proto = Box::pin(Proto::new(Serializer));
        proto.as_mut().set_logger(Some(&**RPC_LOG));
        proto.as_mut().register_handler(1, |x: i64| -> Result<i64, ExceptionPtr> { Ok(x * 2) })?;
        let (ss, addr) = fixed_listen()?;
        let mut server = Box::pin(proto.as_mut().new_server_with_options_socket(so, ss, limits));
        server.as_mut().start()?;
        Ok(Env { server, proto, port: port_of(&addr) })
    }

    fn server(&mut self) -> &mut Server {
        // SAFETY: the server is not moved out of its pin.
        unsafe { self.server.as_mut().get_unchecked_mut() }
    }

    fn connections(&mut self) -> usize {
        let mut n = 0;
        self.server().foreach_connection(|_| n += 1);
        n
    }

    fn finish(mut self) -> Result<(), ExceptionPtr> {
        self.server().stop().get()?;
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

/// A protocol and a started client of the raw server on `port`.
fn raw_server_client(
    port: u16,
    co: ClientOptions,
) -> Result<(Pin<Box<Client>>, Pin<Box<Proto>>), ExceptionPtr> {
    let mut proto = Box::pin(Proto::new(Serializer));
    proto.as_mut().set_logger(Some(&**RPC_LOG));
    let addr = make_ipv4_address_ip_port(LOCALHOST, port);
    let mut c =
        Box::pin(proto.as_mut().new_client_with_options(co, &addr, &SocketAddress::default()));
    c.as_mut().start()?;
    Ok((c, proto))
}

/// Stops the client, then drops it before its protocol.
fn finish_client(mut c: Pin<Box<Client>>, proto: Pin<Box<Proto>>) -> Result<(), ExceptionPtr> {
    cm(&mut c).stop().get()?;
    drop(c);
    drop(proto);
    Ok(())
}

// ---- server side ----

teseladb::engine_test_case!(the_server_drops_peers_with_bad_negotiation_frames, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::new(ServerOptions::default())?;
        let cases: Vec<(Vec<u8>, bool)> = vec![
            // A wrong magic.
            ([b"XXXXXXXX".to_vec(), 0u32.to_le_bytes().to_vec()].concat(), false),
            // The fixed part cut short.
            (MAGIC[..5].to_vec(), true),
            // Fewer feature bytes than announced.
            ([MAGIC.to_vec(), 20u32.to_le_bytes().to_vec(), vec![1, 2, 3]].concat(), true),
            // A feature record shorter than its header.
            ([MAGIC.to_vec(), 4u32.to_le_bytes().to_vec(), vec![0; 4]].concat(), false),
            // A feature whose data runs past the frame.
            (
                [
                    MAGIC.to_vec(),
                    8u32.to_le_bytes().to_vec(),
                    0u32.to_le_bytes().to_vec(),
                    100u32.to_le_bytes().to_vec(),
                ]
                .concat(),
                false,
            ),
        ];
        for (bytes, close_write) in cases {
            let h = raw_client(env.port, move |s| {
                io(s.write_all(&bytes))?;
                if close_write {
                    io(s.shutdown(Shutdown::Write))?;
                }
                read_to_eof(s)
            });
            // The server answers nothing and closes the connection.
            assert!(join(h)?.is_empty());
        }
        wait_until("the server to drop every peer", || env.connections() == 0)?;
        assert!(logged("wrong protocol magic: 5858585858585858"));
        assert!(logged("unexpected eof during negotiation frame"));
        assert!(logged("bad feature data format in negotiation frame"));
        assert!(logged("buffer underflow in feature data in negotiation frame"));
        env.finish()
    })
});

teseladb::engine_test_case!(the_server_reads_requests_split_over_segments_and_short_headers, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::new(ServerOptions::default())?;
        let h = raw_client(env.port, |s| {
            io(s.write_all(&neg_frame(&[(UNKNOWN_FEATURE, b"zz")])))?;
            let reply = read_neg(s)?;
            // The server answers none of the features: it knows none of them.
            if !reply.is_empty() {
                return Err(format!("unexpected features {:?}", reply));
            }
            // A request whose payload comes in two segments.
            let req = request_frame(None, 1, 7, &21i64.to_ne_bytes());
            io(s.write_all(&req[..23]))?;
            io(s.flush())?;
            std::thread::sleep(Duration::from_millis(50));
            io(s.write_all(&req[23..]))?;
            let (id, _, payload) = read_response(s, false)?;
            // A request for a verb nobody registered.
            io(s.write_all(&request_frame(None, 99, 8, &[])))?;
            let (uid, _, upayload) = read_response(s, false)?;
            // A request header cut short.
            io(s.write_all(&[1, 2, 3, 4, 5]))?;
            io(s.shutdown(Shutdown::Write))?;
            let rest = read_to_eof(s)?;
            Ok((id, payload, uid, upayload, rest))
        });
        let (id, payload, uid, upayload, rest) = join(h)?;
        assert_eq!(id, 7);
        assert_eq!(payload, 42i64.to_ne_bytes());
        assert_eq!(uid, -8);
        assert_eq!(upayload, error_payload(1, &99u64.to_le_bytes()));
        assert!(rest.is_empty());
        wait_until("the server to drop the peer", || env.connections() == 0)?;
        assert!(logged("unexpected eof on a server while reading header: expected 20 got 5"));
        env.finish()
    })
});

teseladb::engine_test_case!(the_server_reads_frames_with_timeouts_and_handler_durations, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let mut env = Env::new(ServerOptions::default())?;
        let h = raw_client(env.port, |s| {
            io(s.write_all(&neg_frame(&[(TIMEOUT, b""), (HANDLER_DURATION, b"")])))?;
            let reply = read_neg(s)?;
            if !has_feature(&reply, TIMEOUT) || !has_feature(&reply, HANDLER_DURATION) {
                return Err(format!("features not granted: {:?}", reply));
            }
            io(s.write_all(&request_frame(Some(5_000), 1, 9, &4i64.to_ne_bytes())))?;
            let r = read_response(s, true)?;
            // A request whose payload is cut short.
            let mut cut = request_frame(Some(0), 1, 10, &[0; 100]);
            cut.truncate(28 + 10);
            io(s.write_all(&cut))?;
            io(s.shutdown(Shutdown::Write))?;
            let rest = read_to_eof(s)?;
            Ok((r, rest))
        });
        let ((id, ht, payload), rest) = join(h)?;
        assert_eq!(id, 9);
        assert!(ht.is_some_and(|h| h != u32::MAX));
        assert_eq!(payload, 8i64.to_ne_bytes());
        assert!(rest.is_empty());
        wait_until("the data log", || {
            logged("unexpected eof on a server while reading data: expected 100 got 10")
        })?;

        // A header with the timeout field, cut short.
        let h = raw_client(env.port, |s| {
            io(s.write_all(&neg_frame(&[(TIMEOUT, b"")])))?;
            read_neg(s)?;
            io(s.write_all(&[0; 10]))?;
            io(s.shutdown(Shutdown::Write))?;
            read_to_eof(s)
        });
        assert!(join(h)?.is_empty());
        wait_until("the header log", || {
            logged("unexpected eof on a server while reading header: expected 28 got 10")
        })?;
        wait_until("the server to drop the peers", || env.connections() == 0)?;
        env.finish()
    })
});

teseladb::engine_test_case!(the_server_drops_short_and_corrupt_compressed_frames, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        static LZ4: Lz4CompressorFactory = Lz4CompressorFactory;
        let so = ServerOptions { compressor_factory: Some(&LZ4), ..ServerOptions::default() };
        let mut env = Env::new(so)?;
        let mut corrupt = 20u32.to_le_bytes().to_vec();
        corrupt.extend_from_slice(&100u32.to_le_bytes());
        corrupt.extend_from_slice(&[0xFF; 16]);
        let mut short_data = 50u32.to_le_bytes().to_vec();
        short_data.extend_from_slice(&[7; 10]);
        let cases: Vec<(Vec<u8>, bool)> =
            vec![(vec![1, 2], true), (short_data, true), (corrupt, false)];
        for (bytes, close_write) in cases {
            let h = raw_client(env.port, move |s| {
                io(s.write_all(&neg_frame(&[(COMPRESS, b"LZ4")])))?;
                let reply = read_neg(s)?;
                if !reply.iter().any(|(k, v)| *k == COMPRESS && v == b"LZ4") {
                    return Err(format!("compression not granted: {:?}", reply));
                }
                io(s.write_all(&bytes))?;
                if close_write {
                    io(s.shutdown(Shutdown::Write))?;
                }
                read_to_eof(s)
            });
            assert!(join(h)?.is_empty());
        }
        wait_until("the server to drop the peers", || env.connections() == 0)?;
        assert!(logged(
            "unexpected eof on a server while reading compression header: expected 4 got 2"
        ));
        assert!(logged("unexpected eof on a server while reading compressed data: expected 50 got 10"));
        assert!(logged("server connection dropped"));
        env.finish()
    })
});

teseladb::engine_test_case!(replies_queued_behind_a_blocked_socket_fail_when_the_peer_resets, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let limits = ResourceLimits { max_memory: 1000, ..ResourceLimits::default() };
        let mut env = Env::with_limits(ServerOptions::default(), limits)?;
        // Verb 2 answers a string of the size asked for.
        env.proto.as_mut().register_handler(2, |n: u32| -> Result<Sstring, ExceptionPtr> {
            Ok(Sstring::filled(n as usize, b'z'))
        })?;
        let port = env.port;
        let h = std::thread::spawn(move || -> PeerResult<()> {
            let mut s = raw_connect(port)?;
            io(s.write_all(&neg_frame(&[])))?;
            read_neg(&mut s)?;
            // Replies far larger than the socket buffers, which this peer never reads.
            let mut out = Vec::new();
            for id in 1..=4 {
                out.extend(request_frame(None, 2, id, &(8u32 << 20).to_ne_bytes()));
            }
            // A request over the memory limit: its error reply queues behind the others.
            out.extend(request_frame(None, 2, 5, &[0u8; 2000]));
            io(s.write_all(&out))?;
            std::thread::sleep(Duration::from_millis(300));
            // Closing with unread data resets the connection.
            drop(s);
            Ok(())
        });
        join(h)?;
        wait_until("the queued replies to fail", || {
            logged("got exception while processing a message")
                && logged("got exception while processing an oversized message")
        })?;
        assert!(logged("large than memory limit 1000"));
        wait_until("the server to drop the peer", || env.connections() == 0)?;
        env.finish()
    })
});

teseladb::engine_test_case!(stream_connections_need_a_known_parent_on_a_streaming_server, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let so = ServerOptions {
            streaming_domain: Some(StreamingDomainType::new(77)),
            ..ServerOptions::default()
        };
        let mut env = Env::new(so)?;
        let port = env.port;
        // The parent connection: the server gives it an id.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let parent = raw_client(port, move |s| {
            io(s.write_all(&neg_frame(&[])))?;
            let reply = read_neg(s)?;
            let id = reply
                .iter()
                .find(|(k, _)| *k == CONNECTION_ID)
                .map(|(_, v)| v.clone())
                .ok_or("no connection id")?;
            tx.send(id).map_err(|e| e.to_string())?;
            // Wait until the streams are tried, then make one more call.
            done_rx.recv().map_err(|e| e.to_string())?;
            io(s.write_all(&request_frame(None, 1, 1, &3i64.to_ne_bytes())))?;
            let r = read_response(s, false)?;
            io(s.shutdown(Shutdown::Write))?;
            read_to_eof(s)?;
            Ok(r)
        });
        let mut parent_id = None;
        wait_until("the parent id", || {
            if let Ok(v) = rx.try_recv() {
                parent_id = Some(v);
            }
            parent_id.is_some()
        })?;
        let parent_id = parent_id.unwrap_or_default();
        assert_eq!(parent_id.len(), 8);

        // A stream of that parent, whose first frame header is cut short.
        let pid = parent_id.clone();
        let h = raw_client(port, move |s| {
            io(s.write_all(&neg_frame(&[(STREAM_PARENT, &pid)])))?;
            let reply = read_neg(s)?;
            if !has_feature(&reply, CONNECTION_ID) {
                return Err("no stream id".to_string());
            }
            io(s.write_all(&[1, 2]))?;
            io(s.shutdown(Shutdown::Write))?;
            read_to_eof(s)
        });
        assert!(join(h)?.is_empty());
        wait_until("the stream header log", || {
            logged("unexpected eof on a stream while reading header: expected 4 got 2")
        })?;

        // Streams of a parent nobody knows, and of a parent on a shard with no such server.
        let unknown = ConnectionId::make_id(999_999, 0).id().to_le_bytes().to_vec();
        let other_shard = ConnectionId::make_id(1, 1).id().to_le_bytes().to_vec();
        for parent in [unknown, other_shard] {
            let h = raw_client(port, move |s| {
                io(s.write_all(&neg_frame(&[(STREAM_PARENT, &parent)])))?;
                read_to_eof(s)
            });
            assert!(join(h)?.is_empty());
        }
        wait_until("the unknown parent log", || logged("Unknown parent connection"))?;
        wait_until("the other shard log", || {
            logged("Shard 1 does not have server with streaming domain 77")
        })?;

        // The parent still works.
        done_tx.send(()).map_err(|e| ExceptionPtr::new(RuntimeError(e.to_string())))?;
        let (id, _, payload) = join(parent)?;
        assert_eq!(id, 1);
        assert_eq!(payload, 6i64.to_ne_bytes());
        wait_until("the server to drop the peers", || env.connections() == 0)?;
        env.finish()?;

        // A server without a streaming domain refuses stream connections.
        let mut plain = Env::new(ServerOptions::default())?;
        let h = raw_client(plain.port, |s| {
            io(s.write_all(&neg_frame(&[(STREAM_PARENT, &[1, 0, 0, 0, 0, 0, 0, 0])])))?;
            read_to_eof(s)
        });
        assert!(join(h)?.is_empty());
        wait_until("the refusal log", || logged("streaming is not configured for the server"))?;
        wait_until("the server to drop the peer", || plain.connections() == 0)?;
        plain.finish()
    })
});

// ---- client side ----

teseladb::engine_test_case!(a_client_rejects_a_server_with_a_wrong_magic, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let (l, port) = raw_listener()?;
        let h = raw_server(l, |s| {
            read_neg(s)?;
            io(s.write_all(&[b"NOTRPC!!".as_slice(), &0u32.to_le_bytes()[..]].concat()))?;
            read_to_eof(s)
        });
        let (mut c, proto) = raw_server_client(port, ClientOptions::default())?;
        let r = cm(&mut c).await_connection().get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        let v = proto.make_client::<fn(i32) -> Future<i32>>(1);
        assert!(v.call(cm(&mut c), &(1,)).get().is_err());
        assert!(logged("wrong protocol magic: 4e4f545250432121"));
        finish_client(c, proto)?;
        join(h)?;
        Ok(())
    })
});

teseladb::engine_test_case!(a_client_rejects_a_compression_it_does_not_support, {
    async_(|| -> Result<(), ExceptionPtr> {
        static LZ4: Lz4CompressorFactory = Lz4CompressorFactory;
        let with_lz4 = ClientOptions { compressor_factory: Some(&LZ4), ..ClientOptions::default() };
        for (co, answer, expected) in [
            (with_lz4, "ZSTD", "RPC server responded with compression ZSTD - unsupported"),
            (ClientOptions::default(), "LZ4", "RPC server responded with compression LZ4 - unsupported"),
        ] {
            let (l, port) = raw_listener()?;
            let h = raw_server(l, move |s| {
                read_neg(s)?;
                io(s.write_all(&neg_frame(&[(COMPRESS, answer.as_bytes())])))?;
                read_to_eof(s)
            });
            let (mut c, proto) = raw_server_client(port, co)?;
            let r = cm(&mut c).await_connection().get();
            assert!(r.is_err_and(|e| exc_what(&e) == expected));
            finish_client(c, proto)?;
            join(h)?;
        }
        Ok(())
    })
});

teseladb::engine_test_case!(a_client_skips_replies_nobody_waits_for_and_records_durations, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let (l, port) = raw_listener()?;
        let h = raw_server(l, |s| {
            let asked = read_neg(s)?;
            let conn_id = 0x1234_0000u64.to_le_bytes();
            io(s.write_all(&neg_frame(&[
                (HANDLER_DURATION, b""),
                (UNKNOWN_FEATURE, b"x"),
                (CONNECTION_ID, &conn_id),
            ])))?;
            let (verb, id, payload) = read_request(s)?;
            let x = i32::from_ne_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let mut out = Vec::new();
            // A reply to a call nobody made.
            out.extend(response_frame(1_000, Some(5), &[]));
            // Error replies to calls nobody made: unknown verb, user error, cut short.
            out.extend(response_frame(-1_001, Some(5), &error_payload(1, &42u64.to_le_bytes())));
            out.extend(response_frame(-1_002, Some(5), &error_payload(0, b"boom")));
            out.extend(response_frame(-1_003, Some(5), &[1, 2]));
            // The real reply, with the duration of its handler.
            out.extend(response_frame(id, Some(1_500), &(x + 1).to_ne_bytes()));
            io(s.write_all(&out))?;
            // A second call, whose reply has no duration.
            let (_, id2, payload2) = read_request(s)?;
            let y = i32::from_ne_bytes([payload2[0], payload2[1], payload2[2], payload2[3]]);
            io(s.write_all(&response_frame(id2, Some(u32::MAX), &(y + 1).to_ne_bytes())))?;
            // A reply header cut short.
            io(s.write_all(&[1, 2, 3, 4, 5]))?;
            io(s.shutdown(Shutdown::Write))?;
            read_to_eof(s)?;
            Ok((asked, verb))
        });
        let (mut c, proto) = raw_server_client(port, ClientOptions::default())?;
        let v = proto.make_client::<fn(i32) -> Future<i32>>(3);
        assert_eq!(v.call(cm(&mut c), &(5,)).get()?, 6);
        assert!(cm(&mut c).get_connection_id() == ConnectionId::new(0x1234_0000));
        let st = cm(&mut c).get_stats();
        assert_eq!(st.replied, 1);
        assert_eq!(st.delay_samples, 1);
        assert_eq!(st.exception_received, 0);
        assert!(logged("got a reply for an expired message id"));
        assert!(logged("unknown verb exception 42 ignored"));
        assert!(logged("ignoring error response: boom"));
        assert!(logged("ignoring error response: deserialization buffer underflow"));
        assert_eq!(v.call(cm(&mut c), &(10,)).get()?, 11);
        assert_eq!(cm(&mut c).get_stats().delay_samples, 1);
        wait_until("the cut reply log", || {
            logged("unexpected eof on a client while reading header: expected 16 got 5")
        })?;
        let r = v.call(cm(&mut c), &(1,)).get();
        assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
        finish_client(c, proto)?;
        let (asked, verb) = join(h)?;
        assert_eq!(verb, 3);
        // The client asked for timeouts and handler durations.
        assert!(has_feature(&asked, TIMEOUT));
        assert!(has_feature(&asked, HANDLER_DURATION));
        Ok(())
    })
});

teseladb::engine_test_case!(a_client_drops_a_connection_whose_replies_are_cut_short, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let co = ClientOptions { send_handler_duration: false, ..ClientOptions::default() };
        // The payload is cut short, then the header.
        let cuts: [(Vec<u8>, &str); 2] = [
            (
                [7i64.to_le_bytes().as_slice(), &10u32.to_le_bytes()[..], &[1u8, 2, 3][..]].concat(),
                "unexpected eof on a client while reading data: expected 10 got 3",
            ),
            (vec![1, 2, 3, 4, 5], "unexpected eof on a client while reading header: expected 12 got 5"),
        ];
        for (cut, log) in cuts {
            let (l, port) = raw_listener()?;
            let h = raw_server(l, move |s| {
                read_neg(s)?;
                io(s.write_all(&neg_frame(&[])))?;
                read_request(s)?;
                io(s.write_all(&cut))?;
                io(s.shutdown(Shutdown::Write))?;
                read_to_eof(s)
            });
            let (mut c, proto) = raw_server_client(port, co.clone())?;
            let v = proto.make_client::<fn(i32) -> Future<i32>>(1);
            let r = v.call(cm(&mut c), &(1,)).get();
            assert!(r.is_err_and(|e| is_rpc_error(&e, ErrorKind::Closed)));
            wait_until("the cut reply log", || logged(log))?;
            finish_client(c, proto)?;
            join(h)?;
        }
        Ok(())
    })
});
