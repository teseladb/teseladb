//! `teseladb_server::run` end to end without a separate process (001 FR-017): `start` with a log
//! file in JSON, replaced or appended, the development cluster, and every error with its exit
//! code (1 execution, 2 usage).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use teseladb_server::cli::USAGE;
use teseladb_server::log::{Format, Level, Logger};
use teseladb_server::node::{Node, NodeConfig, lock_dir};
use teseladb_server::{program, ready_line, run, serve, version_text};

/// A directory removed when the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("teseladb-run-{name}-{}", std::process::id()));
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

/// Runs `args`; the node stops on its third look at `stop`.
fn run_args(args: &[&str]) -> (i32, String, String) {
    let polls = AtomicUsize::new(0);
    let stop = || polls.fetch_add(1, Ordering::Relaxed) >= 2;
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err, &stop);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn start_args<'a>(data: &'a Path, extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        "start",
        "--insecure",
        "--cores",
        "1",
        "--peer-listen",
        "127.0.0.1:0",
        "--port",
        "0",
        "--admin-listen",
        "127.0.0.1:0",
        // Several nodes on one machine: none opens the HTTP port of 019.
        "--http-port",
        "0",
    ];
    a.extend(["--dbpath", data.to_str().unwrap()]);
    a.extend_from_slice(extra);
    a
}

#[test]
fn start_logs_to_a_file_replaced_or_appended() {
    let d = Dir::new("logfile");
    let data = d.0.join("data");
    let log = d.0.join("teseladb.log");
    let log_s = log.to_str().unwrap();
    let (code, out, err) =
        run_args(&start_args(&data, &["--log-format", "json", "--logpath", log_s]));
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.starts_with("{\"msg\":\"ready\",\"node_id\":1,\"cluster_id\":\""), "{out}");
    let first = fs::read_to_string(&log).unwrap();
    assert!(first.contains("\"msg\":\"insecure mode: no TLS nor authentication\""), "{first}");
    assert!(first.contains("\"msg\":\"stopping\""), "{first}");

    // The same directory again: replaced without --logappend, appended with it.
    let (code, out, _) = run_args(&start_args(&data, &["--logpath", log_s, "--quiet"]));
    assert_eq!(code, 0);
    assert!(out.starts_with("ready node_id=1 cluster_id="), "{out}");
    let quiet = fs::read_to_string(&log).unwrap();
    assert!(quiet.contains("warn insecure mode") && !quiet.contains("stopping"), "{quiet}");
    let (code, _, _) = run_args(&start_args(&data, &["--logpath", log_s, "--logappend"]));
    assert_eq!(code, 0);
    let both = fs::read_to_string(&log).unwrap();
    assert!(both.starts_with(&quiet) && both.contains("info stopping"), "{both}");
}

#[test]
fn start_errors_have_exit_code_1_or_2() {
    let d = Dir::new("errors");
    let data = d.0.join("data");
    // A log file that cannot be created.
    let (code, _, err) = run_args(&start_args(&data, &["--logpath", "/nonexistent/dir/x.log"]));
    assert_eq!(code, 1);
    assert!(err.starts_with("teseladb: --logpath /nonexistent/dir/x.log: "), "{err}");
    // A data directory another process holds.
    let lock = lock_dir(&data).unwrap();
    let (code, _, err) = run_args(&start_args(&data, &[]));
    assert_eq!(code, 1);
    assert!(err.contains("another process is using the data directory"), "{err}");
    drop(lock);
    // A data directory created with other cores.
    assert_eq!(run_args(&start_args(&data, &[])).0, 0);
    let mut two = start_args(&data, &[]);
    two[3] = "2";
    let (code, _, err) = run_args(&two);
    assert_eq!(code, 1);
    assert!(err.contains("start with --cores 1"), "{err}");
    // A configuration file that does not exist is a usage error that names it.
    let (code, _, err) = run_args(&start_args(&data, &["--config", "/nonexistent/x.yaml"]));
    assert_eq!(code, 2);
    assert!(err.starts_with("--config /nonexistent/x.yaml: "), "{err}");
    // Usage errors.
    let (code, _, err) = run_args(&["start", "--insecure-allow-remote"]);
    assert_eq!(code, 2);
    assert!(err.contains("--insecure-allow-remote"), "{err}");
    let (code, _, err) = run_args(&[]);
    assert_eq!((code, err.trim()), (2, USAGE.trim()));
}

#[test]
fn version_help_and_the_development_cluster() {
    assert_eq!(run_args(&["--version"]), (0, version_text(), String::new()));
    assert_eq!(version_text(), format!("teseladb {}\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(run_args(&["--help"]), (0, USAGE.to_owned(), String::new()));
    // On ports the system chooses: a node of this machine may already hold the default ones.
    let (code, out, _) = run_args(&["dev", "--nodes", "2", "--port", "0"]);
    assert_eq!(code, 0);
    let ready: Vec<&str> = out.lines().collect();
    assert_eq!(ready.len(), 2, "{out}");
    assert!(ready[0].starts_with("ready node_id=1 ") && ready[1].starts_with("ready node_id=2 "));
}

#[test]
fn the_ready_line_in_both_formats() {
    let id = [0xab; 16];
    let hex = "ab".repeat(16);
    assert_eq!(
        ready_line(Format::Text, 3, &id, "127.0.0.1:9"),
        format!("ready node_id=3 cluster_id={hex} peer=127.0.0.1:9\n")
    );
    assert_eq!(
        ready_line(Format::Json, 3, &id, "a\"b"),
        format!(
            "{{\"msg\":\"ready\",\"node_id\":3,\"cluster_id\":\"{hex}\",\"peer\":\"a\\\"b\"}}\n"
        )
    );
}

#[test]
fn a_node_that_is_not_ready_in_time_gives_up() {
    let node =
        Node::start(NodeConfig::default(), || Ok(teseladb_raftlog::MemDisk::new()), None).unwrap();
    let log = Logger::new(Level::Info, Format::Text, Box::new(std::io::sink()));
    let mut out = Vec::new();
    assert_eq!(serve(node, Format::Text, &log, &mut out, &|| true, Duration::ZERO), 1);
    assert!(out.is_empty());
}

#[test]
fn start_with_only_a_configuration_file() {
    let d = Dir::new("config");
    let conf = d.0.join("teseladb.yaml");
    let yaml = format!(
        "storage:\n  dbPath: {}\nnet:\n  port: 0\nteseladb:\n  insecure: true\n  cores: 1\n  \
         peerListen: 127.0.0.1:0\n  adminListen: 127.0.0.1:0\n  http:\n    port: 0\n",
        d.0.join("data").display()
    );
    fs::write(&conf, yaml).unwrap();
    let (code, out, err) = run_args(&["start", "--config", conf.to_str().unwrap()]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.starts_with("ready node_id=1 "), "{out}");
    assert!(d.0.join("data").join("node.meta").exists());
}

/// The program installs its stop handler before it runs anything; one it cannot install ends it
/// with 1 and says why (001 FR-017).
#[test]
fn the_program_stops_when_it_cannot_handle_stop_signals() {
    let args = vec!["--version".to_string()];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let ok = || Ok(());
    assert_eq!(program(&args, &mut out, &mut err, &ok, &|| true), 0);
    assert_eq!(String::from_utf8(out).unwrap(), version_text());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let refused = || Err(std::io::Error::other("refused"));
    assert_eq!(program(&args, &mut out, &mut err, &refused, &|| true), 1);
    assert!(out.is_empty());
    let err = String::from_utf8(err).unwrap();
    assert_eq!(err, "teseladb: cannot handle stop signals: refused\n");
}
