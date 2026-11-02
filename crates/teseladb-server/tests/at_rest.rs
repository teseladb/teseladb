//! Encryption at rest of a node (013 T026, FR-015, SC-005): with `--enableEncryption` no value
//! written appears in clear in any file of the data directory (log, SST, manifests), each file has
//! its own data key, the node restarts with the same key, and refuses to start without it or with
//! another one. The options choose the cipher and where the key comes from.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_crypto::at_rest::CipherMode;
use teseladb_io::FileDisk;
use teseladb_lsm::LsmConfig;
use teseladb_meta::bootstrap::DATA_START;
use teseladb_raft::Role;
use teseladb_server::at_rest::{self, AtRestOpts};
use teseladb_server::node::{Node, NodeConfig, NodeError, lock_dir};

const READY: Duration = Duration::from_secs(20);
const ASK: Duration = Duration::from_secs(5);
const SECRET: &str = "a-secret-value-of-the-customer";

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("teseladb-atrest-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config(key: Option<&[u8; 32]>, mode: CipherMode) -> NodeConfig {
    let lsm = LsmConfig {
        memtable_bytes: 16 << 10,
        block_bytes: 4096,
        target_file_bytes: 64 << 10,
        l1_bytes: 256 << 10,
        cache_bytes: 1 << 20,
        ..LsmConfig::default()
    };
    let mut cfg = NodeConfig { cores: Some(2), lsm, ..NodeConfig::default() };
    let encryption = key.map(|k| at_rest::ring(k, mode));
    cfg.lsm.encryption = encryption.clone();
    cfg.storage.encryption = encryption;
    cfg
}

fn start(dir: &Dir, cfg: NodeConfig) -> Result<Node<FileDisk>, NodeError> {
    let path = dir.0.clone();
    Node::start(cfg, || FileDisk::open(&path), Some(lock_dir(&dir.0)?))
}

fn key(i: u32) -> Vec<u8> {
    [&DATA_START[..], format!("doc-{i:05}").as_bytes()].concat()
}

fn put(n: &Node<FileDisk>, i: u32) {
    let end = Instant::now() + READY;
    let value = format!("{SECRET}-{i:05}");
    while n.put(&key(i), value.as_bytes(), ASK).is_err() {
        assert!(Instant::now() < end, "write {i}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_flushed(n: &Node<FileDisk>) {
    let end = Instant::now() + READY;
    loop {
        let s = n.data_status(ASK).expect("the range's core answers");
        if s.role == Some(Role::Leader) && s.flushed > 0 {
            return;
        }
        assert!(Instant::now() < end, "never flushed: {s:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Every file under `dir` with its bytes.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                todo.push(p);
            } else {
                let bytes = fs::read(&p).unwrap();
                out.push((p, bytes));
            }
        }
    }
    out
}

#[test]
fn no_value_shows_in_clear_and_the_node_needs_its_key() {
    for mode in [CipherMode::Cbc, CipherMode::Gcm] {
        let dir = Dir::new(mode.as_str());
        let k = [42u8; 32];
        let n = start(&dir, config(Some(&k), mode)).unwrap();
        assert!(n.wait_ready(READY));
        for i in 0..300 {
            put(&n, i);
        }
        wait_flushed(&n);
        n.stop();
        let all = files(&dir.0);
        assert!(all.iter().any(|(p, _)| p.extension().is_some_and(|e| e == "sst")), "{all:?}");
        for (p, bytes) in &all {
            let leak = bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes());
            assert!(!leak, "{mode:?}: a value in clear in {}", p.display());
        }
        // The same key: everything is back.
        let n = start(&dir, config(Some(&k), mode)).unwrap();
        assert!(n.wait_ready(READY));
        let want = format!("{SECRET}-00007").into_bytes();
        let end = Instant::now() + READY;
        while n.get(&key(7), ASK).ok().flatten() != Some(want.clone()) {
            assert!(Instant::now() < end, "document 7 after the restart");
            thread::sleep(Duration::from_millis(10));
        }
        n.stop();
        // Without the key, or with another one, the node does not start.
        assert!(start(&dir, config(None, mode)).is_err(), "{mode:?} without a key");
        assert!(start(&dir, config(Some(&[43; 32]), mode)).is_err(), "{mode:?} another key");
    }
}

#[test]
fn the_options_choose_the_cipher_and_the_key() {
    let dir = Dir::new("options");
    let master = [5u8; 32];
    // Off: no ring.
    assert!(at_rest::from_opts(&AtRestOpts::default(), None, Some(&master)).unwrap().is_none());
    // On with the cluster's master key; the default cipher is AES256-CBC.
    let on = AtRestOpts { enable: true, ..AtRestOpts::default() };
    let ring = at_rest::from_opts(&on, None, Some(&master)).unwrap().unwrap();
    assert_eq!(ring.ring().mode(), CipherMode::Cbc);
    // A node without a PKI (--insecure) needs a key file.
    let e = at_rest::from_opts(&on, None, None).unwrap_err();
    assert!(e.contains("--encryptionKeyFile"), "{e}");
    // A key file: 32 bytes in Base64, readable only by its owner.
    let file = dir.0.join("key");
    fs::write(&file, "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWY=\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let with_file =
        AtRestOpts { key_file: Some(file.clone()), mode: CipherMode::Gcm, ..on.clone() };
    let ring = at_rest::from_opts(&with_file, None, None).unwrap().unwrap();
    assert_eq!(ring.ring().mode(), CipherMode::Gcm);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let e = at_rest::from_opts(&with_file, None, None).unwrap_err();
    assert!(e.contains("permissions"), "{e}");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&file, "not base64 !").unwrap();
    assert!(at_rest::from_opts(&with_file, None, None).unwrap_err().contains("Base64"));
    // A directory passes the permission check and does not read.
    let as_dir = dir.0.join("keydir");
    fs::create_dir(&as_dir).unwrap();
    fs::set_permissions(&as_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let dir_opts = AtRestOpts { key_file: Some(as_dir), ..with_file.clone() };
    assert!(at_rest::from_opts(&dir_opts, None, None).unwrap_err().contains("keydir"));
    fs::write(&file, "c2hvcnQ=").unwrap();
    assert!(at_rest::from_opts(&with_file, None, None).unwrap_err().contains("32 bytes"));
    let missing = AtRestOpts { key_file: Some(dir.0.join("none")), ..on };
    assert!(at_rest::from_opts(&missing, None, None).unwrap_err().contains("none"));
    // The key never appears in what is printed.
    assert!(!format!("{with_file:?}").contains("QUJD"));
}

#[test]
fn the_command_line_and_the_configuration_file_take_the_options() {
    use teseladb_server::cli::{Command, parse, parse_with};
    let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    let start = |s: &str| match parse(&args(s)) {
        Ok(Command::Start(o)) => *o,
        other => panic!("{s}: {other:?}"),
    };
    let o = start("start --enableEncryption --encryptionCipherMode AES256-GCM");
    assert_eq!((o.at_rest.enable, o.at_rest.mode), (true, CipherMode::Gcm));
    let o = start("start --enableEncryption --encryptionKeyFile /k");
    assert_eq!(o.at_rest.key_file.as_deref(), Some(Path::new("/k")));
    assert_eq!(o.at_rest.mode, CipherMode::Cbc);
    assert!(!start("start").at_rest.enable);
    for bad in [
        "start --encryptionKeyFile /k",
        "start --encryptionCipherMode AES256-GCM",
        "start --enableEncryption --encryptionCipherMode AES128-CBC",
    ] {
        assert!(parse(&args(bad)).is_err(), "{bad}");
    }
    // KMIP: its options, its defaults, and a key file with it refused as `refnode` refuses it.
    let o = start(
        "start --enableEncryption --kmipServerName a,b --kmipPort 5697 --kmipServerCAFile /ca \
         --kmipClientCertificateFile /c --kmipClientCertificatePassword pw --kmipKeyIdentifier 7 \
         --kmipRotateMasterKey --kmipConnectRetries 2 --kmipConnectTimeoutMS 900 \
         --kmipKeyStatePollingSeconds -1 --kmipActivateKeys false --kmipUseLegacyProtocol true",
    );
    let k = o.at_rest.kmip.clone().unwrap();
    assert_eq!(k.servers, ["a", "b"]);
    assert_eq!((k.port, k.connect_retries, k.polling), (5697, 2, None));
    assert_eq!(k.connect_timeout, std::time::Duration::from_millis(900));
    assert!(k.rotate && !k.activate_keys && k.legacy);
    assert_eq!(k.key_identifier.as_deref(), Some("7"));
    assert_eq!(k.client_certificate_password.unwrap().0, "pw");
    assert_eq!(format!("{:?}", o.at_rest).matches("pw").count(), 0);
    let k = start("start --enableEncryption --kmipServerName a --kmipKeyStatePollingSeconds 5")
        .at_rest
        .kmip
        .unwrap();
    assert_eq!(k.polling, Some(std::time::Duration::from_secs(5)));
    let d = start("start --enableEncryption --kmipServerName a").at_rest.kmip.unwrap();
    assert_eq!(
        d,
        teseladb_server::kmip::KmipOpts { servers: vec!["a".into()], ..Default::default() }
    );
    // Without --enableEncryption the KMIP options are taken and not used, as in `refnode`.
    assert!(start("start --kmipServerName a").at_rest.kmip.is_none());
    for bad in [
        "start --enableEncryption --encryptionKeyFile /k --kmipServerName a",
        "start --enableEncryption --kmipServerName a --kmipKeyStatePollingSeconds -2",
        "start --enableEncryption --kmipServerName a --kmipPort x",
        "start --enableEncryption --kmipServerName a --kmipActivateKeys maybe",
    ] {
        assert!(parse(&args(bad)).is_err(), "{bad}");
    }
    let e = format!(
        "{:?}",
        parse(&args("start --enableEncryption --encryptionKeyFile /k --kmipServerName a"))
    );
    assert!(e.contains("Must specify either an encryption key file or a KMIP server"), "{e}");
    let yaml = "security:\n  enableEncryption: true\n  kmip:\n    serverName: k\n    port: 7\n";
    let Ok(Command::Start(o)) =
        parse_with(&args("start --config /c.yaml"), &|_| Ok(yaml.as_bytes().to_vec()))
    else {
        panic!()
    };
    let k = o.at_rest.kmip.clone().unwrap();
    assert_eq!((k.servers, k.port), (vec!["k".to_owned()], 7));
    let shown = format!("{:?}", teseladb_server::parsed_options(&o));
    assert!(shown.contains("kmip") && shown.contains("serverName"), "{shown}");
    let yaml = "security:\n  enableEncryption: true\n  encryptionCipherMode: AES256-GCM\n";
    let Ok(Command::Start(o)) =
        parse_with(&args("start --config /c.yaml"), &|_| Ok(yaml.as_bytes().to_vec()))
    else {
        panic!()
    };
    assert_eq!((o.at_rest.enable, o.at_rest.mode), (true, CipherMode::Gcm));
    // getCmdLineOpts names them as `refnode` does.
    let mut o = start("start --insecure");
    for (k, v) in [
        ("enableEncryption", ""),
        ("encryptionKeyFile", "/k"),
        ("encryptionCipherMode", "AES256-GCM"),
        ("kmipServerName", "k"),
        ("kmipPort", "7"),
        ("kmipActivateKeys", "false"),
        ("kmipClientCertificatePassword", "secret-pw"),
        ("kmipUseLegacyProtocol", ""),
    ] {
        o.given.push((k.into(), v.into()));
    }
    let text = format!("{:?}", teseladb_server::parsed_options(&o));
    for k in ["enableEncryption", "encryptionKeyFile", "encryptionCipherMode"] {
        assert!(text.contains(k), "{k}");
    }
    assert!(!text.contains("secret-pw") && text.contains("<password>"), "{text}");
    assert!(text.contains("port\": 7") || text.contains("Int32(7)"), "{text}");
}

#[test]
fn an_insecure_start_with_encryption_needs_a_key_file() {
    let dir = Dir::new("insecure");
    let data = dir.0.join("data");
    let argv: Vec<String> = [
        "start",
        "--insecure",
        "--enableEncryption",
        "--port",
        "0",
        "--admin-listen",
        "127.0.0.1:0",
        // Several nodes on one machine: none opens the HTTP port of 019.
        "--http-port",
        "0",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(["--dbpath".to_owned(), data.display().to_string()])
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    assert_eq!(code, 1);
    assert!(String::from_utf8_lossy(&err).contains("--encryptionKeyFile"));
}
