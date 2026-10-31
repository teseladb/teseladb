//! The PKI of the first start (013 T011, FR-004): CA, node certificate, master key and root
//! credentials in the data directory with mode 0600 for secrets, the same material on the next
//! start, refusal when a piece is missing or does not fit, and no secret in what is shown.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use teseladb_server::pki::{self, PkiError, PkiParams};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::{CrlCheck, Purpose, VerifyOptions, verify_chain, verify_hostname};

const NOW: i64 = 1_800_000_000;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-pki-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn params() -> PkiParams {
    PkiParams {
        now: NOW,
        node_names: vec![GeneralName::Dns("localhost".into()), GeneralName::Ip(vec![127, 0, 0, 1])],
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn first_start_creates_everything_and_the_next_one_reloads_it() {
    let d = dir("first");
    let first = pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap();
    assert!(first.created);
    for (file, want) in [
        (pki::CA_CERT, 0o644),
        (pki::NODE_CERT, 0o644),
        (pki::CA_KEY, 0o600),
        (pki::NODE_KEY, 0o600),
        (pki::MASTER_KEY, 0o600),
        (pki::ROOT_CREDENTIALS, 0o600),
    ] {
        assert_eq!(mode(&d.join(file)), want, "{file}");
    }
    assert_eq!(mode(&d.join(pki::PKI_DIR)), 0o700);
    // The node certificate chains to the CA for servers and clients, with its names.
    let anchors = [first.ca.clone()];
    for purpose in [Purpose::ServerAuth, Purpose::ClientAuth] {
        let opts = VerifyOptions { purpose, crl_check: CrlCheck::None, ..VerifyOptions::new(NOW) };
        assert!(verify_chain(&first.node_chain[0], &[], &anchors, &opts).is_ok());
    }
    assert_eq!(verify_hostname(&first.node_chain[0], "localhost"), Ok(()));
    assert_eq!(verify_hostname(&first.node_chain[0], "127.0.0.1"), Ok(()));
    assert_eq!(first.node_key.public_key(), first.node_chain[0].public_key);
    // The CA key on disk is encrypted with the master key.
    let ca_key = std::fs::read_to_string(d.join(pki::CA_KEY)).unwrap();
    assert!(ca_key.contains("ENCRYPTED PRIVATE KEY"));
    // What the caller shows names the credentials file and holds no secret.
    let shown = format!("{first:?} {}", first.summary());
    let creds = std::fs::read_to_string(d.join(pki::ROOT_CREDENTIALS)).unwrap();
    let password = creds.lines().find_map(|l| l.strip_prefix("password=")).unwrap();
    assert!(password.len() >= 32);
    assert!(!shown.contains(password));
    assert!(first.summary().contains(&d.join(pki::ROOT_CREDENTIALS).display().to_string()));
    assert!(!shown.contains(&ca_key));

    let again =
        pki::load_or_create(&d, &params(), &mut |_| panic!("no randomness on reload")).unwrap();
    assert!(!again.created);
    assert_eq!(again.ca.der(), first.ca.der());
    assert_eq!(again.node_chain[0].der(), first.node_chain[0].der());
    assert_eq!(again.ca_key.public_key(), first.ca_key.public_key());
    assert_eq!(std::fs::read_to_string(d.join(pki::ROOT_CREDENTIALS)).unwrap(), creds);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_missing_or_foreign_piece_stops_the_start() {
    let d = dir("missing");
    pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap();
    // Without the master key the node cannot open the CA key: it says so.
    let master = std::fs::read(d.join(pki::MASTER_KEY)).unwrap();
    std::fs::remove_file(d.join(pki::MASTER_KEY)).unwrap();
    let e = pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err();
    assert_eq!(e, PkiError::Missing(pki::MASTER_KEY));
    assert!(e.to_string().contains(pki::MASTER_KEY));
    // Another master key does not open the CA key.
    std::fs::write(d.join(pki::MASTER_KEY), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n")
        .unwrap();
    assert_eq!(
        pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
        PkiError::WrongMasterKey
    );
    std::fs::write(d.join(pki::MASTER_KEY), "not base64\n").unwrap();
    assert_eq!(
        pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
        PkiError::Invalid(pki::MASTER_KEY)
    );
    std::fs::write(d.join(pki::MASTER_KEY), &master).unwrap();
    // A node certificate that the CA did not sign.
    let node = std::fs::read_to_string(d.join(pki::NODE_CERT)).unwrap();
    std::fs::write(d.join(pki::NODE_CERT), std::fs::read_to_string(d.join(pki::CA_CERT)).unwrap())
        .unwrap();
    assert_eq!(
        pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
        PkiError::Invalid(pki::NODE_CERT)
    );
    std::fs::write(d.join(pki::NODE_CERT), "garbage").unwrap();
    assert_eq!(
        pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
        PkiError::Invalid(pki::NODE_CERT)
    );
    std::fs::write(d.join(pki::NODE_CERT), node).unwrap();
    let ok = pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap();
    assert!(ok.summary().starts_with("PKI loaded from"));
    // Pieces of another node's PKI: its CA certificate, its node key; and a directory where a
    // file should be.
    let other = dir("other");
    pki::load_or_create(&other, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap();
    for file in [pki::CA_CERT, pki::NODE_KEY] {
        let mine = std::fs::read(d.join(file)).unwrap();
        std::fs::copy(other.join(file), d.join(file)).unwrap();
        let want = if file == pki::CA_CERT {
            PkiError::Invalid(pki::CA_KEY)
        } else {
            PkiError::Invalid(pki::NODE_KEY)
        };
        assert_eq!(
            pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
            want
        );
        std::fs::write(d.join(file), mine).unwrap();
    }
    // A node certificate from another CA, and one with this CA's name but another key.
    let mine = std::fs::read(d.join(pki::NODE_CERT)).unwrap();
    std::fs::copy(other.join(pki::NODE_CERT), d.join(pki::NODE_CERT)).unwrap();
    assert_eq!(
        pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
        PkiError::Invalid(pki::NODE_CERT)
    );
    let real = pki::load_or_create(&other, &params(), &mut |_| Ok(())).unwrap();
    let ca_here = teseladb_x509::Certificate::from_pem_all(
        &std::fs::read_to_string(d.join(pki::CA_CERT)).unwrap(),
    )
    .unwrap()
    .remove(0);
    let mut fake_ca = real.ca.clone();
    fake_ca.subject = ca_here.subject.clone();
    let p = teseladb_x509::generate::CertificateParams::node(
        [9; 16],
        teseladb_x509::Name::default(),
        NOW - 60,
        NOW + 86_400,
    );
    let mut p = p;
    p.alt_names = vec![GeneralName::Dns("x".into())];
    let forged =
        teseladb_x509::generate::issue(&p, &real.node_key.public_key(), &fake_ca, &real.ca_key)
            .unwrap();
    std::fs::write(d.join(pki::NODE_CERT), forged.to_pem()).unwrap();
    assert_eq!(
        pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
        PkiError::Invalid(pki::NODE_CERT)
    );
    std::fs::write(d.join(pki::NODE_CERT), mine).unwrap();
    let creds = d.join(pki::ROOT_CREDENTIALS);
    std::fs::remove_file(d.join(pki::NODE_KEY)).unwrap();
    std::fs::create_dir(d.join(pki::NODE_KEY)).unwrap();
    assert_eq!(
        pki::load_or_create(&d, &params(), &mut |b| teseladb_sys::fill_random(b)).unwrap_err(),
        PkiError::Invalid(pki::NODE_KEY)
    );
    assert!(creds.exists());
    let _ = std::fs::remove_dir_all(&other);
    // A failing random source on the first start.
    let empty = dir("norandom");
    let e =
        pki::load_or_create(&empty, &params(), &mut |_| Err(std::io::Error::other("no entropy")))
            .unwrap_err();
    assert!(matches!(e, PkiError::Io(_)));
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn the_root_user_comes_from_the_credentials_file() {
    use teseladb_auth::scram::Mechanism;
    use teseladb_auth::users::{RoleName, User};
    let d = dir("root-user");
    let mut random = |b: &mut [u8]| {
        b.iter_mut().enumerate().for_each(|(i, x)| *x = i as u8);
        Ok(())
    };
    assert_eq!(pki::root_user(&d, &mut random), Ok(None));
    // The password and the salts come from different bytes, as from the system's source.
    let mut other = |b: &mut [u8]| {
        b.iter_mut().enumerate().for_each(|(i, x)| *x = (i as u8).wrapping_mul(37) ^ 0x5a);
        Ok(())
    };
    pki::load_or_create(&d, &params(), &mut other).unwrap();
    let doc = pki::root_user(&d, &mut random).unwrap().unwrap();
    let raw = teseladb_bson::RawDoc::from_bytes(&doc).unwrap();
    let user = User::from_document(&teseladb_bson::Document::from(raw)).unwrap();
    assert_eq!((user.name.user.as_str(), user.name.db.as_str()), (pki::ROOT_USER, "admin"));
    assert_eq!(user.roles, vec![RoleName::new("root", "admin")]);
    assert!(user.credentials(Mechanism::ScramSha1).is_some());
    assert!(user.credentials(Mechanism::ScramSha256).is_some());
    assert_eq!(user.user_id.map(|id| id[6] >> 4), Some(4), "a version 4 UUID");
    let text = std::fs::read_to_string(d.join(pki::ROOT_CREDENTIALS)).unwrap();
    let password = text.lines().find_map(|l| l.strip_prefix("password=")).unwrap();
    assert!(!String::from_utf8_lossy(&doc).contains(password), "no password in the document");
    // A file without the password, or one that cannot be read, stops the start.
    std::fs::write(d.join(pki::ROOT_CREDENTIALS), "user=root\n").unwrap();
    assert_eq!(pki::root_user(&d, &mut random), Err(PkiError::Invalid(pki::ROOT_CREDENTIALS)));
    std::fs::remove_file(d.join(pki::ROOT_CREDENTIALS)).unwrap();
    std::fs::create_dir(d.join(pki::ROOT_CREDENTIALS)).unwrap();
    assert!(matches!(pki::root_user(&d, &mut random), Err(PkiError::Io(_))));
    let mut failing = |_: &mut [u8]| Err(std::io::Error::other("no entropy"));
    std::fs::remove_dir(d.join(pki::ROOT_CREDENTIALS)).unwrap();
    std::fs::write(d.join(pki::ROOT_CREDENTIALS), "user=root\npassword=p\n").unwrap();
    assert!(matches!(pki::root_user(&d, &mut failing), Err(PkiError::Io(_))));
    let _ = std::fs::remove_dir_all(&d);
}

fn random(b: &mut [u8]) -> std::io::Result<()> {
    teseladb_sys::fill_random(b)
}

#[test]
fn a_renewal_replaces_the_node_pair_and_survives_a_crash_at_any_step() {
    let d = dir("renew");
    let first = pki::load_or_create(&d, &params(), &mut random).unwrap();
    pki::renew_node(&d, params().node_names, NOW + 10, &mut random).unwrap();
    let renewed = pki::load(&d).unwrap();
    assert_ne!(renewed.node_key.public_key(), first.node_key.public_key());
    assert_eq!(renewed.ca.der(), first.ca.der());
    assert_eq!(verify_hostname(&renewed.node_chain[0], "localhost"), Ok(()));
    assert_eq!(renewed.node_chain[0].not_before, NOW + 10 - 300);
    assert_eq!(mode(&d.join(pki::NODE_KEY)), 0o600);
    for f in [pki::NODE_KEY_NEW, pki::NODE_CERT_NEW, pki::NODE_CERT_TMP] {
        assert!(!d.join(f).exists(), "{f}");
    }
    let pair = |p: &pki::Pki| p.node_key.public_key();
    let copy = |from: &str, to: &str| std::fs::copy(d.join(from), d.join(to)).unwrap();
    // Crash before the certificate was committed: the old pair stays, the leftovers go.
    let other = dir("renew-other");
    pki::load_or_create(&other, &params(), &mut random).unwrap();
    std::fs::copy(other.join(pki::NODE_KEY), d.join(pki::NODE_KEY_NEW)).unwrap();
    std::fs::write(d.join(pki::NODE_CERT_TMP), b"half a certif").unwrap();
    assert_eq!(pair(&pki::load(&d).unwrap()), pair(&renewed));
    assert!(!d.join(pki::NODE_KEY_NEW).exists() && !d.join(pki::NODE_CERT_TMP).exists());
    // Crash after the commit, before or between the renames: the new pair takes over.
    pki::renew_node(&d, params().node_names, NOW, &mut random).unwrap();
    let now = pki::load(&d).unwrap();
    copy(pki::NODE_KEY, pki::NODE_KEY_NEW);
    copy(pki::NODE_CERT, pki::NODE_CERT_NEW);
    std::fs::copy(other.join(pki::NODE_KEY), d.join(pki::NODE_KEY)).unwrap();
    assert_eq!(pair(&pki::load(&d).unwrap()), pair(&now));
    copy(pki::NODE_CERT, pki::NODE_CERT_NEW);
    assert_eq!(pair(&pki::load(&d).unwrap()), pair(&now));
    assert!(!d.join(pki::NODE_CERT_NEW).exists());
    // The leftovers of an unfinished renewal do not stop the next one.
    std::fs::write(d.join(pki::NODE_KEY_NEW), b"x").unwrap();
    std::fs::write(d.join(pki::NODE_CERT_TMP), b"x").unwrap();
    pki::renew_node(&d, params().node_names, NOW, &mut random).unwrap();
    assert!(pki::load(&d).is_ok());
    // A leftover that cannot be removed (a directory in its place) stops the load.
    std::fs::create_dir_all(d.join(pki::NODE_CERT_TMP).join("in")).unwrap();
    assert!(matches!(pki::load(&d), Err(PkiError::Io(_))));
    std::fs::remove_dir_all(d.join(pki::NODE_CERT_TMP)).unwrap();
    // Without a PKI nothing is renewed.
    let empty = dir("renew-empty");
    assert_eq!(
        pki::renew_node(&empty, params().node_names, NOW, &mut random),
        Err(PkiError::Missing(pki::MASTER_KEY))
    );
    // A random source that fails stops the renewal before anything is written.
    let mut broken = |_: &mut [u8]| Err(std::io::Error::other("no entropy"));
    assert!(matches!(
        pki::renew_node(&d, params().node_names, NOW, &mut broken),
        Err(PkiError::Io(_))
    ));
    for x in [d, other, empty] {
        let _ = std::fs::remove_dir_all(x);
    }
}

/// Between nodes a node sends its chain with the CA, once even when its certificate file
/// already holds the CA.
#[test]
fn the_peer_chain_carries_the_ca_once() {
    let d = dir("peer-chain");
    let p = pki::load_or_create(&d, &params(), &mut random).unwrap();
    let chain = |p: &pki::Pki| pki::peer_tls(p, false).server().identity.chain.clone();
    let served = chain(&p);
    assert_eq!(served.len(), 2);
    assert_eq!(served[1].der(), p.ca.der());
    let both = format!("{}{}", p.node_chain[0].to_pem(), p.ca.to_pem());
    std::fs::write(d.join(pki::NODE_CERT), both).unwrap();
    let p = pki::load(&d).unwrap();
    assert_eq!(p.node_chain.len(), 2);
    assert_eq!(chain(&p).len(), 2);
    let _ = std::fs::remove_dir_all(d);
}
