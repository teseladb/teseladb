#![cfg(target_os = "linux")]
//! The memory limit of an RPC sink used away from the shard of its connection holds until
//! its frames are freed. A frame is made on the sink's shard and sent from the connection's
//! shard, which works on a copy that keeps the original alive until its bytes are written
//! out. Before, the sink took its units back as soon as the send returned, while the frame
//! could still be held on the other side, so a sink could pile up any number of frames.
//!
//! The stream connection's socket holds, once negotiated, every packet it is given (as a
//! peer that does not take data would): the sends return at once, and the frames stay
//! alive. With frames of 64 KiB against the 100 KiB limit, the second call must wait.

#![allow(unsafe_code)]

#[path = "common/loopback_socket.rs"]
mod loopback_socket;

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use loopback_socket::{
    LoopbackBuffer, LoopbackBufferType, LoopbackConnectedSocketImpl, LoopbackConnectionFactory,
};
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{DataSink, DataSinkImpl, DataSource};
use teseladb::engine::core::sharded::make_foreign;
use teseladb::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timer::{Clock, SteadyClockType};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb::engine::net::api::{
    ConnectedSocket, ConnectedSocketInputStreamConfig, KeepaliveParams, Socket, Transport,
    make_ipv4_address,
};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use teseladb::engine::net::stack::{ConnectedSocketImpl, SocketImpl};
use teseladb::engine::rpc::rpc::{
    Client, ClientOptions, Protocol, ResourceLimits, Server, ServerOptions, StreamingDomainType,
};
use teseladb::engine::rpc::rpc_impl::{Read, SerializerInput, SerializerOutput, Write};
use teseladb::engine::rpc::rpc_types::{MAX_STREAM_BUFFERS_MEMORY, Type};

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

// ---- a socket that can hold what is written to it ----

/// The packets a holding socket keeps, once `on` is set.
#[derive(Default)]
struct Hold {
    on: Cell<bool>,
    held: RefCell<Vec<Packet>>,
}

/// A sink that keeps the packets it is given while its hold is on.
struct HoldSink {
    sink: DataSink,
    hold: Rc<Hold>,
}

impl DataSinkImpl for HoldSink {
    fn put(&mut self, p: Packet) -> Future<()> {
        if self.hold.on.get() {
            self.hold.held.borrow_mut().push(p);
            return make_ready_future(());
        }
        self.sink.put(p)
    }
    fn flush(&mut self) -> Future<()> {
        if self.hold.on.get() {
            return make_ready_future(());
        }
        self.sink.flush()
    }
    fn close(&mut self) -> Future<()> {
        self.sink.close()
    }
}

/// A loopback connection whose sink is a [`HoldSink`].
struct HoldConnection {
    inner: LoopbackConnectedSocketImpl,
    hold: Rc<Hold>,
}

impl ConnectedSocketImpl for HoldConnection {
    fn source(&mut self) -> DataSource {
        self.inner.source()
    }
    fn source_with_config(&mut self, csisc: ConnectedSocketInputStreamConfig) -> DataSource {
        self.inner.source_with_config(csisc)
    }
    fn sink(&mut self) -> DataSink {
        DataSink::new(Box::new(HoldSink { sink: self.inner.sink(), hold: self.hold.clone() }))
    }
    fn shutdown_input(&mut self) {
        self.inner.shutdown_input()
    }
    fn shutdown_output(&mut self) {
        self.inner.shutdown_output()
    }
    fn set_nodelay(&mut self, nodelay: bool) -> Result<(), ExceptionPtr> {
        self.inner.set_nodelay(nodelay)
    }
    fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        self.inner.get_nodelay()
    }
    fn set_keepalive(&mut self, keepalive: bool) -> Result<(), ExceptionPtr> {
        self.inner.set_keepalive(keepalive)
    }
    fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        self.inner.get_keepalive()
    }
    fn set_keepalive_parameters(&mut self, p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        self.inner.set_keepalive_parameters(p)
    }
    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        self.inner.get_keepalive_parameters()
    }
    unsafe fn set_sockopt(
        &mut self,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the caller's contract.
        unsafe { self.inner.set_sockopt(level, optname, data, len) }
    }
    unsafe fn get_sockopt(
        &self,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: the caller's contract.
        unsafe { self.inner.get_sockopt(level, optname, data, len) }
    }
    fn local_address(&self) -> SocketAddress {
        self.inner.local_address()
    }
    fn remote_address(&self) -> SocketAddress {
        self.inner.remote_address()
    }
    fn wait_input_shutdown(&mut self) -> Future<()> {
        self.inner.wait_input_shutdown()
    }
}

/// A client socket that connects to the loopback server of the same shard.
struct HoldSocket {
    factory: *mut LoopbackConnectionFactory,
    hold: Rc<Hold>,
    /// The buffers of the connection, aborted by a shutdown.
    buffers: Vec<LwSharedPtr<LoopbackBuffer>>,
}

impl HoldSocket {
    fn new(factory: *mut LoopbackConnectionFactory, hold: Rc<Hold>) -> Self {
        HoldSocket { factory, hold, buffers: Vec::new() }
    }
}

impl SocketImpl for HoldSocket {
    fn connect(&mut self, _: SocketAddress, _: SocketAddress, _: Transport) -> Future<ConnectedSocket> {
        let b1 = make_lw_shared(LoopbackBuffer::new(None, LoopbackBufferType::ServerTx));
        let b2 = make_lw_shared(LoopbackBuffer::new(None, LoopbackBufferType::ClientTx));
        let hold = self.hold.clone();
        self.buffers = vec![b1.clone(), b2.clone()];
        // SAFETY: the factory outlives every socket of the test.
        unsafe { &mut *self.factory }
            .make_new_server_connection(make_foreign(b1.clone()), b2.clone())
            .then(move |()| {
                let inner = LoopbackConnectedSocketImpl::new(make_foreign(b2), b1);
                make_ready_future(ConnectedSocket::from_impl(Box::new(HoldConnection { inner, hold })))
            })
    }
    fn set_reuseaddr(&mut self, _: bool) -> Result<(), ExceptionPtr> {
        Ok(())
    }
    fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr> {
        Ok(false)
    }
    fn shutdown(&mut self) -> Result<(), ExceptionPtr> {
        for b in &self.buffers {
            b.abort();
        }
        Ok(())
    }
}

/// A value moved to another shard and back, used by one shard at a time.
struct AssertSend<T>(T);

// SAFETY: the test moves the value to one other shard and back, and only one shard uses it
// at a time.
unsafe impl<T> Send for AssertSend<T> {}

impl<T> AssertSend<T> {
    /// The value, taken through the whole wrapper so that closures capture it whole.
    fn into_inner(self) -> T {
        self.0
    }
}

/// How many frames the sink tries to send.
const FRAMES: usize = 4;

/// The payload of each frame: more than half the sink's memory limit, so that two frames
/// cannot be outstanding together.
const PAYLOAD: usize = 64 * 1024;

teseladb::engine_test_case!(a_sink_waits_for_its_frames_to_be_freed, {
    async_(|| -> Result<(), ExceptionPtr> {
        assert!(Smp::count() >= 2, "the test needs two shards");
        const _: () = assert!(2 * PAYLOAD > MAX_STREAM_BUFFERS_MEMORY);
        let mut lcf = Box::new(LoopbackConnectionFactory::new_default());
        let lcfp: *mut LoopbackConnectionFactory = &mut *lcf;
        let mut proto = Box::pin(Proto::new(Serializer));
        let so = ServerOptions {
            streaming_domain: Some(StreamingDomainType::new(51)),
            ..ServerOptions::default()
        };
        let mut server: Pin<Box<Server>> = Box::pin(proto.as_mut().new_server_with_options_socket(
            so,
            lcf.get_server_socket(),
            ResourceLimits::default(),
        ));
        server.as_mut().start()?;
        let any = make_ipv4_address(&Ipv4Addr::default());
        let plain = Rc::new(Hold::default());
        let mut c: Pin<Box<Client>> = Box::pin(proto.as_mut().new_client_with_options_socket(
            ClientOptions::default(),
            Socket::from_impl(Box::new(HoldSocket::new(lcfp, plain))),
            &any,
            &SocketAddress::default(),
        ));
        c.as_mut().start()?;
        // SAFETY: the client is not moved out of its pin.
        let cm = unsafe { c.as_mut().get_unchecked_mut() };

        let hold = Rc::new(Hold::default());
        let socket = Socket::from_impl(Box::new(HoldSocket::new(lcfp, hold.clone())));
        let sink = cm.make_stream_sink_with::<Serializer, (Sstring,)>(socket).get()?;
        // The stream is negotiated: from now on its socket keeps what it is given.
        hold.on.set(true);

        // The sink goes to shard 1 and sends from there, each call waiting at most 300 ms
        // for the sink's memory.
        let moved = AssertSend(sink);
        let back = Smp::submit_to(1, move || {
            let mut s = moved.into_inner();
            async_(move || -> Result<AssertSend<_>, ExceptionPtr> {
                let payload = Sstring::from("x".repeat(PAYLOAD).as_str());
                let mut sent = 0;
                for _ in 0..FRAMES {
                    let deadline = <SteadyClockType as Clock>::add(
                        <SteadyClockType as Clock>::now(),
                        Duration::from_millis(300),
                    );
                    // SAFETY: the sink lives on this shard until it is closed below.
                    let call = unsafe { s.call(&(payload.clone(),)) };
                    if with_timeout::<SteadyClockType, ()>(deadline, call).get().is_err() {
                        break;
                    }
                    sent += 1;
                }
                Ok(AssertSend((s, sent)))
            })
        })
        .get()?;
        let (sink, sent) = back.into_inner();

        // Free the held frames and let the rest through, then close the sink on its shard.
        hold.on.set(false);
        sleep(Duration::from_millis(50)).get()?;
        hold.held.borrow_mut().clear();
        let moved = AssertSend(sink);
        let closed = Smp::submit_to(1, move || {
            let mut s = moved.into_inner();
            // SAFETY: `s` moves into the continuation, so it lives until the close resolves.
            unsafe { s.close() }.then_wrapped(move |f: Future<()>| {
                let ok = f.get().is_ok();
                make_ready_future(AssertSend((s, ok)))
            })
        })
        .get()?;
        let (sink, close_ok) = closed.into_inner();
        drop(sink);
        cm.stop().get()?;
        // SAFETY: the server is not moved out of its pin.
        unsafe { server.as_mut().get_unchecked_mut() }.stop().get()?;
        drop(c);
        drop(server);
        drop(proto);
        lcf.destroy_all_shards().get()?;

        assert!(close_ok, "the sink did not close");
        assert_eq!(sent, 1, "the sink sent {sent} frames of {PAYLOAD} bytes while the first was held");
        Ok(())
    })
});
