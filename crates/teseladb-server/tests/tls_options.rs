//! The TLS options of `teseladb start` (013 T013, FR-007): their checks, their names in the
//! configuration file, `getCmdLineOpts` without the password, and the port's TLS built from them
//! or from the PKI of the data directory.

use std::path::PathBuf;

use teseladb_auth::sasl::SaslMechanism;
use teseladb_server::cli::{Command, Password, TlsOpts, parse, parse_with};
use teseladb_server::pki::{self, PkiParams};
use teseladb_server::tls_port::{self, TlsMode};
use teseladb_tls::ClientAuth;
use teseladb_tls::suites::{CipherSuite, NamedGroup};
use teseladb_x509::CrlCheck;
use teseladb_x509::ext::GeneralName;

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn bad(s: &str) -> String {
    parse(&args(s)).expect_err(s).to_string()
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-tlsopts-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn options_are_checked() {
    assert!(bad("start --tlsMode requireSSL").contains("--tlsMode"));
    assert!(bad("start --insecure --tlsMode requireTLS").contains("--insecure"));
    assert!(bad("start --insecure --tlsCAFile /ca.pem").contains("--insecure"));
    // Without --insecure they configure the secure start.
    let Command::Start(o) = parse(&args("start --tlsMode requireTLS --tlsCAFile /ca.pem")).unwrap()
    else {
        panic!()
    };
    assert_eq!(o.tls.ca_file.as_deref(), Some(std::path::Path::new("/ca.pem")));
    // The configuration file names reach the same options.
    let yaml = "net:\n  tls:\n    mode: allowTLS\n    CAFile: /ca.pem\n";
    let r =
        parse_with(&args("start --insecure --config /c.yaml"), &|_| Ok(yaml.as_bytes().to_vec()));
    assert!(r.expect_err("tls with --insecure").to_string().contains("--insecure"));
    // The password never shows in the options' debug text.
    let o = TlsOpts { certificate_key_file_password: Some("s3cret".into()), ..TlsOpts::default() };
    assert!(!format!("{o:?}").contains("s3cret"));
    let _ = Command::Help;
}

#[test]
fn the_port_comes_from_the_options_or_the_pki() {
    let d = dir("port");
    let p = pki::load_or_create(
        &d,
        &PkiParams { now: 1_800_000_000, node_names: vec![GeneralName::Dns("localhost".into())] },
        &mut |b| teseladb_sys::fill_random(b),
    )
    .unwrap();
    // Defaults: requireTLS with the node certificate, and no client certificates asked for.
    let port = tls_port::build(&TlsOpts::default(), &p).unwrap();
    assert_eq!(port.mode, TlsMode::RequireTls);
    let cfg = port.config().unwrap();
    assert_eq!(cfg.identity.chain[0].der(), p.node_chain[0].der());
    assert_eq!(cfg.client_auth, ClientAuth::None);
    // A certificate key file (encrypted with the password), a CA file and a CRL file.
    let both = format!(
        "{}{}",
        std::fs::read_to_string(d.join(pki::CA_CERT)).unwrap(),
        std::fs::read_to_string(d.join(pki::CA_KEY)).unwrap()
    );
    std::fs::write(d.join("both.pem"), both).unwrap();
    let password = std::fs::read_to_string(d.join(pki::MASTER_KEY)).unwrap().trim().to_string();
    let crl =
        teseladb_x509::generate::issue_crl(&p.ca, &p.ca_key, 1_799_999_000, 1_900_000_000, 1, &[])
            .unwrap();
    std::fs::write(
        d.join("ca.crl"),
        teseladb_x509::pem::encode(teseladb_x509::pem::label::CRL, crl.der()),
    )
    .unwrap();
    let opts = TlsOpts {
        mode: Some(TlsMode::PreferTls),
        certificate_key_file: Some(d.join("both.pem")),
        public_certificate_key_file: None,
        certificate_key_file_password: Some(Password(password)),
        ca_file: Some(d.join(pki::CA_CERT)),
        crl_file: Some(d.join("ca.crl")),
        allow_without_certificates: false,
        fips_mode: false,
        ocsp_disabled: false,
    };
    let port = tls_port::build(&opts, &p).unwrap();
    assert_eq!(port.mode, TlsMode::PreferTls);
    let cfg = port.config().unwrap();
    assert_eq!(cfg.client_auth, ClientAuth::Required);
    assert_eq!(cfg.trust.crl_check, CrlCheck::Leaf);
    assert_eq!(cfg.trust.crls.len(), 1);
    assert_eq!(cfg.identity.key.public_key(), p.ca_key.public_key());
    let optional = TlsOpts { allow_without_certificates: true, ..opts.clone() };
    assert_eq!(
        tls_port::build(&optional, &p).unwrap().config().unwrap().client_auth,
        ClientAuth::Optional
    );
    // disabled has no TLS; unreadable or wrong files name the option.
    let off = TlsOpts { mode: Some(TlsMode::Disabled), ..TlsOpts::default() };
    assert!(tls_port::build(&off, &p).unwrap().config().is_none());
    let wrong_pw = TlsOpts { certificate_key_file_password: Some("nope".into()), ..opts.clone() };
    assert!(tls_port::build(&wrong_pw, &p).unwrap_err().contains("--tlsCertificateKeyFile"));
    let missing = TlsOpts { ca_file: Some(d.join("none.pem")), ..opts.clone() };
    assert!(tls_port::build(&missing, &p).unwrap_err().contains("--tlsCAFile"));
    let no_ca = TlsOpts { ca_file: Some(d.join("ca.crl")), ..opts.clone() };
    assert!(tls_port::build(&no_ca, &p).unwrap_err().contains("--tlsCAFile"));
    let bad_crl = TlsOpts { crl_file: Some(d.join(pki::CA_CERT)), ..opts.clone() };
    assert!(tls_port::build(&bad_crl, &p).unwrap_err().contains("--tlsCRLFile"));
    std::fs::write(
        d.join("garbage.pem"),
        "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    let garbage = TlsOpts { certificate_key_file: Some(d.join("garbage.pem")), ..opts.clone() };
    assert!(tls_port::build(&garbage, &p).unwrap_err().contains("--tlsCertificateKeyFile"));
    let other_key = format!(
        "{}{}",
        std::fs::read_to_string(d.join(pki::NODE_CERT)).unwrap(),
        std::fs::read_to_string(d.join(pki::CA_KEY)).unwrap()
    );
    std::fs::write(d.join("mismatch.pem"), other_key).unwrap();
    let mismatch = TlsOpts { certificate_key_file: Some(d.join("mismatch.pem")), ..opts.clone() };
    assert!(tls_port::build(&mismatch, &p).unwrap_err().contains("does not match"));
    let no_key = TlsOpts { certificate_key_file: Some(d.join(pki::CA_CERT)), ..opts };
    assert!(tls_port::build(&no_key, &p).unwrap_err().contains("--tlsCertificateKeyFile"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn get_cmd_line_opts_hides_the_password() {
    let argv = args(
        "start --tlsCertificateKeyFilePassword s3cret --tlsCertificateKeyFilePassword=s3cret --port 1",
    );
    let shown = teseladb_server::redact_password(&argv).join(" ");
    assert!(!shown.contains("s3cret"));
    assert_eq!(shown.matches("<password>").count(), 2);
    let mut o = match parse_with(&args("start --insecure"), &|_| Ok(Vec::new())) {
        Ok(Command::Start(o)) => *o,
        other => panic!("{other:?}"),
    };
    for (k, v) in [
        ("tlsMode", "requireTLS"),
        ("tlsCertificateKeyFile", "/k.pem"),
        ("tlsCertificateKeyFilePassword", "s3cret"),
        ("tlsCAFile", "/ca.pem"),
        ("tlsCRLFile", "/c.crl"),
        ("tlsAllowConnectionsWithoutCertificates", ""),
    ] {
        o.given.push((k.into(), v.into()));
    }
    let parsed = teseladb_server::parsed_options(&o);
    let text = format!("{parsed:?}");
    assert!(!text.contains("s3cret"));
    for k in [
        "mode",
        "certificateKeyFile",
        "certificateKeyFilePassword",
        "CAFile",
        "CRLFile",
        "allowConnectionsWithoutCertificates",
    ] {
        assert!(text.contains(k), "{k}");
    }
}

/// What `--tlsFIPSMode` leaves on every port and outbound client (013 T040, FR-029).
fn assert_fips(suites: &[CipherSuite], groups: &[NamedGroup]) {
    assert_eq!(suites, [CipherSuite::Aes128GcmSha256, CipherSuite::Aes256GcmSha384]);
    assert_eq!(groups, [NamedGroup::Secp256r1, NamedGroup::Secp384r1, NamedGroup::Secp521r1]);
}

#[test]
fn fips_mode_limits_every_port_and_refuses_scram_sha_1() {
    // The option, its name in the configuration file and in getCmdLineOpts.
    let Command::Start(o) = parse(&args("start --tlsFIPSMode")).unwrap() else { panic!() };
    assert!(o.tls.fips_mode);
    // SCRAM-SHA-1 leaves the default mechanisms (the reference server 8.3 and later in FIPS mode)
    // ...
    assert_eq!(o.mechanisms, [SaslMechanism::ScramSha256, SaslMechanism::X509]);
    assert!(o.ldap.fips && o.at_rest.kmip.is_none());
    let yaml = "net:\n  tls:\n    FIPSMode: true\n";
    let Ok(Command::Start(from_file)) =
        parse_with(&args("start --config /c.yaml"), &|_| Ok(yaml.as_bytes().to_vec()))
    else {
        panic!()
    };
    assert!(from_file.tls.fips_mode);
    let mut shown = *from_file;
    shown.given.push(("tlsFIPSMode".into(), String::new()));
    assert!(format!("{:?}", teseladb_server::parsed_options(&shown)).contains("FIPSMode"));
    // ... and naming it refuses to start with the reference server 9.0.4's message (measured, exit
    // code 2).
    for list in ["SCRAM-SHA-1", "SCRAM-SHA-256,SCRAM-SHA-1"] {
        let line = format!("start --tlsFIPSMode --setParameter authenticationMechanisms={list}");
        assert_eq!(bad(&line), "SCRAM-SHA-1 is not allowed in FIPS mode.");
    }
    let Command::Start(sha1) =
        parse(&args("start --setParameter authenticationMechanisms=SCRAM-SHA-1")).unwrap()
    else {
        panic!()
    };
    assert!(!sha1.tls.fips_mode && !sha1.ldap.fips);
    assert_eq!(sha1.mechanisms, [SaslMechanism::ScramSha1]);
    // The clients' port, the administration port and the peer port.
    let d = dir("fips");
    let p = pki::load_or_create(
        &d,
        &PkiParams { now: 1_800_000_000, node_names: vec![GeneralName::Dns("localhost".into())] },
        &mut |b| teseladb_sys::fill_random(b),
    )
    .unwrap();
    let fips = TlsOpts { fips_mode: true, ..TlsOpts::default() };
    for port in [tls_port::build(&fips, &p).unwrap(), tls_port::admin(&fips, &p).unwrap()] {
        let cfg = port.config().unwrap();
        assert_fips(&cfg.suites, &cfg.groups);
    }
    let peer = pki::peer_tls(&p, true);
    assert_fips(&peer.server().suites, &peer.server().groups);
    assert_fips(&peer.client().suites, &peer.client().groups);
    // Without the option every suite and group stays.
    let cfg = tls_port::build(&TlsOpts::default(), &p).unwrap();
    assert_eq!(cfg.config().unwrap().suites, CipherSuite::ALL);
    assert_eq!(pki::peer_tls(&p, false).client().groups, NamedGroup::ALL);
    let _ = std::fs::remove_dir_all(&d);
}
