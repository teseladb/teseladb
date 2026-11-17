#![cfg(target_os = "linux")]
//! An RPC stream whose peer sends its last frames and the end of the stream and then
//! closes the connection delivers those frames: the source reads the queued values and
//! then the end of the stream. The connection's end, which follows the end-of-stream frame,
//! does not drop them and turn the end of the stream into a "stream closed" error.

#![allow(unsafe_code)]
#![allow(clippy::type_complexity)]

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{ListenOptions, ServerSocket, make_ipv4_address_ip_port};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::rpc::rpc::{
    Client, ClientOptions, Protocol, ResourceLimits, Server, ServerOptions, StreamingDomainType,
};
use teseladb::engine::rpc::rpc_impl::{Read, SerializerInput, SerializerOutput, Write};
use teseladb::engine::rpc::rpc_types::{Sink, Source, Type};

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
    fn read<I: SerializerInput>(
        &self,
        in_: &mut I,
        _: Type<Sstring>,
    ) -> Result<Sstring, ExceptionPtr> {
        let mut l = [0u8; 4];
        in_.read(&mut l)?;
        let mut s = uninitialized_string(u32::from_ne_bytes(l) as usize);
        in_.read(s.as_bytes_mut())?;
        Ok(s)
    }
}

type Proto = Protocol<Serializer, u32>;

// ---- helpers ----

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

teseladb::engine_test_case!(the_end_of_a_stream_survives_the_close_of_its_connection, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut env = Env::new(41)?;
        let kept = register_keeper::<(i32,), (Sstring,)>(&mut env, 1)?;
        // Verb 2 gets a sink over the stream of the source the client passes, and closes
        // it: the client's source gets the end of the stream.
        env.proto.as_mut().register_handler(
            2,
            move |mut sink: Sink<(Sstring,)>| -> Future<u64> {
                let id = sink.get_id().id();
                let keep = sink.clone();
                // SAFETY: `keep`, a copy of the sink, lives until the close resolves.
                unsafe { sink.close() }.then(move |()| {
                    drop(keep);
                    make_ready_future(id)
                })
            },
        )?;
        let mut c = env.client()?;
        let mut sink = cm(&mut c).make_stream_sink::<Serializer, (i32,)>().get()?;
        let open = env.proto.make_client::<fn(Sink<(i32,)>) -> Future<Source<(Sstring,)>>>(1);
        let mut source = open.call(cm(&mut c), &(sink.clone(),)).get()?;
        let pass = env.proto.make_client::<fn(Source<(Sstring,)>) -> Future<u64>>(2);
        pass.call(cm(&mut c), &(source.clone(),)).get()?;
        let (mut ssrc, ssink) = kept.borrow_mut().take().unwrap_or_else(|| panic!("no stream"));

        // The client ends its sink; once the server has read the end, both sides of its end
        // of the stream are closed and it closes the stream's connection.
        // SAFETY: `sink` lives in this frame until the end of the test.
        unsafe { sink.close() }.get()?;
        // SAFETY: `ssrc` lives in this frame until the end of the test.
        assert_eq!(unsafe { ssrc.call() }.get()?, None);
        sleep(Duration::from_millis(100)).get()?;

        // The end of the stream reached the client before the connection closed.
        // SAFETY: `source` lives in this frame until the end of the test.
        let r = unsafe { source.call() }.get().map_err(|e| e.to_string());
        assert_eq!(r, Ok(None), "the end of the stream was lost");
        cm(&mut c).stop().get()?;
        drop((ssrc, ssink, source, sink));
        drop(c);
        env.finish(false)
    })
});
