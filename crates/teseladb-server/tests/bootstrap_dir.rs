//! 013 T050 (FR-035, SC-007): `teseladb start --bootstrap-dir <dir>` takes the cluster's CA and
//! the `root` password from `<dir>` on the first start. The node's certificate is signed by that
//! CA, `root` signs in with that password, a client certificate of `$external` signed by that CA
//! enters the administration port with `clusterAdmin`, files that are not 0600 stop the start, and
//! a later start with another directory changes nothing.

mod started;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use started::{Client, credentials, dir, ok, root_client, run_once, start_with};
use teseladb_server::admin_http::{Method, request_with};
use teseladb_server::pki::{CA_CERT, NODE_CERT};
use teseladb_bson::{Document, Value};
use teseladb_crypto::ec::NamedCurve;
use teseladb_tls::{CertifiedKey, Trust};
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Certificate, Name, PrivateKey, oid, pem};

const ROOT_PASSWORD: &str = "deployer-root-pw-31";

/// A CA of its own, as the deployer keeps it.
struct Ca {
    cert: Certificate,
    key: PrivateKey,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn ca(seed: u8, name: &str) -> Ca {
    let key = PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).unwrap();
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::ORGANIZATION, "teseladb"), (oid::COMMON_NAME, name)]),
        now() - 60,
        now() + 86_400 * 365,
    );
    p.ca = Some(Some(0));
    p.ext_key_usage.clear();
    let cert = generate::self_signed(&p, &key).unwrap();
    Ca { cert, key }
}

/// Writes the bootstrap files of `ca` and the root password into `d` with `mode`.
fn bootstrap(d: &Path, ca: &Ca, mode: u32) {
    std::fs::create_dir_all(d).unwrap();
    let files = [
        ("ca.pem", ca.cert.to_pem()),
        ("ca-key.pem", pem::encode(pem::label::PRIVATE_KEY, &ca.key.to_pkcs8())),
        ("root-password", format!("{ROOT_PASSWORD}\n")),
    ];
    for (name, text) in files {
        let f = d.join(name);
        std::fs::write(&f, text).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

fn disk_cert(data: &Path, file: &str) -> Certificate {
    Certificate::from_pem_all(&std::fs::read_to_string(data.join(file)).unwrap()).unwrap().remove(0)
}

/// A client certificate of `$external` signed by `ca`.
fn operator(ca: &Ca) -> CertifiedKey {
    let key = PrivateKey::generate_ec(NamedCurve::P256, &[9; 40]).unwrap();
    let subject = Name::build(&[
        (oid::ORGANIZATION, "teseladb"),
        (oid::ORGANIZATIONAL_UNIT, "deployers"),
        (oid::COMMON_NAME, "worker"),
    ]);
    let p = CertificateParams::node([6; 16], subject, now() - 60, now() + 86_400);
    let cert = generate::issue(&p, &key.public_key(), &ca.cert, &ca.key).unwrap();
    CertifiedKey { chain: vec![cert], key }
}

/// `root` of the node whose data is in `data`: its credentials file holds the bootstrap
/// password, which signs in.
fn root(port: u16, ca: &Certificate, data: &Path) -> Client {
    assert_eq!(credentials(data), ("root".to_owned(), ROOT_PASSWORD.to_owned()));
    root_client(port, ca, data)
}

#[test]
fn a_first_start_takes_the_ca_and_the_root_password_of_the_bootstrap_directory() {
    let d = dir("bootstrap");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let given = ca(3, "Deployer CA");
    let boot = d.0.join("boot");
    // Files any other user may read stop the start, and nothing is created.
    bootstrap(&boot, &given, 0o644);
    let data_arg = data.to_str().unwrap();
    let boot_arg = boot.to_str().unwrap();
    let (code, err) =
        run_once(&["start", "--dbpath", data_arg, "--bootstrap-dir", boot_arg, "--port", "0"]);
    assert_ne!(code, 0);
    assert!(err.contains("0600"), "{err}");
    assert!(!data.join(CA_CERT).exists());
    // With 0600 the node takes them: its CA is the given one and signs its certificate.
    bootstrap(&boot, &given, 0o600);
    let node = start_with(&data, &log, &["--bootstrap-dir", boot_arg]);
    assert_eq!(disk_cert(&data, CA_CERT).der(), given.cert.der());
    let leaf = disk_cert(&data, NODE_CERT);
    assert!(leaf.verify_signed_by(&given.cert.public_key).is_ok());
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(!log_text.contains(ROOT_PASSWORD), "the password stays out of the log");
    // `root` signs in with the given password and makes the deployer an operator.
    let mut c = root(node.port, &given.cert, &data);
    let roles = Value::Array(
        ["clusterAdmin", "clusterMonitor"]
            .iter()
            .map(|r| Value::Document(Document::new().with("role", *r).with("db", "admin")))
            .collect(),
    );
    let user = "CN=worker,OU=deployers,O=teseladb";
    let r = c.command("$external", Document::new().with("createUser", user).with("roles", roles));
    assert!(ok(&r), "{r:?}");
    // The deployer's certificate, signed by its own CA, enters the administration port.
    let mut client = teseladb_tls::ClientConfig::new(Trust::anchors(vec![given.cert.clone()]));
    client.identity = Some(operator(&given));
    let (status, body) =
        request_with(&node.admin, Some(Arc::new(client)), Method::Get, "/admin/nodes", b"")
            .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    // The node says which one it is, so that whoever runs it ties its address to its number (021 FR-040).
    let text: String =
        String::from_utf8_lossy(&body).chars().filter(|c| !c.is_whitespace()).collect();
    assert!(text.contains("\"self\":1"), "{text}");
    drop(node);
    // A later start with another directory changes neither the CA nor root's password.
    let other = d.0.join("other");
    bootstrap(&other, &ca(4, "Another CA"), 0o600);
    let node = start_with(&data, &log, &["--bootstrap-dir", other.to_str().unwrap()]);
    assert_eq!(disk_cert(&data, CA_CERT).der(), given.cert.der());
    root(node.port, &given.cert, &data);
}

#[test]
fn a_bootstrap_directory_without_its_files_or_with_a_key_of_another_ca_stops_the_start() {
    let d = dir("bootstrap-bad");
    let data = d.0.join("data");
    let boot = d.0.join("boot");
    let given = ca(5, "Deployer CA");
    bootstrap(&boot, &given, 0o600);
    std::fs::remove_file(boot.join("root-password")).unwrap();
    let run = |boot: &Path| {
        let data = data.to_str().unwrap();
        run_once(&["start", "--dbpath", data, "--bootstrap-dir", boot.to_str().unwrap()])
    };
    let (code, err) = run(&boot);
    assert_ne!(code, 0);
    assert!(err.contains("root-password"), "{err}");
    // The key of another CA does not go with the certificate.
    bootstrap(&boot, &given, 0o600);
    let other = ca(6, "Other CA");
    let key = boot.join("ca-key.pem");
    std::fs::write(&key, pem::encode(pem::label::PRIVATE_KEY, &other.key.to_pkcs8())).unwrap();
    let (code, err) = run(&boot);
    assert_ne!(code, 0);
    assert!(err.contains("ca-key.pem"), "{err}");
    assert!(!data.join(CA_CERT).exists());
}
