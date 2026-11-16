//! Marshalling of RPC calls: how arguments and results travel, and how handlers and
//! client callables are typed.
//!
//! - Serialization: a protocol's serializer implements [`Write`] and [`Read`] for each
//!   type it sends, over a [`SerializerOutput`] / [`SerializerInput`]. Blanket
//!   implementations cover references, [`Sink`], [`Source`], [`Tuple`] and [`Optional`]
//!   (an optional trailing argument is absent when the frame has no more data); `()` and
//!   [`NoWaitType`] carry no data.
//! - [`RpcType`]: every type that travels as an argument or a return value implements it,
//!   giving the type the client sees (optionals and smart pointers removed) and how a
//!   reply is waited for ([`WaitStyle`]: [`WaitType`] or [`NoWaitType`]). The crate
//!   implements it for the standard and RPC types; `rpc_plain_type` gives the identity
//!   implementation for user types.
//! - [`RpcHandlerFn`]: a server handler, implemented for closures of each supported form
//!   (with or without [`ClientInfo`] and an [`OptTimePoint`] first) and arity. Its marker
//!   parameter names the form so the implementations do not overlap; when a closure fits
//!   two forms (a first argument of type `OptTimePoint`), the caller names the marker
//!   with a turbofish. [`recv_helper`] wraps a handler into the function the server runs
//!   for each request.
//! - [`SendHelper`] (from [`send_helper`]) is the client callable of a verb, with the
//!   timeout and cancellation variants.
//! - Handlers and clients take their arguments as one tuple type parameter.
//!
//! Streams ([`Sink`], [`Source`]) may be used on a shard other than their connection's;
//! the cross-shard parts are explained at the stream implementations below.

use std::marker::PhantomData;
use std::pin::Pin;

use crate::engine::core::byteorder::{le_to_cpu, write_le};
use crate::engine::core::future::{
    ExceptionPtr, Future, Futurize, HostBox, HostedPromise, Promise, PromiseHost, RuntimeError,
    make_exception_future, make_ready_future,
};
use crate::engine::core::gate::{Gate, GateClosedException, Holder, try_with_gate};
use crate::engine::core::scheduling::SchedulingGroup;
use crate::engine::core::semaphore::{SemaphoreTimedOut, get_units, with_semaphore};
use crate::engine::core::sharded::{ForeignPtr, make_foreign};
use crate::engine::core::shared_ptr::{LwSharedPtr, SharedPtr, make_shared, static_pointer_cast};
use crate::engine::core::simple_stream::{
    FragmentedMemoryOutputStream, MeasuringOutputStream, MemoryOutputStream,
    SimpleMemoryOutputStream,
};
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::when_all::when_all;
use crate::engine::rpc::rpc::{
    Client, Connection, DeferredSndBuf, HandlerWithHolder, IdType, Protocol, ReplyHandler,
    ReplyPromise, RpcDurationNs, RpcHandler, RpcHandlerFunc, RpcSinkImpl, RpcSourceImpl,
    RpcTimePoint, Server, ServerConnection, XshardPtr, make_shard_local_buffer_copy, xshard_get,
};
use crate::engine::rpc::rpc_types::{
    Bufs, BufsIterator, Cancellable, ClientInfo, ConnectionId, DeserializerStream,
    MAX_STREAM_BUFFERS_MEMORY, NoWaitType, OptTimePoint, Optional, RcvBuf, RpcClockType, Sink,
    SinkImpl, SndBuf, Source, SourceImpl, Tuple, Type, closed_error, make_deserializer_stream,
    remote_verb_error, stream_closed, unknown_exception_error, unknown_verb_error,
};
use crate::engine_assert;

/// The kind of an error reply on the wire (a 32-bit value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ExceptionType {
    /// The handler failed; the reply carries its message.
    User = 0,
    /// The server does not know the verb; the reply carries the verb.
    UnknownVerb = 1,
}

/// The types that travel as rpc arguments or return values, with their derived types.
pub trait RpcType: 'static {
    /// The type with an [`Optional`] wrapper removed (itself otherwise).
    type RemoveOptional: 'static;
    /// The type with a smart pointer removed (itself otherwise), as the client returns it.
    type DropSmartPtr: RpcType;
    /// How a reply of this type is waited for.
    type WaitSignature: WaitStyle;
    /// How a reply of a future of this type is waited for.
    type FutureWaitSignature: WaitStyle;
}

/// Implements `RpcType` as the primary templates do (no optional, no smart pointer,
/// a reply is waited for) for each listed type.
#[macro_export]
macro_rules! rpc_plain_type {
    ($($t:ty),* $(,)?) => {
        $(
            impl $crate::engine::rpc::rpc_impl::RpcType for $t {
                type RemoveOptional = $t;
                type DropSmartPtr = $t;
                type WaitSignature = $crate::engine::rpc::rpc_impl::WaitType;
                type FutureWaitSignature = $crate::engine::rpc::rpc_impl::WaitType;
            }
        )*
    };
}

rpc_plain_type!(
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    Sstring,
    String,
    ConnectionId
);

/// An optional travels as its value; the client sees the plain type.
impl<T: 'static> RpcType for Optional<T> {
    type RemoveOptional = T;
    type DropSmartPtr = Optional<T>;
    type WaitSignature = WaitType;
    type FutureWaitSignature = WaitType;
}

macro_rules! rpc_generic_plain_type {
    ($($t:ident<$p:ident>),*) => {
        $(
            impl<$p: 'static> RpcType for $t<$p> {
                type RemoveOptional = $t<$p>;
                type DropSmartPtr = $t<$p>;
                type WaitSignature = WaitType;
                type FutureWaitSignature = WaitType;
            }
        )*
    };
}

rpc_generic_plain_type!(Vec<T>, Option<T>, Tuple<T>, Sink<T>, Source<T>);

/// [`RpcType`] for the smart pointers: the client sees the type they point to.
macro_rules! rpc_smart_ptr_type {
    ($($t:ident),*) => {
        $(
            impl<T: RpcType> RpcType for $t<T> {
                type RemoveOptional = $t<T>;
                type DropSmartPtr = T;
                type WaitSignature = WaitType;
                type FutureWaitSignature = WaitType;
            }
        )*
    };
}

rpc_smart_ptr_type!(Box, SharedPtr, LwSharedPtr);

impl<P: std::ops::Deref + 'static> RpcType for ForeignPtr<P>
where
    P::Target: RpcType + Sized,
{
    type RemoveOptional = ForeignPtr<P>;
    type DropSmartPtr = P::Target;
    type WaitSignature = WaitType;
    type FutureWaitSignature = WaitType;
}

/// A handler returning [`NoWaitType`] (or a future of it) sends no reply.
impl RpcType for NoWaitType {
    type RemoveOptional = NoWaitType;
    type DropSmartPtr = NoWaitType;
    type WaitSignature = NoWaitType;
    type FutureWaitSignature = NoWaitType;
}

/// The calls that wait for a reply (the opposite of [`NoWaitType`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WaitType;

// tags to tell whether we want a const client_info& parameter
/// `do_want_client_info`, for handlers taking `&ClientInfo`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DoWantClientInfo;
/// `do_want_client_info`, for handlers taking `&mut ClientInfo`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DoWantClientInfoMut;
/// Tag: the handler does not take the client info.
#[derive(Clone, Copy, Debug, Default)]
pub struct DontWantClientInfo;

// tags to tell whether we want a opt_time_point parameter
/// Tag: the handler takes the optional time point (the call's deadline).
#[derive(Clone, Copy, Debug, Default)]
pub struct DoWantTimePoint;
/// Tag: the handler does not take the optional time point.
#[derive(Clone, Copy, Debug, Default)]
pub struct DontWantTimePoint;

/// The return type of a verb handler: a future, a value that may fail
/// (`Result<T, ExceptionPtr>`) or nothing (`()`). Its value is `Futurize::Output`.
pub trait RpcRet: Futurize {
    /// How the client waits for the reply.
    type Wait: WaitStyle;
    /// The return type of the client callable.
    type ClientRet: RpcRet;
}

impl<T: RpcType> RpcRet for Future<T> {
    type Wait = T::FutureWaitSignature;
    type ClientRet = Future<T>;
}

impl<T: RpcType> RpcRet for Result<T, ExceptionPtr> {
    type Wait = T::WaitSignature;
    type ClientRet = Result<T::DropSmartPtr, ExceptionPtr>;
}

impl RpcRet for () {
    type Wait = WaitType;
    type ClientRet = ();
}

/// The output stream a serializer writes to.
pub trait SerializerOutput {
    /// Writes `data`.
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr>;
}

impl SerializerOutput for MeasuringOutputStream {
    #[inline]
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        MeasuringOutputStream::write(self, data);
        Ok(())
    }
}

impl SerializerOutput for SerializerStream {
    #[inline]
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        MemoryOutputStream::write(self, data)
    }
}

/// The input stream a serializer reads from.
pub trait SerializerInput {
    /// Fills `data`.
    fn read(&mut self, data: &mut [u8]) -> Result<(), ExceptionPtr>;
    /// The bytes left.
    fn size(&self) -> usize;
    /// The connection the frame came from (for `sink` and `source` arguments).
    fn connection(&self) -> *mut Connection;
}

/// The input of the deserializers: the stream over a frame and its connection.
pub struct UnmarshallInput {
    stream: DeserializerStream,
    conn: *mut Connection,
}

impl SerializerInput for UnmarshallInput {
    #[inline]
    fn read(&mut self, data: &mut [u8]) -> Result<(), ExceptionPtr> {
        DeserializerStream::read(&mut self.stream, data)
    }
    #[inline]
    fn size(&self) -> usize {
        self.stream.size()
    }
    #[inline]
    fn connection(&self) -> *mut Connection {
        self.conn
    }
}

/// Writes a `T` to an output stream; implemented by the serializer of a protocol for
/// each type it sends.
pub trait Write<T: ?Sized> {
    /// Writes `data` to `out`.
    fn write<O: SerializerOutput>(&self, out: &mut O, data: &T) -> Result<(), ExceptionPtr>;
}

/// `T read(const serializer&, Input& input, type<T> type_tag)`, implemented by the
/// serializer of a protocol for each type it receives.
pub trait Read<T> {
    /// Reads a `T` from `in_`.
    fn read<I: SerializerInput>(&self, in_: &mut I, type_tag: Type<T>) -> Result<T, ExceptionPtr>;
}

// `()` and `NoWaitType` carry no data.
impl<S: ?Sized> Write<()> for S {
    #[inline]
    fn write<O: SerializerOutput>(&self, _out: &mut O, _data: &()) -> Result<(), ExceptionPtr> {
        Ok(())
    }
}

impl<S: ?Sized> Read<()> for S {
    #[inline]
    fn read<I: SerializerInput>(&self, _in: &mut I, _t: Type<()>) -> Result<(), ExceptionPtr> {
        Ok(())
    }
}

impl<S: ?Sized> Write<NoWaitType> for S {
    #[inline]
    fn write<O: SerializerOutput>(
        &self,
        _out: &mut O,
        _data: &NoWaitType,
    ) -> Result<(), ExceptionPtr> {
        Ok(())
    }
}

impl<S: ?Sized> Read<NoWaitType> for S {
    #[inline]
    fn read<I: SerializerInput>(
        &self,
        _in: &mut I,
        _t: Type<NoWaitType>,
    ) -> Result<NoWaitType, ExceptionPtr> {
        Ok(NoWaitType)
    }
}

/// Serializes a connection id (8 bytes, little endian).
#[inline]
pub fn serialize_connection_id(id: &ConnectionId) -> Sstring {
    Sstring::from_bytes(&id.id().to_le_bytes())
}

/// Deserializes a connection id. A string shorter than 8 bytes is padded with zeros
/// instead of being read past its end.
#[inline]
pub fn deserialize_connection_id(s: &Sstring) -> ConnectionId {
    let b = s.as_bytes();
    let mut raw = [0u8; 8];
    let n = b.len().min(8);
    raw[..n].copy_from_slice(&b[..n]);
    ConnectionId::new(u64::from_le_bytes(raw))
}

// Smart pointers are written as what they point to.
macro_rules! write_smart_ptr {
    ($($t:ident),*) => {
        $(
            impl<S: Write<T> + ?Sized, T> Write<$t<T>> for S {
                #[inline]
                fn write<O: SerializerOutput>(&self, out: &mut O, t: &$t<T>) -> Result<(), ExceptionPtr> {
                    self.write(out, &**t)
                }
            }
        )*
    };
}

write_smart_ptr!(Box, SharedPtr, LwSharedPtr);

impl<S: Write<P::Target> + ?Sized, P: std::ops::Deref + 'static> Write<ForeignPtr<P>> for S
where
    P::Target: Sync,
{
    #[inline]
    fn write<O: SerializerOutput>(
        &self,
        out: &mut O,
        t: &ForeignPtr<P>,
    ) -> Result<(), ExceptionPtr> {
        self.write(out, &**t)
    }
}

/// A reference is written as the value it points to.
impl<'a, S: Write<T> + ?Sized, T: ?Sized> Write<&'a T> for S {
    #[inline]
    fn write<O: SerializerOutput>(&self, out: &mut O, arg: &&'a T) -> Result<(), ExceptionPtr> {
        self.write(out, *arg)
    }
}

/// Writes a connection id as its serialized string.
#[inline]
fn put_connection_id<O: SerializerOutput>(
    cid: &ConnectionId,
    out: &mut O,
) -> Result<(), ExceptionPtr> {
    let id = serialize_connection_id(cid);
    out.write(id.as_bytes())
}

/// A sink argument is written as the id of its stream connection.
impl<S: ?Sized, T: 'static> Write<Sink<T>> for S {
    #[inline]
    fn write<O: SerializerOutput>(&self, out: &mut O, arg: &Sink<T>) -> Result<(), ExceptionPtr> {
        put_connection_id(&arg.get_id(), out)
    }
}

/// A source argument is written as the id of its stream connection.
impl<S: ?Sized, T: 'static> Write<Source<T>> for S {
    #[inline]
    fn write<O: SerializerOutput>(&self, out: &mut O, arg: &Source<T>) -> Result<(), ExceptionPtr> {
        put_connection_id(&arg.get_id(), out)
    }
}

/// A tuple is written element by element.
impl<S, T: MarshallArgs<S>> Write<Tuple<T>> for S {
    #[inline]
    fn write<O: SerializerOutput>(&self, out: &mut O, arg: &Tuple<T>) -> Result<(), ExceptionPtr> {
        arg.0.do_marshall(self, out)
    }
}

/// `do_marshall(serializer, out, args...)` over a tuple of arguments.
pub trait MarshallArgs<S: ?Sized> {
    /// Writes the elements, left to right.
    fn do_marshall<O: SerializerOutput>(
        &self,
        serializer: &S,
        out: &mut O,
    ) -> Result<(), ExceptionPtr>;
}

/// Reads a tuple of arguments from a frame.
///
/// A Rust tuple expression evaluates left to right, so the elements are read in order
/// straight into the tuple.
pub trait UnmarshallArgs<S: ?Sized>: Sized {
    /// Reads the elements, left to right.
    fn do_unmarshall<I: SerializerInput>(serializer: &S, in_: &mut I)
    -> Result<Self, ExceptionPtr>;
}

/// The argument tuple with every [`Optional`] wrapper removed, as the client takes it.
pub trait RemoveOptionalTuple {
    /// The arguments of the client.
    type Type: 'static;
}

macro_rules! args_tuple_impls {
    ($(($($a:ident $i:tt),*))*) => {
        $(
            impl<S: ?Sized $(+ Write<$a>)*, $($a),*> MarshallArgs<S> for ($($a,)*) {
                #[inline]
                #[allow(unused_variables)]
                fn do_marshall<O: SerializerOutput>(&self, serializer: &S, out: &mut O) -> Result<(), ExceptionPtr> {
                    // The elements are written in order, left to right.
                    $(serializer.write(out, &self.$i)?;)*
                    Ok(())
                }
            }

            impl<S: ?Sized $(+ UnmarshalOne<$a>)*, $($a),*> UnmarshallArgs<S> for ($($a,)*) {
                #[inline]
                #[allow(unused_variables)]
                fn do_unmarshall<I: SerializerInput>(serializer: &S, in_: &mut I) -> Result<Self, ExceptionPtr> {
                    Ok(($(<S as UnmarshalOne<$a>>::doit(serializer, in_)?,)*))
                }
            }

            impl<$($a: RpcType),*> RemoveOptionalTuple for ($($a,)*) {
                type Type = ($($a::RemoveOptional,)*);
            }

            impl<$($a: 'static),*> RpcType for ($($a,)*) where ($($a,)*): 'static {
                type RemoveOptional = ($($a,)*);
                type DropSmartPtr = ($($a,)*);
                type WaitSignature = WaitType;
                type FutureWaitSignature = WaitType;
            }
        )*
    };
}

args_tuple_impls! {
    ()
    (A0 0)
    (A0 0, A1 1)
    (A0 0, A1 1, A2 2)
    (A0 0, A1 1, A2 2, A3 3)
    (A0 0, A1 1, A2 2, A3 3, A4 4)
    (A0 0, A1 1, A2 2, A3 3, A4 4, A5 5)
    (A0 0, A1 1, A2 2, A3 3, A4 4, A5 5, A6 6)
    (A0 0, A1 1, A2 2, A3 3, A4 4, A5 5, A6 6, A7 7)
    (A0 0, A1 1, A2 2, A3 3, A4 4, A5 5, A6 6, A7 7, A8 8)
    (A0 0, A1 1, A2 2, A3 3, A4 4, A5 5, A6 6, A7 7, A8 8, A9 9)
    (A0 0, A1 1, A2 2, A3 3, A4 4, A5 5, A6 6, A7 7, A8 8, A9 9, A10 10)
    (A0 0, A1 1, A2 2, A3 3, A4 4, A5 5, A6 6, A7 7, A8 8, A9 9, A10 10, A11 11)
}

/// The output stream of a serializer: the buffers of a frame being built.
pub type SerializerStream = MemoryOutputStream<BufsIterator>;

/// The stream the serializers write a frame to.
///
/// # Safety
/// `output` stays alive and in place, and unused elsewhere, while the stream is used.
#[inline]
pub unsafe fn make_serializer_stream(output: &mut SndBuf) -> SerializerStream {
    let size = output.size as usize;
    match &mut output.bufs {
        // SAFETY: the caller's contract covers the buffer.
        Bufs::One(b) => MemoryOutputStream::Simple(unsafe {
            SimpleMemoryOutputStream::new(b.get_write(), b.size(), 0)
        }),
        // SAFETY: the caller's contract covers the buffers.
        Bufs::Many(ar) => MemoryOutputStream::Fragmented(FragmentedMemoryOutputStream::new(
            unsafe { BufsIterator::begin(ar) },
            size,
        )),
    }
}

/// Serializes `args` into a frame with `head_space` free bytes in front.
#[inline]
pub fn marshall<S: ?Sized, A: MarshallArgs<S> + ?Sized>(
    serializer: &S,
    head_space: usize,
    args: &A,
) -> Result<SndBuf, ExceptionPtr> {
    let mut measure = MeasuringOutputStream::default();
    args.do_marshall(serializer, &mut measure)?;
    let mut ret = SndBuf::with_size(measure.size() + head_space);
    // SAFETY: the stream is used only in this function, before `ret` is returned; the
    // buffers' storage is on the heap and does not move with `ret`.
    let mut out = unsafe { make_serializer_stream(&mut ret) };
    out.skip(head_space)?;
    args.do_marshall(serializer, &mut out)?;
    Ok(ret)
}

/// Reads a `T` through the serializer, with the [`Type`] tag that selects the type.
#[inline]
pub fn read_via_type_marker<T, S: Read<T> + ?Sized, I: SerializerInput>(
    serializer: &S,
    in_: &mut I,
) -> Result<T, ExceptionPtr> {
    serializer.read(in_, Type::default())
}

/// How a top-level argument is read.
///
/// The general case is [`Read`]; the implementations below handle `Optional`, `Sink`,
/// `Source` and `Tuple`.
pub trait UnmarshalOne<T> {
    /// Reads a `T`.
    fn doit<I: SerializerInput>(&self, in_: &mut I) -> Result<T, ExceptionPtr>;
}

impl<S: Read<T> + ?Sized, T> UnmarshalOne<T> for S {
    #[inline]
    fn doit<I: SerializerInput>(&self, in_: &mut I) -> Result<T, ExceptionPtr> {
        read_via_type_marker::<T, S, I>(self, in_)
    }
}

/// An optional argument: absent when the frame has no more data.
impl<S: Read<T> + ?Sized, T> Read<Optional<T>> for S {
    #[inline]
    fn read<I: SerializerInput>(
        &self,
        in_: &mut I,
        _t: Type<Optional<T>>,
    ) -> Result<Optional<T>, ExceptionPtr> {
        if in_.size() != 0 {
            Ok(Optional(Some(read_via_type_marker::<T, S, I>(self, in_)?)))
        } else {
            Ok(Optional(None))
        }
    }
}

/// Reads a connection id from its serialized string.
#[inline]
fn get_connection_id<I: SerializerInput>(in_: &mut I) -> Result<ConnectionId, ExceptionPtr> {
    let mut id = [0u8; std::mem::size_of::<ConnectionId>()];
    in_.read(&mut id)?;
    Ok(ConnectionId::new(u64::from_le_bytes(id)))
}

/// A sink argument: the stream connection named by the id that follows.
impl<S: 'static, T: MarshallArgs<S> + 'static> Read<Sink<T>> for S {
    fn read<I: SerializerInput>(
        &self,
        in_: &mut I,
        _t: Type<Sink<T>>,
    ) -> Result<Sink<T>, ExceptionPtr> {
        let id = get_connection_id(in_)?;
        // SAFETY: the connection outlives the deserialization of its frames.
        let con = unsafe { (*in_.connection()).get_stream(id)? };
        Ok(make_rpc_sink::<S, T>(con))
    }
}

/// A source argument: the stream connection named by the id that follows.
impl<S: 'static, T: UnmarshallArgs<S> + 'static> Read<Source<T>> for S {
    fn read<I: SerializerInput>(
        &self,
        in_: &mut I,
        _t: Type<Source<T>>,
    ) -> Result<Source<T>, ExceptionPtr> {
        let id = get_connection_id(in_)?;
        // SAFETY: as for sinks.
        let con = unsafe { (*in_.connection()).get_stream(id)? };
        let p = make_shared(RpcSourceImpl::<S, T>::new(con));
        // SAFETY: the cast is the unsizing of the same object.
        let imp = unsafe {
            static_pointer_cast(&p, |q: *mut RpcSourceImpl<S, T>| q as *mut dyn SourceImpl<T>)
        };
        Ok(Source::new(imp))
    }
}

/// A tuple argument, read element by element.
impl<S: ?Sized, T: UnmarshallArgs<S>> Read<Tuple<T>> for S {
    #[inline]
    fn read<I: SerializerInput>(
        &self,
        in_: &mut I,
        _t: Type<Tuple<T>>,
    ) -> Result<Tuple<T>, ExceptionPtr> {
        Ok(Tuple(T::do_unmarshall(self, in_)?))
    }
}

/// A sink of the rpc streams over `con`.
pub(crate) fn make_rpc_sink<S: 'static, T: MarshallArgs<S> + 'static>(
    con: crate::engine::rpc::rpc_types::XshardConnectionPtr,
) -> Sink<T> {
    let p = make_shared(RpcSinkImpl::<S, T>::new(con));
    // SAFETY: the cast is the unsizing of the same object.
    let imp =
        unsafe { static_pointer_cast(&p, |q: *mut RpcSinkImpl<S, T>| q as *mut dyn SinkImpl<T>) };
    Sink::new(imp)
}

/// Deserializes the arguments `A` (a tuple) of a frame received on `c`.
///
/// # Safety
/// `S` must be the serializer of the protocol that made `c`.
#[inline]
pub unsafe fn unmarshall<S: 'static, A: UnmarshallArgs<S>>(
    c: &mut Connection,
    mut input: RcvBuf,
) -> Result<A, ExceptionPtr> {
    let conn: *mut Connection = c;
    // SAFETY: `input` lives until the end of this function, and the stream is used only here.
    let mut in_ = UnmarshallInput { stream: unsafe { make_deserializer_stream(&mut input) }, conn };
    // SAFETY: guaranteed by the caller.
    let serializer = unsafe { &mut *c.serializer::<S>() };
    A::do_unmarshall(serializer, &mut in_)
}

/// The exception of an error reply.
pub fn unmarshal_exception(d: &mut RcvBuf) -> Result<ExceptionPtr, ExceptionPtr> {
    // SAFETY: `d` is borrowed for the whole function, and the stream is used only here.
    let mut data = unsafe { make_deserializer_stream(d) };

    let mut v32 = [0u8; 4];
    DeserializerStream::read(&mut data, &mut v32)?;
    let ex_type = le_to_cpu(u32::from_ne_bytes(v32));
    DeserializerStream::read(&mut data, &mut v32)?;
    let ex_len = le_to_cpu(u32::from_ne_bytes(v32));

    let ex = match ex_type {
        t if t == ExceptionType::User as u32 => {
            let mut s = vec![0u8; ex_len as usize];
            DeserializerStream::read(&mut data, &mut s)?;
            // The bytes move into the message without a copy (a message that is not UTF-8 is
            // replaced lossily, Rust strings being UTF-8).
            let s = String::from_utf8(s)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            ExceptionPtr::new(remote_verb_error(s))
        }
        t if t == ExceptionType::UnknownVerb as u32 => {
            let mut v64 = [0u8; 8];
            DeserializerStream::read(&mut data, &mut v64)?;
            ExceptionPtr::new(unknown_verb_error(le_to_cpu(u64::from_ne_bytes(v64))))
        }
        _ => ExceptionPtr::new(unknown_exception_error()),
    };
    Ok(ex)
}

/// The promise of a call and how its reply is read; `T` is the value of the reply
/// (`()` for none).
pub struct RcvReply<S, T: 'static> {
    /// Whether the promise is set.
    pub done: bool,
    /// The promise of the call.
    pub p: Promise<T>,
    _marker: PhantomData<fn(&S)>,
}

impl<S, T: 'static> Default for RcvReply<S, T> {
    fn default() -> Self {
        RcvReply { done: false, p: Promise::new(), _marker: PhantomData }
    }
}

impl<S, T: 'static> RcvReply<S, T> {
    /// Sets the value of the call.
    #[inline]
    pub fn set_value(&mut self, v: T) {
        self.done = true;
        self.p.set_value(v);
    }
}

impl<S: Read<T> + 'static, T: 'static> RcvReply<S, T> {
    /// Reads the reply from `input`.
    #[inline]
    pub fn get_reply(&mut self, dst: &mut Client, input: RcvBuf) -> Result<(), ExceptionPtr> {
        // SAFETY: the reply handler belongs to a client of the protocol of S.
        let (v,) = unsafe { unmarshall::<S, (T,)>(dst, input)? };
        self.set_value(v);
        Ok(())
    }
}

impl<S, T: 'static> ReplyPromise for RcvReply<S, T> {
    #[inline]
    fn fail(&mut self, ex: ExceptionPtr) {
        self.done = true;
        self.p.set_exception(ex);
    }
}

impl<S, T: 'static> Drop for RcvReply<S, T> {
    fn drop(&mut self) {
        if !self.done {
            self.p.set_exception(ExceptionPtr::new(closed_error()));
        }
    }
}

/// How a call waits for its reply and how a handler sends it: [`WaitType`] waits and
/// replies, [`NoWaitType`] does neither.
pub trait WaitStyle: Default + 'static {
    /// The future the client returns for a reply of value `V`.
    type ClientFuture<V: 'static>: 'static;

    /// A client future failed with `ex`.
    fn make_exception_future<V: 'static>(ex: ExceptionPtr) -> Self::ClientFuture<V>;

    /// Registers the wait for the reply of `msg_id`.
    fn wait_for_reply<S: Read<V> + 'static, V: 'static>(
        self,
        timeout: Option<RpcTimePoint>,
        start: RpcTimePoint,
        cancel: Option<Pin<&mut Cancellable>>,
        dst: &mut Client,
        msg_id: IdType,
    ) -> Self::ClientFuture<V>;

    /// Joins the send of the request and the wait: the future of the wait, once the request
    /// is sent.
    fn join<V: 'static>(request: Future<()>, wait: Self::ClientFuture<V>) -> Self::ClientFuture<V>;

    /// Sends the reply of a handler.
    fn reply<S: Write<V> + 'static, V: 'static>(
        self,
        ret: Future<V>,
        msg_id: i64,
        client: SharedPtr<ServerConnection>,
        timeout: Option<RpcTimePoint>,
        handler_duration: Option<RpcDurationNs>,
    ) -> Future<()>;
}

impl WaitStyle for WaitType {
    type ClientFuture<V: 'static> = Future<V>;

    #[inline]
    fn make_exception_future<V: 'static>(ex: ExceptionPtr) -> Future<V> {
        make_exception_future(ex)
    }

    fn wait_for_reply<S: Read<V> + 'static, V: 'static>(
        self,
        timeout: Option<RpcTimePoint>,
        start: RpcTimePoint,
        cancel: Option<Pin<&mut Cancellable>>,
        dst: &mut Client,
        msg_id: IdType,
    ) -> Future<V> {
        let lambda = |r: &mut RcvReply<S, V>,
                      dst: &mut Client,
                      msg_id: IdType,
                      mut data: RcvBuf|
         -> Result<(), ExceptionPtr> {
            if msg_id >= 0 {
                dst.get_stats_internal().replied += 1;
                r.get_reply(dst, data)
            } else {
                dst.get_stats_internal().exception_received += 1;
                r.done = true;
                r.p.set_exception(unmarshal_exception(&mut data)?);
                Ok(())
            }
        };
        let mut r = Box::new(ReplyHandler::new(lambda, RcvReply::<S, V>::default()));
        r.base.start = start;
        let fut = r.reply.p.get_future();
        dst.wait_for_reply(msg_id, r, timeout, cancel);
        fut
    }

    #[inline]
    fn join<V: 'static>(request: Future<()>, wait: Future<V>) -> Future<V> {
        when_all((request, wait)).then(|(mut r0, r1): (Future<()>, Future<V>)| {
            r0.ignore_ready_future();
            r1 // return future of wait_for_reply
        })
    }

    fn reply<S: Write<V> + 'static, V: 'static>(
        self,
        ret: Future<V>,
        mut msg_id: i64,
        client: SharedPtr<ServerConnection>,
        timeout: Option<RpcTimePoint>,
        handler_duration: Option<RpcDurationNs>,
    ) -> Future<()> {
        // SAFETY: the shared pointer keeps the connection alive; handlers run on its shard.
        let cl = unsafe { client.get_mut_unchecked() };
        if !cl.error() {
            // SAFETY: the handler was registered on the protocol of S.
            let serializer = unsafe { &mut *cl.serializer::<S>() };
            let data = match ret
                .get()
                .and_then(|v| marshall(serializer, RESPONSE_FRAME_HEADROOM, &(v,)))
            {
                Ok(d) => d,
                Err(ex) => {
                    msg_id = -msg_id;
                    match marshall_user_exception(&ex) {
                        Ok(d) => d,
                        Err(e) => return make_exception_future(e),
                    }
                }
            };

            cl.respond(msg_id, data, timeout, handler_duration)
        } else {
            let mut ret = ret;
            ret.ignore_ready_future();
            make_ready_future(())
        }
    }
}

/// The reply frame of an exception thrown by a handler.
fn marshall_user_exception(ex: &ExceptionPtr) -> Result<SndBuf, ExceptionPtr> {
    let what = match ex.get() {
        Some(e) => e.what(),
        None => String::new(),
    };
    let len = what.len() as u32;
    let mut data = SndBuf::with_size(RESPONSE_FRAME_HEADROOM + 2 * 4 + len as usize);
    // SAFETY: as in marshall().
    let mut os = unsafe { make_serializer_stream(&mut data) };
    os.skip(RESPONSE_FRAME_HEADROOM)?;
    let mut v32 = (ExceptionType::User as u32).to_le_bytes();
    SerializerOutput::write(&mut os, &v32)?;
    v32 = len.to_le_bytes();
    SerializerOutput::write(&mut os, &v32)?;
    SerializerOutput::write(&mut os, what.as_bytes())?;
    Ok(data)
}

// No reply is sent or waited for.
impl WaitStyle for NoWaitType {
    type ClientFuture<V: 'static> = Future<()>;

    #[inline]
    fn make_exception_future<V: 'static>(ex: ExceptionPtr) -> Future<()> {
        make_exception_future(ex)
    }

    /// The client does not wait: the future resolves once the request is sent.
    #[inline]
    fn wait_for_reply<S: Read<V> + 'static, V: 'static>(
        self,
        _timeout: Option<RpcTimePoint>,
        _start: RpcTimePoint,
        _cancel: Option<Pin<&mut Cancellable>>,
        _dst: &mut Client,
        _msg_id: IdType,
    ) -> Future<()> {
        make_ready_future(())
    }

    #[inline]
    fn join<V: 'static>(request: Future<()>, wait: Future<()>) -> Future<()> {
        when_all((request, wait)).then(|(mut r0, r1): (Future<()>, Future<()>)| {
            r0.ignore_ready_future();
            r1 // return future of wait_for_reply
        })
    }

    fn reply<S: Write<V> + 'static, V: 'static>(
        self,
        r: Future<V>,
        msgid: i64,
        client: SharedPtr<ServerConnection>,
        _timeout: Option<RpcTimePoint>,
        _handler_duration: Option<RpcDurationNs>,
    ) -> Future<()> {
        if let Err(ex) = r.get() {
            let what = match ex.get() {
                Some(e) => e.what(),
                None => String::new(),
            };
            client.get_logger().client_msg(
                client.info_ref(),
                msgid,
                &Sstring::from(format!("exception \"{}\" in no_wait handler ignored", what)),
            );
        }
        make_ready_future(())
    }
}

// Convert a relative timeout (a duration) to an absolute one (time_point).
// Do the calculation safely so that a very large duration will be capped by
// time_point::max, instead of wrapping around to ancient history.
/// The deadline `relative` nanoseconds from now, capped at the end of time.
#[inline]
pub fn relative_timeout_to_absolute(relative: RpcDurationNs) -> RpcTimePoint {
    let now = RpcClockType::now().nanos_since_epoch();
    RpcTimePoint::from_nanos(now + relative.min(RpcTimePoint::max().nanos_since_epoch() - now))
}

// Refer to struct request_frame for more details
/// The head space of a request frame.
pub const REQUEST_FRAME_HEADROOM: usize = 28;

/// The verbs of a protocol: hashable, and convertible to and from the 64-bit verb of the
/// frames (with integer-cast semantics).
pub trait RpcMsgType: std::hash::Hash + Eq + Copy + 'static {
    /// The verb as the 64-bit value of the frames.
    fn to_u64(self) -> u64;
    /// The verb of the 64-bit value `v` of a frame.
    fn from_u64(v: u64) -> Self;
}

macro_rules! rpc_msg_type_int {
    ($($t:ty),*) => {
        $(
            impl RpcMsgType for $t {
                #[inline]
                fn to_u64(self) -> u64 {
                    self as u64
                }
                #[inline]
                fn from_u64(v: u64) -> Self {
                    v as $t
                }
            }
        )*
    };
}

rpc_msg_type_int!(u8, u16, u32, u64, i8, i16, i32, i64);

/// The client callable of verb `t`: it takes the client connection and the arguments `A`
/// (a tuple), marshalls them, sends them to the server and waits for a reply of type `R`;
/// after receiving the reply it unmarshalls it and resolves the caller's future.
pub struct SendHelper<S, M, R, A> {
    t: M,
    _sig: PhantomData<fn(&S, &A) -> R>,
}

impl<S, M: Copy, R, A> Clone for SendHelper<S, M, R, A> {
    fn clone(&self) -> Self {
        SendHelper { t: self.t, _sig: PhantomData }
    }
}

/// The future a client of a verb with return type `R` gets.
pub type ClientFutureOf<R> =
    <<R as RpcRet>::Wait as WaitStyle>::ClientFuture<<R as Futurize>::Output>;

/// The client callable of verb `xt`.
#[inline]
pub fn send_helper<S, M, R, A>(xt: M) -> SendHelper<S, M, R, A> {
    SendHelper { t: xt, _sig: PhantomData }
}

impl<S, M, R, A> SendHelper<S, M, R, A>
where
    S: Read<R::Output> + 'static,
    M: RpcMsgType,
    R: RpcRet,
    A: MarshallArgs<S>,
{
    /// Sends the call and waits for its reply, if the verb has one.
    pub fn send(
        &self,
        dst: &mut Client,
        timeout: Option<RpcTimePoint>,
        mut cancel: Option<Pin<&mut Cancellable>>,
        args: &A,
    ) -> ClientFutureOf<R> {
        if dst.error() {
            return <R::Wait as WaitStyle>::make_exception_future::<R::Output>(ExceptionPtr::new(
                closed_error(),
            ));
        }

        let start = RpcClockType::now();
        // send message
        let msg_id = dst.next_message_id();
        // SAFETY: the helper was made by the protocol of S, whose clients carry it.
        let serializer = unsafe { &mut *dst.serializer::<S>() };
        // A serialization failure fails the returned future.
        let data = match marshall(serializer, REQUEST_FRAME_HEADROOM, args) {
            Ok(d) => d,
            Err(e) => return <R::Wait as WaitStyle>::make_exception_future::<R::Output>(e),
        };

        // prepare reply handler, if return type is now_wait_type this does nothing, since no reply will be sent
        let request =
            dst.request(self.t.to_u64(), msg_id, data, timeout, cancel.as_mut().map(Pin::as_mut));
        let wait =
            R::Wait::default().wait_for_reply::<S, R::Output>(timeout, start, cancel, dst, msg_id);
        <R::Wait as WaitStyle>::join::<R::Output>(request, wait)
    }

    /// Calls the verb on `dst` with `args`, with no timeout.
    #[inline]
    pub fn call(&self, dst: &mut Client, args: &A) -> ClientFutureOf<R> {
        self.send(dst, None, None, args)
    }

    /// Calls the verb on `dst` with `args`, failing with a timeout error at `timeout`.
    #[inline]
    pub fn call_until(
        &self,
        dst: &mut Client,
        timeout: RpcTimePoint,
        args: &A,
    ) -> ClientFutureOf<R> {
        self.send(dst, Some(timeout), None, args)
    }

    /// Calls the verb with a deadline; `cancel` can cancel the call.
    #[inline]
    pub fn call_until_cancellable(
        &self,
        dst: &mut Client,
        timeout: RpcTimePoint,
        cancel: Pin<&mut Cancellable>,
        args: &A,
    ) -> ClientFutureOf<R> {
        self.send(dst, Some(timeout), Some(cancel), args)
    }

    /// Calls the verb on `dst` with `args`, failing with a timeout error after `timeout`.
    #[inline]
    pub fn call_for(
        &self,
        dst: &mut Client,
        timeout: RpcDurationNs,
        args: &A,
    ) -> ClientFutureOf<R> {
        self.send(dst, Some(relative_timeout_to_absolute(timeout)), None, args)
    }

    /// Calls the verb with a relative timeout; `cancel` can cancel the call.
    #[inline]
    pub fn call_for_cancellable(
        &self,
        dst: &mut Client,
        timeout: RpcDurationNs,
        cancel: Pin<&mut Cancellable>,
        args: &A,
    ) -> ClientFutureOf<R> {
        self.send(dst, Some(relative_timeout_to_absolute(timeout)), Some(cancel), args)
    }

    /// Calls the verb with no timeout; `cancel` can cancel the call.
    #[inline]
    pub fn call_cancellable(
        &self,
        dst: &mut Client,
        cancel: Pin<&mut Cancellable>,
        args: &A,
    ) -> ClientFutureOf<R> {
        self.send(dst, None, Some(cancel), args)
    }
}

// Refer to struct response_frame for more details
/// The head space of a response frame.
pub const RESPONSE_FRAME_HEADROOM: usize = 16;

/// A handler of a verb, in one of the supported forms (argument lists before the verb's
/// own arguments): `Ret (In...)`, `Ret (const client_info&, In...)`,
/// `Ret (client_info&, In...)`, `Ret (const client_info&, opt_time_point, In...)`,
/// `Ret (client_info&, opt_time_point, In...)` and `Ret (opt_time_point, In...)`.
pub trait RpcHandlerFn<Marker>: 'static {
    /// The return type of the handler.
    type Ret: RpcRet;
    /// The verb's own arguments, as a tuple (without client info and time point).
    type Args: 'static;
    /// Whether the handler takes the client info (a tag type).
    type WantClientInfo: Default;
    /// Whether the handler takes the optional time point (a tag type).
    type WantTimePoint: Default;

    /// Calls the handler with
    /// the client info and the time point it wants (`maybe_add_client_info`,
    /// `maybe_add_time_point`) and the arguments.
    fn apply(
        &mut self,
        info: &mut ClientInfo,
        time_point: OptTimePoint,
        args: Self::Args,
    ) -> Self::Ret;
}

/// The marker of a handler form: its tags, its return type and its arguments.
pub struct HandlerSignature<Wci, Wtp, Ret, Args>(PhantomData<fn(Wci, Wtp, Args) -> Ret>);

macro_rules! handler_impls {
    ($(($($a:ident $v:ident),*))*) => {
        $(
            // General case
            impl<F, R: RpcRet, $($a: 'static),*> RpcHandlerFn<HandlerSignature<DontWantClientInfo, DontWantTimePoint, R, ($($a,)*)>> for F
            where
                F: FnMut($($a),*) -> R + 'static,
            {
                type Ret = R;
                type Args = ($($a,)*);
                type WantClientInfo = DontWantClientInfo;
                type WantTimePoint = DontWantTimePoint;
                #[inline]
                #[allow(unused_variables)]
                fn apply(&mut self, info: &mut ClientInfo, time_point: OptTimePoint, ($($v,)*): Self::Args) -> R {
                    self($($v),*)
                }
            }

            // Handlers that receive the client info.
            impl<F, R: RpcRet, $($a: 'static),*> RpcHandlerFn<HandlerSignature<DoWantClientInfo, DontWantTimePoint, R, ($($a,)*)>> for F
            where
                F: for<'c> FnMut(&'c ClientInfo, $($a),*) -> R + 'static,
            {
                type Ret = R;
                type Args = ($($a,)*);
                type WantClientInfo = DoWantClientInfo;
                type WantTimePoint = DontWantTimePoint;
                #[inline]
                #[allow(unused_variables)]
                fn apply(&mut self, info: &mut ClientInfo, time_point: OptTimePoint, ($($v,)*): Self::Args) -> R {
                    self(info, $($v),*)
                }
            }

            impl<F, R: RpcRet, $($a: 'static),*> RpcHandlerFn<HandlerSignature<DoWantClientInfoMut, DontWantTimePoint, R, ($($a,)*)>> for F
            where
                F: for<'c> FnMut(&'c mut ClientInfo, $($a),*) -> R + 'static,
            {
                type Ret = R;
                type Args = ($($a,)*);
                type WantClientInfo = DoWantClientInfoMut;
                type WantTimePoint = DontWantTimePoint;
                #[inline]
                #[allow(unused_variables)]
                fn apply(&mut self, info: &mut ClientInfo, time_point: OptTimePoint, ($($v,)*): Self::Args) -> R {
                    self(info, $($v),*)
                }
            }

            // Handlers that receive the client info and the optional time point.
            impl<F, R: RpcRet, $($a: 'static),*> RpcHandlerFn<HandlerSignature<DoWantClientInfo, DoWantTimePoint, R, ($($a,)*)>> for F
            where
                F: for<'c> FnMut(&'c ClientInfo, OptTimePoint, $($a),*) -> R + 'static,
            {
                type Ret = R;
                type Args = ($($a,)*);
                type WantClientInfo = DoWantClientInfo;
                type WantTimePoint = DoWantTimePoint;
                #[inline]
                fn apply(&mut self, info: &mut ClientInfo, time_point: OptTimePoint, ($($v,)*): Self::Args) -> R {
                    self(info, time_point, $($v),*)
                }
            }

            impl<F, R: RpcRet, $($a: 'static),*> RpcHandlerFn<HandlerSignature<DoWantClientInfoMut, DoWantTimePoint, R, ($($a,)*)>> for F
            where
                F: for<'c> FnMut(&'c mut ClientInfo, OptTimePoint, $($a),*) -> R + 'static,
            {
                type Ret = R;
                type Args = ($($a,)*);
                type WantClientInfo = DoWantClientInfoMut;
                type WantTimePoint = DoWantTimePoint;
                #[inline]
                fn apply(&mut self, info: &mut ClientInfo, time_point: OptTimePoint, ($($v,)*): Self::Args) -> R {
                    self(info, time_point, $($v),*)
                }
            }

            // Handlers that receive the optional time point.
            impl<F, R: RpcRet, $($a: 'static),*> RpcHandlerFn<HandlerSignature<DontWantClientInfo, DoWantTimePoint, R, ($($a,)*)>> for F
            where
                F: FnMut(OptTimePoint, $($a),*) -> R + 'static,
            {
                type Ret = R;
                type Args = ($($a,)*);
                type WantClientInfo = DontWantClientInfo;
                type WantTimePoint = DoWantTimePoint;
                #[inline]
                #[allow(unused_variables)]
                fn apply(&mut self, info: &mut ClientInfo, time_point: OptTimePoint, ($($v,)*): Self::Args) -> R {
                    self(time_point, $($v),*)
                }
            }

            impl<R: RpcRet, $($a: 'static),*> ClientSignature for fn($($a),*) -> R {
                type Ret = R;
                type Args = ($($a,)*);
            }
        )*
    };
}

/// The signature a client is made from ([`Protocol::make_client`]): a function pointer type
/// `fn(In...) -> Ret`.
pub trait ClientSignature {
    /// The return type of the verb.
    type Ret: RpcRet;
    /// The arguments of the verb, as a tuple.
    type Args: 'static;
}

handler_impls! {
    ()
    (A0 a0)
    (A0 a0, A1 a1)
    (A0 a0, A1 a1, A2 a2)
    (A0 a0, A1 a1, A2 a2, A3 a3)
    (A0 a0, A1 a1, A2 a2, A3 a3, A4 a4)
    (A0 a0, A1 a1, A2 a2, A3 a3, A4 a4, A5 a5)
    (A0 a0, A1 a1, A2 a2, A3 a3, A4 a4, A5 a5, A6 a6)
    (A0 a0, A1 a1, A2 a2, A3 a3, A4 a4, A5 a5, A6 a6, A7 a7)
    (A0 a0, A1 a1, A2 a2, A3 a3, A4 a4, A5 a5, A6 a6, A7 a7, A8 a8)
    (A0 a0, A1 a1, A2 a2, A3 a3, A4 a4, A5 a5, A6 a6, A7 a7, A8 a8, A9 a9)
}

/// Calls the handler with the client info, time point and arguments it takes, and
/// returns its result as a future (a failure becomes a failed future).
#[inline]
pub fn apply<F: RpcHandlerFn<M>, M>(
    func: &mut F,
    info: &mut ClientInfo,
    time_point: OptTimePoint,
    args: F::Args,
) -> Future<<F::Ret as Futurize>::Output> {
    <F::Ret as Futurize>::invoke(move || func.apply(info, time_point, args))
}

// A handler is always moved into the boxed function; a caller that keeps the
// handler passes a closure that borrows it instead.

/// The server function of a handler: it unmarshalls all parameters, calls the handler,
/// marshalls the return value and sends it back to the client.
pub fn recv_helper<S, F, M>(func: F) -> RpcHandlerFunc
where
    S: Write<<F::Ret as Futurize>::Output> + 'static,
    F: RpcHandlerFn<M>,
    F::Args: UnmarshallArgs<S>,
    M: 'static,
{
    type WaitStyleOf<F, M> = <<F as RpcHandlerFn<M>>::Ret as RpcRet>::Wait;
    let mut func = func;
    Box::new(
        move |client: SharedPtr<ServerConnection>,
              timeout: Option<RpcTimePoint>,
              msg_id: i64,
              data: RcvBuf,
              guard: Holder|
              -> Future<()> {
            // SAFETY: the shared pointer keeps the connection alive; handlers run on its shard.
            let cl = unsafe { client.get_mut_unchecked() };
            let memory_consumed = cl.estimate_request_size(data.size as usize);
            if memory_consumed > cl.max_request_size() {
                let err = format!(
                    "request size {} large than memory limit {}",
                    memory_consumed,
                    cl.max_request_size()
                );
                cl.get_logger().addr(&cl.peer_address(), &Sstring::from(err.as_str()));
                // FIXME: future is discarded
                let c = client.clone();
                // SAFETY: the server outlives its connections and is pinned once started.
                let _ =
                    try_with_gate(unsafe { Server::reply_gate_of(cl.get_server()) }, move || {
                        let c2 = c.clone();
                        WaitStyleOf::<F, M>::default()
                            .reply::<S, <F::Ret as Futurize>::Output>(
                                <F::Ret as Futurize>::make_exception_future(ExceptionPtr::new(
                                    RuntimeError(err),
                                )),
                                msg_id,
                                c,
                                timeout,
                                None,
                            )
                            .handle_exception(move |eptr| {
                                c2.get_logger().client_msg(
                                    c2.info_ref(),
                                    msg_id,
                                    &Sstring::from(format!(
                                        "got exception while processing an oversized message: {}",
                                        eptr
                                    )),
                                );
                            })
                    })
                    .handle_exception_type(|_: &GateClosedException| { /* ignore */ });
                return make_ready_future(());
            }
            // note: apply is executed asynchronously with regards to networking so we cannot chain futures here by doing "return apply()"
            // The handler lives in the registered function, which the gate holder `g` keeps
            // registered until the reply is sent.
            let funcp: *mut F = &mut func;
            let mut f = cl.wait_for_resources(memory_consumed, timeout).then(move |permit| {
                let g = guard;
                // FIXME: future is discarded
                // SAFETY: the shared pointer keeps the connection alive, and its server, and so the
                // gate, outlives its connections and stays pinned (see Server::reply_gate).
                let gate = unsafe { Server::reply_gate_of(client.get_server()) };
                let _ = try_with_gate(gate, move || -> Future<()> {
                    // SAFETY: as above.
                    let cl = unsafe { client.get_mut_unchecked() };
                    // SAFETY: the handler was registered on the protocol of S.
                    match unsafe { unmarshall::<S, F::Args>(cl, data) } {
                        Ok(args) => {
                            let start = RpcClockType::now();
                            // SAFETY: the handler stays registered while `g` is held (see above).
                            let func = unsafe { &mut *funcp };
                            let ret = apply(func, cl.info(), OptTimePoint(timeout), args);
                            ret.then_wrapped(move |ret: Future<<F::Ret as Futurize>::Output>| {
                                let c2 = client.clone();
                                let elapsed = RpcClockType::now().nanos_since_epoch()
                                    - start.nanos_since_epoch();
                                WaitStyleOf::<F, M>::default()
                                    .reply::<S, <F::Ret as Futurize>::Output>(
                                        ret,
                                        msg_id,
                                        client,
                                        timeout,
                                        Some(elapsed),
                                    )
                                    .handle_exception(move |eptr| {
                                        let _permit = permit;
                                        c2.get_logger().client_msg(
                                            c2.info_ref(),
                                            msg_id,
                                            &Sstring::from(format!(
                                                "got exception while processing a message: {}",
                                                eptr
                                            )),
                                        );
                                    })
                            })
                        }
                        Err(e) => {
                            cl.get_logger().client_msg(
                                cl.info_ref(),
                                msg_id,
                                &Sstring::from(format!(
                                    "caught exception while processing a message: {}",
                                    e
                                )),
                            );
                            make_ready_future(())
                        }
                    }
                })
                .handle_exception_type(move |_: &GateClosedException| {
                    let _g = g; /* ignore */
                });
            });

            if timeout.is_some() {
                f = f.handle_exception_type(|_: &SemaphoreTimedOut| { /* ignore */ });
            }

            f
        },
    )
}

/// The handler as given: the boxed handler function does not need to be copyable, so
/// this is the identity.
#[inline]
pub fn make_copyable_function<F>(func: F) -> F {
    func
}

/// The client side signature of a verb: the return type is converted from a smart
/// pointer to the type it points to, and [`Optional`] arguments to plain ones.
///
/// Examples: `fn(i32, Optional<i64>) -> Box<i32>` becomes `fn(i32, i64) -> i32`;
/// `fn(f32) -> f64` stays the same.
pub struct ClientFunctionType<Ret, In>(PhantomData<fn(In) -> Ret>);

impl<Ret: RpcRet, In: RemoveOptionalTuple> ClientFunctionType<Ret, In> {
    /// The client callable of verb `t`.
    #[inline]
    pub fn send_helper<S, M>(t: M) -> SendHelper<S, M, Ret::ClientRet, In::Type> {
        send_helper(t)
    }
}

/// The client a verb with return type `R` and arguments `A` gets.
pub type ClientOf<S, M, R, A> =
    SendHelper<S, M, <R as RpcRet>::ClientRet, <A as RemoveOptionalTuple>::Type>;

impl<S: 'static, M: RpcMsgType> Protocol<S, M> {
    /// The client callable of verb `t`, for return type `R` and arguments `A`.
    #[inline]
    fn make_client_sig<R: RpcRet, A: RemoveOptionalTuple>(&self, t: M) -> ClientOf<S, M, R, A> {
        ClientFunctionType::<R, A>::send_helper(t)
    }

    /// Returns a callable that invokes the verb `t` on the remote.
    ///
    /// `Func` is the signature of the verb, a function pointer type `fn(Args...) -> Ret`;
    /// it must be the same as, or compatible with, the one the server registered with
    /// [`Protocol::register_handler`]. The returned callable takes the client and the
    /// arguments, and returns a future of `Ret`.
    #[inline]
    pub fn make_client<Func: ClientSignature>(&self, t: M) -> ClientOf<S, M, Func::Ret, Func::Args>
    where
        Func::Args: RemoveOptionalTuple,
    {
        self.make_client_sig::<Func::Ret, Func::Args>(t)
    }

    /// Registers `func` as the handler called when the remote invokes the verb `t`, run in
    /// the scheduling group `sg`.
    ///
    /// The type of `func` determines the signature of the verb. `sg` lets different verbs
    /// run in different scheduling groups; the connection's
    /// [`IsolationConfig`](crate::engine::rpc::rpc::IsolationConfig) can
    /// instead choose the group per invocation.
    ///
    /// Returns a client of the verb, as [`Protocol::make_client`] does. Most callers
    /// discard it, since the real clients live on a remote node.
    ///
    /// # Errors
    ///
    /// Returns an error if a handler is already registered for `t`.
    pub fn register_handler_in<F, Mk>(
        self: Pin<&mut Self>,
        t: M,
        sg: SchedulingGroup,
        func: F,
    ) -> Result<ClientOf<S, M, F::Ret, F::Args>, ExceptionPtr>
    where
        S: Write<<F::Ret as Futurize>::Output>,
        F: RpcHandlerFn<Mk>,
        F::Args: UnmarshallArgs<S> + RemoveOptionalTuple,
        Mk: 'static,
    {
        // SAFETY: registering touches the handler map only; the protocol is not moved.
        let me = unsafe { self.get_unchecked_mut() };
        let recv = recv_helper::<S, F, Mk>(func);
        me.register_receiver(
            t,
            RpcHandler { sg, func: make_copyable_function(recv), use_gate: Gate::new() },
        )?;
        Ok(me.make_client_sig::<F::Ret, F::Args>(t))
    }

    /// Registers `func` as the handler called when the remote invokes the verb `t`, run in
    /// the default scheduling group.
    ///
    /// As [`Protocol::register_handler_in`]; the type of `func` determines the signature of
    /// the verb, and the returned client is usually discarded.
    ///
    /// # Errors
    ///
    /// Returns an error if a handler is already registered for `t`.
    #[inline]
    pub fn register_handler<F, Mk>(
        self: Pin<&mut Self>,
        t: M,
        func: F,
    ) -> Result<ClientOf<S, M, F::Ret, F::Args>, ExceptionPtr>
    where
        S: Write<<F::Ret as Futurize>::Output>,
        F: RpcHandlerFn<Mk>,
        F::Args: UnmarshallArgs<S> + RemoveOptionalTuple,
        Mk: 'static,
    {
        self.register_handler_in(t, SchedulingGroup::default(), func)
    }

    /// Unregisters the handler of the verb `t`.
    ///
    /// From the call on, new invocations of the verb fail; the returned future resolves
    /// once the handlers already running have finished and the handler is removed.
    pub fn unregister_handler(self: Pin<&mut Self>, t: M) -> Future<()> {
        // SAFETY: unregistering touches the handler map only; the protocol is not moved.
        let me = unsafe { self.get_unchecked_mut() };
        let this: *mut Protocol<S, M> = me;
        if let Some(h) = me._handlers.get_mut(&t) {
            return h.use_gate.close().finally(move || {
                // SAFETY: the protocol is pinned and outlives its handlers: dropping it drops
                // their gates, and a gate dropped with this close pending aborts.
                unsafe { (*this)._handlers.remove(&t) };
            });
        }
        make_ready_future(())
    }

    /// Whether a handler is registered (and not being unregistered) for `msg_id`.
    pub fn has_handler(&self, msg_id: M) -> bool {
        match self._handlers.get(&msg_id) {
            None => false,
            Some(h) => !h.use_gate.is_closed(),
        }
    }

    /// The handler of verb `msg_id`, with a holder that keeps it registered while in use.
    pub(crate) fn get_handler_impl(&mut self, msg_id: u64) -> Option<HandlerWithHolder> {
        let h = self._handlers.get_mut(&M::from_u64(msg_id))?;
        // SAFETY: the handler is boxed, so its gate stays in place while it is registered.
        match unsafe { Pin::new_unchecked(&h.use_gate) }.hold() {
            Ok(holder) => Some(HandlerWithHolder { handler: &mut **h, holder }),
            // unregistered, just ignore
            Err(_) => None,
        }
    }
}

// The `this` of a stream (sink or source) that its closures carry, as `XshardPtr`, to the
// shard of its connection. It meets the contract of
// `XshardPtr::new`: the stream outlives every cross-shard call (its user waits for close(),
// which takes the whole stream semaphore, or for the reads), and the two shards never touch
// the same field at once: a sink's remote part uses only `_remote_state` (used on that shard
// alone) and the connection pointer (not written after construction), and the remote part
// of close() runs once every send has released its units, after their last write of `_ex`;
// a source's owner shard waits for the remote refill before it touches `_bufs` again.
// Because a sink's two shards can run at the same time, neither side of a sink's closures forms
// a reference to the whole sink: each reaches the fields it uses through `addr_of!` on the
// pointer (`_remote_state` and `base._con` on the shard of the connection, `base._ex` and
// `base._con` on the owner shard).

impl<S: 'static, Out: MarshallArgs<S> + 'static> SinkImpl<Out> for RpcSinkImpl<S, Out> {
    #[inline]
    fn base(&self) -> &crate::engine::rpc::rpc_types::SinkImplBase {
        &self.base
    }

    fn call(&mut self, args: &Out) -> Future<()> {
        // note that we use remote serializer pointer, so if serailizer needs a state
        // it should have per-cpu one
        // SAFETY: the connection is alive while the sink holds it; S is its protocol's
        // serializer.
        let serializer = unsafe { &mut *(*xshard_get(&self.base._con)).serializer::<S>() };
        let mut data = match marshall(serializer, 4, args) {
            Ok(d) => d,
            Err(e) => return make_exception_future(e),
        };
        const _: () = assert!(SndBuf::CHUNK_SIZE >= 4, "send buffer chunk size is too small");
        let len = data.size - 4;
        // SAFETY: the frame has 4 bytes of head space in its first chunk.
        unsafe { write_le::<u32>(data.front().get_write(), len) };
        // we do not want to dead lock on huge packets, so let them in
        // but only one at a time
        let size = (data.size as usize).min(MAX_STREAM_BUFFERS_MEMORY);
        let seq_num = self._next_seq_num.0;
        self._next_seq_num.0 += 1;
        let this: *mut Self = self;
        let data = make_foreign(Box::new(data));
        // SAFETY: the sink, and so its semaphore, outlives its sends (its user waits for close(),
        // which takes all the units back), and does not move.
        unsafe { get_units(&mut self.base._sem, size) }.then(move |su| -> Future<()> {
            // The owner shard touches only `base._ex` and reads `base._con` here, through
            // pointers to those fields, never through a reference to the whole sink: the shard
            // of the connection may be inside the remote part of an earlier send, which uses
            // `_remote_state`.
            // SAFETY: the sink outlives its sends (close() waits for them); `_ex` is written
            // only on this shard, and not while this shared borrow lives.
            if let Some(ex) = unsafe { &*core::ptr::addr_of!((*this).base._ex) } {
                return make_exception_future(ex.clone());
            }
            // SAFETY: as above; the connection pointer is not written after construction.
            let owner = unsafe { (*core::ptr::addr_of!((*this).base._con)).get_owner_shard() };
            let mut data = Some(data);
            // It is OK to discard this future. The user is required to
            // wait for it when closing.
            // SAFETY: see the note on the `this` of a stream, above `impl SinkImpl for RpcSinkImpl`.
            let xthis = unsafe { XshardPtr::new(this) };
            let _ = Smp::submit_to(owner, move || -> Future<()> {
                let p: *mut Self = xthis.get();
                // SAFETY: as above; this part runs on the shard of the connection (see the note
                // on the `this` of a stream) and reaches only the fields it owns: the connection
                // pointer (read) and `_remote_state`, which no other shard touches.
                let con = unsafe { &mut *xshard_get(&*core::ptr::addr_of!((*p).base._con)) };
                if con.error() {
                    return make_exception_future(ExceptionPtr::new(closed_error()));
                }
                if con.sink_closed() {
                    return make_exception_future(ExceptionPtr::new(stream_closed()));
                }

                // SAFETY: as above.
                let rs = unsafe { &mut *core::ptr::addr_of_mut!((*p)._remote_state) };

                let Some(data) = data.take() else {
                    unreachable!("the call runs once");
                };
                let local_data = make_shard_local_buffer_copy(data);
                let seq_num_diff = seq_num.wrapping_sub(rs.last_seq_num);
                if seq_num_diff > 1 {
                    let e = rs.out_of_order_bufs.entry(seq_num).or_insert_with(|| {
                        HostBox::new(DeferredSndBuf {
                            pr: HostedPromise::new(),
                            data: Some(local_data),
                        })
                    });
                    // SAFETY: the entry is a live, bound host; its promise is reached through
                    // the raw pointer to it.
                    return unsafe {
                        HostedPromise::get_future(DeferredSndBuf::hosted_promise(e.as_ptr()))
                    };
                }

                rs.last_seq_num = seq_num;
                let ret_fut = con.send(local_data, None, None);
                while let Some(e) = rs.out_of_order_bufs.first_entry() {
                    if *e.key() != rs.last_seq_num + 1 {
                        break;
                    }
                    rs.last_seq_num = *e.key();
                    let d = e.remove();
                    let p = d.as_ptr();
                    // SAFETY: `d` owns the live buffer; its fields are reached through the raw
                    // pointer, so no reference covers the promise its future points into.
                    let (data, pr) = unsafe {
                        (
                            (*p).data.take(),
                            HostedPromise::take_writer(DeferredSndBuf::hosted_promise(p)),
                        )
                    };
                    // The entry was bound by HostBox::new and its writer is taken only here.
                    let (Some(data), Some(pr)) = (data, pr) else {
                        unreachable!("a deferred buffer is sent once");
                    };
                    let fut = con.send(data, None, None);
                    fut.forward_to_base(pr);
                }
                ret_fut
            })
            .then_wrapped(move |mut f: Future<()>| {
                let _su = su;
                // SAFETY: as above: on the owner shard, through the `_ex` field alone.
                let ex = unsafe { &mut *core::ptr::addr_of_mut!((*this).base._ex) };
                if f.failed() && ex.is_none() {
                    // first error is the interesting one
                    *ex = Some(f.get_exception());
                } else {
                    f.ignore_ready_future();
                }
            });
            make_ready_future(())
        })
    }

    fn flush(&mut self) -> Future<()> {
        let this: *mut Self = self;
        // wait until everything is sent out before returning.
        let func = move || -> Future<()> {
            // SAFETY: the sink outlives its flush.
            match unsafe { &(*this).base._ex } {
                Some(ex) => make_exception_future(ex.clone()),
                None => make_ready_future(()),
            }
        };
        // SAFETY: the sink, and so its semaphore, outlives its flush and does not move.
        unsafe { with_semaphore(&mut self.base._sem, MAX_STREAM_BUFFERS_MEMORY, func) }
    }

    fn close(&mut self) -> Future<()> {
        let this: *mut Self = self;
        let func = move || {
            // SAFETY: the sink outlives its close; the connection pointer is read through its
            // field alone.
            let owner = unsafe { (*core::ptr::addr_of!((*this).base._con)).get_owner_shard() };
            // SAFETY: see the note on the `this` of a stream, above `impl SinkImpl for RpcSinkImpl`.
            let xthis = unsafe { XshardPtr::new(this) };
            Smp::submit_to(owner, move || -> Future<()> {
                let p: *mut Self = xthis.get();
                // SAFETY: as above; this part runs on the shard of the connection (see the note
                // on the `this` of a stream) and reads the connection pointer and, once every
                // send has released its units, `_ex`, through those fields alone.
                let con: *mut Connection =
                    xshard_get(unsafe { &*core::ptr::addr_of!((*p).base._con) });
                // SAFETY: as above; no send writes `_ex` any more.
                let ex = unsafe { &*core::ptr::addr_of!((*p).base._ex) };
                // SAFETY: the connection is alive while the sink holds it.
                let c = unsafe { &mut *con };
                if c.sink_closed() {
                    // double close, should not happen!
                    return make_exception_future(ExceptionPtr::new(stream_closed()));
                }
                let f = if !c.error() && ex.is_none() {
                    // SAFETY: S is the serializer of the connection's protocol.
                    let serializer = unsafe { &mut *c.serializer::<S>() };
                    match marshall(serializer, 4, &()) {
                        Ok(mut data) => {
                            const _: () = assert!(
                                SndBuf::CHUNK_SIZE >= 4,
                                "send buffer chunk size is too small"
                            );
                            // SAFETY: the frame is the 4 bytes of head space.
                            unsafe { write_le::<u32>(data.front().get_write(), u32::MAX) }; // max len fragment marks an end of a stream
                            c.send(data, None, None)
                        }
                        Err(e) => make_exception_future(e),
                    }
                } else {
                    match ex {
                        Some(ex) => make_exception_future(ex.clone()),
                        None => make_exception_future(ExceptionPtr::new(closed_error())),
                    }
                };
                // SAFETY: as above.
                f.finally(move || unsafe { (*con).close_sink() })
            })
        };
        // SAFETY: the sink, and so its semaphore, outlives its close and does not move.
        unsafe { with_semaphore(&mut self.base._sem, MAX_STREAM_BUFFERS_MEMORY, func) }
    }
}

impl<S: 'static, Out: 'static> Drop for RpcSinkImpl<S, Out> {
    fn drop(&mut self) {
        // A failure to close might leave some continuations running after
        // this is destroyed, leading to use-after-free bugs.
        // SAFETY: the connection is alive while the sink holds it.
        engine_assert!(unsafe { (*xshard_get(&self.base._con)).sink_closed() });
    }
}

impl<S: 'static, In: UnmarshallArgs<S> + 'static> RpcSourceImpl<S, In> {
    /// Deserializes the first received buffer.
    fn process_one_buffer(&mut self) -> Future<Option<In>> {
        let buf: ForeignPtr<Box<RcvBuf>> = std::mem::take(self.base._bufs.front_mut());
        self.base._bufs.pop_front();
        // SAFETY: the connection is alive while the source holds it; S is its protocol's
        // serializer.
        let r = unsafe {
            unmarshall::<S, In>(
                &mut *xshard_get(&self.base._con),
                make_shard_local_buffer_copy(buf),
            )
        };
        match r {
            Ok(args) => make_ready_future(Some(args)),
            Err(e) => make_exception_future(e),
        }
    }
}

impl<S: 'static, In: UnmarshallArgs<S> + 'static> SourceImpl<In> for RpcSourceImpl<S, In> {
    #[inline]
    fn base(&self) -> &crate::engine::rpc::rpc_types::SourceImplBase {
        &self.base
    }

    #[inline]
    fn base_mut(&mut self) -> &mut crate::engine::rpc::rpc_types::SourceImplBase {
        &mut self.base
    }

    fn call(&mut self) -> Future<Option<In>> {
        if !self.base._bufs.empty() {
            return self.process_one_buffer();
        }

        let this: *mut Self = self;
        // SAFETY: see the note on the `this` of a stream, above `impl SinkImpl for RpcSinkImpl`.
        let xthis = unsafe { XshardPtr::new(this) };
        // refill buffers from remote cpu
        Smp::submit_to(self.base._con.get_owner_shard(), move || -> Future<()> {
            let this = xthis.get();
            // SAFETY: the source outlives its reads; this part runs on the shard of the
            // connection and fills the buffers of the source (see the note on the `this`
            // of a stream).
            let me = unsafe { &mut *this };
            let con: *mut Connection = xshard_get(&me.base._con);
            // SAFETY: the connection is alive while the source holds it.
            let c = unsafe { &mut *con };
            if c._source_closed {
                return make_exception_future(ExceptionPtr::new(stream_closed()));
            }
            // SAFETY: the buffers are the source's, which outlives the receive.
            unsafe { c.stream_receive(&mut me.base._bufs) }.then_wrapped(
                move |f: Future<()>| -> Future<()> {
                    // SAFETY: as above.
                    let (me, c) = unsafe { (&mut *this, &mut *con) };
                    if f.failed() {
                        let ex = f.get_exception();
                        return c.close_source().then_wrapped(move |mut f: Future<()>| {
                            f.ignore_ready_future();
                            make_exception_future::<()>(ex)
                        });
                    }
                    if me.base._bufs.empty() {
                        // nothing to read -> eof
                        return c.close_source().then_wrapped(|mut f: Future<()>| {
                            f.ignore_ready_future();
                            make_ready_future(())
                        });
                    }
                    make_ready_future(())
                },
            )
        })
        .then(move |()| -> Future<Option<In>> {
            // SAFETY: as above.
            let me = unsafe { &mut *this };
            if me.base._bufs.empty() { make_ready_future(None) } else { me.process_one_buffer() }
        })
    }
}

impl<Out: 'static> Sink<Out> {
    /// The id of the stream connection.
    pub fn get_id(&self) -> ConnectionId {
        // SAFETY: the connection is alive while the sink holds it.
        unsafe { (*xshard_get(&self.impl_().base()._con)).get_connection_id() }
    }
}

impl<In: 'static> Source<In> {
    /// The id of the stream connection.
    pub fn get_id(&self) -> ConnectionId {
        // SAFETY: the connection is alive while the source holds it.
        unsafe { (*xshard_get(&self.impl_().base()._con)).get_connection_id() }
    }

    /// A sink over the connection of this source, whose values are `Out` serialized by `S`.
    pub fn make_sink<S: 'static, Out: MarshallArgs<S> + 'static>(&self) -> Sink<Out> {
        make_rpc_sink::<S, Out>(self.impl_().base()._con.clone())
    }
}

// `StreamingDomainType` implements `Hash` in rpc.rs.
