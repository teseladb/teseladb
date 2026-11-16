#![cfg(target_os = "linux")]
//! Tests of the TLS credential objects without a handshake: Diffie-Hellman parameters,
//! certificates from bytes and files (valid, malformed, missing), trust anchors, revocation
//! lists, key pairs and PKCS#12 bundles given in the wrong form, priority strings, client
//! authentication and session resumption settings, the credentials builder, the verification
//! callback, and the error and name texts.

#[path = "common/tls_certs.rs"]
mod tls_certs;

use std::cell::Cell;
use std::rc::Rc;

use teseladb::engine::core::future::{Exception, ExceptionPtr};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::tls::{
    AbstractCredentials, CertificateCredentials, ClientAuth, CredentialsBuilder, DhLevel,
    DhParams, DnCallback, GnutlsDatum, ServerCredentials, SessionResumeMode, SessionType,
    SubjectAltName, SubjectAltNameType, SubjectAltNameValue, TlsError, TlsOptions,
    VerificationError, X509Cert, X509CrtFormat, error_category, format_as,
};
use teseladb::engine_thread_test_case;

use tls_certs::certfile;

fn s(x: &str) -> Sstring {
    Sstring::from(x)
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(certfile(name)).expect("a certificate file")
}

fn message(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

const GARBAGE: &[u8] = b"-----BEGIN CERTIFICATE-----\nnot base64 at all\n-----END CERTIFICATE-----\n";

engine_thread_test_case!(dh_parameters_from_levels_bytes_and_files, {
    for lvl in [DhLevel::LEGACY, DhLevel::MEDIUM, DhLevel::HIGH, DhLevel::ULTRA] {
        let _ = DhParams::new(lvl);
    }
    assert_eq!(DhLevel::default(), DhLevel::LEGACY);
    assert!(DhParams::from_blob(b"not pkcs3", X509CrtFormat::PEM).is_err());
    assert!(DhParams::from_blob(b"\x30\x00", X509CrtFormat::DER).is_err());
    assert!(DhParams::from_file(&s("/nonexistent/dh.pem"), X509CrtFormat::PEM).get().is_err());
    // A file that is not a parameter file.
    assert!(DhParams::from_file(&s(&certfile("test.crt")), X509CrtFormat::PEM).get().is_err());
    // Server credentials with parameters of a level.
    if let Ok(dh) = DhParams::new(DhLevel::MEDIUM) {
        let mut sc = ServerCredentials::with_dh_params(&dh).expect("server credentials");
        sc.set_client_auth(ClientAuth::REQUIRE);
        sc.set_client_auth(ClientAuth::REQUEST);
        sc.set_client_auth(ClientAuth::NONE);
    }
});

engine_thread_test_case!(certificates_from_bytes_and_files, {
    let crt = read("test.crt");
    assert!(X509Cert::new(&crt, X509CrtFormat::PEM).is_ok());
    assert!(X509Cert::new(GARBAGE, X509CrtFormat::PEM).is_err());
    assert!(X509Cert::new(b"\x30\x03\x02\x01\x00", X509CrtFormat::DER).is_err());
    assert!(X509Cert::from_file(&s(&certfile("test.crt")), X509CrtFormat::PEM).get().is_ok());
    let e = X509Cert::from_file(&s("/nonexistent/cert.pem"), X509CrtFormat::PEM).get().err().expect("missing");
    assert!(!message(&e).is_empty());
});

engine_thread_test_case!(credentials_reject_malformed_inputs, {
    let mut c = CertificateCredentials::new();
    let ca = read("tls-ca-bundle.pem");
    c.set_x509_trust(&ca, X509CrtFormat::PEM).expect("trust");
    assert!(c.set_x509_trust(GARBAGE, X509CrtFormat::PEM).is_err());
    assert!(c.set_x509_crl(b"not a crl", X509CrtFormat::PEM).is_err());
    assert!(c.set_x509_crl(b"\x30\x00", X509CrtFormat::DER).is_err());
    let crt = read("test.crt");
    let key = read("test.key");
    c.set_x509_key(&crt, &key, X509CrtFormat::PEM).expect("key pair");
    assert!(c.set_x509_key(&crt, b"not a key", X509CrtFormat::PEM).is_err());
    assert!(c.set_x509_key(GARBAGE, &key, X509CrtFormat::PEM).is_err());
    assert!(c.set_simple_pkcs12(b"not pkcs12", X509CrtFormat::DER, &s("pw")).is_err());
    assert!(c.set_simple_pkcs12(b"not pkcs12", X509CrtFormat::PEM, &s("")).is_err());

    // Files.
    c.set_x509_trust_file(&s(&certfile("tls-ca-bundle.pem")), X509CrtFormat::PEM).get().expect("trust file");
    assert!(c.set_x509_trust_file(&s("/nonexistent/ca.pem"), X509CrtFormat::PEM).get().is_err());
    assert!(c.set_x509_crl_file(&s("/nonexistent/crl.pem"), X509CrtFormat::PEM).get().is_err());
    assert!(c.set_x509_crl_file(&s(&certfile("test.crt")), X509CrtFormat::PEM).get().is_err());
    c.set_x509_key_file(&s(&certfile("test.crt")), &s(&certfile("test.key")), X509CrtFormat::PEM)
        .get()
        .expect("key files");
    assert!(c.set_x509_key_file(&s("/nonexistent/c"), &s("/nonexistent/k"), X509CrtFormat::PEM).get().is_err());
    assert!(c.set_x509_key_file(&s(&certfile("test.crt")), &s("/nonexistent/k"), X509CrtFormat::PEM).get().is_err());
    assert!(c.set_simple_pkcs12_file(&s("/nonexistent/x.p12"), X509CrtFormat::DER, &s("")).get().is_err());
    assert!(c.set_simple_pkcs12_file(&s(&certfile("test.crt")), X509CrtFormat::DER, &s("")).get().is_err());
});

engine_thread_test_case!(credentials_settings, {
    let mut c = CertificateCredentials::default();
    c.set_priority_string(&s("NORMAL")).expect("a valid priority string");
    let e = c.set_priority_string(&s("NORMAL:+BOGUS-THING")).err().expect("invalid");
    assert!(!message(&e).is_empty());
    assert!(c.set_priority_string(&s("NOR\0MAL")).is_err());
    c.set_enable_certificate_verification(false);
    c.set_enable_certificate_verification(true);
    let calls = Rc::new(Cell::new(0));
    let k = calls.clone();
    c.set_dn_verification_callback(Some(DnCallback::new(move |_t, _s, _i| k.set(k.get() + 1))));
    c.set_dn_verification_callback(None);
    // The system store may be missing in a minimal environment; both outcomes are fine.
    let _ = c.set_system_trust().get();

    let mut sc = ServerCredentials::new().expect("server credentials");
    sc.set_client_auth(ClientAuth::REQUIRE);
    sc.set_session_resume_mode(SessionResumeMode::TLS13_SESSION_TICKET).expect("tickets");
    sc.set_session_resume_mode(SessionResumeMode::NONE).expect("none");
    sc.base.set_priority_string(&s("SECURE256")).expect("priority");
});

engine_thread_test_case!(builder_builds_and_applies, {
    let mut b = CredentialsBuilder::default();
    b.set_dh_level(DhLevel::MEDIUM);
    b.set_client_auth(ClientAuth::REQUEST);
    b.set_priority_string(&s("NORMAL"));
    b.set_session_resume_mode(SessionResumeMode::TLS13_SESSION_TICKET).expect("tickets");
    b.set_x509_trust(&read("tls-ca-bundle.pem"), X509CrtFormat::PEM).expect("trust");
    b.set_x509_key(&read("test.crt"), &read("test.key"), X509CrtFormat::PEM).expect("key");
    b.set_x509_crl(b"not a crl", X509CrtFormat::PEM).expect("kept for later");
    b.set_simple_pkcs12(b"not pkcs12", X509CrtFormat::DER, &s("")).expect("kept for later");
    // The malformed entries fail when the credentials are built.
    assert!(b.build_certificate_credentials().is_err());
    assert!(b.build_server_credentials().is_err());

    let mut good = CredentialsBuilder::default();
    good.set_x509_trust(&read("tls-ca-bundle.pem"), X509CrtFormat::PEM).expect("trust");
    good.set_x509_key(&read("test.crt"), &read("test.key"), X509CrtFormat::PEM).expect("key");
    good.set_priority_string(&s("NORMAL"));
    assert!(good.build_certificate_credentials().is_ok());
    // A server needs DH parameters, or fails without them.
    let _ = good.build_server_credentials();
    good.set_dh_level(DhLevel::LEGACY);
    good.set_client_auth(ClientAuth::REQUIRE);
    let sc = good.build_server_credentials().expect("server credentials");
    drop(sc);
    let mut c = CertificateCredentials::new();
    good.apply_to(&mut c).expect("applied");
    let _ = good.set_system_trust().get();
    good.set_x509_trust_file(&s(&certfile("tls-ca-bundle.pem")), X509CrtFormat::PEM).get().expect("file");
    assert!(good.set_x509_trust_file(&s("/nonexistent"), X509CrtFormat::PEM).get().is_err());
});

#[test]
fn texts_and_small_types() {
    let c = error_category();
    assert_eq!(c.name(), "GnuTLS");
    assert!(!c.message(-50).is_empty());
    let e = TlsError { code: -50, what: String::new() };
    assert_eq!(format!("{e}"), c.message(-50));
    let e = TlsError { code: -50, what: "ctx".to_string() };
    assert_eq!(format!("{e}"), format!("ctx: {}", c.message(-50)));
    assert_eq!(format!("{}", VerificationError("bad".to_string())), "bad");

    let names = [
        (SubjectAltNameType::dnsname, "DNS"),
        (SubjectAltNameType::rfc822name, "EMAIL"),
        (SubjectAltNameType::uri, "URI"),
        (SubjectAltNameType::ipaddress, "IP"),
        (SubjectAltNameType::othername, "OTHERNAME"),
        (SubjectAltNameType::dn, "DIRNAME"),
    ];
    for (t, n) in names {
        assert_eq!(format_as(t), n);
        assert_eq!(format!("{t}"), n);
    }
    let a = SubjectAltName { type_: SubjectAltNameType::dnsname, value: SubjectAltNameValue::String(s("x.org")) };
    assert_eq!(format!("{a}"), "DNS=x.org");
    let ip = InetAddress::from_sstring(&s("10.0.0.1")).expect("an address");
    let b = SubjectAltName { type_: SubjectAltNameType::ipaddress, value: SubjectAltNameValue::Address(ip) };
    assert_eq!(format!("{b}"), "IP=10.0.0.1");

    let o = TlsOptions::default();
    assert!(o.wait_for_eof_on_shutdown && o.verify_certificate);
    assert!(o.server_name.is_empty() && o.session_resume_data.is_empty());
    assert_eq!(ClientAuth::default(), ClientAuth::NONE);
    assert_eq!(SessionResumeMode::default(), SessionResumeMode::NONE);

    let mut d = GnutlsDatum::with_size(16);
    assert_eq!(d.0.size, 16);
    d.assign(GnutlsDatum::with_size(4));
    assert_eq!(d.0.size, 4);
    d.assign(GnutlsDatum::new());
    assert!(d.0.data.is_null());
    let _ = GnutlsDatum::default();
}

#[test]
fn verification_callbacks_may_fail() {
    let mut ok = DnCallback::new(|_t: SessionType, _s: Sstring, _i: Sstring| ());
    assert!(ok.call(SessionType::CLIENT, s("cn=a"), s("cn=b")).is_ok());
    let mut no = DnCallback::new(|t: SessionType, subject: Sstring, _i: Sstring| -> Result<(), ExceptionPtr> {
        assert_eq!(t, SessionType::SERVER);
        Err(ExceptionPtr::from(VerificationError(subject.to_string())))
    });
    let e = no.call(SessionType::SERVER, s("cn=x"), s("cn=y")).err().expect("refused");
    assert_eq!(message(&e), "cn=x");
}
