//! TLS paths beyond tls_test (the original's tls.cc): the socket options of a TLS connection
//! reach the TCP socket below it; the named and local-address connect helpers, the TLS
//! `Socket` (its address reuse, a second connect that has no credentials left) and
//! `wrap_client_with_name` all verify the server's name; a PKCS#12 bundle serves a
//! connection, given to a builder or to the server credentials; a revocation list that
//! revokes the server's certificate makes the client's verification fail; and the session
//! helpers refuse a plain socket ("Not a TLS socket") and an empty one (ENOTCONN).
#![cfg(all(target_os = "linux", feature = "tls"))]
#![allow(unsafe_code)]
// The deprecated name-only helpers are part of what is tested.
#![allow(deprecated)]

#[path = "common/tls_certs.rs"]
mod tls_certs;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, InvalidArgument};
use teseladb::engine::core::reactor::{connect_from, listen_with};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::shared_ptr::SharedPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{
    ConnectedSocket, KeepaliveParams, ListenOptions, LoadBalancingAlgorithm, ServerSocket,
    TcpKeepaliveParams, make_ipv4_address_ip_port,
};
use teseladb::engine::net::socket_defs::{SocketAddress, Transport};
use teseladb::engine::net::tls::{
    self, AbstractCredentials, CertificateCredentials, TlsOptions, VerificationError, X509CrtFormat,
};
use teseladb::engine_test_case;
use tls_certs::certfile;

const NAME: &str = "test.example.org";
const SOL_SOCKET: i32 = 1;
const SO_KEEPALIVE: i32 = 9;

fn cf(file: &str) -> Sstring {
    Sstring::from(certfile(file).as_str())
}

/// Listening options that keep every connection on this shard: with the default distribution
/// a connection made while another is open goes to the other shard's accept.
fn reuse() -> ListenOptions {
    ListenOptions {
        reuse_address: true,
        lba: LoadBalancingAlgorithm::Fixed,
        fixed_cpu: this_shard_id(),
        ..ListenOptions::default()
    }
}

/// A TLS listener on 127.0.0.1:`port` serving test.crt.
fn server(port: u16) -> Result<(ServerSocket, SocketAddress), ExceptionPtr> {
    let mut b = tls::CredentialsBuilder::default();
    b.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM)
        .get()?;
    let addr = make_ipv4_address_ip_port(0x7f00_0001, port);
    Ok((
        tls::listen(b.build_server_credentials()?, addr, reuse())?,
        addr,
    ))
}

/// Client credentials that trust the test CA.
fn client_creds() -> Result<SharedPtr<CertificateCredentials>, ExceptionPtr> {
    let mut b = tls::CredentialsBuilder::default();
    b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM)
        .get()?;
    b.build_certificate_credentials()
}

/// Sends `msg` from `c` and reads it at `s`: the handshake (and the name check) completes.
fn exchange(
    c: &mut ConnectedSocket,
    s: &mut ConnectedSocket,
    msg: &str,
) -> Result<(), ExceptionPtr> {
    let mut out = c.output(8192);
    let mut in_ = s.input(Default::default());
    out.write_str(msg).get()?;
    let f = out.flush();
    let buf = in_.read().get()?;
    f.get()?;
    assert_eq!(buf.as_slice(), msg.as_bytes());
    out.close().get()?;
    in_.close().get()?;
    Ok(())
}

/// Accepts and exchanges a message with `client`, which must succeed.
fn serve_one(ss: &mut ServerSocket, client: Future<ConnectedSocket>) -> Result<(), ExceptionPtr> {
    let accepted = ss.accept();
    let mut c = client.get()?;
    let mut s = accepted.get()?.connection;
    exchange(&mut c, &mut s, "hello")
}

/// Accepts `client` and returns the error with which the client's handshake fails; the
/// server side reads meanwhile, so that its half of the handshake runs.
fn failed_handshake(
    ss: &mut ServerSocket,
    client: Future<ConnectedSocket>,
) -> Result<ExceptionPtr, ExceptionPtr> {
    let accepted = ss.accept();
    let mut c = client.get()?;
    let mut s = accepted.get()?.connection;
    let mut in_ = s.input(Default::default());
    let server_read = in_.read();
    let e = tls::get_dn_information(&mut c)
        .get()
        .err()
        .expect("the handshake fails");
    drop(c);
    let _ = server_read.get();
    Ok(e)
}

engine_test_case!(tls_connections_pass_socket_options_to_tcp, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut ss, addr) = server(14811)?;
        let accepted = ss.accept();
        let mut c = tls::connect_with_name(client_creds()?, addr, NAME.into()).get()?;
        let mut s = accepted.get()?.connection;
        assert!(!c.get_nodelay()?);
        c.set_nodelay(true)?;
        assert!(c.get_nodelay()?);
        c.set_keepalive(true)?;
        assert!(c.get_keepalive()?);
        let p = KeepaliveParams::Tcp(TcpKeepaliveParams {
            idle: Duration::from_secs(20),
            interval: Duration::from_secs(2),
            count: 3,
        });
        c.set_keepalive_parameters(&p)?;
        assert_eq!(c.get_keepalive_parameters()?, p);
        let off: i32 = 0;
        // SAFETY: `off` is an int of the size given.
        unsafe { c.set_sockopt(SOL_SOCKET, SO_KEEPALIVE, (&raw const off).cast(), 4) }?;
        let mut v: i32 = -1;
        // SAFETY: `v` is an int of the size given.
        let n = unsafe { c.get_sockopt(SOL_SOCKET, SO_KEEPALIVE, (&raw mut v).cast(), 4) }?;
        assert_eq!((n, v), (0, 0));
        assert!(c.remote_address() == addr);
        assert!(s.remote_address() == c.local_address());
        exchange(&mut c, &mut s, "options")?;
        ss.abort_accept();
        Ok(())
    })
});

engine_test_case!(named_and_local_connect_helpers_verify_the_name, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut ss, addr) = server(14812)?;
        let local = make_ipv4_address_ip_port(0x7f00_0001, 0);
        serve_one(
            &mut ss,
            tls::connect_local_with_name(client_creds()?, addr, local, NAME.into()),
        )?;
        let opts = TlsOptions {
            server_name: NAME.into(),
            ..Default::default()
        };
        serve_one(
            &mut ss,
            tls::connect_local(client_creds()?, addr, local, opts),
        )?;
        let plain = connect_from(addr, SocketAddress::default(), Transport::TCP).get()?;
        serve_one(
            &mut ss,
            tls::wrap_client_with_name(client_creds()?, plain, NAME.into()),
        )?;
        // A wrong name fails the verification.
        let opts = TlsOptions {
            server_name: "wrong.example.org".into(),
            ..Default::default()
        };
        let e = failed_handshake(&mut ss, tls::connect(client_creds()?, addr, opts))?;
        assert!(e.downcast_ref::<VerificationError>().is_some(), "{e}");
        ss.abort_accept();
        Ok(())
    })
});

engine_test_case!(a_tls_socket_connects_once_with_its_credentials, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut ss, addr) = server(14813)?;
        let mut sk = tls::socket_with_name(client_creds()?, NAME.into());
        sk.set_reuseaddr(true)?;
        assert!(sk.get_reuseaddr()?);
        let accepted = ss.accept();
        let mut c = sk
            .connect(addr, SocketAddress::default(), Transport::TCP)
            .get()?;
        let mut s = accepted.get()?.connection;
        exchange(&mut c, &mut s, "socket")?;
        sk.shutdown()?;
        // The credentials went to the first connection: a second connect has none.
        let mut again = tls::socket(client_creds()?, TlsOptions::default());
        let accepted = ss.accept();
        let _first = again
            .connect(addr, SocketAddress::default(), Transport::TCP)
            .get()?;
        let _ = accepted.get()?;
        let accepted = ss.accept();
        let e = again
            .connect(addr, SocketAddress::default(), Transport::TCP)
            .get()
            .err()
            .expect("no credentials left");
        assert!(
            e.to_string().contains("not connected") || e.to_string().contains("107"),
            "{e}"
        );
        let _ = accepted.get();
        ss.abort_accept();
        Ok(())
    })
});

/// Runs `openssl args...` in `dir`.
fn openssl(dir: &Path, args: &[&str]) {
    let status = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("openssl");
    assert!(status.success(), "openssl {args:?}: {status}");
}

/// A fresh directory under the system temporary directory.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cov2-tls-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

engine_test_case!(a_pkcs12_bundle_serves_connections, {
    async_(|| -> Result<(), ExceptionPtr> {
        let dir = scratch("p12");
        let p12 = dir.join("test.p12");
        openssl(
            &dir,
            &[
                "pkcs12",
                "-export",
                "-in",
                &certfile("test.crt"),
                "-inkey",
                &certfile("test.key"),
                "-out",
                p12.to_str().unwrap(),
                "-passout",
                "pass:cov2pw",
            ],
        );
        let p12s = Sstring::from(p12.to_str().unwrap());
        // Through the builder.
        let mut b = tls::CredentialsBuilder::default();
        b.set_simple_pkcs12_file(&p12s, X509CrtFormat::DER, &"cov2pw".into())
            .get()?;
        let addr = make_ipv4_address_ip_port(0x7f00_0001, 14814);
        let mut ss = tls::listen(b.build_server_credentials()?, addr, reuse())?;
        serve_one(
            &mut ss,
            tls::connect_with_name(client_creds()?, addr, NAME.into()),
        )?;
        ss.abort_accept();
        drop(ss);
        // Directly on server credentials.
        let mut direct = tls::ServerCredentials::new()?;
        direct
            .set_simple_pkcs12_file(&p12s, X509CrtFormat::DER, &"cov2pw".into())
            .get()?;
        let addr2 = make_ipv4_address_ip_port(0x7f00_0001, 14815);
        let mut ss2 = tls::listen(
            teseladb::engine::core::shared_ptr::make_shared(direct),
            addr2,
            reuse(),
        )?;
        serve_one(
            &mut ss2,
            tls::connect_with_name(client_creds()?, addr2, NAME.into()),
        )?;
        ss2.abort_accept();
        // A wrong password is refused.
        let mut bad = tls::CredentialsBuilder::default();
        bad.set_simple_pkcs12_file(&p12s, X509CrtFormat::DER, &"nope".into())
            .get()?;
        assert!(
            bad.build_server_credentials().is_err(),
            "the bundle does not open"
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    })
});

engine_test_case!(a_revocation_list_rejects_the_revoked_server, {
    async_(|| -> Result<(), ExceptionPtr> {
        let dir = scratch("crl");
        std::fs::write(dir.join("index.txt"), "").unwrap();
        std::fs::write(dir.join("crlnumber"), "01\n").unwrap();
        std::fs::write(
            dir.join("ca.cnf"),
            "[ ca ]\ndefault_ca = d\n[ d ]\ndatabase = index.txt\ncrlnumber = crlnumber\n\
             default_md = sha256\ndefault_crl_days = 30\n",
        )
        .unwrap();
        let (key, ca, crt) = (
            certfile("catest.key"),
            certfile("catest.pem"),
            certfile("test.crt"),
        );
        openssl(
            &dir,
            &[
                "ca", "-config", "ca.cnf", "-keyfile", &key, "-cert", &ca, "-revoke", &crt,
            ],
        );
        openssl(
            &dir,
            &[
                "ca", "-config", "ca.cnf", "-keyfile", &key, "-cert", &ca, "-gencrl", "-out",
                "crl.pem",
            ],
        );
        let crl = Sstring::from(dir.join("crl.pem").to_str().unwrap());
        let (mut ss, addr) = server(14816)?;

        // Without the list the server is accepted.
        serve_one(
            &mut ss,
            tls::connect_with_name(client_creds()?, addr, NAME.into()),
        )?;

        let mut b = tls::CredentialsBuilder::default();
        b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM)
            .get()?;
        b.set_x509_crl_file(&crl, X509CrtFormat::PEM).get()?;
        let opts = TlsOptions {
            server_name: NAME.into(),
            ..Default::default()
        };
        let e = failed_handshake(
            &mut ss,
            tls::connect(b.build_certificate_credentials()?, addr, opts),
        )?;
        assert!(e.downcast_ref::<VerificationError>().is_some(), "{e}");
        assert!(e.to_string().contains("revoked"), "{e}");
        ss.abort_accept();
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    })
});

engine_test_case!(session_helpers_refuse_plain_and_empty_sockets, {
    async_(|| -> Result<(), ExceptionPtr> {
        let addr = make_ipv4_address_ip_port(0x7f00_0001, 14817);
        let mut ss = listen_with(addr, reuse())?;
        let accepted = ss.accept();
        let mut plain = connect_from(addr, SocketAddress::default(), Transport::TCP).get()?;
        let _s = accepted.get()?;
        let e = tls::get_dn_information(&mut plain)
            .get()
            .err()
            .expect("plain");
        assert!(e.downcast_ref::<InvalidArgument>().is_some(), "{e}");
        assert!(e.to_string().contains("Not a TLS socket"), "{e}");
        let mut empty = ConnectedSocket::default();
        let e = tls::get_dn_information(&mut empty)
            .get()
            .err()
            .expect("empty");
        assert!(
            e.to_string().contains("107") || e.to_string().contains("connected"),
            "{e}"
        );
        ss.abort_accept();
        Ok(())
    })
});
