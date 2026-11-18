//! Tests of Unix domain sockets: echo over path, abstract and autobound addresses on both
//! ends, aborting an accept, and sending to a named socket from an unbound one.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::Pin;
use std::sync::LazyLock;

use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::loop_::do_until;
use teseladb::engine::core::reactor::{
    engine, listen, make_bound_datagram_channel, make_unbound_datagram_channel,
};
use teseladb::engine::core::thread::{Thread, async_};
use teseladb::engine::net::api::{AcceptResult, ServerSocket};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{SocketAddress, Transport};
use teseladb::engine::net::unix_address::UnixDomainAddr;
use teseladb::engine::util::log::Logger;

#[allow(dead_code)]
static IPLOG: LazyLock<Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("unix_domain").expect("logger registered once"));

const AF_UNIX: i32 = 1;

fn ud(path: &[u8]) -> SocketAddress {
    SocketAddress::from_unix_domain_addr(&UnixDomainAddr::new(path))
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

struct UdServerClient {
    test_message: String,
    server_addr: SocketAddress,
    client_path: Option<Vec<u8>>,
    rounds: i32,
    rounds_left: i32,
    lstn_sock: *mut ServerSocket,
    th: Option<Thread>,
    abort_after: i32, // if set - force the listening socket down after that number of rounds
    planned_abort: bool, // set when abort_accept() is called
}

impl UdServerClient {
    fn new(server_path: &[u8], client_path: Option<&[u8]>, rounds: i32) -> Self {
        Self::with_abort(server_path, client_path, rounds, 0)
    }

    fn with_abort(
        server_path: &[u8],
        client_path: Option<&[u8]>,
        rounds: i32,
        abort_run: i32,
    ) -> Self {
        UdServerClient {
            test_message: "are you still the same?".to_string(),
            server_addr: ud(server_path),
            client_path: client_path.map(|p| p.to_vec()),
            rounds,
            rounds_left: rounds,
            lstn_sock: std::ptr::null_mut(),
            th: None,
            abort_after: abort_run,
            planned_abort: false,
        }
    }

    fn init_server(&mut self) -> Future<()> {
        let this: *mut Self = self;
        do_with(try_or_fail!(listen(self.server_addr)), move |lstn: &mut ServerSocket| {
            // SAFETY: the client object lives in the test's do_with
            // until run() resolves, which waits for this future.
            let me = unsafe { &mut *this };
            me.lstn_sock = lstn; // required when aborting (on some tests)
            let lstnp: *mut ServerSocket = lstn;

            //  start the clients here, where we know the server is listening
            me.th = Some(
                Thread::new(move || -> Result<(), ExceptionPtr> {
                    // SAFETY: the client object lives in the test's do_with until run() resolves, which waits for this thread.
                    let me = unsafe { &mut *this };
                    for _ in 0..me.rounds {
                        if me.abort_after != 0 {
                            me.abort_after -= 1;
                            if me.abort_after == 0 {
                                me.planned_abort = true;
                                // SAFETY: the listening socket lives in do_with until the server loop ends.
                                unsafe { (*me.lstn_sock).abort_accept() };
                                break;
                            }
                        }
                        me.client_round()?;
                    }
                    Ok(())
                })
                .expect("thread"),
            );

            do_until(
                // SAFETY: the client object lives in the test's do_with until run() resolves, which waits for this loop.
                move || unsafe { (*this).rounds_left <= 0 },
                move || {
                    // SAFETY: as above.
                    unsafe { (*lstnp).accept() }.then(move |from_accept: AcceptResult| {
                        // SAFETY: the client object lives in the test's do_with until run() resolves, which waits for this loop.
                        let me = unsafe { &mut *this };
                        let mut cn = from_accept.connection;
                        let cn_addr = from_accept.remote_address;
                        me.rounds_left -= 1;
                        //  verify the client address
                        if let Some(cp) = &me.client_path {
                            let _tmmp = ud(cp);
                            assert_eq!(cn_addr, ud(cp));
                        }

                        do_with((cn.input(Default::default()), cn.output(8192)), |io| {
                            let iop: *mut (
                                teseladb::engine::core::iostream::InputStream<u8>,
                                teseladb::engine::core::iostream::OutputStream<u8>,
                            ) = io;
                            // SAFETY: do_with keeps both streams alive until this chain resolves.
                            unsafe { (*iop).0.read() }
                                .then(move |bb| {
                                    let mut ans = String::from("-");
                                    if !bb.empty() && bb.size() != 0 {
                                        ans =
                                            format!("+{}", String::from_utf8_lossy(bb.as_slice()));
                                    }
                                    // SAFETY: do_with keeps both streams alive until this chain resolves.
                                    let out = unsafe { &mut (*iop).1 };
                                    // SAFETY: do_with keeps both streams alive until this chain resolves.
                                    out.write_str(&ans)
                                        .then(move |()| unsafe { (*iop).1.flush() })
                                        .then(move |()| unsafe { (*iop).1.close() })
                                })
                                .then(move |()| unsafe { (*iop).0.close() })
                                .then(|()| make_ready_future(()))
                        })
                        .then(|()| make_ready_future(()))
                    })
                },
            )
            .handle_exception(move |e: ExceptionPtr| -> Future<()> {
                // OK to get here only if the test was a "planned abort" one
                // SAFETY: the client object lives in the test's do_with until run() resolves, which waits for this chain.
                if !unsafe { (*this).planned_abort } {
                    return make_exception_future(e);
                }
                make_ready_future(())
            })
            // SAFETY: the client object lives in the test's do_with until run() resolves, which waits for this chain.
            .finally(move || match unsafe { (*this).th.as_mut() } {
                Some(th) => th.join(),
                None => make_ready_future(()),
            })
        })
    }

    /// Send a message to the server, and expect (almost) the same string back.
    /// If 'client_path' is set, the client binds to the named path.
    // Runs in an engine thread.
    fn client_round(&mut self) -> Result<(), ExceptionPtr> {
        let mut cc = match &self.client_path {
            // SAFETY: the reactor of this shard runs the test.
            Some(cp) => unsafe { (*engine()).net() }
                .connect(self.server_addr, ud(cp), Transport::TCP)
                .get()?,
            // SAFETY: `engine()` is the reactor of this shard, which runs the test.
            None => unsafe { (*engine()).net() }
                .connect(self.server_addr, SocketAddress::default(), Transport::TCP)
                .get()?,
        };

        let mut inp = cc.input(Default::default());
        let mut out = cc.output(8192);

        out.write_str(&self.test_message).get()?;
        out.flush().get()?;
        let bb = inp.read().get()?;
        assert_eq!(bb.as_slice(), format!("+{}", self.test_message).as_bytes());
        inp.close().get()?;
        out.close().get()?;
        Ok(())
    }

    fn run(&mut self) -> Future<()> {
        let this: *mut Self = self;
        async_(move || -> Result<(), ExceptionPtr> {
            // SAFETY: the caller keeps the object alive until run() resolves.
            let serverfut = unsafe { (*this).init_server() };
            serverfut.get()?;
            Ok(())
        })
    }
}

/// Makes a directory of this process the current one, once: the sockets the tests name by a
/// relative path are made there. The tests run from their crate's directory, which on a
/// development machine is a folder shared by the host, where the kernel loses the socket files a
/// test left: nothing could remove or even stat them again (`rm: cannot remove '3': Operation
/// not supported`, 18-11-2026), and the tests that bind those names failed from then on.
fn in_own_dir() {
    static MOVED: std::sync::Once = std::sync::Once::new();
    MOVED.call_once(|| {
        let dir = std::env::temp_dir().join(format!("unix_domain_test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the directory of the test's sockets");
        std::env::set_current_dir(&dir).expect("the test's directory as the current one");
    });
}

fn rm(what: &str) {
    let res = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("rm -f {}", what))
        .status()
        .map(|s| s.code().unwrap_or(-1))
        .unwrap_or(-1);
    assert_eq!(res, 0);
}

//  testing the various address types, both on the server and on the
//  client side

teseladb::engine_test_case!(unixdomain_server, {
    rm("/tmp/ry");
    let uds = UdServerClient::new(b"/tmp/ry", None, 3);
    do_with(uds, |uds: &mut UdServerClient| uds.run())
});

teseladb::engine_test_case!(unixdomain_abs, {
    let sv_name = *b"\x00111";
    //ud_server_client uds(string{"\0111",4}, string{"\0112",4}, 1);
    let uds = UdServerClient::new(&sv_name, None, 4);
    do_with(uds, |uds: &mut UdServerClient| uds.run())
    //return make_ready_future<>();
});

teseladb::engine_test_case!(unixdomain_abs_bind, {
    let sv_name = *b"\x00111";
    let cl_name = *b"\x00112";
    let uds = UdServerClient::new(&sv_name, Some(&cl_name), 1);
    do_with(uds, |uds: &mut UdServerClient| uds.run())
});

teseladb::engine_test_case!(unixdomain_abs_bind_2, {
    let sv_name = *b"\x001\x00\x0a1";
    let cl_name = *b"\x001\x00\x0a2";
    let uds = UdServerClient::new(&sv_name, Some(&cl_name), 2);
    do_with(uds, |uds: &mut UdServerClient| uds.run())
});

teseladb::engine_test_case!(unixdomain_text, {
    let addr1 = ud(b"abc");
    assert_eq!(format!("{}", addr1), "abc");
    let addr2 = ud(b"");
    assert_eq!(format!("{}", addr2), "{unnamed}");
    let addr3 = ud(b"\0abc\0");
    assert_eq!(format!("{}", addr3), "@abc_");
    make_ready_future(())
});

teseladb::engine_test_case!(unixdomain_bind, {
    in_own_dir();
    rm("111 112");
    let uds = UdServerClient::new(b"111", Some(b"112"), 1);
    do_with(uds, |uds: &mut UdServerClient| uds.run())
});

teseladb::engine_test_case!(unixdomain_short, {
    in_own_dir();
    rm("3");
    let uds = UdServerClient::new(b"3", None, 10);
    do_with(uds, |uds: &mut UdServerClient| uds.run())
});

//  test our ability to abort the accept()'ing on a socket.
//  The test covers a specific bug in the handling of abort_accept()
teseladb::engine_test_case!(unixdomain_abort, {
    let sockname = String::from("7"); // note: no portable & warnings-free option
    let _ = std::fs::remove_file(&sockname);
    let uds = UdServerClient::with_abort(sockname.as_bytes(), None, 10, 4);
    do_with(uds, move |uds: &mut UdServerClient| {
        uds.run().finally(move || {
            let _ = std::fs::remove_file(&sockname);
            make_ready_future(())
        })
    })
});

// From man 7 unix:
//  If a bind(2) call specifies addrlen as sizeof(sa_family_t), or
//  the SO_PASSCRED socket option was specified for a socket that was
//  not explicitly bound to an address, then the socket is autobound
//  to an abstract address.
fn autobind() -> SocketAddress {
    let mut addr = SocketAddress {
        addr_length: std::mem::size_of::<u16>() as u32, // offsetof(sockaddr_un, sun_path)
        ..SocketAddress::default()
    };
    addr.u.sa.sa_family = AF_UNIX as u16;
    addr
}

fn to_string(mut p: Packet) -> String {
    p.linearize_all();
    let f = p.frag(0);
    // SAFETY: a fragment points at `size` readable bytes.
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(f.base, f.size) }).into_owned()
}

teseladb::engine_test_case!(unixdomain_datagram_autobind, {
    teseladb::engine::core::thread::async_(|| {
        let mut chan1 = make_unbound_datagram_channel(AF_UNIX as u16)?;
        let mut chan2 = make_bound_datagram_channel(&autobind())?;

        chan1.send_str(&chan2.local_address(), "hello").get()?;

        let mut dgram = chan2.receive().get()?;
        let received = to_string(std::mem::take(dgram.get_data()));
        assert_eq!(received, "hello");
        Ok(())
    })
});

unsafe extern "C" {
    fn mkdtemp(template: *mut u8) -> *mut u8;
}

teseladb::engine_test_case!(unixdomain_datagram_named_bound, {
    teseladb::engine::core::thread::async_(|| {
        // Create a temporary directory for the socket file using mkdtemp.
        let mut tmpdir = *b"/tmp/engine-test-XXXXXX\0";
        // SAFETY: a NUL-terminated, writable template.
        let tmpdir_ptr = unsafe { mkdtemp(tmpdir.as_mut_ptr()) };
        if tmpdir_ptr.is_null() {
            return Err(ExceptionPtr::new(RuntimeError("mkdtemp failed".to_string())));
        }
        let dir = String::from_utf8_lossy(&tmpdir[..tmpdir.len() - 1]).into_owned();

        // Create a socket file in the temporary directory.
        let socket_path = format!("{}/socket", dir);
        let mut named_receiver = make_bound_datagram_channel(&ud(socket_path.as_bytes()))?;

        // Verify that a socket file was created.
        assert!(std::path::Path::new(&socket_path).exists());

        // Send a message to the named socket using an unbound socket.
        let mut sender = make_unbound_datagram_channel(AF_UNIX as u16)?;
        sender.send_str(&ud(socket_path.as_bytes()), "hihi").get()?;
        let mut dgram = named_receiver.receive().get()?;
        let received = to_string(std::mem::take(dgram.get_data()));
        assert_eq!(received, "hihi");

        // Try to be nice and remove the temporary directory.
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    })
});
