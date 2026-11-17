//! TLS peers with crafted certificates: a client certificate whose subject alternative names
//! hold every kind gnutls reports (DNS name, e-mail, URI, IPv4 and IPv6 addresses, other name
//! and directory name), read by the server with and without a filter of kinds, with the
//! distinguished names of both ends. The certificates are made with openssl in a temporary
//! directory. The values are those the reference engine (AnotherProduct 25.05.0, `src/net/tls.cc`,
//! `get_alt_name_information`) returns from the same gnutls calls: strings without their
//! trailing NUL, addresses as `inet_address`, and the kinds gnutls does not name skipped.
#![cfg(all(target_os = "linux", feature = "tls"))]
// Changing shared credentials is an unsafe engine interface; the `unsafe` block states why it
// is sound.
#![allow(unsafe_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::shared_ptr::SharedPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::api::{ListenOptions, make_ipv4_address_ip_port};
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::tls::{
    self, AbstractCredentials, ClientAuth, SubjectAltNameType, SubjectAltNameValue, TlsOptions,
    X509CrtFormat,
};
use teseladb::engine_thread_test_case;

const EXTENSIONS: &str = "\
[srv]
subjectAltName = DNS:localhost, IP:127.0.0.1
[cli]
subjectAltName = @alt
[alt]
DNS.1 = client.example
email.1 = c@example.org
URI.1 = spiffe://example/cov4
IP.1 = 127.0.0.1
IP.2 = ::1
otherName.1 = 1.2.3.4;UTF8:other
dirName.1 = dir_sect
[dir_sect]
CN = dirname
O = cov4
";

fn openssl(dir: &Path, args: &[&str]) {
    let out = Command::new("openssl").current_dir(dir).args(args).output().expect("openssl");
    assert!(out.status.success(), "openssl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A CA, a server certificate and a client certificate with every alternative name kind.
fn make_certs(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cov4_err_tls_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the certificates' directory");
    std::fs::write(dir.join("ext.cnf"), EXTENSIONS).expect("the extensions");
    let ec = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes"];
    let mut ca = vec!["req", "-x509"];
    ca.extend(ec);
    ca.extend(["-keyout", "ca.key", "-out", "ca.pem", "-days", "2", "-subj", "/CN=cov4-ca"]);
    openssl(&dir, &ca);
    for (name, cn, ext) in [("srv", "/CN=localhost", "srv"), ("cli", "/O=cov4/CN=client", "cli")] {
        let key = format!("{name}.key");
        let csr = format!("{name}.csr");
        let pem = format!("{name}.pem");
        let mut req = vec!["req"];
        req.extend(ec);
        req.extend(["-keyout", &key, "-out", &csr, "-subj", cn]);
        openssl(&dir, &req);
        openssl(
            &dir,
            &[
                "x509",
                "-req",
                "-in",
                &csr,
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                &pem,
                "-days",
                "2",
                "-extfile",
                "ext.cnf",
                "-extensions",
                ext,
            ],
        );
    }
    dir
}

fn p(dir: &Path, name: &str) -> Sstring {
    Sstring::from(dir.join(name).display().to_string().as_str())
}

fn credentials(dir: &Path, name: &str) -> Result<tls::CredentialsBuilder, ExceptionPtr> {
    let mut b = tls::CredentialsBuilder::default();
    b.set_x509_key_file(
        &p(dir, &format!("{name}.pem")),
        &p(dir, &format!("{name}.key")),
        X509CrtFormat::PEM,
    )
    .get()?;
    b.set_x509_trust_file(&p(dir, "ca.pem"), X509CrtFormat::PEM).get()?;
    Ok(b)
}

fn string_of(v: &SubjectAltNameValue) -> String {
    match v {
        SubjectAltNameValue::String(s) => s.as_str().to_string(),
        SubjectAltNameValue::Address(a) => format!("{a}"),
    }
}

engine_thread_test_case!(every_alt_name_kind_of_a_client_certificate, {
    let dir = make_certs("alt");
    let mut sb = credentials(&dir, "srv").unwrap();
    sb.set_client_auth(ClientAuth::REQUIRE);
    let serv: SharedPtr<tls::ServerCredentials> = sb.build_server_credentials().unwrap();
    let cli = credentials(&dir, "cli").unwrap().build_certificate_credentials().unwrap();
    let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    opts.set_fixed_cpu(this_shard_id());
    let mut server = tls::listen(serv, make_ipv4_address_ip_port(0x7f00_0001, 0), opts).unwrap();
    let addr = server.local_address().unwrap();

    let accepted = server.accept();
    let topts = TlsOptions { server_name: "localhost".into(), ..TlsOptions::default() };
    let mut c = tls::connect(cli, addr, topts).get().unwrap();
    let mut s = accepted.get().unwrap();

    // The handshake runs with the first I/O: the client writes while the server asks.
    let falt = tls::get_alt_name_information(&mut s.connection, HashSet::new());
    let mut out = c.output(1024);
    out.write_str("hello").get().unwrap();
    out.flush().get().unwrap();
    let all = falt.get().unwrap();
    let mut seen: Vec<(SubjectAltNameType, String)> =
        all.iter().map(|n| (n.type_, string_of(&n.value))).collect();
    seen.sort_by_key(|(t, v)| (*t as i32, v.clone()));
    let has = |t: SubjectAltNameType, v: &str| seen.iter().any(|(st, sv)| *st == t && sv == v);
    assert!(has(SubjectAltNameType::dnsname, "client.example"), "{seen:?}");
    assert!(has(SubjectAltNameType::rfc822name, "c@example.org"), "{seen:?}");
    assert!(has(SubjectAltNameType::uri, "spiffe://example/cov4"), "{seen:?}");
    let v4 = format!("{}", InetAddress::from_sstring(&Sstring::from("127.0.0.1")).unwrap());
    let v6 = format!("{}", InetAddress::from_sstring(&Sstring::from("::1")).unwrap());
    assert!(has(SubjectAltNameType::ipaddress, &v4), "{seen:?}");
    assert!(has(SubjectAltNameType::ipaddress, &v6), "{seen:?}");
    assert!(seen.iter().any(|(t, _)| *t == SubjectAltNameType::othername), "{seen:?}");
    assert!(
        seen.iter().any(|(t, v)| *t == SubjectAltNameType::dn && v.contains("dirname")),
        "{seen:?}"
    );

    // A filter keeps only the kinds asked for.
    let only: HashSet<SubjectAltNameType> =
        [SubjectAltNameType::uri, SubjectAltNameType::ipaddress].into_iter().collect();
    let some = tls::get_alt_name_information(&mut s.connection, only.clone()).get().unwrap();
    assert_eq!(some.len(), 3, "one URI and two addresses");
    assert!(some.iter().all(|n| only.contains(&n.type_)));

    // The distinguished names of the peer on each end.
    let dn = tls::get_dn_information(&mut s.connection).get().unwrap().expect("the client's DN");
    assert!(dn.subject.as_str().contains("client"), "{}", dn.subject.as_str());
    assert!(dn.issuer.as_str().contains("cov4-ca"), "{}", dn.issuer.as_str());
    let dn = tls::get_dn_information(&mut c).get().unwrap().expect("the server's DN");
    assert!(dn.subject.as_str().contains("localhost"), "{}", dn.subject.as_str());

    out.close().get().unwrap();
    c.shutdown_input();
    c.shutdown_output();
    s.connection.shutdown_input();
    s.connection.shutdown_output();
    server.abort_accept();
    let _ = std::fs::remove_dir_all(&dir);
});

/// A client without a certificate, against a server that asks for one and against a server
/// that requires one: the first handshake succeeds (`session::verify` accepts a missing
/// certificate unless the client authentication is `REQUIRE`), the second fails on the server
/// and the client's connection fails too.
fn anonymous_client(auth: ClientAuth) -> (Result<(), ExceptionPtr>, Result<(), ExceptionPtr>) {
    let dir = make_certs(if auth == ClientAuth::REQUIRE { "require" } else { "request" });
    let mut sb = credentials(&dir, "srv").unwrap();
    sb.set_client_auth(auth);
    let serv = sb.build_server_credentials().unwrap();
    let mut cb = tls::CredentialsBuilder::default();
    cb.set_x509_trust_file(&p(&dir, "ca.pem"), X509CrtFormat::PEM).get().unwrap();
    let cli = cb.build_certificate_credentials().unwrap();
    let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    opts.set_fixed_cpu(this_shard_id());
    let mut server = tls::listen(serv, make_ipv4_address_ip_port(0x7f00_0001, 0), opts).unwrap();
    let addr = server.local_address().unwrap();
    let accepted = server.accept();
    let topts = TlsOptions { server_name: "localhost".into(), ..TlsOptions::default() };
    let mut c = tls::connect(cli, addr, topts).get().unwrap();
    let mut s = accepted.get().unwrap();
    let mut input = s.connection.input(Default::default());
    let read = input.read();
    let mut out = c.output(1024);
    let client = out.write_str("hello").get().and_then(|()| out.flush().get());
    let server_side = read.get().map(|b| assert_eq!(b.as_slice(), b"hello"));
    if let Err(first) = &server_side {
        // A session whose handshake failed keeps its error: the next read and the
        // information calls fail with it (`session::get`, `session::state_checked_access`).
        let again = input.read().get().err().expect("the failed session fails again");
        assert_eq!(again.to_string(), first.to_string());
        assert!(tls::get_dn_information(&mut s.connection).get().is_err());
    }
    let _ = out.close().get();
    c.shutdown_input();
    s.connection.shutdown_input();
    s.connection.shutdown_output();
    server.abort_accept();
    let _ = std::fs::remove_dir_all(&dir);
    (client, server_side)
}

// A client that trusts no authority but has verification turned off connects to the server
// (`session::verify` returns at once); the server's session holds no resumption ticket to
// hand out, which gives empty session data.
engine_thread_test_case!(a_client_with_verification_turned_off, {
    let dir = make_certs("noverify");
    let serv = credentials(&dir, "srv").unwrap().build_server_credentials().unwrap();
    let cli = tls::CredentialsBuilder::default().build_certificate_credentials().unwrap();
    // SAFETY: the test holds no other reference to the credentials across this call.
    unsafe { cli.get_mut_unchecked() }.set_enable_certificate_verification(false);
    let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    opts.set_fixed_cpu(this_shard_id());
    let mut server = tls::listen(serv, make_ipv4_address_ip_port(0x7f00_0001, 0), opts).unwrap();
    let addr = server.local_address().unwrap();
    let accepted = server.accept();
    let mut c = tls::connect(cli, addr, TlsOptions::default()).get().unwrap();
    let mut s = accepted.get().unwrap();
    let mut input = s.connection.input(Default::default());
    let read = input.read();
    let mut out = c.output(1024);
    out.write_str("hello").get().unwrap();
    out.flush().get().unwrap();
    assert_eq!(read.get().unwrap().as_slice(), b"hello");
    let data = tls::get_session_resume_data(&mut s.connection).get().unwrap();
    assert!(data.is_empty(), "a server session has no ticket to resume with");
    out.close().get().unwrap();
    c.shutdown_input();
    s.connection.shutdown_input();
    s.connection.shutdown_output();
    server.abort_accept();
    let _ = std::fs::remove_dir_all(&dir);
});

engine_thread_test_case!(a_client_without_certificate_when_one_is_asked_for, {
    let (client, server) = anonymous_client(ClientAuth::REQUEST);
    client.unwrap();
    server.unwrap();
});

engine_thread_test_case!(a_client_without_certificate_when_one_is_required, {
    let (_client, server) = anonymous_client(ClientAuth::REQUIRE);
    let e = server.expect_err("the server refuses the anonymous client");
    let text = e.to_string();
    assert!(
        text.contains("certificate") || text.contains("Certificate") || text.contains("TLS"),
        "{text}"
    );
});
