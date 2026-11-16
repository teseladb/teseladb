//! The certificate files the TLS tests read (`cert_location`).
//!
//! They are made with openssl: a server certificate and its CA for `test.example.org` and
//! for `other.apa.org`, signed from `tests/data/cert.cfg.in`, and the mutual-TLS set (a CA,
//! a server and two clients); `tls-ca-bundle.pem` and `https-server.py` are copied from
//! `tests/data`. The openssl commands run once, into `$CARGO_TARGET_TMPDIR/tls-certs-<key>`,
//! and later test processes reuse that directory; `certfile()` resolves names there.
//! `openssl` must be on the `PATH`.
//!
//! `<key>` is a hash of everything the files are made from: this module's source (the names,
//! domains, key sizes and openssl arguments) and the contents of `cert.cfg.in` and of the
//! copied files. Changing any of them selects a new directory, so certificates made with
//! older settings are never reused.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The directory with the test data files.
fn unit_source_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Marker written after every file is in place, so a later process reuses them.
const DONE_MARKER: &str = ".tls-certs-done";

/// The template of the server certificate configuration, in `tests/data`.
const CONFIG_TEMPLATE: &str = "cert.cfg.in";

/// The files copied unchanged from `tests/data` into the certificate directory.
const COPIED_FILES: [&str; 2] = ["tls-ca-bundle.pem", "https-server.py"];

/// This module's source: the certificate settings are written in it.
const GENERATOR_SOURCE: &str = include_str!("tls_certs.rs");

/// 64-bit FNV-1a offset basis and prime.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Folds `bytes`, preceded by their length (so that concatenations differ), into the FNV-1a
/// hash `h`. FNV-1a is used because its result does not depend on the toolchain.
fn fnv1a(h: u64, bytes: &[u8]) -> u64 {
    let len = (bytes.len() as u64).to_le_bytes();
    len.iter().chain(bytes).fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME))
}

/// The cache key of the generated files: a hash of the generator and of every input file.
fn settings_key() -> String {
    let source_dir = unit_source_dir();
    let inputs = std::iter::once(CONFIG_TEMPLATE).chain(COPIED_FILES);
    let h = inputs.fold(fnv1a(FNV_OFFSET, GENERATOR_SOURCE.as_bytes()), |h, f| {
        let data = fs::read(source_dir.join(f)).unwrap_or_else(|e| panic!("read {f}: {e}"));
        fnv1a(fnv1a(h, f.as_bytes()), &data)
    });
    format!("{h:016x}")
}

/// Runs `openssl args...` in `dir`; panics if it cannot run or fails.
fn openssl(dir: &Path, args: &[&str]) {
    let status = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("openssl is required for performing tests! ({e})"));
    assert!(status.success(), "openssl {:?} failed: {status}", args);
}

/// A server certificate to generate: its server name and domain.
struct Certgen<'a> {
    server: &'a str,
    domain: &'a str,
}

impl Certgen<'_> {
    /// Writes the configuration from `cert.cfg.in`, then the private key, request, CA key,
    /// CA root, signed certificate and its DER form.
    fn generate(&self, dir: &Path) {
        let name = self.server; // the file names derive from the server name
        let country = "SE";
        let state = "Stockholm";
        let locality = state;
        let org = self.domain;
        let unit = self.domain;
        let common = format!("{}.{}", self.server, self.domain);
        let email = format!("postmaster@{}", self.domain);
        let alt_email_1 = format!("alt1@{}", self.domain);
        let alt_email_2 = format!("alt2@{}", self.domain);
        let alt_ip_1 = "127.0.0.1";
        let alt_dns = common.clone();
        let width = "4096";
        let days = "3650";
        let alg = "RSA";
        let alg_opts = format!("rsa_keygen_bits:{width}");

        let privkey = format!("{name}.key");
        let req = format!("{name}.csr");
        let cert = format!("{name}.crt");
        let cert_der = format!("{name}.crt.der");
        let caprivkey = format!("ca{name}.key");
        let caroot = format!("ca{name}.pem");
        let cfg = format!("{name}.cfg");

        let template =
            fs::read_to_string(unit_source_dir().join(CONFIG_TEMPLATE)).expect("cert.cfg.in");
        let configured = [
            ("@CERT_WIDTH@", width),
            ("@CERT_PRIVKEY@", privkey.as_str()),
            ("@CERT_COUNTRY@", country),
            ("@CERT_STATE@", state),
            ("@CERT_LOCALITY@", locality),
            ("@CERT_ORG@", org),
            ("@CERT_UNIT@", unit),
            ("@CERT_COMMON@", common.as_str()),
            ("@CERT_EMAIL@", email.as_str()),
            ("@CERT_ALT_EMAIL_1@", alt_email_1.as_str()),
            ("@CERT_ALT_EMAIL_2@", alt_email_2.as_str()),
            ("@CERT_ALT_IP_1@", alt_ip_1),
            ("@CERT_ALT_DNS@", alt_dns.as_str()),
        ]
        .iter()
        .fold(template, |t, (k, v)| t.replace(k, v));
        fs::write(dir.join(&cfg), configured).expect("write cfg");

        openssl(
            dir,
            &["genpkey", "-quiet", "-out", &privkey, "-algorithm", alg, "-pkeyopt", &alg_opts],
        );
        openssl(dir, &["req", "-new", "-key", &privkey, "-out", &req, "-config", &cfg]);
        openssl(
            dir,
            &["genpkey", "-quiet", "-out", &caprivkey, "-algorithm", alg, "-pkeyopt", &alg_opts],
        );
        openssl(
            dir,
            &[
                "req", "-x509", "-new", "-nodes", "-key", &caprivkey, "-days", days, "-config",
                &cfg, "-out", &caroot,
            ],
        );
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &req,
                "-CA",
                &caroot,
                "-CAkey",
                &caprivkey,
                "-CAcreateserial",
                "-out",
                &cert,
                "-days",
                days,
                "-extensions",
                "req_ext",
                "-extfile",
                &cfg,
            ],
        );
        openssl(dir, &["x509", "-in", &cert, "-out", &cert_der, "-outform", "der"]);
    }
}

/// Makes an EC key and a certificate for `common_name`: signed by `ca` with serial `n` when
/// given, self-signed otherwise.
fn sign_cert(dir: &Path, name: &str, ca: Option<(&str, u32)>, common_name: &str) {
    let cert = format!("{name}.crt");
    let privkey = format!("{name}.key");
    let extension = "subjectAltName = IP:127.0.0.1";
    let subj = format!("/C=GB/ST=London/L=London/O=Example/OU=Core/CN={common_name}");

    openssl(dir, &["ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", &privkey]);

    match ca {
        Some((ca, serial_num)) => {
            // create a request instead of a cert if CA is specified
            let req = format!("{name}.csr");
            openssl(
                dir,
                &["req", "-new", "-sha256", "-key", &privkey, "-out", &req, "-subj", &subj],
            );

            // sign the cert with the request
            let ca_cert = format!("{ca}.crt");
            let ca_privkey = format!("{ca}.key");
            let serial = serial_num.to_string();
            openssl(
                dir,
                &[
                    "x509",
                    "-req",
                    "-days",
                    "1000",
                    "-sha256",
                    "-set_serial",
                    &serial,
                    "-in",
                    &req,
                    "-CA",
                    &ca_cert,
                    "-CAkey",
                    &ca_privkey,
                    "-out",
                    &cert,
                ],
            );
        }
        None => {
            // create a cert if CA is not specified
            openssl(
                dir,
                &[
                    "req", "-new", "-x509", "-sha256", "-key", &privkey, "-out", &cert, "-subj",
                    &subj, "-addext", extension,
                ],
            );
        }
    }
}

/// The mutual-TLS certificates: a CA, a server and two clients.
fn gen_mtls_certs(dir: &Path) {
    // create a CA cert
    let cert_ca = "mtls_ca";
    sign_cert(dir, cert_ca, None, "example.com");
    // server certificates
    sign_cert(dir, "mtls_server", Some((cert_ca, 1)), "example.com");
    // client1 certificates
    sign_cert(dir, "mtls_client1", Some((cert_ca, 2)), "client1.org");
    // client2 certificates
    sign_cert(dir, "mtls_client2", Some((cert_ca, 3)), "client2.org");
}

/// Makes every file in `dir`, unless a complete set is already there.
///
/// The files are made in a directory private to this process and then renamed to `dir`, so
/// that a test process running at the same time never sees a partial set. When another
/// process renamed its set first, that one is kept and this one is removed.
fn generate(dir: &Path) {
    if dir.join(DONE_MARKER).exists() {
        return;
    }
    let mut staging = dir.as_os_str().to_owned();
    staging.push(format!(".tmp-{}", std::process::id()));
    let staging = PathBuf::from(staging);
    // Left over by an earlier process with the same id that did not finish.
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).expect("create cert dir");
    Certgen { server: "test", domain: "example.org" }.generate(&staging);
    Certgen { server: "other", domain: "apa.org" }.generate(&staging);
    gen_mtls_certs(&staging);
    // Copied from the test data directory.
    for f in COPIED_FILES {
        fs::copy(unit_source_dir().join(f), staging.join(f))
            .unwrap_or_else(|e| panic!("copy {f}: {e}"));
    }
    fs::write(staging.join(DONE_MARKER), b"").expect("write marker");
    // A directory without the marker is an incomplete set from an interrupted run.
    if dir.exists() && !dir.join(DONE_MARKER).exists() {
        let _ = fs::remove_dir_all(dir);
    }
    if fs::rename(&staging, dir).is_err() {
        assert!(
            dir.join(DONE_MARKER).exists(),
            "cannot move the certificates to {}",
            dir.display()
        );
        let _ = fs::remove_dir_all(&staging);
    }
}

/// The directory with the generated files, made on first use.
pub fn cert_location() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("tls-certs-{}", settings_key()));
        generate(&dir);
        dir
    })
}

/// The path of the generated file `file`.
pub fn certfile(file: &str) -> String {
    cert_location().join(file).to_string_lossy().into_owned()
}
