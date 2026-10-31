//! The command line of the contract `cli-base.md` (001 FR-017): defaults, every option in both
//! forms, `start` without `--insecure` until 013, the insecure-mode rules, the errors and the
//! binary's exit codes.

use std::path::PathBuf;
use std::process::Command as Process;

use teseladb_server::cli::{Command, StartOpts, USAGE, parse, parse_with};
use teseladb_server::log::{Format, Level};

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

/// `start` with an empty file behind any `--config`.
fn start(s: &str) -> StartOpts {
    match parse_with(&args(s), &|_| Ok(Vec::new())) {
        Ok(Command::Start(o)) => *o,
        other => panic!("{s}: {other:?}"),
    }
}

fn bad(s: &str) -> String {
    parse(&args(s)).expect_err(s).to_string()
}

#[test]
fn start_defaults() {
    let o = start("start --insecure");
    assert_eq!(o.dbpath, PathBuf::from("./teseladb-data"));
    assert_eq!(o.cores, None, "the machine's logical cores");
    assert_eq!(o.memory_limit, None, "three quarters of the process's memory");
    assert_eq!((o.bind_ip.as_str(), o.port), ("127.0.0.1", 27017));
    assert_eq!(o.peer_listen.to_string(), "127.0.0.1:27100");
    assert_eq!(o.admin_listen.to_string(), "127.0.0.1:27200");
    assert!(o.join.is_empty());
    assert!(o.insecure && !o.insecure_allow_remote);
    assert_eq!((o.log_format, o.log_level), (Format::Text, Level::Info));
    assert_eq!((o.config, o.logpath, o.logappend), (None, None, false));
}

#[test]
fn every_start_option_in_both_forms() {
    let o = start(
        "start --insecure --dbpath /d --cores 4 --bind_ip localhost --port=27018 \
         --peer-listen 127.0.0.1:7000 --admin-listen=127.0.0.1:7001 \
         --join 127.0.0.1:9,,127.0.0.1:10 --log-format json --config c.yaml \
         --logpath /l.log --logappend=yes --quiet",
    );
    assert_eq!(o.dbpath, PathBuf::from("/d"));
    assert_eq!(o.cores, Some(4));
    assert_eq!((o.bind_ip.as_str(), o.port), ("localhost", 27018));
    assert_eq!(o.peer_listen.to_string(), "127.0.0.1:7000");
    assert_eq!(o.admin_listen.to_string(), "127.0.0.1:7001");
    assert_eq!(o.join, ["127.0.0.1:9", "127.0.0.1:10"]);
    assert_eq!((o.log_format, o.log_level), (Format::Json, Level::Warn));
    assert_eq!(o.config, Some(PathBuf::from("c.yaml")));
    assert_eq!((o.logpath, o.logappend), (Some(PathBuf::from("/l.log")), true));
    assert_eq!(start("start --insecure --verbose").log_level, Level::Debug);
    // The node's memory (012 FR-013), in bytes or with a unit.
    assert_eq!(start("start --insecure --memory-limit 8G").memory_limit, Some(8 << 30));
    assert_eq!(start("start --insecure --memory-limit=1048576").memory_limit, Some(1 << 20));
    assert!(bad("start --insecure --memory-limit 3X").starts_with("--memory-limit: "));
    assert!(!start("start --insecure --logappend=no").logappend);
    // Addresses that are not loopback with --insecure only when asked for.
    let o =
        start("start --insecure --insecure-allow-remote --bind_ip 0.0.0.0 --peer-listen 0.0.0.0:1");
    assert!(o.insecure_allow_remote);
    assert_eq!(o.bind_ip, "0.0.0.0");
}

#[test]
fn secure_mode_is_the_default_and_listens_anywhere() {
    let o = start("start --dbpath /d");
    assert!(!o.insecure && !o.insecure_allow_remote);
    let o = start("start --bind_ip 0.0.0.0 --peer-listen 0.0.0.0:1 --admin-listen 0.0.0.0:2");
    assert_eq!(o.bind_ip, "0.0.0.0");
    assert!(bad("start --insecure-allow-remote").contains("--insecure"));
}

#[test]
fn errors_name_what_is_wrong() {
    for (cmd, what) in [
        ("bogus", "unknown subcommand `bogus`"),
        ("start --insecure --nope", "unknown option --nope"),
        ("start --insecure --cores", "--cores needs a value"),
        ("start --insecure --cores 0", "--cores: `0` (1 to 256)"),
        ("start --insecure --cores 257", "--cores"),
        ("start --insecure --cores x", "--cores"),
        ("start --insecure --port 70000", "--port"),
        ("start --insecure --peer-listen nowhere", "--peer-listen: expected <ip>:<port>"),
        ("start --insecure --admin-listen 1.2.3", "--admin-listen"),
        ("start --insecure --log-format xml", "--log-format: unknown log format `xml`"),
        ("start --insecure --bind_ip 0.0.0.0", "--bind_ip 0.0.0.0: with --insecure only loopback"),
        ("start --insecure --peer-listen 10.0.0.1:1", "--peer-listen 10.0.0.1"),
        ("start --insecure --admin-listen 10.0.0.1:1", "--admin-listen 10.0.0.1"),
        ("start --insecure-allow-remote", "--insecure-allow-remote only makes sense"),
        ("start --insecure=maybe", "--insecure: expected a boolean, got `maybe`"),
        ("start --insecure --quiet --verbose", "--quiet and --verbose"),
        ("start --insecure stray", "`teseladb start` takes no argument `stray`"),
        ("dev --nodes 0", "--nodes: `0` (1 to 7)"),
        ("dev --nodes 8", "--nodes"),
        ("dev --cores 0", "--cores"),
        ("dev --join x", "unknown option --join"),
        ("dev extra", "`teseladb dev` takes no argument `extra`"),
    ] {
        let m = bad(cmd);
        assert!(m.contains(what), "{cmd}: {m}");
    }
    assert_eq!(bad(""), USAGE);
}

#[test]
fn dev_version_and_help() {
    match parse(&args("dev")).unwrap() {
        Command::Dev(d) => assert_eq!((d.nodes, d.cores), (3, 1)),
        other => panic!("{other:?}"),
    }
    match parse(&args("dev --nodes 5 --cores=2")).unwrap() {
        Command::Dev(d) => assert_eq!((d.nodes, d.cores, d.test_commands), (5, 2, false)),
        other => panic!("{other:?}"),
    }
    match parse(&args("dev --setParameter enableTestCommands=1")).unwrap() {
        Command::Dev(d) => assert!(d.test_commands),
        other => panic!("{other:?}"),
    }
    assert!(parse(&args("dev --setParameter other=1")).is_err());
    assert_eq!(parse(&args("--version")).unwrap(), Command::Version);
    assert_eq!(parse(&args("--help")).unwrap(), Command::Help);
    assert_eq!(parse(&args("-h")).unwrap(), Command::Help);
}

fn binary(args: &[&str]) -> (i32, String, String) {
    let out = Process::new(env!("CARGO_BIN_EXE_teseladb")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_binary_answers_with_the_contract_exit_codes() {
    let (code, out, _) = binary(&["--version"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("teseladb ") && out.contains(env!("CARGO_PKG_VERSION")), "{out}");
    let (code, out, _) = binary(&["--help"]);
    assert_eq!((code, out.as_str()), (0, USAGE));
    let (code, _, err) = binary(&["start", "--insecure-allow-remote"]);
    assert_eq!(code, 2);
    assert!(err.contains("--insecure-allow-remote"), "{err}");
    let (code, _, err) = binary(&["start", "--insecure", "--cores", "0"]);
    assert_eq!(code, 2);
    assert!(err.contains("--cores"), "{err}");
    let (code, _, err) = binary(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("usage:"), "{err}");
}

/// The transactions' parameters at startup, with a `refnode` 9.0.4's messages (008 FR-010,
/// FR-020; measured on 31-10-2026).
#[test]
fn set_parameter_takes_the_transactions_limits() {
    let o = start("start --insecure");
    assert_eq!((o.transaction_lifetime_secs, o.max_concurrent_transactions), (60, 10_000));
    let o = start(
        "start --insecure --setParameter transactionLifetimeLimitSeconds=5 \
         --setParameter maxConcurrentMultiDocumentTransactions=0",
    );
    assert_eq!((o.transaction_lifetime_secs, o.max_concurrent_transactions), (5, 0));
    let name = "maxConcurrentMultiDocumentTransactions";
    for v in ["a", "2.5"] {
        assert_eq!(
            bad(&format!("start --insecure --setParameter {name}={v}")),
            format!("Bad value for parameter \"{name}\": Did not consume whole string.")
        );
    }
    assert_eq!(
        bad(&format!("start --insecure --setParameter {name}=-1")),
        format!(
            "Bad value for parameter \"{name}\": Invalid value for parameter {name}: -1 is not \
             greater than or equal to 0"
        )
    );
    let name = "transactionLifetimeLimitSeconds";
    assert_eq!(
        bad(&format!("start --insecure --setParameter {name}=0")),
        format!(
            "Bad value for parameter \"{name}\": Invalid value for parameter {name}: 0 is not \
             greater than or equal to 1"
        )
    );
}

#[test]
fn set_parameter_turns_the_test_commands_on() {
    assert!(!start("start --insecure").test_commands);
    assert!(start("start --insecure --setParameter enableTestCommands=1").test_commands);
    assert!(start("start --insecure --setParameter=enableTestCommands=true").test_commands);
    // The last one wins, as each repetition adds to the list.
    let o = start(
        "start --insecure --setParameter enableTestCommands=1 --setParameter enableTestCommands=0",
    );
    assert!(!o.test_commands);
    assert_eq!(
        bad("start --insecure --setParameter nope=1"),
        "Illegal --setParameter parameter: \"nope\""
    );
    // useInternalAuthzForX509 takes the four spellings `refnode` 9.0.4 takes (013 FR-033).
    assert!(!start("start --insecure").use_internal_authz_for_x509);
    for (v, on) in [("true", true), ("1", true), ("false", false), ("0", false)] {
        let o = start(&format!("start --insecure --setParameter useInternalAuthzForX509={v}"));
        assert_eq!(o.use_internal_authz_for_x509, on, "{v}");
    }
    for v in ["yes", "FALSE"] {
        assert_eq!(
            bad(&format!("start --insecure --setParameter useInternalAuthzForX509={v}")),
            "Bad value for parameter \"useInternalAuthzForX509\": Value is not a valid boolean"
        );
    }
    // A parameter that exists but cannot be set at startup (measured on `refnode` 9.0.4).
    assert_eq!(
        bad("start --insecure --setParameter authSchemaVersion=5"),
        "Cannot use --setParameter to set \"authSchemaVersion\" at startup"
    );
    assert!(bad("start --insecure --setParameter enableTestCommands=2").contains("boolean"));
    assert!(bad("start --insecure --setParameter enableTestCommands").contains("<name>=<value>"));
}

#[test]
fn gssapi_stops_the_start_without_a_keytab() {
    let dir = std::env::temp_dir().join(format!("teseladb-cli-gssapi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keytab = dir.join("missing.keytab");
    let out = Process::new(env!("CARGO_BIN_EXE_teseladb"))
        .args(["start", "--cores", "1", "--port", "0", "--peer-listen", "127.0.0.1:0"])
        .args(["--admin-listen", "127.0.0.1:0", "--dbpath"])
        .arg(dir.join("data"))
        .args(["--setParameter", "authenticationMechanisms=GSSAPI,SCRAM-SHA-256"])
        .env("KRB5_KTNAME", format!("FILE:{}", keytab.display()))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("gssapi could not acquire server credential: "), "{err}");
    assert!(err.contains("missing.keytab"), "{err}");
}

#[test]
fn tags_are_name_value_pairs_sorted_by_name() {
    let o = start("start --insecure --tags rack=r1,dc=east");
    let want = [("dc", "east"), ("rack", "r1")].map(|(n, v)| (n.to_owned(), v.to_owned()));
    assert_eq!(o.tags, want);
    assert!(start("start --insecure").tags.is_empty());
    assert_eq!(bad("start --insecure --tags dc"), "--tags dc: a tag is name=value");
    assert_eq!(bad("start --insecure --tags =x"), "--tags =x: a tag is name=value");
    assert_eq!(bad("start --insecure --tags dc=a,dc=b"), "--tags: tag dc given twice");
}
