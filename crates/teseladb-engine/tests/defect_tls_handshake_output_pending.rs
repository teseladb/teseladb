#![cfg(all(target_os = "linux", feature = "tls"))]
//! A TLS handshake that fails in its verification (here a DN callback that aborts the
//! connection and fails) while one of its writes is still in flight waits for that write,
//! and ignores its result, before failing. Before, it failed at once and left the write's
//! future pending in the session: once the write resolved with an error, nothing consumed it,
//! and the session was destroyed holding an abandoned failed future (which the test runner
//! reports as a failure).
//!
//! The server's writes are deferred by one reactor tick, so that a write is still in flight
//! when gnutls finishes the handshake and the DN callback runs.
#![allow(unsafe_code)]

#[path = "common/loopback_socket.rs"]
mod loopback_socket;
#[path = "common/tls_certs.rs"]
mod tls_certs;

use loopback_socket::{LoopbackBuffer, LoopbackBufferType, LoopbackConnectedSocketImpl};
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{DataSink, DataSinkImpl, DataSource};
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::sharded::make_foreign;
use teseladb::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{ConnectedSocket, ConnectedSocketInputStreamConfig, KeepaliveParams};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::net::stack::ConnectedSocketImpl;
use teseladb::engine::net::tls::{
    self, AbstractCredentials, ClientAuth, SessionType, TlsOptions, VerificationError, X509CrtFormat};
use tls_certs::certfile;

fn cf(file: &str) -> Sstring {
    Sstring::from(certfile(file).as_str())
}

/// A sink that defers every write by one reactor tick.
struct DeferredSink {
    sink: DataSink,
}

impl DataSinkImpl for DeferredSink {
    fn put(&mut self, p: Packet) -> Future<()> {
        let this: *mut DeferredSink = self;
        let mut p = Some(p);
        // SAFETY: the sink outlives its pending puts (its stream waits for them).
        yield_().then(move |()| unsafe { (*this).sink.put(p.take().unwrap_or_default()) })
    }
    fn flush(&mut self) -> Future<()> {
        self.sink.flush()
    }
    fn close(&mut self) -> Future<()> {
        self.sink.close()
    }
}

/// A loopback socket whose writes are deferred.
struct DeferredSocket {
    inner: LoopbackConnectedSocketImpl,
}

impl ConnectedSocketImpl for DeferredSocket {
    fn source(&mut self) -> DataSource {
        self.inner.source()
    }
    fn source_with_config(&mut self, csisc: ConnectedSocketInputStreamConfig) -> DataSource {
        self.inner.source_with_config(csisc)
    }
    fn sink(&mut self) -> DataSink {
        DataSink::new(Box::new(DeferredSink { sink: self.inner.sink() }))
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

teseladb::engine_test_case!(test_output_pending_exception_on_destroy, {
    async_(|| -> Result<(), ExceptionPtr> {
        let tls12_priority = Sstring::from("NORMAL:-VERS-TLS-ALL:+VERS-TLS1.2");

        let mut cb = tls::CredentialsBuilder::default();
        cb.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).get()?;
        cb.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
        cb.set_priority_string(&tls12_priority);
        cb.set_dh_level(tls::DhLevel::default());
        let client_creds = cb.build_certificate_credentials()?;

        let mut sb = tls::CredentialsBuilder::default();
        sb.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).get()?;
        sb.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
        sb.set_priority_string(&tls12_priority);
        sb.set_client_auth(ClientAuth::REQUIRE);
        sb.set_dh_level(tls::DhLevel::default());
        let server_creds = sb.build_server_credentials()?;

        let b1: LwSharedPtr<LoopbackBuffer> =
            make_lw_shared(LoopbackBuffer::new(None, LoopbackBufferType::ServerTx));
        let b2: LwSharedPtr<LoopbackBuffer> =
            make_lw_shared(LoopbackBuffer::new(None, LoopbackBufferType::ClientTx));
        let ssi = Box::new(DeferredSocket {
            inner: LoopbackConnectedSocketImpl::new(make_foreign(b1.clone()), b2.clone()),
        });
        let csi = Box::new(LoopbackConnectedSocketImpl::new(make_foreign(b2), b1.clone()));

        let aborted = b1.clone();
        // SAFETY: the test runs on one shard and holds no other reference to the credentials.
        unsafe { server_creds.get_mut_unchecked() }.set_dn_verification_callback(Some(
            tls::DnCallback::new(move |_: SessionType, _: Sstring, _: Sstring| -> Result<(), ExceptionPtr> {
                // Abort the server's sending buffer while the callback runs, so that the
                // deferred write already in flight resolves with an error afterwards.
                aborted.abort();
                Err(ExceptionPtr::new(VerificationError("aborting handshake from DN callback".to_string())))
            }),
        ));

        let mut ss = tls::wrap_server(server_creds, ConnectedSocket::from_impl(ssi)).get()?;
        let mut cs = tls::wrap_client(
            client_creds,
            ConnectedSocket::from_impl(csi),
            TlsOptions { server_name: Sstring::from("test.example.org"), ..TlsOptions::default() },
        )
        .get()?;

        // Start writing on the client side.
        let mut out = Box::new(cs.output(8192));
        let outp: *mut teseladb::engine::core::iostream::OutputStream<u8> = &mut *out;
        // SAFETY: `out` stays boxed until the write is waited for below.
        let client = unsafe { (*outp).write(&[0u8; 10]) }
            .then(move |()| unsafe { (*outp).flush() })
            .handle_exception(|_e: ExceptionPtr| make_ready_future(()));

        // Reading on the server drives the handshake, which runs the DN callback: it aborts
        // the server's sending buffer and fails.
        let mut input = ss.input(Default::default());
        let r = input.read().get();
        assert!(
            matches!(r, Err(ref e) if e.is::<VerificationError>()),
            "the handshake did not fail with the callback's error: {:?}",
            r.as_ref().err()
        );
        client.get()?;
        drop(input);
        drop(out);
        drop(ss);
        drop(cs);
        Ok(())
    })
});
