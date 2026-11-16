//! Tests of the RPC building blocks that need no connection: frames of one and of several
//! chunks, frames handed between shards, the marshalling of plain values, smart pointers,
//! references, tuples and optional values, the decoding of error replies, the protocol
//! logger, the default isolation, copyable filters and isolation functions, the conversions
//! of the verb types, deadlines and connection ids.
//!
//! The test program runs its reactor with two shards (set before `main`), so that a frame
//! can be made on one shard and copied on the other.
#![cfg(target_os = "linux")]
// The tests write into engine buffers through raw pointers; every `unsafe` block states why
// it is sound.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_ready_future};
use teseladb::engine::core::lowres_clock::LowresClock;
use teseladb::engine::core::scheduling::current_scheduling_group;
use teseladb::engine::core::sharded::{ForeignPtr, make_foreign, this_shard_id};
use teseladb::engine::core::shared_ptr::{make_lw_shared, make_shared};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::make_ipv4_address_ip_port;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::rpc::rpc::Client;
use teseladb::engine::rpc::rpc::{
    Connection, ConnectionFilterFn, CopyableFn, IsolationConfig, Logger as RpcLogger,
    ProtocolFeatures, ReplyHandlerBase, ReplyHandlerData, ResourceLimits, RpcTimePoint,
    ShardLocalBuffer, StreamingDomainType, default_isolate_connection, estimate_request_size,
    make_shard_local_buffer_copy,
};
use teseladb::engine::rpc::rpc_impl::{
    ExceptionType, Read, RpcMsgType, SerializerInput, SerializerOutput, Write,
    deserialize_connection_id, marshall, relative_timeout_to_absolute, send_helper,
    serialize_connection_id, unmarshal_exception,
};
use teseladb::engine::rpc::rpc_types::{
    Bufs, ClientInfo, ConnectionId, ErrorKind, NoWaitType, Optional, RcvBuf, SndBuf, Tuple, Type,
};
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine::util::log::{LogLevel, Logger, Ostream};

/// Starts the test reactor with two shards before `main` (and before any test asks for it),
/// so the cross-shard cases have a second shard on any machine.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov_rpc_units_test".to_string(), "--smp=2".to_string()];
        global_test_runner().start(&args);
    }
    start
};

/// The serializer of the tests: values in native byte order, strings with a 32-bit length.
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

/// An input over bytes held by the test, for the readers that need no connection.
struct BytesInput {
    data: Vec<u8>,
    pos: usize,
}

impl BytesInput {
    fn new(data: Vec<u8>) -> Self {
        BytesInput { data, pos: 0 }
    }
}

impl SerializerInput for BytesInput {
    fn read(&mut self, out: &mut [u8]) -> Result<(), ExceptionPtr> {
        if self.data.len() - self.pos < out.len() {
            return Err(ExceptionPtr::new(RuntimeError("short input".to_string())));
        }
        out.copy_from_slice(&self.data[self.pos..self.pos + out.len()]);
        self.pos += out.len();
        Ok(())
    }
    fn size(&self) -> usize {
        self.data.len() - self.pos
    }
    fn connection(&self) -> *mut Connection {
        std::ptr::null_mut()
    }
}

/// The bytes of a frame, its buffers joined.
fn bufs_bytes(bufs: &Bufs) -> Vec<u8> {
    match bufs {
        Bufs::One(b) => b.as_slice().to_vec(),
        Bufs::Many(v) => v.iter().flat_map(|b| b.as_slice().to_vec()).collect(),
    }
}

/// Fills every buffer of `bufs` with the pattern `i % 251`, counted over the whole frame.
fn fill_pattern(bufs: &mut Bufs) {
    let mut n = 0usize;
    let mut fill = |b: &mut TemporaryBuffer<u8>| {
        let len = b.size();
        // SAFETY: the buffer owns `len` writable bytes.
        let s = unsafe { std::slice::from_raw_parts_mut(b.get_write(), len) };
        for c in s {
            *c = (n % 251) as u8;
            n += 1;
        }
    };
    match bufs {
        Bufs::One(b) => fill(b),
        Bufs::Many(v) => v.iter_mut().for_each(fill),
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn exc_what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

// ---- frames ----

#[test]
fn a_frame_larger_than_a_chunk_is_split_into_chunks() {
    let size = 2 * SndBuf::CHUNK_SIZE + 100;
    let mut b = SndBuf::with_size(size);
    assert_eq!(b.size as usize, size);
    match &b.bufs {
        Bufs::Many(v) => {
            assert_eq!(v.len(), 3);
            assert_eq!(v[0].size(), SndBuf::CHUNK_SIZE);
            assert_eq!(v[1].size(), SndBuf::CHUNK_SIZE);
            assert_eq!(v[2].size(), 100);
        }
        Bufs::One(_) => panic!("a frame of {} bytes must have several chunks", size),
    }
    // The front of a chunked frame is its first chunk.
    assert_eq!(b.front().size(), SndBuf::CHUNK_SIZE);
    // SAFETY: the first chunk has at least one writable byte.
    unsafe { *b.front().get_write() = 0xAB };
    match &b.bufs {
        Bufs::Many(v) => assert_eq!(v[0].as_slice()[0], 0xAB),
        Bufs::One(_) => unreachable!(),
    }

    let small = SndBuf::with_size(10);
    assert!(matches!(small.bufs, Bufs::One(ref x) if x.size() == 10));
}

#[test]
fn shard_local_buffers_report_their_size_and_buffers() {
    let mut s = <SndBuf as ShardLocalBuffer>::sized(7);
    assert_eq!(s.size_of(), 7);
    assert!(matches!(s.bufs(), Bufs::Many(v) if v.is_empty()));
    *s.bufs_mut() = Bufs::One(TemporaryBuffer::copy_from(b"abc"));
    assert_eq!(bufs_bytes(s.bufs()), b"abc");

    let mut r = <RcvBuf as ShardLocalBuffer>::sized(9);
    assert_eq!(r.size_of(), 9);
    *r.bufs_mut() =
        Bufs::Many(vec![TemporaryBuffer::copy_from(b"ab"), TemporaryBuffer::copy_from(b"c")]);
    assert_eq!(bufs_bytes(r.bufs()), b"abc");
}

/// Makes a frame of `size` bytes on shard 1, filled with the pattern, and copies it on this
/// shard; the copy must hold the same bytes without owning them.
fn copy_from_other_shard_snd(size: usize) -> Result<(), ExceptionPtr> {
    let foreign: ForeignPtr<Box<SndBuf>> = Smp::submit_to(1, move || {
        let mut b = SndBuf::with_size(size);
        fill_pattern(&mut b.bufs);
        Ok::<ForeignPtr<Box<SndBuf>>, ExceptionPtr>(make_foreign(Box::new(b)))
    })
    .get()?;
    assert_eq!(foreign.get_owner_shard(), 1);
    let local = make_shard_local_buffer_copy(foreign);
    assert_eq!(local.size as usize, size);
    assert_eq!(bufs_bytes(&local.bufs), pattern(size));
    let chunks = match &local.bufs {
        Bufs::One(_) => 1,
        Bufs::Many(v) => v.len(),
    };
    assert_eq!(chunks, size.div_ceil(SndBuf::CHUNK_SIZE).max(1));
    // Dropping the copy sends the source frame back to shard 1 to be freed.
    drop(local);
    Ok(())
}

fn copy_from_other_shard_rcv(pieces: &'static [usize]) -> Result<(), ExceptionPtr> {
    let total: usize = pieces.iter().sum();
    let foreign: ForeignPtr<Box<RcvBuf>> = Smp::submit_to(1, move || {
        let mut v: Vec<TemporaryBuffer<u8>> =
            pieces.iter().map(|&n| TemporaryBuffer::<u8>::new(n)).collect();
        let mut bufs = if v.len() == 1 { Bufs::One(v.remove(0)) } else { Bufs::Many(v) };
        fill_pattern(&mut bufs);
        let r = RcvBuf { size: total as u32, su: None, bufs };
        Ok::<ForeignPtr<Box<RcvBuf>>, ExceptionPtr>(make_foreign(Box::new(r)))
    })
    .get()?;
    let local = make_shard_local_buffer_copy(foreign);
    assert_eq!(local.size as usize, total);
    assert_eq!(bufs_bytes(&local.bufs), pattern(total));
    Ok(())
}

teseladb::engine_test_case!(frames_made_on_another_shard_are_shared_not_copied, {
    async_(|| -> Result<(), ExceptionPtr> {
        assert!(Smp::count() >= 2, "the test needs two shards");
        copy_from_other_shard_snd(100)?;
        copy_from_other_shard_snd(2 * SndBuf::CHUNK_SIZE + 17)?;
        copy_from_other_shard_rcv(&[33])?;
        copy_from_other_shard_rcv(&[10, 20, 5])?;

        // On the owner shard the frame itself is handed back.
        let mut own = SndBuf::with_size(5);
        fill_pattern(&mut own.bufs);
        let foreign = make_foreign(Box::new(own));
        assert_eq!(foreign.get_owner_shard(), this_shard_id());
        let back = make_shard_local_buffer_copy(foreign);
        assert_eq!(bufs_bytes(&back.bufs), pattern(5));
        Ok(())
    })
});

// ---- marshalling ----

#[test]
fn marshalled_values_and_pointers_have_the_same_bytes() {
    let plain = marshall(&Serializer, 0, &(5i32,)).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    let expected = bufs_bytes(&plain.bufs);
    assert_eq!(expected, 5i32.to_ne_bytes());

    let five = 5i32;
    let by_ref = marshall(&Serializer, 0, &(&five,)).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(bufs_bytes(&by_ref.bufs), expected);
    let boxed =
        marshall(&Serializer, 0, &(Box::new(5i32),)).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(bufs_bytes(&boxed.bufs), expected);
    let shared = marshall(&Serializer, 0, &(make_shared(5i32),))
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(bufs_bytes(&shared.bufs), expected);
    let lw = marshall(&Serializer, 0, &(make_lw_shared(5i32),))
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(bufs_bytes(&lw.bufs), expected);
    let foreign = marshall(&Serializer, 0, &(make_foreign(Box::new(5i32)),))
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(bufs_bytes(&foreign.bufs), expected);

    // Unit values and the no-wait marker write nothing; head space is left in front.
    let empty =
        marshall(&Serializer, 3, &((), NoWaitType)).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(empty.size, 3);

    // A tuple is written element by element.
    let t = marshall(&Serializer, 0, &(Tuple((1i32, 2i64)),))
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    let mut tb = 1i32.to_ne_bytes().to_vec();
    tb.extend_from_slice(&2i64.to_ne_bytes());
    assert_eq!(bufs_bytes(&t.bufs), tb);
}

#[test]
fn a_large_argument_is_marshalled_into_chunks() {
    let len = SndBuf::CHUNK_SIZE + 1000;
    let mut s = uninitialized_string(len);
    for (i, c) in s.as_bytes_mut().iter_mut().enumerate() {
        *c = (i % 251) as u8;
    }
    let head = 28;
    let out = marshall(&Serializer, head, &(s,)).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(out.size as usize, head + 4 + len);
    assert!(matches!(out.bufs, Bufs::Many(ref v) if v.len() == 2));
    let bytes = bufs_bytes(&out.bufs);
    assert_eq!(&bytes[head..head + 4], &(len as u32).to_ne_bytes());
    assert_eq!(&bytes[head + 4..], &pattern(len)[..]);
}

#[test]
fn the_measuring_stream_counts_what_is_written() {
    use teseladb::engine::core::simple_stream::MeasuringOutputStream;
    let mut m = MeasuringOutputStream::default();
    SerializerOutput::write(&mut m, b"abcd").unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    SerializerOutput::write(&mut m, b"ef").unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(m.size(), 6);
}

#[test]
fn readers_of_units_optionals_and_tuples() {
    let s = Serializer;
    let mut empty = BytesInput::new(Vec::new());
    <Serializer as Read<()>>::read(&s, &mut empty, Type::default())
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    let nw = <Serializer as Read<NoWaitType>>::read(&s, &mut empty, Type::default())
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(nw, NoWaitType);

    // An optional value is absent when the frame has no more data, present otherwise.
    let none = <Serializer as Read<Optional<i32>>>::read(&s, &mut empty, Type::default())
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(none.0, None);
    let mut seven = BytesInput::new(7i32.to_ne_bytes().to_vec());
    let some = <Serializer as Read<Optional<i32>>>::read(&s, &mut seven, Type::default())
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(some.0, Some(7));
    assert_eq!(seven.size(), 0);

    let mut tb = 3i32.to_ne_bytes().to_vec();
    tb.extend_from_slice(&(-4i64).to_ne_bytes());
    let mut ti = BytesInput::new(tb);
    let t = <Serializer as Read<Tuple<(i32, i64)>>>::read(&s, &mut ti, Type::default())
        .unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(t, Tuple((3, -4)));

    // A short frame fails the reader of the optional value.
    let mut short = BytesInput::new(vec![1, 2]);
    assert!(<Serializer as Read<Optional<i32>>>::read(&s, &mut short, Type::default()).is_err());
}

// ---- error replies ----

fn rcv(bytes: &[u8]) -> RcvBuf {
    RcvBuf::from_buffer(TemporaryBuffer::copy_from(bytes))
}

fn error_frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = kind.to_le_bytes().to_vec();
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn rpc_kind(e: &ExceptionPtr) -> Option<ErrorKind> {
    e.downcast_ref::<teseladb::engine::rpc::rpc_types::Error>().map(|x| x.kind())
}

#[test]
fn error_replies_decode_to_their_kind() {
    let mut user = rcv(&error_frame(ExceptionType::User as u32, b"it failed"));
    let e = unmarshal_exception(&mut user).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(rpc_kind(&e), Some(ErrorKind::RemoteVerb));
    assert_eq!(exc_what(&e), "it failed");

    let mut verb = rcv(&error_frame(ExceptionType::UnknownVerb as u32, &42u64.to_le_bytes()));
    let e = unmarshal_exception(&mut verb).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(rpc_kind(&e), Some(ErrorKind::UnknownVerb(42)));

    // Any other kind is an unknown exception, whatever follows.
    let mut other = rcv(&error_frame(77, b""));
    let e = unmarshal_exception(&mut other).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(rpc_kind(&e), Some(ErrorKind::UnknownException));

    // A message that is not UTF-8 is kept, lossily.
    let mut bad_utf8 = rcv(&error_frame(ExceptionType::User as u32, &[0x66, 0xFF, 0x67]));
    let e = unmarshal_exception(&mut bad_utf8).unwrap_or_else(|e| panic!("{}", exc_what(&e)));
    assert_eq!(exc_what(&e), "f\u{FFFD}g");

    // Frames too short for their header or their verb fail.
    assert!(unmarshal_exception(&mut rcv(&[1, 0])).is_err());
    let mut short_verb = rcv(&error_frame(ExceptionType::UnknownVerb as u32, &[1, 2, 3]));
    assert!(unmarshal_exception(&mut short_verb).is_err());
}

// ---- logger ----

/// Everything the loggers write, once `capture_logs` has redirected them.
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
    Logger::set_ostream(Ostream::Custom(Box::new(SharedBuf(LOG_BUF.clone()))));
}

fn logged(s: &str) -> bool {
    let v = match LOG_BUF.lock() {
        Ok(v) => v.clone(),
        Err(p) => p.into_inner().clone(),
    };
    String::from_utf8_lossy(&v).contains(s)
}

static UNITS_LOG: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("cov_rpc_units").unwrap_or_else(|e| panic!("{}", exc_what(&e))));

#[test]
fn the_protocol_logger_names_the_client_and_the_message() {
    capture_logs();
    UNITS_LOG.set_level(LogLevel::Debug);
    let mut l = RpcLogger::default();
    let addr = make_ipv4_address_ip_port(0x0102_0304, 5);
    let info = ClientInfo {
        addr,
        server: std::ptr::null_mut(),
        conn_id: ConnectionId::make_id(1, 0),
        user_data: Default::default(),
    };
    // Without an engine logger nothing is written.
    l.client(&info, &Sstring::from("unit-silent"));
    assert!(!logged("unit-silent"));

    l.set(Some(&**UNITS_LOG));
    l.client_msg(&info, 7, &Sstring::from("unit-msg"));
    l.client_msg_level(&info, 8, LogLevel::Warn, "unit-msg-level");
    l.client(&info, &Sstring::from("unit-client"));
    l.client_level(&info, LogLevel::Debug, "unit-client-level");
    assert!(logged("client 1.2.3.4:5 msg_id 7:  unit-msg"));
    assert!(logged("client 1.2.3.4:5 msg_id 8:  unit-msg-level"));
    assert!(logged("client 1.2.3.4:5: unit-client"));
    assert!(logged("client 1.2.3.4:5: unit-client-level"));
}

// ---- options and helpers ----

teseladb::engine_test_case!(the_default_isolation_is_the_current_group, {
    let sg = current_scheduling_group();
    assert!(IsolationConfig::default().sched_group == sg);
    assert!(default_isolate_connection(Sstring::from("any cookie")).sched_group == sg);
    make_ready_future(())
});

#[test]
fn filters_and_isolation_functions_are_copyable() {
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let filter: Box<dyn ConnectionFilterFn> = Box::new(move |a: &SocketAddress| {
        c.set(c.get() + 1);
        a.is_unspecified()
    });
    let copy = filter.clone();
    assert!(copy(&SocketAddress::default()));
    assert!(!filter(&make_ipv4_address_ip_port(0x7f00_0001, 1)));
    assert_eq!(calls.get(), 2);

    let f: Box<dyn CopyableFn<i32, i32>> = Box::new(|x: i32| x * 3);
    let g = f.clone();
    assert_eq!(g(4), 12);
    assert_eq!(f(5), 15);
}

#[test]
fn limits_estimate_the_memory_of_a_request() {
    let lim =
        ResourceLimits { basic_request_size: 100, bloat_factor: 3, ..ResourceLimits::default() };
    assert_eq!(estimate_request_size(&lim, 10), 130);
    assert_eq!(StreamingDomainType::new(77).to_string(), "77");
    assert!(ProtocolFeatures::COMPRESS < ProtocolFeatures::HANDLER_DURATION);
}

#[test]
fn verb_types_convert_to_and_from_the_wire() {
    assert_eq!(200u8.to_u64(), 200);
    assert_eq!(<u8 as RpcMsgType>::from_u64(0x1_05), 5);
    assert_eq!(<u16 as RpcMsgType>::from_u64(7), 7);
    assert_eq!(9u16.to_u64(), 9);
    assert_eq!(<u64 as RpcMsgType>::from_u64(u64::MAX), u64::MAX);
    assert_eq!((-1i8).to_u64(), u64::MAX);
    assert_eq!(<i8 as RpcMsgType>::from_u64(u64::MAX), -1);
    assert_eq!(<i16 as RpcMsgType>::from_u64(3), 3);
    assert_eq!((-2i16).to_u64(), u64::MAX - 1);
    assert_eq!(<i32 as RpcMsgType>::from_u64(4), 4);
    assert_eq!(5i32.to_u64(), 5);
    assert_eq!(<i64 as RpcMsgType>::from_u64(6), 6);
    assert_eq!(6i64.to_u64(), 6);
}

#[test]
fn a_verb_client_can_be_copied() {
    let h = send_helper::<Serializer, u32, Future<i32>, (i32,)>(3);
    let copy = h.clone();
    // Both copies are callables of the same verb; they carry no other state.
    drop(h);
    drop(copy);
}

teseladb::engine_test_case!(relative_deadlines_are_capped_at_the_end_of_time, {
    let before = LowresClock::now();
    let near = relative_timeout_to_absolute(Duration::from_millis(5).as_nanos() as i64);
    assert!(near >= before);
    assert!(relative_timeout_to_absolute(i64::MAX) == RpcTimePoint::max());
    make_ready_future(())
});

#[test]
fn connection_ids_round_trip_and_short_ids_are_padded() {
    let id = ConnectionId::make_id(0x1234, 3);
    let s = serialize_connection_id(&id);
    assert_eq!(s.size(), 8);
    assert!(deserialize_connection_id(&s) == id);
    let short = deserialize_connection_id(&Sstring::from_bytes(&[1, 2]));
    assert!(short == ConnectionId::new(0x0201));
}

// ---- reply handlers ----

/// A reply handler that keeps only the defaults of the trait.
struct DefaultsHandler {
    base: ReplyHandlerData,
    calls: Rc<Cell<u32>>,
}

impl ReplyHandlerBase for DefaultsHandler {
    fn data(&mut self) -> &mut ReplyHandlerData {
        &mut self.base
    }
    fn call(
        &mut self,
        _client: &mut Client,
        _msg_id: i64,
        _data: RcvBuf,
    ) -> Result<(), ExceptionPtr> {
        self.calls.set(self.calls.get() + 1);
        Ok(())
    }
}

#[test]
fn reply_handlers_ignore_timeouts_and_cancels_by_default() {
    let calls = Rc::new(Cell::new(0));
    let mut h = DefaultsHandler { base: ReplyHandlerData::default(), calls: calls.clone() };
    h.timeout();
    h.cancel();
    assert!(h.data().pcancel.is_null());
    assert_eq!(calls.get(), 0);
}
