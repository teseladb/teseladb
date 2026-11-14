//! What a node reports to the diagnostic commands (006 FR-023): its options under RefDb's
//! configuration names, its in-memory logs and the facts of the host and the process.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use teseladb_bson::{Document, Value};
use teseladb_server::cli::{Command, parse};
use teseladb_server::diag::{Diag, PortStats, ProcessConfig};
use teseladb_server::log::{self, Format, Level, Logger, RAM_LOG_LINES};
use teseladb_server::parsed_options;

fn opts(line: &str) -> teseladb_server::cli::StartOpts {
    let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    match parse(&args).unwrap() {
        Command::Start(o) => *o,
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_options_given_take_refdb_names() {
    let o = opts(
        "start --insecure --port 27311 --bind_ip 127.0.0.1 --dbpath /d --maxConns 10 \
         --logpath /l --logappend --quiet --networkMessageCompressors zstd \
         --setParameter enableTestCommands=1 --cores 2",
    );
    let p = parsed_options(&o);
    let net = Document::new()
        .with("bindIp", "127.0.0.1")
        .with("maxIncomingConnections", Value::Int64(10))
        .with("compression", Document::new().with("compressors", "zstd"))
        .with("port", 27311);
    assert_eq!(p.get("net"), Some(&Value::Document(net)));
    assert_eq!(p.get("storage"), Some(&Value::Document(Document::new().with("dbPath", "/d"))));
    let log = Document::new()
        .with("destination", "file")
        .with("path", "/l")
        .with("logAppend", true)
        .with("quiet", true);
    let Some(Value::Document(got)) = p.get("systemLog") else { panic!("{p:?}") };
    for (k, v) in log.iter() {
        assert_eq!(got.get(k), Some(v), "{k}");
    }
    let teseladb = Document::new().with("cores", "2").with("insecure", true);
    assert_eq!(p.get("teseladb"), Some(&Value::Document(teseladb)));
    assert_eq!(
        p.get("setParameter"),
        Some(&Value::Document(Document::new().with("enableTestCommands", "1")))
    );
    let o = opts("start --insecure --verbose");
    let Some(Value::Document(got)) = parsed_options(&o).get("systemLog").cloned() else { panic!() };
    assert_eq!(got.get("verbosity"), Some(&Value::Int32(1)));
}

#[test]
fn the_logs_keep_their_last_lines_and_the_startup_warnings() {
    let tag = "diag-test-line";
    let logger = Logger::new(Level::Info, Format::Text, Box::new(std::io::sink()));
    logger.warn(tag, &[]);
    let (_, warnings) = log::startup_warnings();
    assert!(warnings.iter().any(|l| l.contains(tag)));
    logger.debug("not written", &[]);
    for i in 0..RAM_LOG_LINES + 5 {
        logger.info(tag, &[("i", &i)]);
    }
    let (total, lines) = log::recent();
    assert!(total >= i64::try_from(RAM_LOG_LINES + 6).unwrap());
    assert_eq!(lines.len(), RAM_LOG_LINES);
    assert!(lines.iter().all(|l| !l.ends_with('\n') && !l.contains("not written")));
    assert!(lines.last().is_some_and(|l| l.contains(&format!("i={}", RAM_LOG_LINES + 4))));
    log::mark_started();
    logger.warn("after-start-warning", &[]);
    let (_, warnings) = log::startup_warnings();
    assert!(!warnings.iter().any(|l| l.contains("after-start-warning")));
}

#[test]
fn the_facts_come_from_the_host_and_the_port() {
    let open = Arc::new(AtomicUsize::new(2));
    let process = ProcessConfig {
        argv: vec!["teseladb".into(), "start".into()],
        parsed: Document::new().with("net", Document::new().with("port", 1)).to_bytes().unwrap(),
        dbpath: Some(std::env::temp_dir()),
    };
    let d = Diag::new(process, 27311, open, 10);
    PortStats::add(&d.stats.total_created, 3);
    PortStats::add(&d.stats.requests, 4);
    let f = d.facts();
    assert_eq!(f.pid, i64::from(std::process::id()));
    assert_eq!(f.argv, ["teseladb", "start"]);
    assert_eq!(f.parsed.get("net"), Some(&Value::Document(Document::new().with("port", 1))));
    assert_eq!((f.connections.current, f.connections.available), (2, 8));
    assert_eq!((f.connections.total_created, f.network.requests), (3, 4));
    assert!(!f.host.hostname.is_empty() && f.host.num_cores >= 1 && f.host.mem_size_mb > 0);
    assert!(f.host_name().ends_with(":27311"));
    // What every write names the node with, kept from the start: the same host and port.
    assert_eq!(d.host_port(), format!("{}:{}", f.host.hostname, f.port));
    assert!(f.fs_total >= f.fs_used && f.fs_total > 0);
    assert!(f.resources.resident_bytes > 0 && f.resources.threads >= 1);
    assert!(f.now_ms >= f.start_ms);
    let none = Diag::new(ProcessConfig::default(), 27017, Arc::new(AtomicUsize::new(0)), 1);
    let f = none.facts();
    assert_eq!((f.fs_total, f.parsed.iter().count()), (0, 0));
    assert!(!f.host_name().contains(':'));
    // The keytab of GSSAPI, when the node has one.
    assert!(none.kerberos().is_empty());
    let mut keytab = vec![0x05, 0x02];
    let mut entry = 1u16.to_be_bytes().to_vec();
    for t in ["TESELA.TEST", teseladb_names::KERBEROS_SERVICE] {
        entry.extend_from_slice(&u16::try_from(t.len()).unwrap().to_be_bytes());
        entry.extend_from_slice(t.as_bytes());
    }
    entry.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 2, 0, 17, 0, 16]);
    entry.extend_from_slice(&[7; 16]);
    keytab.extend_from_slice(&i32::try_from(entry.len()).unwrap().to_be_bytes());
    keytab.extend_from_slice(&entry);
    let keys = teseladb_auth::kerberos::parse_keytab(&keytab).unwrap();
    let with = none.with_kerberos(Some(Arc::new(keys)));
    assert_eq!(with.kerberos()[0].principal, "refdb@TESELA.TEST");
}
