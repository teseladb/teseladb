//! Tests of TLS over gnutls: credentials and certificate loading, echo over TLS with and
//! without client certificates, verification failures and their subject and issuer, the
//! distinguished-names callback, mutual TLS, credential reloading, session resumption, ALPN,
//! closes and shutdowns, and a TLS client against an HTTPS server process.
//!
//! The certificates come from `common/tls_certs.rs`, and `certfile()` finds them. The cases marked
//! `enable_if_with_networking` run only with `--cfg engine_testing_with_networking`; the ones
//! marked `enable_if_without_networking` only without it.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/loopback_socket.rs"]
mod loopback_socket;
#[path = "common/tls_certs.rs"]
mod tls_certs;
#[path = "common/tmpdir.rs"]
mod tmpdir;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use loopback_socket::{LoopbackBuffer, LoopbackBufferType, LoopbackConnectedSocketImpl};
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, Promise, make_exception_future, make_ready_future,
};
use teseladb::engine::core::gate::{Gate, GateClosedException, try_with_gate};
use teseladb::engine::core::iostream::{
    DataSink, DataSinkImpl, DataSource, InputStream, OutputStream, OutputStreamOptions,
};
use teseladb::engine::core::loop_::{
    StopIteration, do_for_each, do_until, parallel_for_each, repeat, repeat_until_value,
};
use teseladb::engine::core::queue::Queue;
use teseladb::engine::core::reactor::{engine, listen_with};
use teseladb::engine::core::semaphore::Semaphore;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sharded::{Sharded, ShardedService, make_foreign};
use teseladb::engine::core::shared_ptr::{LwSharedPtr, SharedPtr, make_lw_shared, make_shared};
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine::core::timer::{Clock, SteadyClockType, Timer};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb::engine::net::api::{
    AcceptResult, ConnectedSocket, ListenOptions, ServerSocket, make_ipv4_address,
    make_ipv4_address_ip_port,
};
use teseladb::engine::net::api::{ConnectedSocketInputStreamConfig, KeepaliveParams};
use teseladb::engine::net::dns;
use teseladb::engine::net::inet_address::Family;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use teseladb::engine::net::stack::ConnectedSocketImpl;
use teseladb::engine::net::tls::{
    self, AbstractCredentials, ClientAuth, SessionType, TlsOptions, VerificationError,
    X509CrtFormat,
};
use teseladb::engine::util::file::rename_file;
use teseladb::engine::util::process::experimental::{Process, SpawnParameters, spawn_process_with};
use tls_certs::{cert_location, certfile};
use tmpdir::Tmpdir;

fn ss(s: &str) -> Sstring {
    Sstring::from(s)
}

/// The path of the generated certificate file `file`.
fn cf(file: &str) -> Sstring {
    Sstring::from(certfile(file).as_str())
}

/// The bytes of `buf` as a string.
fn buf_str(buf: &TemporaryBuffer<u8>) -> Sstring {
    Sstring::from_bytes(buf.as_slice())
}

/// TLS options with the server name `name`.
fn with_server_name(name: &Sstring) -> TlsOptions {
    TlsOptions { server_name: name.clone(), ..Default::default() }
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

/// The message of the exception `e` holds.
fn exc_what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

/// The `sharded<>` a `shared_ptr` holds: it lives in the shared block and never moves.
// A mutable reference from a shared handle.
#[allow(clippy::mut_from_ref)]
fn sharded_pin<S: ShardedService>(p: &SharedPtr<Sharded<S>>) -> Pin<&mut Sharded<S>> {
    // SAFETY: the sharded object stays in its heap block until the last pointer goes; the
    // test reaches it from its shard only, with no other reference live across the call.
    unsafe { Pin::new_unchecked(p.get_mut_unchecked()) }
}

/// Mutable access through a shared pointer.
// A mutable reference from a shared handle.
#[allow(clippy::mut_from_ref)]
fn creds_mut<T>(p: &SharedPtr<T>) -> &mut T {
    // SAFETY: the test runs on one shard and holds no other reference to the object across
    // this call.
    unsafe { p.get_mut_unchecked() }
}

fn connect_to_ssl_addr(
    certs: SharedPtr<tls::CertificateCredentials>,
    addr: SocketAddress,
    name: Sstring,
) -> Future<()> {
    repeat_until_value(move || {
        tls::connect(certs.clone(), addr, with_server_name(&name)).then(|s: ConnectedSocket| {
            do_with(s, |s: &mut ConnectedSocket| {
                let sp: *mut ConnectedSocket = s;
                do_with(s.output(8192), move |os: &mut OutputStream<u8>| {
                    let osp: *mut OutputStream<u8> = os;
                    let msg = ss("GET / HTTP/1.0\r\n\r\n");
                    let f = os.write_sstring(&msg);
                    f.then(move |()| {
                        // SAFETY: the do_with()s keep the socket and both streams
                        // alive until the chain resolves.
                        let f = unsafe { (*osp).flush() };
                        f.then(move |()| {
                            // SAFETY: the outer do_with keeps the socket alive until the chain resolves.
                            do_with(
                                (unsafe { (*sp).input(Default::default()) }, Sstring::new()),
                                |st| {
                                    let stp: *mut (InputStream<u8>, Sstring) = st;
                                    do_until(
                                        // SAFETY: do_with keeps the input stream and the string alive until the loop ends.
                                        move || unsafe { (*stp).0.eof() },
                                        move || {
                                            // SAFETY: do_with keeps the input stream and the string alive until the loop ends.
                                            let f = unsafe { (*stp).0.read() };
                                            f.then(move |buf: TemporaryBuffer<u8>| {
                                                // SAFETY: do_with keeps the input stream and the string alive until the loop ends.
                                                unsafe { (*stp).1.append_bytes(buf.as_slice()) };
                                            })
                                        },
                                    )
                                    .then(
                                        move |()| -> Future<Option<bool>> {
                                            // SAFETY: do_with keeps the string alive until this chain resolves.
                                            let buffer = unsafe { &(*stp).1 };
                                            if buffer.empty() {
                                                // Some servers close the connection soon after connect when the first write comes
                                                // late (certificate verification, scheduling or a busy host can delay it), so an
                                                // empty read here means "retry". A real failure arrives as a protocol error or an
                                                // HTTP error instead.
                                                return make_ready_future(None);
                                            }
                                            assert!(buffer.size() > 8);
                                            assert_eq!(
                                                buffer.substr(0, 5).unwrap_or_default(),
                                                ss("HTTP/")
                                            );
                                            make_ready_future(Some(true))
                                        },
                                    )
                                },
                            )
                        })
                    })
                    // SAFETY: the inner do_with keeps the output stream alive until this step resolves.
                    .finally(move || unsafe { (*osp).close() })
                })
            })
        })
    })
    .discard_result()
}

const GOOGLE_NAME: &str = "www.google.com";

thread_local! {
    static GOOGLE: RefCell<SocketAddress> = RefCell::new(SocketAddress::default());
}

// broken out from below. to allow pre-lookup
fn google_address() -> Future<SocketAddress> {
    if GOOGLE.with(|g| g.borrow().is_unspecified()) {
        return dns::dns::resolve_name(&ss(GOOGLE_NAME), Some(Family::INET)).then(|addr| {
            GOOGLE.with(|g| *g.borrow_mut() = SocketAddress::from_inet_address(&addr, 443));
            google_address()
        });
    }
    make_ready_future(GOOGLE.with(|g| *g.borrow()))
}

fn connect_to_ssl_google(certs: SharedPtr<tls::CertificateCredentials>) -> Future<()> {
    google_address()
        .then(move |addr: SocketAddress| connect_to_ssl_addr(certs, addr, ss(GOOGLE_NAME)))
}

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_simple_x509_client_with_google,
    {
        let certs = make_shared(tls::CertificateCredentials::new());
        let c2 = certs.clone();
        creds_mut(&certs)
            .set_x509_trust_file(&cf("tls-ca-bundle.pem"), X509CrtFormat::PEM)
            .then(move |()| connect_to_ssl_google(c2))
    }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_x509_client_with_system_trust,
    {
        let certs = make_shared(tls::CertificateCredentials::new());
        let c2 = certs.clone();
        creds_mut(&certs).set_system_trust().then(move |()| connect_to_ssl_google(c2))
    }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_x509_client_with_builder_system_trust,
    {
        let mut b = tls::CredentialsBuilder::default();
        let _ = b.set_system_trust();
        match b.build_certificate_credentials() {
            Ok(c) => connect_to_ssl_google(c),
            Err(e) => make_exception_future(e),
        }
    }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_x509_client_with_builder_system_trust_multiple,
    {
        // avoid getting parallel connects stuck on dns lookup (if running single case).
        // pre-lookup www.google.com
        google_address().then(|_: SocketAddress| -> Future<()> {
            let mut b = tls::CredentialsBuilder::default();
            let _ = b.set_system_trust();
            let creds = match b.build_certificate_credentials() {
                Ok(c) => c,
                Err(e) => return make_exception_future(e),
            };

            parallel_for_each(0..20, move |_i: i32| connect_to_ssl_google(creds.clone()))
        })
    }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_x509_client_with_system_trust_and_priority_strings,
    {
        let prios: Vec<Sstring> = [
            "NORMAL:+ARCFOUR-128", // means normal ciphers plus ARCFOUR-128.
            "SECURE128:-VERS-SSL3.0:+COMP-DEFLATE", // means that only secure ciphers are enabled, SSL3.0 is disabled, and libz compression enabled.
            "SECURE256:+SECURE128",
            "NORMAL:%COMPAT",
            "NORMAL:-MD5",
            "NONE:+VERS-TLS-ALL:+MAC-ALL:+RSA:+AES-128-CBC:+SIGN-ALL:+COMP-NULL",
            "NORMAL:+ARCFOUR-128",
            "SECURE128:-VERS-TLS1.0:+COMP-DEFLATE",
            "SECURE128:+SECURE192:-VERS-TLS-ALL:+VERS-TLS1.2",
        ]
        .iter()
        .map(|s| ss(s))
        .collect();
        do_with(prios, |prios: &mut Vec<Sstring>| {
            let pp: *const Vec<Sstring> = prios;
            do_for_each(0..prios.len(), move |i: usize| -> Future<()> {
                // SAFETY: do_with keeps the strings alive, and in place, until the loop resolves.
                let prio: &Sstring = unsafe { &(&*pp)[i] };
                let mut b = tls::CredentialsBuilder::default();
                let _ = b.set_system_trust();
                b.set_priority_string(prio);
                match b.build_certificate_credentials() {
                    Ok(c) => connect_to_ssl_google(c),
                    Err(e) => make_exception_future(e),
                }
            })
        })
    }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_x509_client_with_system_trust_and_priority_strings_fail,
    {
        let prios: Vec<Sstring> = ["NONE", "NONE:+CURVE-SECP256R1"].iter().map(|s| ss(s)).collect();
        do_with(prios, |prios: &mut Vec<Sstring>| {
            let pp: *const Vec<Sstring> = prios;
            do_for_each(0..prios.len(), move |i: usize| -> Future<()> {
                // SAFETY: do_with keeps the strings alive, and in place, until the loop resolves.
                let prio: &Sstring = unsafe { &(&*pp)[i] };
                let mut b = tls::CredentialsBuilder::default();
                let _ = b.set_system_trust();
                b.set_priority_string(prio);
                match b.build_certificate_credentials() {
                    Ok(c) => {
                        return connect_to_ssl_google(c)
                            .then(|()| -> () { panic!("Expected exception") })
                            .handle_exception(|_ep: ExceptionPtr| {
                                // ok.
                            });
                    }
                    Err(_) => {
                        // also ok
                    }
                }
                make_ready_future(())
            })
        })
    }
);

/// An HTTPS server process (`tests/data/https-server.py`) for the client tests.
struct HttpsServer {
    _cert: Sstring,
    _addr: String,
    _process: Process,
    _port: u16,
}

impl HttpsServer {
    fn spawn(addr: &str, key: &Sstring, cert: &Sstring) -> Result<Process, ExceptionPtr> {
        let httpd = cert_location().join("https-server.py");
        let argv: Vec<Sstring> = vec![
            ss("httpd"),
            ss("--server"),
            ss(&format!("{}:{}", addr, 0)),
            ss("--key"),
            key.clone(),
            ss("--cert"),
            cert.clone(),
        ];
        spawn_process_with(&httpd, SpawnParameters { argv, ..Default::default() }).get()
    }

    // https-server.py picks an available port and listens on it. when it is
    // ready to serve, it prints out the listening port. without hardwiring to
    // a fixed port, we are able to run multiple tests in parallel.
    fn read_port(process: &mut Process) -> Result<u16, ExceptionPtr> {
        let line: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let l = line.clone();
        let mut cout = process.cout();
        cout.consume(move |mut buf: TemporaryBuffer<u8>| {
            if let Some(newline) = buf.as_slice().iter().position(|&c| c == b'\n') {
                let consumed = newline;
                l.borrow_mut().push_str(&String::from_utf8_lossy(&buf.as_slice()[..consumed]));
                buf.trim_front(consumed);
                make_ready_future(teseladb::engine::core::iostream::stop_consuming(buf))
            } else {
                l.borrow_mut().push_str(&String::from_utf8_lossy(buf.as_slice()));
                make_ready_future(teseladb::engine::core::iostream::stop_consuming(
                    TemporaryBuffer::default(),
                ))
            }
        })
        .get()?;
        let port = line.borrow().trim().parse::<u16>().map_err(|e| {
            ExceptionPtr::new(teseladb::engine::core::future::InvalidArgument::new(format!(
                "stoul: {e}"
            )))
        })?;
        Ok(port)
    }

    fn new(ca: &str) -> Result<Self, ExceptionPtr> {
        let addr = "127.0.0.1".to_string();
        let cert = cf(&format!("{}.crt", ca));
        let mut process = Self::spawn(&addr, &cf(&format!("{}.key", ca)), &cert)?;
        let port = Self::read_port(&mut process)?;
        Ok(HttpsServer { _cert: cert, _addr: addr, _process: process, _port: port })
    }

    fn new_default() -> Result<Self, ExceptionPtr> {
        Self::new("mtls_ca")
    }

    fn cert(&self) -> &Sstring {
        &self._cert
    }
    /// The address the server listens on; fails on a bad address.
    fn addr(&self) -> Result<SocketAddress, ExceptionPtr> {
        Ok(SocketAddress::from_ipv4_addr(Ipv4Addr::from_string_port(&self._addr, self._port)?))
    }
    fn name(&self) -> Sstring {
        // should be identical to the one passed as the "-addext" option when
        // generating the cert.
        ss("127.0.0.1")
    }
}

impl Drop for HttpsServer {
    fn drop(&mut self) {
        // A failure here terminates the program.
        self._process.terminate().expect("https server: terminate the server process");
        let _ = self._process.wait().discard_result().get();
    }
}

teseladb::engine_test_case!(
    #[cfg_attr(engine_testing_with_networking, ignore)]
    test_simple_x509_client_with_local_server,
    {
        teseladb::engine::core::thread::async_(|| {
            let certs = make_shared(tls::CertificateCredentials::new());
            let server = HttpsServer::new_default()?;
            creds_mut(&certs).set_x509_trust_file(server.cert(), X509CrtFormat::PEM).get()?;
            connect_to_ssl_addr(certs, server.addr()?, server.name()).get()?;
            Ok(())
        })
    }
);

teseladb::engine_test_case!(test_x509_client_with_builder, {
    teseladb::engine::core::thread::async_(|| {
        let mut b = tls::CredentialsBuilder::default();
        let server = HttpsServer::new_default()?;
        b.set_x509_trust_file(server.cert(), X509CrtFormat::PEM).get()?;
        connect_to_ssl_addr(b.build_certificate_credentials()?, server.addr()?, Sstring::new())
            .get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_x509_client_with_builder_multiple, {
    teseladb::engine::core::thread::async_(|| {
        let mut b = tls::CredentialsBuilder::default();
        let server = HttpsServer::new_default()?;
        b.set_x509_trust_file(server.cert(), X509CrtFormat::PEM).get()?;
        let creds = b.build_certificate_credentials()?;
        let addr = server.addr()?;
        parallel_for_each(0..20, move |_i: i32| {
            connect_to_ssl_addr(creds.clone(), addr, Sstring::new())
        })
        .get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_x509_client_with_priority_strings, {
    teseladb::engine::core::thread::async_(|| {
        let prios: Vec<Sstring> = [
            "NORMAL:+ARCFOUR-128", // means normal ciphers plus ARCFOUR-128.
            "SECURE128:-VERS-SSL3.0:+COMP-DEFLATE", // means that only secure ciphers are enabled, SSL3.0 is disabled, and libz compression enabled.
            "SECURE256:+SECURE128",
            "NORMAL:%COMPAT",
            "NORMAL:-MD5",
            "NONE:+VERS-TLS-ALL:+MAC-ALL:+RSA:+AES-256-GCM:+SIGN-ALL:+COMP-NULL:+GROUP-EC-ALL",
            "NORMAL:+ARCFOUR-128",
            "SECURE128:-VERS-TLS1.0:+COMP-DEFLATE",
            "SECURE128:+SECURE192:-VERS-TLS-ALL:+VERS-TLS1.2",
        ]
        .iter()
        .map(|s| ss(s))
        .collect();
        let mut b = tls::CredentialsBuilder::default();
        let server = HttpsServer::new_default()?;
        b.set_x509_trust_file(server.cert(), X509CrtFormat::PEM).get()?;
        let addr = server.addr()?;
        let bp: *mut tls::CredentialsBuilder = &mut b;
        let pp: *const Vec<Sstring> = &prios;
        do_for_each(0..prios.len(), move |i: usize| -> Future<()> {
            // SAFETY: the strings live in this thread's frame until the loop is joined.
            let prio: &Sstring = unsafe { &(&*pp)[i] };
            // SAFETY: the builder lives in this thread's frame until the loop is joined.
            let b = unsafe { &mut *bp };
            b.set_priority_string(prio);
            match b.build_certificate_credentials() {
                Ok(c) => connect_to_ssl_addr(c, addr, Sstring::new()),
                Err(e) => make_exception_future(e),
            }
        })
        .get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_x509_client_with_priority_strings_fail, {
    teseladb::engine::core::thread::async_(|| {
        let prios: Vec<Sstring> = ["NONE", "NONE:+CURVE-SECP256R1"].iter().map(|s| ss(s)).collect();
        let mut b = tls::CredentialsBuilder::default();
        let server = HttpsServer::new_default()?;
        b.set_x509_trust_file(server.cert(), X509CrtFormat::PEM).get()?;
        let addr = server.addr()?;
        let bp: *mut tls::CredentialsBuilder = &mut b;
        let pp: *const Vec<Sstring> = &prios;
        do_for_each(0..prios.len(), move |i: usize| -> Future<()> {
            // SAFETY: the strings live in this thread's frame until the loop is joined.
            let prio: &Sstring = unsafe { &(&*pp)[i] };
            // SAFETY: the builder lives in this thread's frame until the loop is joined.
            let b = unsafe { &mut *bp };
            b.set_priority_string(prio);
            match b.build_certificate_credentials() {
                Ok(c) => {
                    return connect_to_ssl_addr(c, addr, Sstring::new())
                        .then(|()| -> () { panic!("Expected exception") })
                        .handle_exception(|_ep: ExceptionPtr| {
                            // ok.
                        });
                }
                Err(_) => {
                    // also ok
                }
            }
            make_ready_future(())
        })
        .get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_failed_connect, {
    let mut b = tls::CredentialsBuilder::default();
    let _ = b.set_system_trust();
    match b.build_certificate_credentials() {
        Ok(c) => connect_to_ssl_addr(c, make_ipv4_address(&Ipv4Addr::default()), Sstring::new())
            .handle_exception(|_: ExceptionPtr| {}),
        Err(e) => make_exception_future(e),
    }
});

teseladb::engine_test_case!(test_non_tls, {
    let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
    let mut server = try_or_fail!(listen_with(addr, opts));

    let c = server.accept();

    let mut b = tls::CredentialsBuilder::default();
    let _ = b.set_system_trust();

    let f = match b.build_certificate_credentials() {
        Ok(creds) => connect_to_ssl_addr(creds, addr, Sstring::new()),
        Err(e) => make_exception_future(e),
    };

    c.then(move |ar: AcceptResult| {
        let s = ar.connection;
        eprintln!("Established connection");
        let mut sp = Box::new(s);
        let spp: *mut ConnectedSocket = &mut *sp;
        let mut t = Box::pin(Timer::<SteadyClockType>::with_callback(move || {
            eprintln!("Killing server side");
            // SAFETY: the socket box lives until the finally() below, which also drops the
            // timer.
            unsafe { *spp = ConnectedSocket::new() };
        }));
        t.as_mut().arm(
            <SteadyClockType as Clock>::add(
                <SteadyClockType as Clock>::now(),
                Duration::from_secs(5),
            ),
            None,
        );
        f.finally(move || {
            let _t = t;
            let _sp = sp;
        })
    })
    .handle_exception(move |_ep: ExceptionPtr| {
        let _server = &server;
        eprintln!("Got expected exception");
    })
});

teseladb::engine_test_case!(test_abort_accept_before_handshake, {
    let dh = match tls::DhParams::new(tls::DhLevel::default()) {
        Ok(d) => make_shared(d),
        Err(e) => return make_exception_future(e),
    };
    let certs = match tls::ServerCredentials::with_shared_dh_params(&dh) {
        Ok(c) => make_shared(c),
        Err(e) => return make_exception_future(e),
    };
    let c2 = certs.clone();
    creds_mut(&certs).set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).then(
        move |()| -> Future<()> {
            let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
            let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
            let mut server = match tls::listen(c2, addr, opts) {
                Ok(s) => s,
                Err(e) => return make_exception_future(e),
            };
            let c = server.accept();
            assert!(!c.available()); // should not be finished

            server.abort_accept();

            c.then(|_: AcceptResult| -> () { panic!("Should not reach") })
                .handle_exception(|_: ExceptionPtr| {
                    // ok
                })
                .finally(move || {
                    let _server = server;
                })
        },
    )
});

/// Server credentials with freshly generated DH parameters.
fn server_credentials_with_dh() -> Result<SharedPtr<tls::ServerCredentials>, ExceptionPtr> {
    let dh = make_shared(tls::DhParams::new(tls::DhLevel::default())?);
    Ok(make_shared(tls::ServerCredentials::with_shared_dh_params(&dh)?))
}

teseladb::engine_test_case!(test_abort_accept_after_handshake, {
    async_(|| -> Result<(), ExceptionPtr> {
        let certs = server_credentials_with_dh()?;
        creds_mut(&certs)
            .set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM)
            .get()?;

        let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
        let mut server = tls::listen(certs, addr, opts)?;
        let sa = server.accept();

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;

        let mut c =
            tls::connect(b.build_certificate_credentials()?, addr, TlsOptions::default()).get()?;
        let mut s = sa.get()?;
        server.abort_accept(); // should not affect the socket we got.
        let mut out = c.output(8192);
        let mut in_ = s.connection.input(Default::default());

        out.write_str("apa").get()?;
        let f = out.flush();
        let buf = in_.read().get()?;
        f.get()?;
        assert!(buf_str(&buf) == "apa");

        out.close().get()?;
        in_.close().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_abort_accept_on_server_before_handshake, {
    async_(|| -> Result<(), ExceptionPtr> {
        let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
        let mut server = listen_with(addr, opts)?;
        let sa = server.accept();

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;

        let creds = b.build_certificate_credentials()?;
        let f = tls::connect(creds, addr, TlsOptions::default());

        server.abort_accept();
        let _ = sa.get();
        server = ServerSocket::default();
        drop(server);

        let r = (|| -> Result<(), ExceptionPtr> {
            // the connect as such should succeed, but the handshare following it
            // should not.
            let mut c = f.get()?;
            let mut out = c.output(8192);
            out.write_str("apa").get()?;
            out.flush().get()?;
            out.close().get()?;
            Ok(())
        })();
        assert!(r.is_err(), "Expected exception");
        Ok(())
    })
});

/// A connection and its input and output streams.
struct Streams {
    // Keeps the connection open while its streams are in use.
    #[allow(dead_code)]
    s: ConnectedSocket,
    in_: InputStream<u8>,
    out: OutputStream<u8>,
}

impl Streams {
    // note: using custom output_stream, because we don't want polled flush
    fn new(mut cs: ConnectedSocket) -> Result<Self, ExceptionPtr> {
        let in_ = cs.input(Default::default());
        let out = OutputStream::with_options(
            cs.output(8192).detach()?,
            8192,
            OutputStreamOptions::default(),
        );
        Ok(Streams { s: cs, in_, out })
    }
}

/// Mutable access to the shared streams.
// A mutable reference from a shared handle.
#[allow(clippy::mut_from_ref)]
fn sm(s: &LwSharedPtr<Streams>) -> &mut Streams {
    // SAFETY: one shard, and every user of the streams runs in a continuation of its own,
    // so no two references are alive at once.
    unsafe { s.get_mut_unchecked() }
}

const MESSAGE: &str = "hej lilla fisk du kan dansa fint";

/// A sharded TLS echo server.
struct Echoserver {
    _socket: ServerSocket,
    _certs: SharedPtr<tls::ServerCredentials>,
    _gate: Gate,
    _stopped: bool,
    _size: usize,
    _ex: Option<ExceptionPtr>,
}

impl Echoserver {
    fn new(message_size: usize, use_dh_params: bool) -> Self {
        let certs = if use_dh_params {
            server_credentials_with_dh()
        } else {
            tls::ServerCredentials::new().map(make_shared)
        };
        Echoserver {
            _socket: ServerSocket::default(),
            // A constructor failure is a panic here.
            _certs: certs.unwrap_or_else(|e| panic!("{}", e)),
            _gate: Gate::new(),
            _stopped: false,
            _size: message_size,
            _ex: None,
        }
    }

    fn listen(
        &mut self,
        addr: SocketAddress,
        crtfile: Sstring,
        keyfile: Sstring,
        ca: ClientAuth,
        trust: Sstring,
    ) -> Future<()> {
        creds_mut(&self._certs).set_client_auth(ca);
        let mut f =
            creds_mut(&self._certs).set_x509_key_file(&crtfile, &keyfile, X509CrtFormat::PEM);
        let this: *mut Echoserver = self;
        if !trust.empty() {
            f = f.then(move |()| {
                // SAFETY: the sharded service outlives its listen() and the
                // accept loop, which stop() waits for through the gate.
                creds_mut(unsafe { &(*this)._certs })
                    .set_x509_trust_file(&trust, X509CrtFormat::PEM)
            });
        }
        f.then(move |()| -> Result<(), ExceptionPtr> {
            // SAFETY: the sharded service outlives its listen() and the accept loop, which stop() waits for through the gate.
            let me = unsafe { &mut *this };
            let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };

            me._socket = tls::listen(me._certs.clone(), addr, opts)?;

            // SAFETY: the gate is a member of the sharded service, which does not move.
            let _ = try_with_gate(unsafe { Pin::new_unchecked(&me._gate) }, move || {
                // SAFETY: the sharded service outlives its accept loop, which stop() waits for through the gate.
                let me = unsafe { &mut *this };
                me._socket
                    .accept()
                    .then(move |ar: AcceptResult| -> Future<()> {
                        let s = ar.connection;
                        let strms = match Streams::new(s) {
                            Ok(st) => make_lw_shared(st),
                            Err(e) => return make_exception_future(e),
                        };
                        let (s1, s2, s3) = (strms.clone(), strms.clone(), strms.clone());
                        repeat(move || {
                            let strms = s1.clone();
                            // SAFETY: the sharded service outlives its accept loop, which stop() waits for through the gate.
                            let size = unsafe { (*this)._size };
                            sm(&strms).in_.read_exactly(size).then(
                                move |buf: TemporaryBuffer<u8>| -> Future<StopIteration> {
                                    if buf.empty() {
                                        return make_ready_future(StopIteration::YES);
                                    }
                                    let tmp = buf_str(&buf);
                                    let s4 = strms.clone();
                                    sm(&strms)
                                        .out
                                        .write_sstring(&tmp)
                                        .then(move |()| sm(&s4).out.flush())
                                        .then(|()| make_ready_future(StopIteration::NO))
                                },
                            )
                        })
                        .finally(move || sm(&s2).out.close())
                        .finally(move || {
                            // `[copy] {}`: the closure owns the copy until it runs (`let _ = x` would not capture it).
                            drop(s3);
                        })
                    })
                    .handle_exception(move |ep: ExceptionPtr| {
                        // SAFETY: the sharded service outlives its accept loop, which stop() waits for through the gate.
                        let me = unsafe { &mut *this };
                        if me._stopped {
                            return;
                        }
                        me._ex = Some(ep);
                    })
            })
            .handle_exception_type(
                |_: &GateClosedException| -> Result<(), ExceptionPtr> {
                    /* ignore */
                    Ok(())
                },
            );
            Ok(())
        })
    }

    fn stop_server(&mut self) -> Future<()> {
        self._stopped = true;
        self._socket.abort_accept();
        let ex = self._ex.clone();
        self._gate.close().handle_exception(
            move |_ignored: ExceptionPtr| -> Result<(), ExceptionPtr> {
                if let Some(ex) = ex {
                    return Err(ex);
                }
                Ok(())
            },
        )
    }
}

impl ShardedService for Echoserver {
    fn stop(&mut self) -> Future<()> {
        self.stop_server()
    }
}

/// The parameters of `run_echo_test` after `name`, with their defaults.
struct EchoTestOptions {
    crt: Sstring,
    key: Sstring,
    ca: ClientAuth,
    client_crt: Sstring,
    client_key: Sstring,
    do_read: bool,
    use_dh_params: bool,
    distinguished_name_callback: Option<tls::DnCallback>,
}

impl Default for EchoTestOptions {
    fn default() -> Self {
        EchoTestOptions {
            crt: cf("test.crt"),
            key: cf("test.key"),
            ca: ClientAuth::NONE,
            client_crt: Sstring::new(),
            client_key: Sstring::new(),
            do_read: true,
            use_dh_params: true,
            distinguished_name_callback: None,
        }
    }
}

fn run_echo_test(message: Sstring, loops: i32, trust: Sstring, name: Sstring) -> Future<()> {
    run_echo_test_with(message, loops, trust, name, EchoTestOptions::default())
}

fn run_echo_test_with(
    message: Sstring,
    loops: i32,
    trust: Sstring,
    name: Sstring,
    o: EchoTestOptions,
) -> Future<()> {
    const PORT: u16 = 4711;

    let msg = make_shared(message);
    let certs = make_shared(tls::CertificateCredentials::new());
    let server = make_shared(Sharded::<Echoserver>::new());
    let addr = make_ipv4_address_ip_port(0x7f000001, PORT);

    assert!(o.do_read || loops == 1);

    let mut f: Future<()> = make_ready_future(());

    if !o.client_crt.empty() && !o.client_key.empty() {
        f = creds_mut(&certs).set_x509_key_file(&o.client_crt, &o.client_key, X509CrtFormat::PEM);
        if o.distinguished_name_callback.is_some() {
            creds_mut(&certs).set_dn_verification_callback(o.distinguished_name_callback);
        }
    }

    let EchoTestOptions { crt, key, ca, do_read, use_dh_params, .. } = o;
    let (certs1, trust1) = (certs.clone(), trust.clone());
    f.then(move |()| creds_mut(&certs1).set_x509_trust_file(&trust1, X509CrtFormat::PEM)).then(move |()| {
        let msg_size = msg.size();
        let (server1, server2, addr1) = (server.clone(), server.clone(), addr);
        sharded_pin(&server)
            .start(move || Echoserver::new(msg_size, use_dh_params))
            .then(move |()| {
                let mut server_trust = Sstring::new();
                if ca != ClientAuth::NONE {
                    server_trust = trust;
                }
                sharded_pin(&server1).invoke_on_all(move |s: &mut Echoserver| {
                    s.listen(addr1, crt.clone(), key.clone(), ca, server_trust.clone())
                })
            })
            .then(move |()| {
                tls::connect(certs, addr, with_server_name(&name)).then(move |s: ConnectedSocket| -> Future<()> {
                    let strms = match Streams::new(s) {
                        Ok(st) => make_lw_shared(st),
                        Err(e) => return make_exception_future(e),
                    };
                    let range = 0..loops;
                    let (s1, s2) = (strms.clone(), strms.clone());
                    do_for_each(range, move |_: i32| {
                        let (strms, msg) = (s1.clone(), msg.clone());
                        let f = sm(&strms).out.write_sstring(&msg);
                        f.then(move |()| {
                            let s5 = strms.clone();
                            sm(&strms).out.flush().then(move |()| {
                                sm(&s5).in_.read_exactly(msg.size()).then(move |buf: TemporaryBuffer<u8>| -> Result<(), ExceptionPtr> {
                                    if buf.empty() {
                                        return Err(ExceptionPtr::new(teseladb::engine::core::future::RuntimeError::new("Unexpected EOF".to_string())));
                                    }
                                    let tmp = buf_str(&buf);
                                    assert!(*msg == tmp);
                                    Ok(())
                                })
                            })
                        })
                    })
                    .then_wrapped(move |f1: Future<()>| {
                        // Always call close()
                        let s3 = s2.clone();
                        (if do_read { sm(&s2).out.close() } else { make_ready_future(()) })
                            .then_wrapped(move |f2: Future<()>| {
                                // Verification errors will be reported by the call to output_stream::close(),
                                // which waits for the flush to actually happen. They can also be reported by the
                                // input_stream::read_exactly() call. We want to keep only one and avoid nested exception mess.
                                if f1.failed() {
                                    let _ = f2.handle_exception(|_ignored: ExceptionPtr| {});
                                    return f1;
                                }
                                let _ = f1.handle_exception(|_ignored: ExceptionPtr| {});
                                f2
                            })
                            .finally(move || {
                                // `[copy] {}`: the closure owns the copy until it runs (`let _ = x` would not capture it).
                                drop(s3);
                            })
                    })
                })
            })
            .finally(move || {
                let s3 = server2.clone();
                sharded_pin(&server2).stop().finally(move || {
                    // `[copy] {}`: the closure owns the copy until it runs (`let _ = x` would not capture it).
                    drop(s3);
                })
            })
    })
}

/*
 * Certificates:
 *
 * Generated by `common/tls_certs.rs` for domain example.org, server test:
 *
 * ->   test.crt
 *      test.csr
 *      catest.pem
 *      catest.key
 *
 * catest == snakeoil root authority for these self-signed certs
 *
 */
teseladb::engine_test_case!(test_simple_x509_client_server, {
    // Make sure we load our own auth trust pem file, otherwise our certs
    // will not validate
    // Must match expected name with cert CA or give empty name to ignore
    // server name
    run_echo_test(ss(MESSAGE), 20, cf("catest.pem"), ss("test.example.org"))
});

teseladb::engine_test_case!(test_simple_x509_client_server_again, {
    run_echo_test(ss(MESSAGE), 20, cf("catest.pem"), ss("test.example.org"))
});

// Do not set dh_params in server certs - let gnutls negotiate.
// (The linked gnutls is version 3.6 or newer.)
teseladb::engine_test_case!(test_simple_server_default_dhparams, {
    run_echo_test_with(
        ss(MESSAGE),
        20,
        cf("catest.pem"),
        ss("test.example.org"),
        EchoTestOptions {
                crt: cf("test.crt"),
                key: cf("test.key"),
                ca: ClientAuth::NONE,
                do_read: true,
                use_dh_params: /* use_dh_params */ false,
                ..Default::default()
            },
    )
});

teseladb::engine_test_case!(test_x509_client_server_cert_validation_fail, {
    // Load a real trust authority here, which our certs are _not_ signed with.
    run_echo_test(ss(MESSAGE), 1, cf("tls-ca-bundle.pem"), Sstring::new())
        .then(|()| -> () { panic!("Should have gotten validation error") })
        .handle_exception(|ep: ExceptionPtr| {
            match ep.downcast_ref::<VerificationError>() {
                Some(e) => {
                    // Verify exception contains info on subject/issuer
                    assert!(e.to_string().contains("Issuer"));
                    assert!(e.to_string().contains("Subject"));
                    // ok.
                }
                None => panic!("Unexpected exception"),
            }
        })
});

teseladb::engine_test_case!(test_x509_client_server_cert_validation_fail_name, {
    // Use trust store with our signer, but wrong host name
    run_echo_test(ss(MESSAGE), 1, cf("tls-ca-bundle.pem"), ss("nils.holgersson.gov"))
        .then(|()| -> () { panic!("Should have gotten validation error") })
        .handle_exception(|ep: ExceptionPtr| {
            if !ep.is::<VerificationError>() {
                panic!("Unexpected exception");
            }
            // ok.
        })
});

/// A message of `size` bytes: `'0' + i % 30` at each position `i`.
fn pattern_message(size: usize) -> Sstring {
    let mut msg = uninitialized_string(size);
    for (i, c) in msg.as_bytes_mut().iter_mut().enumerate() {
        *c = b'0' + (i % 30) as u8;
    }
    msg
}

teseladb::engine_test_case!(test_large_message_x509_client_server, {
    // Make sure we load our own auth trust pem file, otherwise our certs
    // will not validate
    // Must match expected name with cert CA or give empty name to ignore
    // server name
    let msg = pattern_message(512 * 1024);
    run_echo_test(msg, 20, cf("catest.pem"), ss("test.example.org"))
});

fn client_auth_options(client_crt: Sstring, client_key: Sstring) -> EchoTestOptions {
    EchoTestOptions {
        crt: cf("test.crt"),
        key: cf("test.key"),
        ca: ClientAuth::REQUIRE,
        client_crt,
        client_key,
        ..Default::default()
    }
}

teseladb::engine_test_case!(test_simple_x509_client_server_fail_client_auth, {
    // Make sure we load our own auth trust pem file, otherwise our certs
    // will not validate
    // Must match expected name with cert CA or give empty name to ignore
    // server name
    // Server will require certificate auth. We supply none, so should fail connection
    run_echo_test_with(
        ss(MESSAGE),
        20,
        cf("catest.pem"),
        ss("test.example.org"),
        client_auth_options(Sstring::new(), Sstring::new()),
    )
    .then(|()| -> () { panic!("Expected exception") })
    .handle_exception(|_ep: ExceptionPtr| {
        // ok.
    })
});

teseladb::engine_test_case!(test_simple_x509_client_server_client_auth, {
    // Make sure we load our own auth trust pem file, otherwise our certs
    // will not validate
    // Must match expected name with cert CA or give empty name to ignore
    // server name
    // Server will require certificate auth. We supply one, so should succeed with connection
    run_echo_test_with(
        ss(MESSAGE),
        20,
        cf("catest.pem"),
        ss("test.example.org"),
        client_auth_options(cf("test.crt"), cf("test.key")),
    )
});

teseladb::engine_test_case!(test_simple_x509_client_server_client_auth_with_dn_callback, {
    // In addition to the above test, the certificate's subject and issuer
    // Distinguished Names (DNs) will be checked for the occurrence of a specific
    // substring (in this case, the test.example.org url)
    let mut o = client_auth_options(cf("test.crt"), cf("test.key"));
    o.distinguished_name_callback =
        Some(tls::DnCallback::new(|t: SessionType, subject: Sstring, issuer: Sstring| {
            assert!(t == SessionType::CLIENT);
            assert!(subject.contains(b"test.example.org"));
            assert!(issuer.contains(b"test.example.org"));
        }));
    run_echo_test_with(ss(MESSAGE), 20, cf("catest.pem"), ss("test.example.org"), o)
});

teseladb::engine_test_case!(test_simple_x509_client_server_client_auth_dn_callback_fails, {
    // Test throwing an exception from within the Distinguished Names callback
    let mut o = client_auth_options(cf("test.crt"), cf("test.key"));
    o.distinguished_name_callback = Some(tls::DnCallback::new(
        |_: SessionType, _: Sstring, _: Sstring| -> Result<(), ExceptionPtr> {
            Err(ExceptionPtr::new(VerificationError(
                "to test throwing from within the callback".to_string(),
            )))
        },
    ));
    run_echo_test_with(ss(MESSAGE), 20, cf("catest.pem"), ss("test.example.org"), o)
        .then(|()| -> () { panic!("Should have gotten a verification_error exception") })
        .handle_exception(|_: ExceptionPtr| {
            // ok.
        })
});

teseladb::engine_test_case!(test_many_large_message_x509_client_server, {
    // Make sure we load our own auth trust pem file, otherwise our certs
    // will not validate
    // Must match expected name with cert CA or give empty name to ignore
    // server name
    let msg = pattern_message(4 * 1024 * 1024);
    // Sending a huge-ish message a and immediately closing the session (see params)
    // provokes case where tls::vec_push entered race and asserted on broken IO state
    // machine.
    let range = 0..20;
    // Every iteration sends a copy of the whole message.
    do_for_each(range, move |_: i32| {
        let m = msg.clone();
        run_echo_test_with(
            m,
            1,
            cf("catest.pem"),
            ss("test.example.org"),
            EchoTestOptions {
                crt: cf("test.crt"),
                key: cf("test.key"),
                ca: ClientAuth::NONE,
                do_read: false,
                ..Default::default()
            },
        )
    })
});

/// A wrapper of a loopback connected socket whose sink the test controls.
struct MyLoopbackConnectedSocketImpl {
    inner: LoopbackConnectedSocketImpl,
    _sem: *mut Semaphore,
    _close: bool,
}

impl MyLoopbackConnectedSocketImpl {
    fn new(
        s: &mut Semaphore,
        tx: LwSharedPtr<LoopbackBuffer>,
        rx: LwSharedPtr<LoopbackBuffer>,
    ) -> Self {
        MyLoopbackConnectedSocketImpl {
            inner: LoopbackConnectedSocketImpl::new(make_foreign(tx), rx),
            _sem: s,
            _close: false,
        }
    }
}

impl Drop for MyLoopbackConnectedSocketImpl {
    fn drop(&mut self) {
        // SAFETY: the semaphore outlives every socket of the test, which waits on it.
        unsafe { (*self._sem).signal(1) };
    }
}

/// A wrapper of the loopback data sink, controlled by the test through its socket.
struct MySinkImpl {
    _sink: DataSink,
    _impl: *mut MyLoopbackConnectedSocketImpl,
    _p: Promise<()>,
}

impl MySinkImpl {
    fn new(sink: DataSink, impl_: *mut MyLoopbackConnectedSocketImpl) -> Self {
        MySinkImpl { _sink: sink, _impl: impl_, _p: Promise::new() }
    }
}

impl DataSinkImpl for MySinkImpl {
    fn flush(&mut self) -> Future<()> {
        self._sink.flush()
    }
    fn put(&mut self, p: Packet) -> Future<()> {
        // SAFETY: the socket impl outlives its sink.
        if std::mem::replace(unsafe { &mut (*self._impl)._close }, false) {
            let this: *mut MySinkImpl = self;
            let mut p = Some(p);
            // SAFETY: the sink outlives its pending put().
            return self
                ._p
                .get_future()
                .then(move |()| unsafe { (*this).put(p.take().unwrap_or_default()) });
        }
        self._sink.put(p)
    }
    fn close(&mut self) -> Future<()> {
        self._p.set_value(());
        make_ready_future(())
    }
}

impl ConnectedSocketImpl for MyLoopbackConnectedSocketImpl {
    fn source(&mut self) -> DataSource {
        self.inner.source()
    }
    fn source_with_config(&mut self, csisc: ConnectedSocketInputStreamConfig) -> DataSource {
        self.inner.source_with_config(csisc)
    }
    fn sink(&mut self) -> DataSink {
        let me: *mut MyLoopbackConnectedSocketImpl = self;
        DataSink::new(Box::new(MySinkImpl::new(self.inner.sink(), me)))
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

teseladb::engine_test_case!(test_close_timout, {
    teseladb::engine::core::thread::async_(|| {
        let mut b = tls::CredentialsBuilder::default();

        b.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).get()?;
        b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
        b.set_dh_level(tls::DhLevel::default());
        b.set_system_trust().get()?;

        let creds = b.build_certificate_credentials()?;
        let serv = b.build_server_credentials()?;

        let mut sem = Semaphore::new(0);

        const ITERATIONS: usize = 500;

        for _ in 0..ITERATIONS {
            let b1 = make_lw_shared(LoopbackBuffer::new(None, LoopbackBufferType::ServerTx));
            let b2 = make_lw_shared(LoopbackBuffer::new(None, LoopbackBufferType::ClientTx));
            let mut ssi =
                Box::new(MyLoopbackConnectedSocketImpl::new(&mut sem, b1.clone(), b2.clone()));
            let mut csi = Box::new(MyLoopbackConnectedSocketImpl::new(&mut sem, b2, b1));

            let ssir: *mut MyLoopbackConnectedSocketImpl = &mut *ssi;
            let csir: *mut MyLoopbackConnectedSocketImpl = &mut *csi;

            let mut ss_ = tls::wrap_server(serv.clone(), ConnectedSocket::from_impl(ssi)).get()?;
            let mut cs = tls::wrap_client(
                creds.clone(),
                ConnectedSocket::from_impl(csi),
                TlsOptions::default(),
            )
            .get()?;

            let mut os = cs.output(8192).detach()?;
            let mut is = ss_.input(Default::default());

            let f1 = os.put_buffer(TemporaryBuffer::new(10));
            let f2 = is.read();
            f1.get()?;
            f2.get()?;

            // block further writes
            // SAFETY: the tls sockets own both impls and are alive until the end of the iteration.
            unsafe {
                (*ssir)._close = true;
                (*csir)._close = true;
            }
        }

        sem.wait(2 * ITERATIONS).get()?;
        Ok(())
    })
});

/// The path of `name` inside `dir`.
fn path_in(dir: &Tmpdir, name: &str) -> Sstring {
    Sstring::from(dir.path().join(name).to_string_lossy().as_ref())
}

/// Copies the file `from` to `to`.
fn copy_file(from: &str, to: &Sstring) {
    std::fs::copy(from, to.as_str()).unwrap_or_else(|e| panic!("copy_file {from}: {e}"));
}

/// The state the reload callbacks share.
struct ReloadState {
    changed: RefCell<HashSet<Sstring>>,
    p: RefCell<Promise<()>>,
    cert: Sstring,
    key: Sstring,
}

impl ReloadState {
    fn new(cert: Sstring, key: Sstring) -> Rc<Self> {
        Rc::new(ReloadState {
            changed: RefCell::new(HashSet::new()),
            p: RefCell::new(Promise::new()),
            cert,
            key,
        })
    }

    /// Records the changed files and resolves the promise once both the certificate and the key changed.
    fn on_files(&self, files: &HashSet<Sstring>) {
        self.changed.borrow_mut().extend(files.iter().cloned());
        let done = {
            let changed = self.changed.borrow();
            changed.contains(&self.cert) && changed.contains(&self.key)
        };
        if done {
            self.p.borrow_mut().set_value(());
        }
    }

    fn get_future(&self) -> Future<()> {
        self.p.borrow_mut().get_future()
    }
}

teseladb::engine_test_case!(test_reload_certificates, {
    teseladb::engine::core::thread::async_(|| {
        let tmp = Tmpdir::new();

        // copy the wrong certs. We don't trust these
        // blocking calls, but this is a test and engine does not have a copy
        // util and I am lazy...
        copy_file(&certfile("other.crt"), &path_in(&tmp, "test.crt"));
        copy_file(&certfile("other.key"), &path_in(&tmp, "test.key"));

        let cert = path_in(&tmp, "test.crt");
        let key = path_in(&tmp, "test.key");
        let st = ReloadState::new(cert.clone(), key.clone());

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_key_file(&cert, &key, X509CrtFormat::PEM).get()?;
        b.set_dh_level(tls::DhLevel::default());

        let st1 = st.clone();
        let certs = b
            .build_reloadable_server_credentials_simple(
                Some(Rc::new(move |files: &HashSet<Sstring>, ep: Option<ExceptionPtr>| {
                    if ep.is_some() {
                        return;
                    }
                    st1.on_files(files);
                })),
                None,
            )
            .get()?;

        let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
        let mut server = tls::listen(certs, addr, opts)?;

        let mut b2 = tls::CredentialsBuilder::default();
        b2.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;

        {
            let sa = server.accept();
            let mut c =
                tls::connect(b2.build_certificate_credentials()?, addr, TlsOptions::default())
                    .get()?;
            let mut s = sa.get()?;
            let mut in_ = s.connection.input(Default::default());

            let mut out = OutputStream::<u8>::with_options(
                c.output(8192).detach()?,
                4096,
                OutputStreamOptions::default(),
            );

            let r = (|| -> Result<(), ExceptionPtr> {
                out.write_str("apa").get()?;
                let f = out.flush();
                let f2 = in_.read();

                match f.get() {
                    Ok(()) => panic!("should not reach"),
                    Err(e) if e.is::<VerificationError>() => {
                        // ok
                    }
                    Err(e) => return Err(e),
                }
                let _ = out.close().get();

                if f2.get().is_ok() {
                    panic!("should not reach");
                }
                // ok
                let _ = in_.close().get();
                Ok(())
            })();
            match r {
                Ok(()) => {}
                Err(e) if e.is::<VerificationError>() => {
                    // ok
                }
                Err(e) => return Err(e),
            }
        }

        // copy the right (trusted) certs over the old ones.
        copy_file(&certfile("test.crt"), &path_in(&tmp, "test0.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp, "test0.key"));

        rename_file(path_in(&tmp, "test0.crt").as_str(), path_in(&tmp, "test.crt").as_str())
            .get()?;
        rename_file(path_in(&tmp, "test0.key").as_str(), path_in(&tmp, "test.key").as_str())
            .get()?;

        st.get_future().get()?;

        // now it should work
        {
            let sa = server.accept();
            let mut c =
                tls::connect(b2.build_certificate_credentials()?, addr, TlsOptions::default())
                    .get()?;
            let mut s = sa.get()?;
            let mut in_ = s.connection.input(Default::default());

            let mut out = OutputStream::<u8>::with_options(
                c.output(8192).detach()?,
                4096,
                OutputStreamOptions::default(),
            );

            out.write_str("apa").get()?;
            let f = out.flush();
            let buf = in_.read().get()?;
            f.get()?;
            out.close().get()?;
            in_.read().get()?; // ignore - just want eof
            in_.close().get()?;

            assert_eq!(buf_str(&buf), "apa");
        }
        Ok(())
    })
});

teseladb::engine_test_case!(test_reload_broken_certificates, {
    teseladb::engine::core::thread::async_(|| {
        let tmp = Tmpdir::new();

        copy_file(&certfile("test.crt"), &path_in(&tmp, "test.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp, "test.key"));

        let cert = path_in(&tmp, "test.crt");
        let key = path_in(&tmp, "test.key");
        let st = ReloadState::new(cert.clone(), key.clone());

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_key_file(&cert, &key, X509CrtFormat::PEM).get()?;
        b.set_dh_level(tls::DhLevel::default());

        let q: Rc<RefCell<Queue<ExceptionPtr>>> = Rc::new(RefCell::new(Queue::new(10)));

        let (st1, q1) = (st.clone(), q.clone());
        let _certs = b
            .build_reloadable_server_credentials_simple(
                Some(Rc::new(move |files: &HashSet<Sstring>, ep: Option<ExceptionPtr>| {
                    if let Some(ep) = ep {
                        let _ = q1.borrow_mut().push(ep);
                        return;
                    }
                    st1.on_files(files);
                })),
                None,
            )
            .get()?;

        // very intentionally use blocking calls. We want all our modifications to happen
        // before any other continuation is allowed to process.

        let _ = std::fs::remove_file(cert.as_str());
        let _ = std::fs::remove_file(key.as_str());

        std::fs::write(cert.as_str(), "lala land\n").expect("write");
        std::fs::write(key.as_str(), "lala land\n").expect("write");

        // should get one or two exceptions
        // SAFETY: the queue lives in its `Rc` until the pop is waited for below.
        let f = unsafe { q.borrow_mut().pop_eventually() };
        f.get()?;

        let _ = std::fs::remove_file(cert.as_str());
        let _ = std::fs::remove_file(key.as_str());

        copy_file(&certfile("test.crt"), &cert);
        copy_file(&certfile("test.key"), &key);

        // now it should reload
        st.get_future().get()?;
        Ok(())
    })
});

// the same as previous test, but we set a big tolerance for
// reload errors, and verify that either our scheduling/fs is
// super slow, or we got through the changes without failures.
teseladb::engine_test_case!(test_reload_tolerance, {
    teseladb::engine::core::thread::async_(|| {
        let tmp = Tmpdir::new();

        copy_file(&certfile("test.crt"), &path_in(&tmp, "test.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp, "test.key"));

        let cert = path_in(&tmp, "test.crt");
        let key = path_in(&tmp, "test.key");
        let st = ReloadState::new(cert.clone(), key.clone());

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_key_file(&cert, &key, X509CrtFormat::PEM).get()?;
        b.set_dh_level(tls::DhLevel::default());

        let nfails = Rc::new(Cell::new(0i32));

        // use 5s tolerance - this should ensure we don't generate any errors.
        let (st1, nf) = (st.clone(), nfails.clone());
        let _certs = b
            .build_reloadable_server_credentials_simple(
                Some(Rc::new(move |files: &HashSet<Sstring>, ep: Option<ExceptionPtr>| {
                    if ep.is_some() {
                        nf.set(nf.get() + 1);
                        return;
                    }
                    st1.on_files(files);
                })),
                Some(Duration::from_millis(5000)),
            )
            .get()?;

        // very intentionally use blocking calls. We want all our modifications to happen
        // before any other continuation is allowed to process.

        let start = SystemTime::now();

        let _ = std::fs::remove_file(cert.as_str());
        let _ = std::fs::remove_file(key.as_str());

        std::fs::write(cert.as_str(), "lala land\n").expect("write");
        std::fs::write(key.as_str(), "lala land\n").expect("write");

        let _ = std::fs::remove_file(cert.as_str());
        let _ = std::fs::remove_file(key.as_str());

        copy_file(&certfile("test.crt"), &cert);
        copy_file(&certfile("test.key"), &key);

        // now it should reload
        st.get_future().get()?;

        let end = SystemTime::now();

        assert!(
            nfails.get() == 0
                || end.duration_since(start).unwrap_or_default() > Duration::from_secs(4)
        );
        Ok(())
    })
});

/// Waits up to 3 seconds for the reload promise.
fn wait_3s(st: &ReloadState) -> Result<(), ExceptionPtr> {
    with_timeout::<SteadyClockType, ()>(
        <SteadyClockType as Clock>::add(<SteadyClockType as Clock>::now(), Duration::from_secs(3)),
        st.get_future(),
    )
    .get()
}

teseladb::engine_test_case!(test_reload_by_move, {
    teseladb::engine::core::thread::async_(|| {
        let tmp = Tmpdir::new();
        let tmp2 = Tmpdir::new();

        copy_file(&certfile("test.crt"), &path_in(&tmp, "test.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp, "test.key"));
        copy_file(&certfile("test.crt"), &path_in(&tmp2, "test.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp2, "test.key"));

        let cert = path_in(&tmp, "test.crt");
        let key = path_in(&tmp, "test.key");
        let cert2 = path_in(&tmp2, "test.crt");
        let key2 = path_in(&tmp2, "test.key");

        let st = ReloadState::new(cert.clone(), key.clone());

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_key_file(&cert, &key, X509CrtFormat::PEM).get()?;
        b.set_dh_level(tls::DhLevel::default());

        let nfails = Rc::new(Cell::new(0i32));

        // use 5s tolerance - this should ensure we don't generate any errors.
        let (st1, nf) = (st.clone(), nfails.clone());
        let _certs = b
            .build_reloadable_server_credentials_simple(
                Some(Rc::new(move |files: &HashSet<Sstring>, ep: Option<ExceptionPtr>| {
                    if ep.is_some() {
                        nf.set(nf.get() + 1);
                        return;
                    }
                    st1.on_files(files);
                })),
                Some(Duration::from_millis(5000)),
            )
            .get()?;

        // very intentionally use blocking calls. We want all our modifications to happen
        // before any other continuation is allowed to process.

        let _ = std::fs::remove_file(cert.as_str());
        let _ = std::fs::remove_file(key.as_str());

        // deletes should _not_ cause errors/reloads
        match wait_3s(&st) {
            Ok(()) => panic!("should not reach"),
            Err(e) if e.is::<TimedOutError>() => {
                // ok
            }
            Err(e) => return Err(e),
        }

        assert_eq!(st.changed.borrow().len(), 0);

        *st.p.borrow_mut() = Promise::new();

        std::fs::rename(cert2.as_str(), cert.as_str()).expect("rename");
        std::fs::rename(key2.as_str(), key.as_str()).expect("rename");

        // now it should reload
        st.get_future().get()?;

        assert_eq!(st.changed.borrow().len(), 2);
        st.changed.borrow_mut().clear();

        // again, without delete

        copy_file(&certfile("test.crt"), &path_in(&tmp2, "test.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp2, "test.key"));

        *st.p.borrow_mut() = Promise::new();

        std::fs::rename(cert2.as_str(), cert.as_str()).expect("rename");
        std::fs::rename(key2.as_str(), key.as_str()).expect("rename");

        // it should reload here as well.
        st.get_future().get()?;

        // could get two notifications. but not more.
        let mut i = 0;
        loop {
            *st.p.borrow_mut() = Promise::new();
            match wait_3s(&st) {
                Ok(()) => assert!(i == 0),
                Err(e) if e.is::<TimedOutError>() => {
                    // ok
                    break;
                }
                Err(e) => return Err(e),
            }
            i += 1;
        }
        Ok(())
    })
});

teseladb::engine_test_case!(test_closed_write, {
    teseladb::engine::core::thread::async_(|| {
        let mut b = tls::CredentialsBuilder::default();

        b.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).get()?;
        b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
        b.set_dh_level(tls::DhLevel::default());
        b.set_system_trust().get()?;
        b.set_client_auth(ClientAuth::REQUIRE);

        let creds = b.build_certificate_credentials()?;
        let serv = b.build_server_credentials()?;

        let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        opts.set_fixed_cpu(this_shard_id());

        let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
        let mut server = tls::listen(serv, addr, opts)?;

        let check_same_message_two_writes = |out: &mut OutputStream<u8>| {
            let r1 = out.write_str("apa").get().and_then(|()| out.flush().get());
            let ep1 = match r1 {
                Ok(()) => panic!("should not reach"),
                // ok
                Err(e) => e,
            };

            let r2 = out.write_str("apa").get().and_then(|()| out.flush().get());
            let ep2 = match r2 {
                Ok(()) => panic!("should not reach"),
                // ok
                Err(e) => e,
            };

            assert_eq!(exc_what(&ep1), exc_what(&ep2));
        };

        {
            let sa = server.accept();
            let mut c = tls::connect(creds.clone(), addr, TlsOptions::default()).get()?;
            let mut s = sa.get()?;
            let _in = s.connection.input(Default::default());

            let mut out = OutputStream::<u8>::with_options(
                c.output(8192).detach()?,
                4096,
                OutputStreamOptions::default(),
            );
            // close on client end before writing
            out.close().get()?;

            check_same_message_two_writes(&mut out);
        }

        {
            let sa = server.accept();
            let mut c = tls::connect(creds.clone(), addr, TlsOptions::default()).get()?;
            let mut s = sa.get()?;
            let mut in_ = s.connection.input(Default::default());

            let mut out = OutputStream::<u8>::with_options(
                c.output(8192).detach()?,
                4096,
                OutputStreamOptions::default(),
            );

            out.write_str("apa").get()?;
            let f = out.flush();
            in_.read().get()?;
            f.get()?;

            // close on server end before writing
            in_.close().get()?;
            s.connection.shutdown_input();
            s.connection.shutdown_output();

            // we won't get broken pipe until
            // after a while (tm)
            loop {
                if out.write_str("apa").get().and_then(|()| out.flush().get()).is_err() {
                    break;
                }
            }

            // now check we get the same message.
            check_same_message_two_writes(&mut out);
        }
        Ok(())
    })
});

/// An output stream with buffer size `size` over the socket's detached sink.
fn detached_output(c: &mut ConnectedSocket, size: usize) -> Result<OutputStream<u8>, ExceptionPtr> {
    Ok(OutputStream::with_options(c.output(8192).detach()?, size, OutputStreamOptions::default()))
}

/// The client credentials of `test_dn_name_handling`.
fn mtls_client_creds(client: &str) -> Result<SharedPtr<tls::CertificateCredentials>, ExceptionPtr> {
    let mut builder = tls::CredentialsBuilder::default();
    builder.set_x509_trust_file(&cf("mtls_ca.crt"), X509CrtFormat::PEM).get()?;
    builder
        .set_x509_key_file(
            &cf(&format!("{client}.crt")),
            &cf(&format!("{client}.key")),
            X509CrtFormat::PEM,
        )
        .get()?;
    builder.build_certificate_credentials()
}

/*
 * Certificates:
 *
 * Generated by `common/tls_certs.rs` (the mutual-TLS set):
 *
 * ->   mtls_ca.crt
 *      mtls_ca.key
 *      mtls_server.crt
 *      mtls_server.csr
 *      mtls_server.key
 *      mtls_client1.crt
 *      mtls_client1.csr
 *      mtls_client1.key
 *      mtls_client2.crt
 *      mtls_client2.csr
 *      mtls_client2.key
 *
 */
teseladb::engine_test_case!(test_dn_name_handling, {
    teseladb::engine::core::thread::async_(|| {
        // Connect to server using two different client certificates.
        // Make sure that for every client the server can fetch DN string
        // and the string is correct.
        // The setup consist of several certificates:
        // - CA
        // - mtls_server.crt - server certificate
        // - mtls_client1.crt - first client certificate
        // - mtls_client2.crt - second client certificate
        //
        // The test runs server that uses mtls_server.crt.
        // The server accepts two incomming connections, first one uses mtls_client1.crt
        // and the second one uses mtls_client2.crt. Every client sends a short string
        // that server receives and tries to find it in the DN string.

        let addr = make_ipv4_address_ip_port(0x7f000001, 4712);

        let client1_creds = mtls_client_creds("mtls_client1")?;

        let client2_creds = mtls_client_creds("mtls_client2")?;

        let server_creds = {
            let mut builder = tls::CredentialsBuilder::default();
            builder.set_x509_trust_file(&cf("mtls_ca.crt"), X509CrtFormat::PEM).get()?;
            builder
                .set_x509_key_file(
                    &cf("mtls_server.crt"),
                    &cf("mtls_server.key"),
                    X509CrtFormat::PEM,
                )
                .get()?;
            builder.set_client_auth(ClientAuth::REQUIRE);
            builder.build_server_credentials()?
        };

        let fetch_dn = |id: Sstring,
                        client_cred: SharedPtr<tls::CertificateCredentials>|
         -> Result<(), ExceptionPtr> {
            let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
            let mut server_sock = tls::listen(server_creds.clone(), addr, lo)?;

            let sa = server_sock.accept();
            let mut c = tls::connect(client_cred, addr, TlsOptions::default()).get()?;
            let mut s = sa.get()?;

            let mut in_ = s.connection.input(Default::default());
            let mut out = detached_output(&mut c, 1024)?;
            out.write_sstring(&id).get()?;

            let fdn = tls::get_dn_information(&mut s.connection);

            let fout = out.flush();
            let fin = in_.read();

            fout.get()?;

            let dn = fdn.get()?;
            let client_id = fin.get()?;

            in_.close().get()?;
            out.close().get()?;

            s.connection.shutdown_input();
            s.connection.shutdown_output();

            c.shutdown_input();
            c.shutdown_output();

            let dn = dn.expect("dn");
            assert!(dn.subject.contains(client_id.as_slice()));
            Ok(())
        };

        fetch_dn(ss("client1.org"), client1_creds)?;
        fetch_dn(ss("client2.org"), client2_creds)?;
        Ok(())
    })
});

/// The builder, credentials and listener the alt-name, certificate-chain and eof tests share.
fn client_auth_server() -> Result<
    (
        tls::CredentialsBuilder,
        SharedPtr<tls::CertificateCredentials>,
        SocketAddress,
        ServerSocket,
        ListenOptions,
    ),
    ExceptionPtr,
> {
    let mut b = tls::CredentialsBuilder::default();

    b.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).get()?;
    b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
    b.set_client_auth(ClientAuth::REQUIRE);

    let creds = b.build_certificate_credentials()?;
    let serv = b.build_server_credentials()?;

    let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    opts.set_fixed_cpu(this_shard_id());

    let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
    let server = tls::listen(serv, addr, opts.clone())?;
    Ok((b, creds, addr, server, opts))
}

teseladb::engine_test_case!(test_alt_names, {
    teseladb::engine::core::thread::async_(|| {
        let (_b, creds, addr, mut server, _opts) = client_auth_server()?;

        {
            let sa = server.accept();
            let mut c = tls::connect(creds, addr, TlsOptions::default()).get()?;
            let mut s = sa.get()?;

            let mut in_ = s.connection.input(Default::default());
            let mut out = detached_output(&mut c, 1024)?;
            out.write_str("nils").get()?;

            let falt_names = tls::get_alt_name_information(&mut s.connection, HashSet::new());

            let fout = out.flush();
            let fin = in_.read();

            fout.get()?;

            let alt_names = falt_names.get()?;
            fin.get()?;

            in_.close().get()?;
            out.close().get()?;

            s.connection.shutdown_input();
            s.connection.shutdown_output();

            c.shutdown_input();
            c.shutdown_output();

            let ensure_alt_name = |type_: tls::SubjectAltNameType, mut min_count: usize| {
                for v in &alt_names {
                    if type_ != v.type_ {
                        continue;
                    }
                    eprintln!("Alt name type: {}: {}", type_ as i32, v.value);
                    min_count -= 1;
                    if min_count == 0 {
                        return;
                    }
                }
                panic!("Missing {} alt name attributes of type {}", min_count, type_ as i32);
            };

            ensure_alt_name(tls::SubjectAltNameType::ipaddress, 1);
            ensure_alt_name(tls::SubjectAltNameType::rfc822name, 2);
            ensure_alt_name(tls::SubjectAltNameType::dnsname, 1);
        }
        Ok(())
    })
});

teseladb::engine_test_case!(test_peer_certificate_chain_handling, {
    teseladb::engine::core::thread::async_(|| {
        let (_b, creds, addr, mut server, _opts) = client_auth_server()?;

        {
            let sa = server.accept();
            let mut c = tls::connect(creds, addr, TlsOptions::default()).get()?;
            let mut s = sa.get()?;

            let mut in_ = s.connection.input(Default::default());
            let mut out = detached_output(&mut c, 1024)?;
            out.write_str("nils").get()?;

            let fscrts = tls::get_peer_certificate_chain(&mut s.connection);
            let fccrts = tls::get_peer_certificate_chain(&mut c);

            let fout = out.flush();
            let fin = in_.read();

            fout.get()?;

            let scrts = fscrts.get()?;
            let ccrts = fccrts.get()?;
            fin.get()?;

            in_.close().get()?;
            out.close().get()?;

            s.connection.shutdown_input();
            s.connection.shutdown_output();

            c.shutdown_input();
            c.shutdown_output();

            let read_file = |path: &str| -> tls::CertificateData {
                std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"))
            };

            let ders = vec![read_file(&certfile("test.crt.der"))];

            assert!(scrts == ders);
            assert!(ccrts == ders);
        }
        Ok(())
    })
});

teseladb::engine_test_case!(test_skip_wait_for_eof, {
    teseladb::engine::core::thread::async_(|| {
        let (_b, creds, addr, mut server, _opts) = client_auth_server()?;

        {
            // Initiate a connection while specifying that it should not wait for eof on shutdown.
            let sa = server.accept();
            // SAFETY: the reactor of this shard runs the test.
            let c = unsafe { (*engine()).connect(addr) }.get()?;
            let mut c_tls = tls::wrap_client(
                creds,
                c,
                TlsOptions { wait_for_eof_on_shutdown: false, ..Default::default() },
            )
            .get()?;
            let mut s = sa.get()?;

            let mut in_ = s.connection.input(Default::default());
            let mut out = c_tls.output(8192);

            // Write some data in the socket to handshake.
            out.write_str("apa").get()?;
            let f = out.flush();
            let buf = in_.read().get()?;
            f.get()?;
            assert!(buf_str(&buf) == "apa");

            // Prevent the server from reading from the connection.
            // This ensures that it will miss the bye message and not
            // reply with an eof.
            server.abort_accept();

            // Initiate closing of the TLS session
            c_tls.shutdown_input();
            c_tls.shutdown_output();

            // Ensure that the session is closed promptly. When wait_for_eof_on_shutdown is not
            // specified, the call to wait_input_shutdown will hang for 10 seconds waiting for
            // and eof from the server.
            match with_timeout::<SteadyClockType, ()>(
                <SteadyClockType as Clock>::add(
                    <SteadyClockType as Clock>::now(),
                    Duration::from_secs(1),
                ),
                c_tls.wait_input_shutdown(),
            )
            .get()
            {
                Err(e) if e.is::<TimedOutError>() => {
                    panic!(
                        "Timed out while waiting for input shutdown.This indicates the EOF wait was not skipped"
                    );
                }
                r => r?,
            }
        }
        Ok(())
    })
});

/// The first connection of the session ticket tests: exchanges data both ways and returns
/// the ticket.
fn first_session(
    server: &mut ServerSocket,
    creds: &SharedPtr<tls::CertificateCredentials>,
    addr: &SocketAddress,
) -> Result<tls::SessionData, ExceptionPtr> {
    let sa = server.accept();
    let mut c = tls::connect(creds.clone(), *addr, TlsOptions::default()).get()?;
    let mut s = sa.get()?;

    let mut in_ = s.connection.input(Default::default());
    let mut cin = c.input(Default::default());
    let mut out = detached_output(&mut c, 1024)?;
    let mut sout = detached_output(&mut s.connection, 1024)?;

    // write data in both directions. Required for session data to
    // become available.
    out.write_str("nils").get()?;
    let mut fin = in_.read();
    let mut fout = out.flush();

    fout.get()?;
    fin.get()?;

    sout.write_str("banan").get()?;
    fin = cin.read();
    fout = sout.flush();

    fout.get()?;
    fin.get()?;

    assert!(!tls::check_session_is_resumed(&mut c).get()?); // no resume data

    // get ticket data
    let sess_data = tls::get_session_resume_data(&mut c).get()?;
    assert!(!sess_data.is_empty());

    in_.close().get()?;
    out.close().get()?;

    s.connection.shutdown_input();
    s.connection.shutdown_output();

    c.shutdown_input();
    c.shutdown_output();
    Ok(sess_data)
}

/// The resuming connection of the session ticket tests: returns whether it resumed.
fn resumed_session(
    server: &mut ServerSocket,
    creds: &SharedPtr<tls::CertificateCredentials>,
    addr: &SocketAddress,
    sess_data: &tls::SessionData,
) -> Result<bool, ExceptionPtr> {
    let sa = server.accept();

    // tell client to try resuming.
    let tls_opts = TlsOptions { session_resume_data: sess_data.clone(), ..TlsOptions::default() };

    let mut c = tls::connect(creds.clone(), *addr, tls_opts).get()?;
    let mut s = sa.get()?;

    // This is ok. Will force a handshake.
    let f = tls::check_session_is_resumed(&mut c);

    // But we need to force some IO to make the
    // handshake actually happen.
    let mut in_ = s.connection.input(Default::default());
    let mut out = detached_output(&mut c, 1024)?;

    let fin = in_.read();

    out.write_str("nils").get()?;
    let fout = out.flush();

    fout.get()?;
    fin.get()?;

    let resumed = f.get()?;

    in_.close().get()?;
    out.close().get()?;

    s.connection.shutdown_input();
    s.connection.shutdown_output();

    c.shutdown_input();
    c.shutdown_output();
    Ok(resumed)
}

fn do_test_tls13_session_tickets(reset_server: bool) -> Result<(), ExceptionPtr> {
    let mut b = tls::CredentialsBuilder::default();

    b.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).get()?;
    b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
    b.set_session_resume_mode(tls::SessionResumeMode::TLS13_SESSION_TICKET)?;
    b.set_priority_string(&ss("SECURE128:+SECURE192:-VERS-TLS-ALL:+VERS-TLS1.3"));

    let creds = b.build_certificate_credentials()?;
    let mut serv = b.build_server_credentials()?;

    let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    opts.set_fixed_cpu(this_shard_id());

    let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
    let mut server = tls::listen(serv.clone(), addr, opts.clone())?;

    let sess_data = first_session(&mut server, &creds, &addr)?;

    if reset_server {
        server = ServerSocket::default();
        drop(server);
        // rebuild creds
        serv = b.build_server_credentials()?;
        server = tls::listen(serv, addr, opts)?;
    }

    assert!(resumed_session(&mut server, &creds, &addr, &sess_data)?); // Should work
    Ok(())
}

/*
 * Test TLS13 session ticket support.
*/
teseladb::engine_test_case!(test_tls13_session_tickets, {
    teseladb::engine::core::thread::async_(|| do_test_tls13_session_tickets(false))
});

teseladb::engine_test_case!(test_tls13_session_tickets_retain_session_key, {
    teseladb::engine::core::thread::async_(|| do_test_tls13_session_tickets(true))
});

teseladb::engine_test_case!(test_tls13_session_tickets_invalidated_by_reload, {
    teseladb::engine::core::thread::async_(|| {
        let mut b = tls::CredentialsBuilder::default();
        let tmp = Tmpdir::new();

        // copy the wrong certs. We don't trust these
        // blocking calls, but this is a test and engine does not have a copy
        // util and I am lazy...
        copy_file(&certfile("test.crt"), &path_in(&tmp, "test.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp, "test.key"));

        let cert = path_in(&tmp, "test.crt");
        let key = path_in(&tmp, "test.key");
        let p: Rc<RefCell<Promise<()>>> = Rc::new(RefCell::new(Promise::new()));

        b.set_x509_key_file(&cert, &key, X509CrtFormat::PEM).get()?;
        b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
        b.set_session_resume_mode(tls::SessionResumeMode::TLS13_SESSION_TICKET)?;
        b.set_priority_string(&ss("SECURE128:+SECURE192:-VERS-TLS-ALL:+VERS-TLS1.3"));

        let creds = b.build_certificate_credentials()?;
        let p1 = p.clone();
        let serv = b
            .build_reloadable_server_credentials_simple(
                Some(Rc::new(move |_: &HashSet<Sstring>, _: Option<ExceptionPtr>| {
                    p1.borrow_mut().set_value(());
                })),
                None,
            )
            .get()?;

        let reloaded = p.borrow_mut().get_future();

        let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        opts.set_fixed_cpu(this_shard_id());

        let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
        let mut server = tls::listen(serv, addr, opts)?;

        let sess_data = first_session(&mut server, &creds, &addr)?;

        assert!(!reloaded.available());

        // fs::copy_options::overwrite_existing
        copy_file(&certfile("test.crt"), &path_in(&tmp, "test.crt"));
        reloaded.get()?;

        assert!(!resumed_session(&mut server, &creds, &addr, &sess_data)?); // Should NOT work. Keys should have been replaced
        Ok(())
    })
});

/// A pointer handed to another shard, for state the closures share across shards.
/// with `smp::submit_to`.
struct CrossShard<T>(*mut T);

impl<T> Clone for CrossShard<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for CrossShard<T> {}

impl<T> CrossShard<T> {
    /// The pointer, read through the whole wrapper so that closures capture it whole.
    fn ptr(self) -> *mut T {
        self.0
    }
}
// SAFETY: the pointee outlives the cross-shard calls (the test joins them), and only one shard
// touches it at a time.
unsafe impl<T> Send for CrossShard<T> {}

teseladb::engine_test_case!(test_reload_certificates_with_only_shard0_notify, {
    teseladb::engine::core::thread::async_(|| {
        let tmp = Tmpdir::new();

        // copy the wrong certs. We don't trust these
        // blocking calls, but this is a test and engine does not have a copy
        // util and I am lazy...
        copy_file(&certfile("other.crt"), &path_in(&tmp, "test.crt"));
        copy_file(&certfile("other.key"), &path_in(&tmp, "test.key"));

        let cert = path_in(&tmp, "test.crt");
        let key = path_in(&tmp, "test.key");
        let mut p = Promise::<()>::new();

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_key_file(&cert, &key, X509CrtFormat::PEM).get()?;
        b.set_dh_level(tls::DhLevel::default());

        let mut certs = b.build_server_credentials()?;

        let mut shard_1_certs: Option<SharedPtr<tls::ServerCredentials>> = None;
        let (bp, pp, cp, slot) = (
            CrossShard(&mut b),
            CrossShard(&mut p),
            CrossShard(&mut certs),
            CrossShard(&mut shard_1_certs),
        );
        let (cert1, key1) = (cert.clone(), key.clone());
        teseladb::engine::core::smp::Smp::submit_to(1, move || -> Future<()> {
            // SAFETY: see `CrossShard`.
            let b = unsafe { &*bp.ptr() };
            let changed: Rc<RefCell<HashSet<Sstring>>> = Rc::new(RefCell::new(HashSet::new()));
            let (cert, key) = (cert1.clone(), key1.clone());
            b.build_reloadable_server_credentials(
                Some(Rc::new(
                    move |builder: &tls::CredentialsBuilder,
                          files: &HashSet<Sstring>,
                          ep: Option<ExceptionPtr>|
                          -> Future<()> {
                        if ep.is_some() {
                            return make_ready_future(());
                        }
                        changed.borrow_mut().extend(files.iter().cloned());
                        let done =
                            changed.borrow().contains(&cert) && changed.borrow().contains(&key);
                        if done {
                            // shard one certs are not reloadable. We issue a reload of them from shard 0
                            // - to save inotify instances.
                            let builder = CrossShard(
                                builder as *const tls::CredentialsBuilder
                                    as *mut tls::CredentialsBuilder,
                            );
                            return teseladb::engine::core::smp::Smp::submit_to(
                                0,
                                move || -> Result<(), ExceptionPtr> {
                                    // SAFETY: `CrossShard` pointers stay valid: the objects live in the test's frame until the cross-shard calls are waited for, and each one is used by one shard at a time.
                                    unsafe {
                                        (*builder.ptr()).rebuild_server(creds_mut(&*cp.ptr()))?
                                    };
                                    // SAFETY: `CrossShard` pointers stay valid: the promise lives in the test's frame until it is waited for, and only this shard sets it.
                                    unsafe { (*pp.ptr()).set_value(()) };
                                    Ok(())
                                },
                            );
                        }
                        make_ready_future(())
                    },
                )),
                None,
            )
            .then(move |c: SharedPtr<tls::ServerCredentials>| {
                // SAFETY: `CrossShard` pointers stay valid: the slot lives in the test's frame until this submission is waited for, and only this continuation writes it.
                unsafe { *slot.ptr() = Some(c) };
            })
        })
        .get()?;

        let _def = teseladb::engine::util::defer::defer(move || {
            let _ = teseladb::engine::core::smp::Smp::submit_to(1, move || {
                // SAFETY: see `CrossShard`; the credentials are released on the shard that made them.
                unsafe { *slot.ptr() = None };
            })
            .get();
        });

        let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let addr = make_ipv4_address_ip_port(0x7f000001, 4712);
        let mut server = tls::listen(certs.clone(), addr, opts)?;

        let mut b2 = tls::CredentialsBuilder::default();
        b2.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;

        {
            let sa = server.accept();
            let mut c =
                tls::connect(b2.build_certificate_credentials()?, addr, TlsOptions::default())
                    .get()?;
            let mut s = sa.get()?;
            let mut in_ = s.connection.input(Default::default());

            let mut out = detached_output(&mut c, 4096)?;

            let r = (|| -> Result<(), ExceptionPtr> {
                out.write_str("apa").get()?;
                let f = out.flush();
                let f2 = in_.read();

                match f.get() {
                    Ok(()) => panic!("should not reach"),
                    Err(e) if e.is::<VerificationError>() => {
                        // ok
                    }
                    Err(e) => return Err(e),
                }
                let _ = out.close().get();

                if f2.get().is_ok() {
                    panic!("should not reach");
                }
                // ok
                let _ = in_.close().get();
                Ok(())
            })();
            match r {
                Ok(()) => {}
                Err(e) if e.is::<VerificationError>() => {
                    // ok
                }
                Err(e) => return Err(e),
            }
        }

        // copy the right (trusted) certs over the old ones.
        copy_file(&certfile("test.crt"), &path_in(&tmp, "test0.crt"));
        copy_file(&certfile("test.key"), &path_in(&tmp, "test0.key"));

        rename_file(path_in(&tmp, "test0.crt").as_str(), path_in(&tmp, "test.crt").as_str())
            .get()?;
        rename_file(path_in(&tmp, "test0.key").as_str(), path_in(&tmp, "test.key").as_str())
            .get()?;

        // SAFETY: shard 1 sets the promise only through `pp`, before this wait resolves.
        unsafe { (*pp.ptr()).get_future() }.get()?;

        // now it should work
        {
            let sa = server.accept();
            let mut c =
                tls::connect(b2.build_certificate_credentials()?, addr, TlsOptions::default())
                    .get()?;
            let mut s = sa.get()?;
            let mut in_ = s.connection.input(Default::default());

            let mut out = detached_output(&mut c, 4096)?;

            out.write_str("apa").get()?;
            let f = out.flush();
            let buf = in_.read().get()?;
            f.get()?;
            out.close().get()?;
            in_.read().get()?; // ignore - just want eof
            in_.close().get()?;

            assert_eq!(buf_str(&buf), "apa");
        }
        Ok(())
    })
});
