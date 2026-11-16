//! Socket objects of the native network stack: the adapters between the generic socket API
//! ([`ServerSocketImpl`], [`ConnectedSocketImpl`], [`SocketImpl`], data sources and sinks)
//! and the native TCP implementation ([`Tcp`], [`Connection`], [`Listener`]).
//!
//! Every type here is generic over the inet traits of its `Tcp` (in practice TCP over
//! IPv4) and belongs to the shard that owns the connection: connections are shared
//! between the socket, its source and its sink through a shard-local [`LwSharedPtr`] and
//! must never cross shards. The module also keeps per-scheduling-group byte counters of the
//! native stack.

use std::cell::Cell;
use std::time::Duration;

use crate::engine::core::deleter::{Deleter, make_deleter_with};
use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::iostream::{DataSink, DataSinkImpl, DataSource, DataSourceImpl};
use crate::engine::core::reactor::engine_logger;
use crate::engine::core::scheduling::internal::scheduling_group_index;
use crate::engine::core::scheduling::{current_scheduling_group, max_scheduling_groups};
use crate::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::api::{
    AcceptResult, ConnectedSocket, KeepaliveParams, ListenOptions, TcpKeepaliveParams,
    make_ipv4_address_ip_port,
};
use crate::engine::net::inet_address::RuntimeError;
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::net::packet::Packet;
use crate::engine::net::socket_defs::{AF_INET, Ipv4Addr, SocketAddress, Transport};
use crate::engine::net::stack::{ConnectedSocketImpl, ServerSocketImpl, SocketImpl};
use crate::engine::net::tcp::{Connection, Listener, Tcp, TcpInetTraits};

/// Bytes sent and received by the native stack on this shard, per scheduling group.
pub mod native_stack_net_stats {
    use super::*;

    thread_local! {
        /// Bytes sent by the native stack on this shard, indexed by scheduling group.
        pub static BYTES_SENT: [Cell<u64>; max_scheduling_groups() as usize] =
            const { [const { Cell::new(0) }; max_scheduling_groups() as usize] };
        /// Bytes received by the native stack on this shard, indexed by scheduling group.
        pub static BYTES_RECEIVED: [Cell<u64>; max_scheduling_groups() as usize] =
            const { [const { Cell::new(0) }; max_scheduling_groups() as usize] };
    }

    /// Adds `n` to the counter of the current scheduling group.
    #[inline]
    pub(crate) fn add(
        counters: &'static std::thread::LocalKey<[Cell<u64>; max_scheduling_groups() as usize]>,
        n: u64,
    ) {
        let sg_id = scheduling_group_index(current_scheduling_group()) as usize;
        counters.with(|b| {
            // SAFETY: a scheduling group index is always below max_scheduling_groups().
            let c = unsafe { b.get_unchecked(sg_id) };
            c.set(c.get() + n);
        });
    }
}

/// A listening socket of the native stack.
pub struct NativeServerSocketImpl<T: TcpInetTraits> {
    _listener: Listener<T>,
}

impl<T: TcpInetTraits> NativeServerSocketImpl<T> {
    /// Listens on `port` of `proto` with an accept backlog of 100. The listener's
    /// registered state has its own node, so the socket moves freely; `_opt` is ignored.
    pub fn new(proto: &mut Tcp<T>, port: u16, _opt: ListenOptions) -> Self {
        NativeServerSocketImpl { _listener: proto.listen(port, 100) }
    }
}

impl<T: TcpInetTraits> ServerSocketImpl for NativeServerSocketImpl<T> {
    fn accept(&mut self) -> Future<AcceptResult> {
        self._listener.accept().then(|conn: Connection<T>| {
            // Read what is needed from `conn` before it is moved into the shared pointer.
            let ip = conn.foreign_ip().ip;
            let port = conn.foreign_port();
            let shared = make_lw_shared(conn);
            make_ready_future::<AcceptResult>(AcceptResult {
                connection: ConnectedSocket::from_impl(Box::new(
                    NativeConnectedSocketImpl::<T>::new(shared),
                )),
                remote_address: make_ipv4_address_ip_port(ip, port),
            })
        })
    }

    fn abort_accept(&mut self) {
        self._listener.abort_accept();
    }

    fn local_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        let host = self._listener.get_tcp().inet().host_address();
        Ok(SocketAddress::from_ipv4_addr(Ipv4Addr { ip: host.ip, port: self._listener.port() }))
    }
}

/// A connection of the native stack.
pub struct NativeConnectedSocketImpl<T: TcpInetTraits> {
    _conn: LwSharedPtr<Connection<T>>,
}

impl<T: TcpInetTraits> NativeConnectedSocketImpl<T> {
    /// Wraps `conn`.
    pub fn new(conn: LwSharedPtr<Connection<T>>) -> Self {
        NativeConnectedSocketImpl { _conn: conn }
    }
}

/// A client socket of the native stack.
pub struct NativeSocketImpl<T: TcpInetTraits> {
    _proto: *mut Tcp<T>,
    _conn: Option<LwSharedPtr<Connection<T>>>,
}

impl<T: TcpInetTraits> NativeSocketImpl<T> {
    /// Creates an unconnected client socket of `proto` (it keeps the layer's root pointer,
    /// so `proto` must outlive the socket).
    pub fn new(proto: &mut Tcp<T>) -> Self {
        NativeSocketImpl { _proto: proto.root_ptr(), _conn: None }
    }
}

impl<T: TcpInetTraits> SocketImpl for NativeSocketImpl<T> {
    fn connect(
        &mut self,
        sa: SocketAddress,
        _local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket> {
        //TODO: implement SCTP
        assert!(proto == Transport::TCP);

        // FIXME: local is ignored since native stack does not support multiple IPs yet
        assert!(sa.as_posix_sockaddr().sa_family == AF_INET);

        // SAFETY: the tcp layer outlives the sockets made from it.
        let conn = make_lw_shared(unsafe { Tcp::connect_raw(self._proto, sa) });
        self._conn = Some(conn.clone());
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { conn.get_mut_unchecked() }.connected().then(move |()| {
            let csi = Box::new(NativeConnectedSocketImpl::<T>::new(conn));
            make_ready_future::<ConnectedSocket>(ConnectedSocket::from_impl(csi))
        })
    }

    fn set_reuseaddr(&mut self, _reuseaddr: bool) -> Result<(), ExceptionPtr> {
        // FIXME: implement
        crate::log_error!(engine_logger(), "Reuseaddr is not supported by native stack");
        Ok(())
    }

    fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr> {
        // FIXME: implement
        Ok(false)
    }

    fn shutdown(&mut self) -> Result<(), ExceptionPtr> {
        if let Some(c) = &self._conn {
            // SAFETY: the connection is on this shard; the call below is the only access to it
            // while it runs (the tcb layer never reaches a connection).
            unsafe { c.get_mut_unchecked() }.shutdown_connect();
        }
        Ok(())
    }
}

/// The state of a [`NativeDataSourceImpl`], kept in its own node.
struct NativeDataSourceState<T: TcpInetTraits> {
    _conn: LwSharedPtr<Connection<T>>,
    _cur_frag: usize,
    _eof: bool,
    _buf: Packet,
}

/// The read side of a native connection, as a data source.
///
/// Each `get` yields the next fragment of the packet last read from the connection, without
/// copying; when the packet is used up it waits for more data, and an empty read marks end
/// of stream. The state lives in its own node: `get`'s continuation keeps the node's root
/// pointer, because the source itself sits in the `Box` of a `DataSource`, which reasserts
/// its uniqueness each time it moves.
pub struct NativeDataSourceImpl<T: TcpInetTraits> {
    _state: NodePtr<NativeDataSourceState<T>>,
}

impl<T: TcpInetTraits> NativeDataSourceImpl<T> {
    /// A source reading from `conn`.
    pub fn new(conn: LwSharedPtr<Connection<T>>) -> Self {
        NativeDataSourceImpl {
            _state: NodePtr::new(NativeDataSourceState {
                _conn: conn,
                _cur_frag: 0,
                _eof: false,
                _buf: Packet::new(),
            }),
        }
    }

    /// Returns the next buffer: the next fragment of the current packet, or, when it is used
    /// up, the first fragment of the next packet read once data arrives.
    ///
    /// # Safety
    /// `this` is the root pointer of a live source state on this shard, with no reference to
    /// it live.
    unsafe fn get_from(this: *mut NativeDataSourceState<T>) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: guaranteed by the caller; the reference ends before the continuation runs.
        let me = unsafe { &mut *this };
        if me._eof {
            return make_ready_future::<TemporaryBuffer<u8>>(TemporaryBuffer::<u8>::new(0));
        }
        if me._cur_frag != me._buf.nr_frags() as usize {
            // `share()` first: it may move the first fragment out of the packet's internal
            // area, and the buffer must point to where it lives afterwards (which the shared
            // packet keeps alive).
            let p = me._buf.share();
            let f = me._buf.frag(me._cur_frag as u32);
            me._cur_frag += 1;
            // SAFETY: the fragment's bytes live as long as the shared packet `p`, which the
            // buffer's deleter owns.
            return make_ready_future::<TemporaryBuffer<u8>>(unsafe {
                TemporaryBuffer::<u8>::with_deleter(
                    f.base,
                    f.size,
                    make_deleter_with(Deleter::default(), move || drop(p)),
                )
            });
        }
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { me._conn.get_mut_unchecked() }.wait_for_data().then(move |()| {
            // SAFETY: the data source outlives its pending `get`; this runs later from the
            // reactor with no reference to the state live, through its root pointer.
            let me = unsafe { &mut *this };
            // SAFETY: the connection is on this shard; the call below is the only access to it
            // while it runs (the tcb layer never reaches a connection).
            me._buf = unsafe { me._conn.get_mut_unchecked() }.read();
            native_stack_net_stats::add(
                &native_stack_net_stats::BYTES_RECEIVED,
                me._buf.len() as u64,
            );
            me._cur_frag = 0;
            me._eof = me._buf.len() == 0;
            // SAFETY: `me` is not used again; `this` is the root pointer.
            unsafe { Self::get_from(this) }
        })
    }

    /// The connection.
    #[inline]
    fn conn(&self) -> &LwSharedPtr<Connection<T>> {
        // SAFETY: no `&mut` to the state is live outside `get_from`, which is not running.
        unsafe { &self._state.get()._conn }
    }
}

impl<T: TcpInetTraits> DataSourceImpl for NativeDataSourceImpl<T> {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the state's root pointer; no reference to the state is live here.
        unsafe { Self::get_from(self._state.as_ptr()) }
    }

    fn close(&mut self) -> Future<()> {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { self.conn().get_mut_unchecked() }.close_write();
        make_ready_future(())
    }
}

/// The write side of a native connection, as a data sink. Supports batched flushes; a
/// failed batch flush closes the read side of the connection.
pub struct NativeDataSinkImpl<T: TcpInetTraits> {
    _conn: LwSharedPtr<Connection<T>>,
}

impl<T: TcpInetTraits> NativeDataSinkImpl<T> {
    /// A sink writing to `conn`.
    pub fn new(conn: LwSharedPtr<Connection<T>>) -> Self {
        NativeDataSinkImpl { _conn: conn }
    }
}

impl<T: TcpInetTraits> DataSinkImpl for NativeDataSinkImpl<T> {
    fn put(&mut self, p: Packet) -> Future<()> {
        native_stack_net_stats::add(&native_stack_net_stats::BYTES_SENT, p.len() as u64);
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { self._conn.get_mut_unchecked() }.send(p)
    }

    fn close(&mut self) -> Future<()> {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { self._conn.get_mut_unchecked() }.close_write();
        make_ready_future(())
    }

    fn can_batch_flushes(&self) -> bool {
        true
    }

    fn on_batch_flush_error(&mut self) {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { self._conn.get_mut_unchecked() }.close_read();
    }
}

impl<T: TcpInetTraits> ConnectedSocketImpl for NativeConnectedSocketImpl<T> {
    fn source(&mut self) -> DataSource {
        DataSource::new(Box::new(NativeDataSourceImpl::<T>::new(self._conn.clone())))
    }

    fn sink(&mut self) -> DataSink {
        DataSink::new(Box::new(NativeDataSinkImpl::<T>::new(self._conn.clone())))
    }

    fn shutdown_input(&mut self) {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { self._conn.get_mut_unchecked() }.close_read();
    }

    fn shutdown_output(&mut self) {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { self._conn.get_mut_unchecked() }.close_write();
    }

    fn set_nodelay(&mut self, _nodelay: bool) -> Result<(), ExceptionPtr> {
        // FIXME: implement
        Ok(())
    }

    fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        // FIXME: implement
        Ok(true)
    }

    fn set_keepalive(&mut self, _keepalive: bool) -> Result<(), ExceptionPtr> {
        // FIXME: implement
        crate::log_error!(engine_logger(), "Keepalive is not supported by native stack");
        Ok(())
    }

    fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        // FIXME: implement
        Ok(false)
    }

    fn set_keepalive_parameters(&mut self, _p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        // FIXME: implement
        crate::log_error!(
            engine_logger(),
            "Keepalive parameters are not supported by native stack"
        );
        Ok(())
    }

    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        // FIXME: implement
        Ok(KeepaliveParams::Tcp(TcpKeepaliveParams {
            idle: Duration::ZERO,
            interval: Duration::ZERO,
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
        Err(ExceptionPtr::from(RuntimeError(
            "Setting custom socket options is not supported for native stack".to_string(),
        )))
    }

    unsafe fn get_sockopt(
        &self,
        _level: i32,
        _optname: i32,
        _data: *mut u8,
        _len: usize,
    ) -> Result<i32, ExceptionPtr> {
        Err(ExceptionPtr::from(RuntimeError(
            "Getting custom socket options is not supported for native stack".to_string(),
        )))
    }

    fn local_address(&self) -> SocketAddress {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        let c = unsafe { self._conn.get_mut_unchecked() };
        SocketAddress::from_ipv4_addr(Ipv4Addr { ip: c.local_ip().ip, port: c.local_port() })
    }

    fn remote_address(&self) -> SocketAddress {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        let c = unsafe { self._conn.get_mut_unchecked() };
        SocketAddress::from_ipv4_addr(Ipv4Addr { ip: c.foreign_ip().ip, port: c.foreign_port() })
    }

    fn wait_input_shutdown(&mut self) -> Future<()> {
        // SAFETY: the connection is on this shard; the call below is the only access to it
        // while it runs (the tcb layer never reaches a connection).
        unsafe { self._conn.get_mut_unchecked() }.wait_input_shutdown()
    }
}
