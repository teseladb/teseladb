//! `--keyFile` and `--clusterAuthMode` (013 T046, FR-034), as `refnode` 9.0.4 checks them
//! (measured on 31-10-2026; every fault stops the start with code 1), and the warning a node
//! gives at start for each option that relaxes the secure default (constitution IX).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use teseladb_server::cli::{Command, StartOpts, parse_with};
use teseladb_server::cluster_auth::{mode, read_key_file, relaxations};
use teseladb_server::run;
use teseladb_wire::commands::server_params::ClusterAuthMode;

/// A temporary directory of the test, removed at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir()
            .join(format!("teseladb-cluster-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    /// A file with `text` and permission bits `mode`.
    fn file(&self, name: &str, text: &str, mode: u32) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p.to_str().unwrap().to_owned()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start(line: &str) -> StartOpts {
    let args: Vec<String> = line.split_whitespace().map(String::from).collect();
    match parse_with(&args, &|_| Ok(Vec::new())) {
        Ok(Command::Start(o)) => *o,
        other => panic!("{line}: {other:?}"),
    }
}

const KEY: &str = "abcdefghijklmnopqrstuvwxyz0123456789";

#[test]
fn key_files_are_checked_as_refnode_checks_them() {
    let d = Dir::new("keys");
    let good = d.file("good", KEY, 0o600);
    assert_eq!(read_key_file(Path::new(&good)), Ok(vec![KEY.to_owned()]));
    let open = d.file("open", KEY, 0o644);
    assert_eq!(read_key_file(Path::new(&open)), Err(format!("permissions on {open} are too open")));
    let missing = d.0.join("missing");
    let shown = missing.display();
    assert_eq!(
        read_key_file(&missing),
        Err(format!("Error reading file {shown}: No such file or directory"))
    );
    let bad = d.file("bad", "abc$def ghi%", 0o600);
    let err = read_key_file(Path::new(&bad)).unwrap_err();
    assert_eq!(err, format!("invalid char in key file {bad}"));
    assert!(!err.contains("abc"), "the key never shows");
    let empty = d.file("empty", "", 0o400);
    assert_eq!(
        read_key_file(Path::new(&empty)),
        Err("Only strings and sequences are supported for YAML key files".to_owned())
    );
}

#[test]
fn the_mode_follows_the_options() {
    let d = Dir::new("mode");
    let good = d.file("good", KEY, 0o600);
    assert_eq!(mode(&start("start")), Ok(ClusterAuthMode::Undefined));
    assert_eq!(mode(&start(&format!("start --keyFile {good}"))), Ok(ClusterAuthMode::KeyFile));
    let x509 = start(&format!("start --keyFile {good} --clusterAuthMode sendX509"));
    assert_eq!(mode(&x509), Ok(ClusterAuthMode::SendX509));
    assert_eq!(mode(&start("start --clusterAuthMode x509")), Ok(ClusterAuthMode::X509));
    let expects =
        "security.clusterAuthMode expects one of 'keyFile', 'sendKeyFile', 'sendX509', or 'X509'";
    assert_eq!(mode(&start("start --clusterAuthMode bogus")), Err(expects.to_owned()));
    assert_eq!(mode(&start("start --clusterAuthMode X509")), Err(expects.to_owned()));
    let no_file = "Error reading file : No such file or directory";
    assert_eq!(mode(&start("start --clusterAuthMode keyFile")), Err(no_file.to_owned()));
    assert_eq!(mode(&start("start --clusterAuthMode sendKeyFile")), Err(no_file.to_owned()));
    let no_tls = "need to enable TLS via the tlsMode flag";
    let disabled = start("start --tlsMode disabled --clusterAuthMode x509");
    assert_eq!(mode(&disabled), Err(no_tls.to_owned()));
    let open = d.file("open", KEY, 0o640);
    assert_eq!(
        mode(&start(&format!("start --keyFile {open}"))),
        Err(format!("permissions on {open} are too open"))
    );
    // The configuration file's keys.
    let args: Vec<String> = ["start", "--config", "c.yaml"].iter().map(|s| s.to_string()).collect();
    let yaml = format!("security:\n  keyFile: {good}\n  clusterAuthMode: sendKeyFile\n");
    let Ok(Command::Start(o)) = parse_with(&args, &|_| Ok(yaml.clone().into_bytes())) else {
        panic!("the configuration file");
    };
    assert_eq!(mode(&o), Ok(ClusterAuthMode::SendKeyFile));
}

#[test]
fn the_options_that_relax_the_secure_default_are_named() {
    let d = Dir::new("relax");
    let good = d.file("good", KEY, 0o600);
    assert!(relaxations(&start("start")).is_empty());
    assert!(relaxations(&start("start --clusterAuthMode x509")).is_empty());
    let o =
        start(&format!("start --tlsMode preferTLS --keyFile {good} --clusterAuthMode sendKeyFile"));
    let named: Vec<String> = relaxations(&o).into_iter().map(|(o, _)| o).collect();
    assert_eq!(named, ["--tlsMode preferTLS", "--keyFile", "--clusterAuthMode sendKeyFile"]);
    let named: Vec<String> =
        relaxations(&start("start --tlsMode disabled")).into_iter().map(|(o, _)| o).collect();
    assert_eq!(named, ["--tlsMode disabled"]);
    let named: Vec<String> =
        relaxations(&start("start --tlsMode allowTLS")).into_iter().map(|(o, _)| o).collect();
    assert_eq!(named, ["--tlsMode allowTLS"]);
}

/// The start: a bad key file stops it with code 1; a good one, with `preferTLS`, starts and the
/// log names both options, without the key.
#[test]
fn the_start_checks_the_key_file_and_warns() {
    let d = Dir::new("start");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let run_with = |extra: &[&str]| {
        let mut args: Vec<String> = [
            "start",
            "--dbpath",
            data.to_str().unwrap(),
            "--cores",
            "1",
            "--port",
            "0",
            "--peer-listen",
            "127.0.0.1:0",
            "--admin-listen",
            "127.0.0.1:0",
            // Several nodes on one machine: none opens the HTTP port of 019.
            "--http-port",
            "0",
            "--logpath",
            log.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(extra.iter().map(|s| s.to_string()));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args, &mut out, &mut err, &|| true);
        (code, String::from_utf8(err).unwrap())
    };
    let open = d.file("open", KEY, 0o644);
    let (code, err) = run_with(&["--keyFile", &open]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("are too open"), "{err}");
    let good = d.file("good", KEY, 0o600);
    let (code, err) = run_with(&["--keyFile", &good, "--tlsMode", "preferTLS"]);
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("not secure by default") && text.contains("--keyFile"), "{text}");
    assert!(text.contains("--tlsMode preferTLS"), "{text}");
    assert!(!text.contains(KEY), "the log must not show the key");
    // `--keyFile` turns authentication on, which `--insecure` turns off.
    let (code, err) = run_with(&["--insecure", "--keyFile", &good]);
    assert_eq!(code, 2, "{err}");
}

/// Members' certificates (measured on `refnode` and `refrouter` 9.0.4 with `clusterAuthMode` x509):
/// the organization and unit of the node's certificate make a client certificate a member's.
#[test]
fn a_members_certificate_shares_the_nodes_organization() {
    use teseladb_crypto::ec::NamedCurve;
    use teseladb_server::cluster_auth::{is_member, member_subject};
    use teseladb_server::tls_port::{TlsMode, TlsPort};
    use teseladb_tls::{CertifiedKey, ServerConfig};
    use teseladb_x509::generate::{self, CertificateParams};
    use teseladb_x509::{Name, PrivateKey, oid};
    let cert = |attrs: &[(&[u8], &str)]| {
        let key = PrivateKey::generate_ec(NamedCurve::P256, &[3; 40]).unwrap();
        let p = CertificateParams::node([3; 16], Name::build(attrs), 1_700_000_000, 1_900_000_000);
        (generate::self_signed(&p, &key).unwrap(), key)
    };
    let (node, key) = cert(&[
        (oid::ORGANIZATION, "tesela"),
        (oid::ORGANIZATIONAL_UNIT, "cluster"),
        (oid::COMMON_NAME, "localhost"),
    ]);
    let port = TlsPort::new(
        TlsMode::RequireTls,
        Some(ServerConfig::new(CertifiedKey { chain: vec![node.clone()], key })),
    );
    let subject = member_subject(&port).expect("the node's certificate names an organization");
    let (member, _) = cert(&[
        (oid::ORGANIZATION, "tesela"),
        (oid::ORGANIZATIONAL_UNIT, "cluster"),
        (oid::COMMON_NAME, "member"),
    ]);
    let (app, _) = cert(&[
        (oid::ORGANIZATION, "tesela"),
        (oid::ORGANIZATIONAL_UNIT, "clients"),
        (oid::COMMON_NAME, "app"),
    ]);
    assert!(is_member(&member.subject, &subject));
    assert!(!is_member(&app.subject, &subject));
    // A node certificate without organization, unit or domain component has no members.
    let (bare, key) = cert(&[(oid::COMMON_NAME, "localhost")]);
    let bare_port = TlsPort::new(
        TlsMode::RequireTls,
        Some(ServerConfig::new(CertifiedKey { chain: vec![bare], key })),
    );
    assert!(member_subject(&bare_port).is_none());
    assert!(member_subject(&TlsPort::new(TlsMode::Disabled, None)).is_none());
}
