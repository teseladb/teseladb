//! POSIX stack paths beyond cov_posix_test and cov2_posix_sockets_test (the original's
//! posix-stack.cc): without SO_REUSEPORT (never available, as in the original) only shard 0
//! listens in the kernel and hands each connection that the load balancer places on another
//! shard to that shard's listener, which takes it from its queue or completes its pending
//! accept; the port-based balancing; a second listener on the same address; the stacks made
//! directly (the default allocator, another allocator whose buffers the sockets read into,
//! the members the other shards' stack forwards, an unspecified listen address, the UDP
//! channel shorthand); a datagram's destination port; the interfaces' IPv6 flag; connects
//! that cannot bind (an explicit port in use, a local address the host does not have) and a
//! socket shut down before it connects.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]
// `make_udp_channel` is deprecated, and tested here on purpose.
#![allow(deprecated)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use teseladb::engine::core::sleep::sleep_steady;

use teseladb::engine::core::future::{ExceptionPtr, Future, SystemError};
use teseladb::engine::core::memory::MALLOC_ALLOCATOR;
use teseladb::engine::core::polymorphic_temporary_buffer::PolymorphicAllocator;
use teseladb::engine::core::reactor::{connect_from, engine, listen_with};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{
    AcceptResult, ConnectedSocket, ConnectedSocketInputStreamConfig, ListenOptions,
    LoadBalancingAlgorithm, NetworkStack, ServerSocket,
};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::posix_stack::{
    Conntrack, ConntrackHandle, LoadBalancer, PosixApNetworkStack, PosixNetworkStack,
};
use teseladb::engine::net::socket_defs::{AF_INET, AF_INET6, AF_UNIX};
use teseladb::engine::net::socket_defs::{Ipv4Addr, Ipv6Addr, SocketAddress, Transport};
use teseladb::engine::net::unix_address::UnixDomainAddr;
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine::util::program_options::OptionGroup;
use teseladb::engine_test_case;

/// Starts the test reactor with two shards before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov3_net_posix_test".to_string(), "--smp=2".to_string()];
        global_test_runner().start(&args);
    }
    start
};

const EADDRINUSE: i32 = 98;
const EADDRNOTAVAIL: i32 = 99;
const ECONNABORTED: i32 = 103;

/// The error number of a system error (`SystemError` or an I/O error with an OS code).
fn errno_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<SystemError>()
        .map(SystemError::code)
        .or_else(|| e.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error()))
}

fn addr(host: &str, port: u16) -> Result<SocketAddress, ExceptionPtr> {
    Ok(SocketAddress::from_ipv4_addr(Ipv4Addr::from_string_port(host, port)?))
}

/// A loopback port that was free a moment ago.
fn free_port() -> Result<u16, ExceptionPtr> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(ExceptionPtr::from)?;
    Ok(l.local_addr().map_err(ExceptionPtr::from)?.port())
}

thread_local! {
    /// Shard 1's listener and its pending accept.
    static SHARD1: RefCell<(Option<ServerSocket>, Option<Future<AcceptResult>>)> =
        const { RefCell::new((None, None)) };
    /// The connections shard 1 accepted.
    static SHARD1_CONNS: RefCell<Vec<ConnectedSocket>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with shard 1's listener, on shard 1.
fn on_shard1<R: Send + 'static>(
    f: impl FnOnce(&mut ServerSocket) -> Result<R, ExceptionPtr> + Send + 'static,
) -> Result<R, ExceptionPtr> {
    Smp::submit_to(1, move || {
        SHARD1.with(|s| f(s.borrow_mut().0.as_mut().expect("shard 1 listens")))
    })
    .get()
}

/// Listens on shard 1 (the stack of the other shards: no socket in the kernel).
fn listen_on_shard1(sa: SocketAddress) -> Result<(), ExceptionPtr> {
    Smp::submit_to(1, move || -> Result<(), ExceptionPtr> {
        assert_eq!(this_shard_id(), 1);
        let ss = listen_with(sa, ListenOptions { reuse_address: true, ..Default::default() })?;
        assert!(ss.local_address()? == sa);
        SHARD1.with(|s| s.borrow_mut().0 = Some(ss));
        Ok(())
    })
    .get()
}

/// Waits on shard 1 for its pending accept (started by `start_accept_on_shard1`) and returns
/// the remote port of the connection.
fn finish_accept_on_shard1() -> Result<u16, ExceptionPtr> {
    Smp::submit_to(1, || {
        async_(|| -> Result<u16, ExceptionPtr> {
            let f = SHARD1.with(|s| s.borrow_mut().1.take()).expect("an accept is pending");
            let ar = f.get()?;
            let port = ar.remote_address.port();
            // Kept open: a closed connection would lower shard 1's load again.
            SHARD1_CONNS.with(|c| c.borrow_mut().push(ar.connection));
            Ok(port)
        })
    })
    .get()
}

fn start_accept_on_shard1() -> Result<bool, ExceptionPtr> {
    Smp::submit_to(1, || -> Result<bool, ExceptionPtr> {
        SHARD1.with(|s| {
            let mut s = s.borrow_mut();
            let f = s.0.as_mut().expect("shard 1 listens").accept();
            let ready = f.available();
            s.1 = Some(f);
            Ok(ready)
        })
    })
    .get()
}

fn drop_shard1() -> Result<(), ExceptionPtr> {
    Smp::submit_to(1, || {
        SHARD1.with(|s| *s.borrow_mut() = (None, None));
        SHARD1_CONNS.with(|c| c.borrow_mut().clear());
    })
    .get()
}

/// Lets shard 0 accept a connection the kernel completed and hand it over to shard 1.
fn settle() {
    sleep_steady(Duration::from_millis(50)).get().expect("sleep");
}

/// Connects a client from shard 0 and returns it with its local port.
fn client(sa: SocketAddress) -> Result<(ConnectedSocket, u16), ExceptionPtr> {
    let c = connect_from(sa, SocketAddress::default(), Transport::TCP).get()?;
    let port = c.local_address().port();
    Ok((c, port))
}

engine_test_case!(shard_0_hands_connections_to_the_other_shard, {
    async_(|| -> Result<(), ExceptionPtr> {
        assert_eq!(Smp::count(), 2);
        let sa = addr("127.0.0.1", free_port()?)?;
        listen_on_shard1(sa)?;
        // The same address twice on one shard.
        let again =
            Smp::submit_to(1, move || listen_with(sa, ListenOptions::default()).map(|_| ())).get();
        assert_eq!(errno_of(&again.expect_err("in use")), Some(EADDRINUSE));
        // Shard 0 listens in the kernel, balancing by connection count.
        let mut ss = listen_with(sa, ListenOptions { reuse_address: true, ..Default::default() })?;
        let mut clients = Vec::new();

        // First connection: both shards idle, shard 0 takes it.
        let accepting = ss.accept();
        let (c, p) = client(sa)?;
        clients.push(c);
        let mine = accepting.get()?;
        assert_eq!(mine.remote_address.port(), p);
        // Second: shard 1 is the least loaded; it does not accept yet, so the connection
        // waits in its queue while shard 0 accepts again.
        let accepting = ss.accept();
        let (c, p2) = client(sa)?;
        clients.push(c);
        settle();
        assert!(start_accept_on_shard1()?, "queued connections are accepted at once");
        assert_eq!(finish_accept_on_shard1()?, p2);
        // Third goes to shard 0 again.
        let (c, p3) = client(sa)?;
        clients.push(c);
        // Kept open, like the others: a closed connection lowers its shard's load.
        let third = accepting.get()?;
        assert_eq!(third.remote_address.port(), p3);
        // Fourth: shard 1 already waits in accept, which the connection completes.
        let accepting = ss.accept();
        assert!(!start_accept_on_shard1()?);
        let (c, p4) = client(sa)?;
        clients.push(c);
        assert_eq!(finish_accept_on_shard1()?, p4);

        // Aborting shard 1's listener with and without an accept pending.
        assert!(!start_accept_on_shard1()?);
        on_shard1(|l| {
            l.abort_accept();
            Ok(())
        })?;
        let e = finish_accept_on_shard1().expect_err("aborted");
        assert_eq!(errno_of(&e), Some(ECONNABORTED));
        on_shard1(|l| {
            l.abort_accept();
            Ok(())
        })?;
        ss.abort_accept();
        assert!(accepting.get().is_err());
        drop((mine, third));
        drop(clients);
        drop_shard1()?;
        Ok(())
    })
});

engine_test_case!(port_balancing_places_connections_by_the_client_port, {
    async_(|| -> Result<(), ExceptionPtr> {
        let sa = addr("127.0.0.1", free_port()?)?;
        listen_on_shard1(sa)?;
        let mut ss = listen_with(
            sa,
            ListenOptions {
                reuse_address: true,
                lba: LoadBalancingAlgorithm::Port,
                ..Default::default()
            },
        )?;
        // Clients until one has an even port (shard 0) and one an odd port (shard 1).
        let mut seen = [false, false];
        let mut clients = Vec::new();
        let mut accepting = None;
        while !(seen[0] && seen[1]) {
            let pending = accepting.take().unwrap_or_else(|| ss.accept());
            let (c, p) = client(sa)?;
            clients.push(c);
            let shard = (p % 2) as usize;
            if shard == 0 {
                assert_eq!(pending.get()?.remote_address.port(), p);
            } else {
                settle();
                assert!(start_accept_on_shard1()?);
                assert_eq!(finish_accept_on_shard1()?, p);
                // Shard 0 accepts again meanwhile.
                accepting = Some(pending);
            }
            seen[shard] = true;
        }
        ss.abort_accept();
        if let Some(pending) = accepting {
            assert!(pending.get().is_err());
        }
        drop(clients);
        drop_shard1()?;
        Ok(())
    })
});

/// An allocator that counts its blocks.
struct Counting {
    allocated: AtomicUsize,
    freed: AtomicUsize,
}

impl PolymorphicAllocator for Counting {
    fn allocate(&self, n: usize) -> *mut u8 {
        self.allocated.fetch_add(1, Ordering::Relaxed);
        let layout = std::alloc::Layout::from_size_align(n.max(1), 16).expect("a layout");
        // SAFETY: the layout has a non-zero size.
        unsafe { std::alloc::alloc(layout) }
    }

    unsafe fn deallocate(&self, p: *mut u8, n: usize) {
        self.freed.fetch_add(1, Ordering::Relaxed);
        let layout = std::alloc::Layout::from_size_align(n.max(1), 16).expect("a layout");
        // SAFETY: `p` came from `allocate(n)`, with the same layout.
        unsafe { std::alloc::dealloc(p, layout) };
    }
}

static COUNTING: Counting = Counting { allocated: AtomicUsize::new(0), freed: AtomicUsize::new(0) };

engine_test_case!(a_stack_with_its_own_allocator_reads_into_its_buffers, {
    async_(|| -> Result<(), ExceptionPtr> {
        let opts = OptionGroup::new("posix");
        let mut stack = PosixNetworkStack::new(&opts, &COUNTING);
        assert!(!stack.has_per_core_namespace());
        // An unspecified address listens on the IPv4 wildcard.
        let ss = stack.listen(SocketAddress::default(), ListenOptions::default())?;
        let la = ss.local_address()?;
        assert_eq!(la.family(), AF_INET);
        assert!(la.addr().is_addr_any(), "{la}");
        let sa = addr("127.0.0.1", free_port()?)?;
        drop(ss);
        let mut ss =
            stack.listen(sa, ListenOptions { reuse_address: true, ..Default::default() })?;
        let accepting = ss.accept();
        let mut c = connect_from(sa, SocketAddress::default(), Transport::TCP).get()?;
        let mut server = accepting.get()?.connection;
        let mut out = c.output(8192);
        out.write_str("through the allocator").get()?;
        out.flush().get()?;
        let mut input = server.input(ConnectedSocketInputStreamConfig::default());
        let got = input.read().get()?;
        assert_eq!(got.as_slice(), b"through the allocator");
        assert!(COUNTING.allocated.load(Ordering::Relaxed) > 0);
        drop(got);
        out.close().get()?;
        input.close().get()?;
        drop(input);
        drop(server);
        assert!(COUNTING.freed.load(Ordering::Relaxed) > 0);
        Ok(())
    })
});

engine_test_case!(stacks_made_directly_and_the_members_the_other_shards_forward, {
    async_(|| -> Result<(), ExceptionPtr> {
        let opts = OptionGroup::new("posix");
        let mut stack = PosixNetworkStack::new_default(&opts);
        // The UDP shorthand: bound for an address, unbound for an unspecified one.
        let sa = addr("127.0.0.1", 0)?;
        let mut bound = stack.make_udp_channel(&sa)?;
        assert!(bound.local_address().port() != 0);
        let mut unbound = stack.make_udp_channel(&SocketAddress::default())?;
        unbound.close();
        assert!(unbound.is_closed());
        bound.close();

        let mut ap = PosixApNetworkStack::new(&opts, MALLOC_ALLOCATOR);
        assert!(!ap.has_per_core_namespace());
        assert_eq!(ap.supports_ipv6(), stack.supports_ipv6());
        assert_eq!(ap.network_interfaces()?.len(), stack.network_interfaces()?.len());
        let s = ap.stats(0);
        let _ = (s.bytes_sent, s.bytes_received);
        ap.clear_stats(0);
        let mut ch = ap.make_udp_channel(&sa)?;
        let to = ch.local_address();
        let mut tx = ap.make_unbound_datagram_channel(AF_INET)?;
        tx.send(&to, Packet::from_data(b"dst")).get()?;
        let mut d = ch.receive().get()?;
        // The destination port of a datagram is the receiving socket's.
        assert_eq!(d.get_dst_port()?, to.port());
        let mut b2 = ap.make_bound_datagram_channel(&sa)?;
        b2.close();
        ch.close();
        tx.close();
        // The other shards' stack listens on an unspecified address and on UNIX addresses
        // without a kernel socket.
        let wildcard = ap.listen(SocketAddress::default(), ListenOptions::default())?;
        assert_eq!(wildcard.local_address()?.port(), 0);
        let ua = SocketAddress::from_unix_domain_addr(&UnixDomainAddr::new(b"\0cov3_ap_unix"));
        let unix = ap.listen(ua, ListenOptions::default())?;
        assert!(unix.local_address()? == ua);
        drop(unix);
        drop(wildcard);
        let mut s = ap.socket();
        // Shutting down a socket that never connected does nothing.
        s.shutdown()?;
        // The interfaces say whether they have IPv6 addresses.
        // SAFETY: engine() is this shard's running reactor.
        for nif in unsafe { (*engine()).net() }.network_interfaces()? {
            assert_eq!(nif.supports_ipv6(), nif.addresses().iter().any(|a| a.is_ipv6()));
        }
        Ok(())
    })
});

engine_test_case!(connects_that_cannot_bind_their_local_address, {
    async_(|| -> Result<(), ExceptionPtr> {
        let target = addr("127.0.0.1", free_port()?)?;
        let mut l =
            listen_with(target, ListenOptions { reuse_address: true, ..Default::default() })?;
        // An explicit local port already bound: no retry with another port.
        let taken = free_port()?;
        let _holder =
            std::net::TcpListener::bind(("127.0.0.1", taken)).map_err(ExceptionPtr::from)?;
        let e = connect_from(target, addr("127.0.0.1", taken)?, Transport::TCP)
            .get()
            .err()
            .expect("in use");
        assert_eq!(errno_of(&e), Some(EADDRINUSE));
        // A local address the host does not have: the random ports are tried, then port 0,
        // which fails the same way.
        let e = connect_from(target, addr("192.0.2.77", 0)?, Transport::TCP)
            .get()
            .err()
            .expect("not available");
        assert_eq!(errno_of(&e), Some(EADDRNOTAVAIL));
        l.abort_accept();
        Ok(())
    })
});

engine_test_case!(datagrams_to_scopeless_ipv6_and_over_unix_sockets, {
    async_(|| -> Result<(), ExceptionPtr> {
        // SAFETY: this shard's running reactor; each use is a single call.
        let net = || unsafe { (*engine()).net() };
        // A link-local destination without a scope: the route table picks the interface
        // (none may match in a container); a site-local one needs no scope. Either send may
        // fail on a host without IPv6; only the route lookup is of interest.
        if net().supports_ipv6() {
            let mut ch = net().make_unbound_datagram_channel(AF_INET6)?;
            for a in ["[fe80::1]:9", "[fec0::1]:9"] {
                let to = SocketAddress::from_ipv6_addr(&Ipv6Addr::from_string(a)?);
                let _ = ch.send(&to, Packet::from_data(b"x")).get();
            }
            ch.close();
        }
        // A UNIX datagram has no destination port.
        let ua = SocketAddress::from_unix_domain_addr(&UnixDomainAddr::new(b"\0cov3_dgram"));
        let mut rx = net().make_bound_datagram_channel(&ua)?;
        let mut tx = net().make_unbound_datagram_channel(AF_UNIX)?;
        tx.send(&ua, Packet::from_data(b"unix")).get()?;
        let mut d = rx.receive().get()?;
        assert_eq!(d.get_data().len(), 4);
        assert!(d.get_dst_port().is_err());
        rx.close();
        tx.close();
        Ok(())
    })
});

engine_test_case!(connection_tracking_defaults, {
    async_(|| -> Result<(), ExceptionPtr> {
        // A handle that tracks nothing has nothing to give back when dropped.
        drop(ConntrackHandle::default());
        let lb = LoadBalancer::default();
        assert_eq!(lb.next_cpu(), 0);
        assert_eq!(lb.next_cpu(), 1);
        assert_eq!(lb.force_cpu(1), 1);
        lb.closed_cpu(0);
        assert_eq!(lb.next_cpu(), 0);
        let ct = Conntrack::default();
        assert_eq!(ct.get_handle_for(1).cpu(), 1);
        Ok(())
    })
});
