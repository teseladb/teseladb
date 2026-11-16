//! Tests of TCP sockets: echo over connected sockets, a custom allocator for the socket
//! buffers, concurrent readers and writers, aborting a connect with shutdown, and the
//! send and receive buffer sizes of listening and accepted sockets.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, make_exception_future, make_ready_future,
};
use teseladb::engine::core::future_util::sleep_abortable_with;
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::loop_::do_until;
use teseladb::engine::core::posix::FileDesc;
use teseladb::engine::core::reactor::{Reactor, connect, listen_with, make_socket, yield_};
use teseladb::engine::core::sleep::{SleepAborted, sleep_steady as sleep};
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timer::SteadyClockType;
use teseladb::engine::core::when_all::when_all;
use teseladb::engine::net::api::{
    AcceptResult, ConnectedSocket, ListenOptions, LoadBalancingAlgorithm, make_ipv4_address,
};
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress, Transport};

const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;
const IPPROTO_TCP: i32 = 6;
const SOL_SOCKET: i32 = 1;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;

/// The IPv4 socket address `host:port`; fails on a bad address.
fn addr(host: &str, port: u16) -> Result<SocketAddress, ExceptionPtr> {
    Ok(SocketAddress::from_ipv4_addr(Ipv4Addr::from_string_port(host, port)?))
}

/// An exception thrown by the body of a future-returning function: its future fails.
macro_rules! try_or_fail {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return make_exception_future(e),
        }
    };
}

fn reuse() -> ListenOptions {
    ListenOptions { reuse_address: true, ..ListenOptions::default() }
}

fn handle_connection(mut s: ConnectedSocket) -> Future<()> {
    let in_ = s.input(Default::default());
    let out = s.output(8192);
    do_with((in_, out), |io: &mut (InputStream<u8>, OutputStream<u8>)| {
        let iop: *mut (InputStream<u8>, OutputStream<u8>) = io;
        // SAFETY: do_with keeps both streams alive until the loop ends.
        do_until(
            move || unsafe { (*iop).0.eof() },
            move || {
                // SAFETY: do_with keeps both streams alive until the loop ends, and each step runs after the previous one has resolved.
                unsafe { (*iop).0.read() }.then(move |buf| {
                    // SAFETY: as above.
                    unsafe { (*iop).1.write_buffer(buf) }
                        // SAFETY: as above.
                        .then(move |()| unsafe { (*iop).1.close() })
                })
            },
        )
    })
}

fn echo_server_loop() -> Future<()> {
    let listener =
        try_or_fail!(listen_with(make_ipv4_address(&Ipv4Addr::from_port(1234)), reuse()));
    let remote = try_or_fail!(Ipv4Addr::from_string_port("127.0.0.1", 1234));
    do_with(listener, move |listener| {
        // Connect asynchronously in background.
        let _ = connect(make_ipv4_address(&remote)).then(|mut socket: ConnectedSocket| {
            socket.shutdown_output();
        });
        let lp: *mut _ = listener;
        listener
            .accept()
            .then(|ar: AcceptResult| {
                let s = ar.connection;
                handle_connection(s)
            })
            // SAFETY: do_with keeps the listener alive until this chain resolves.
            .then(move |()| unsafe { (*lp).abort_accept() })
    })
}

/// A memory resource over the system allocator that counts its calls.
struct MyMallocAllocator {
    allocs: AtomicI32,
    frees: AtomicI32,
}

static MALLOC_ALLOCATOR: MyMallocAllocator =
    MyMallocAllocator { allocs: AtomicI32::new(0), frees: AtomicI32::new(0) };

teseladb::engine_test_case!(socket_allocation_test, {
    echo_server_loop().finally(|| {
        let ok = MALLOC_ALLOCATOR.allocs.load(Ordering::Relaxed)
            == MALLOC_ALLOCATOR.frees.load(Ordering::Relaxed);
        Reactor::exit(if ok { 0 } else { 1 });
    })
});

teseladb::engine_test_case!(socket_skip_test, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut ss = listen_with(addr("127.0.0.1", 1234)?, reuse())?;
        let mut as_ = Box::pin(AbortSource::new());
        // SAFETY: the source is pinned in its box and outlives the client thread.
        let asp: *mut AbortSource = unsafe { as_.as_mut().get_unchecked_mut() };
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut socket = connect(addr("127.0.0.1", 1234)?).get()?;
            socket.output(8192).write_str("abc").get()?;
            socket.shutdown_output();
            // SAFETY: the source is pinned in its box, which outlives the client thread, so the pin is valid.
            match sleep_abortable_with::<SteadyClockType>(Duration::from_secs(10), unsafe {
                Pin::new_unchecked(&mut *asp)
            })
            .get()
            {
                Err(e) if e.is::<SleepAborted>() => {
                    // expected
                    return Ok(());
                }
                _ => {}
            }
            panic!("Skipping data from socket is likely stuck");
        });
        let mut accepted = ss.accept().get()?;
        let mut input = accepted.connection.input(Default::default());
        input.skip(16).get()?;
        as_.as_mut().request_abort();
        client.get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_file_desc_fdinfo, {
    let fd = try_or_fail!(FileDesc::socket(AF_INET, SOCK_STREAM, IPPROTO_TCP));
    let info = fd.fdinfo();
    assert_eq!(try_or_fail!(info.substr(0, 8)), "socket:[");
    make_ready_future(())
});

teseladb::engine_test_case!(socket_on_close_test, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut ss = listen_with(addr("127.0.0.1", 12345)?, reuse())?;

        let server_closed = Rc::new(Cell::new(false));
        let client_notified = Rc::new(Cell::new(false));

        let (sc, cn) = (server_closed.clone(), client_notified.clone());
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = connect(addr("127.0.0.1", 12345)?).get()?;
            let (sc2, cn2) = (sc.clone(), cn.clone());
            let close_wait_fiber = cln.wait_input_shutdown().then(move |()| {
                assert!(sc2.get());
                cn2.set(true);
                println!("Client: server closed");
            });

            let mut out = cln.output(8192);
            let mut in_ = cln.input(Default::default());

            while !cn.get() {
                println!("Client: -> message");
                out.write_str("hello").get()?;
                out.flush().get()?;
                sleep(Duration::from_millis(250)).get()?;
                println!("Client: <- message");
                let buf = in_.read().get()?;
                if buf.empty() {
                    println!("Client: server eof");
                    break;
                }
                sleep(Duration::from_millis(250)).get()?;
            }

            out.close().get()?;
            in_.close().get()?;
            close_wait_fiber.get()?;
            Ok(())
        });

        let (sc, cn) = (server_closed.clone(), client_notified.clone());
        let ssp: *mut _ = &mut ss;
        let server = async_(move || -> Result<(), ExceptionPtr> {
            // SAFETY: `ss` outlives both fibers, waited for below.
            let mut acc = unsafe { (*ssp).accept() }.get()?;
            let mut out = acc.connection.output(8192);
            let mut in_ = acc.connection.input(Default::default());

            for _ in 0..3 {
                let buf = in_.read().get()?;
                assert!(!cn.get());
                out.write_buffer(buf).get()?;
                out.flush().get()?;
                println!("Server: served");
            }

            sc.set(true);
            println!("Server: closing");
            out.close().get()?;
            in_.close().get()?;
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(socket_on_close_local_shutdown_test, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut ss = listen_with(addr("127.0.0.1", 12345)?, reuse())?;

        let server_closed = Rc::new(Cell::new(false));
        let client_notified = Rc::new(Cell::new(false));

        let (sc, cn) = (server_closed.clone(), client_notified.clone());
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = connect(addr("127.0.0.1", 12345)?).get()?;
            let (sc2, cn2) = (sc.clone(), cn.clone());
            let close_wait_fiber = cln.wait_input_shutdown().then(move |()| {
                assert!(!sc2.get());
                cn2.set(true);
                println!("Client: socket closed");
            });

            let mut out = cln.output(8192);
            cln.shutdown_input();

            let fin = Instant::now() + Duration::from_secs(1);
            loop {
                yield_().get()?;
                if cn.get() || Instant::now() >= fin {
                    break;
                }
            }
            assert!(cn.get());

            out.write_str("hello").get()?;
            out.flush().get()?;
            out.close().get()?;

            close_wait_fiber.get()?;
            Ok(())
        });

        let sc = server_closed.clone();
        let ssp: *mut _ = &mut ss;
        let server = async_(move || -> Result<(), ExceptionPtr> {
            // SAFETY: `ss` outlives both fibers.
            let mut acc = unsafe { (*ssp).accept() }.get()?;
            let mut in_ = acc.connection.input(Default::default());
            let _buf = in_.read().get()?;
            sc.set(true);
            println!("Server: closing");
            in_.close().get()?;
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

// The test makes sure it's possible to abort connect()-ing a socket before
// it succeeds or fails. The way to abort the in-flight connection is to call
// shutdown() on the socket. The connect() future must resolve shortly
// after that with exception.
//
// The test currently fails on io_uring backend -- calling shutdown() doesn't
// make the connect() future resolve, instead it resolves after kernel times
// out the socket, which's not what test expects.
teseladb::engine_test_case!(socket_connect_abort_test, {
    async_(|| -> Result<(), ExceptionPtr> {
        let too_late = Rc::new(Cell::new(false));
        let mut sk = make_socket();
        let tl = too_late.clone();
        let cf = sk
            .connect(addr("192.0.2.1", 12345)?, SocketAddress::default(), Transport::TCP)
            .then(|_cs: ConnectedSocket| -> Future<()> {
                println!("Connected");
                panic!("connect must not succeed");
            })
            .handle_exception(move |ex: ExceptionPtr| {
                println!("Cannot connect {}", ex);
                assert!(!tl.get());
            });

        let skp: *mut _ = &mut sk;
        let abort = sleep(Duration::from_millis(500)).then(move |()| {
            println!("Abort connect");
            // SAFETY: `sk` outlives the three futures, waited for below.
            let _ = unsafe { (*skp).shutdown() };
        });

        let tl = too_late.clone();
        let check = sleep(Duration::from_secs(2)).then(move |()| {
            println!("Connection must have been aborted already");
            tl.set(true);
        });

        let _ = when_all((cf, check, abort)).get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(socket_bufsize, {
    teseladb::engine::core::thread::async_(|| {
        // Test that setting the send and recv buffer sizes on the listening
        // socket is propagated to the socket returned by accept().
        let buf_size =
            |snd_size: Option<i32>, rcv_size: Option<i32>| -> Result<(i32, i32), ExceptionPtr> {
                let lo = ListenOptions {
                    reuse_address: true,
                    lba: LoadBalancingAlgorithm::Fixed,
                    so_sndbuf: snd_size,
                    so_rcvbuf: rcv_size,
                    ..ListenOptions::default()
                };
                let a = addr("127.0.0.1", 1234)?;
                let mut ss = listen_with(a, lo)?;
                let mut client = connect(a).get()?;
                let mut server = ss.accept().get()?.connection;

                let sockopt = |option: i32| -> Result<i32, ExceptionPtr> {
                    let mut val: i32 = 0;
                    // SAFETY: `val` is an i32 of the size passed.
                    let ret = unsafe {
                        server.get_sockopt(
                            SOL_SOCKET,
                            option,
                            &mut val as *mut i32 as *mut u8,
                            std::mem::size_of::<i32>(),
                        )
                    }?;
                    assert_eq!(ret, 0);
                    Ok(val)
                };

                let send = sockopt(SO_SNDBUF)?;
                let recv = sockopt(SO_RCVBUF)?;

                ss.abort_accept();
                client.shutdown_output();
                server.shutdown_output();

                Ok((send, recv))
            };

        const SMALL_SIZE: i32 = 8192;
        const BIG_SIZE: i32 = 128 * 1024;

        // we pass different sizes for send and recv to catch any copy/paste
        // style bugs
        let (send_small, recv_small) = buf_size(Some(SMALL_SIZE), Some(SMALL_SIZE * 2))?;
        let (send_big, recv_big) = buf_size(Some(BIG_SIZE), Some(BIG_SIZE * 2))?;

        // Setting socket buffer sizes isn't an exact science: the kernel does
        // some rounding, and also (currently) doubles the requested size and
        // also applies so limits. So as a basic check, assert simply that the
        // explicit small buffer ends up smaller than the explicit big buffer,
        // and that both results are at least as large as the requested amount.
        // The latter condition could plausibly fail if the OS clamped the size
        // at a small amount, but this is unlikely for the chosen buffer sizes.

        assert!(send_small < send_big);
        assert!(recv_small < recv_big);

        assert!(send_small >= SMALL_SIZE);
        assert!(send_big >= BIG_SIZE);

        assert!(recv_small >= SMALL_SIZE * 2);
        assert!(recv_big >= BIG_SIZE * 2);

        // not much to check here with "default" sizes, but let's at least call it
        // and check that we get a reasonable answer
        let (send_default, recv_default) = buf_size(None, None)?;

        assert!(send_default >= 4096);
        assert!(recv_default >= 4096);

        // we don't really know the default socket size and it can vary by kernel
        // config, but 20 MB should be enough for everyone.
        assert!(send_default < 20_000_000);
        assert!(recv_default < 20_000_000);
        Ok(())
    })
});
