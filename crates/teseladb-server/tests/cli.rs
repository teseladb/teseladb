//! The command line of the contract `cli-base.md` (001 FR-017): defaults, every option in both
//! forms, `start` without `--insecure` until 013, the insecure-mode rules, the errors and the
//! binary's exit codes.

use std::path::PathBuf;
use std::process::Command as Process;
use teseladb_io::node_disk::{DirectIo, IoExecutor};

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
    assert_eq!((o.io_executor, o.direct_io), (IoExecutor::Auto, DirectIo::Auto));
    assert_eq!(o.cpu_list, None, "the CPUs the process may use");
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
    // The disk executor, O_DIRECT and the cores' CPUs (012 FR-003, FR-006, FR-007).
    let o = start("start --insecure --io-executor uring --direct-io off --cpu-list 0-2,7");
    assert_eq!((o.io_executor, o.direct_io), (IoExecutor::Uring, DirectIo::Off));
    assert_eq!(o.cpu_list, Some(vec![0, 1, 2, 7]));
    assert_eq!(start("start --insecure --io-executor threads").io_executor, IoExecutor::Threads);
    assert_eq!(start("start --insecure --direct-io on").direct_io, DirectIo::On);
    // 012 FR-025: off unless asked for.
    assert!(!start("start --insecure").io_uring_sqpoll);
    assert!(start("start --insecure --io-uring-sqpoll").io_uring_sqpoll);
    assert!(bad("start --insecure --io-executor fast").starts_with("--io-executor: "));
    assert!(bad("start --insecure --direct-io yes").starts_with("--direct-io: "));
    assert!(bad("start --insecure --cpu-list 3-1").starts_with("--cpu-list: "));
    let few = bad("start --insecure --cores 4 --cpu-list 0-1");
    assert!(few.contains("--cpu-list names 2 CPUs for 4 cores"), "{few}");
    assert!(!start("start --insecure --logappend=no").logappend);
    // Addresses that are not loopback with --insecure only when asked for.
    let o = start(
        "start --insecure --insecure-allow-remote --bind_ip 0.0.0.0 --peer-listen 0.0.0.0:1 \
         --advertise 10.0.0.5",
    );
    assert!(o.insecure_allow_remote);
    assert_eq!(o.bind_ip, "0.0.0.0");
    // The bootstrap directory of a first start (013 FR-035), on the command line and in YAML;
    // it gives the PKI, which --insecure does not create.
    assert_eq!(start("start --bootstrap-dir /boot").bootstrap_dir, Some(PathBuf::from("/boot")));
    let yaml = teseladb_server::config::yaml_args(b"teseladb:\n  bootstrapDir: /boot\n").unwrap();
    assert_eq!(yaml, ["--bootstrap-dir=/boot"]);
    let both = bad("start --insecure --bootstrap-dir /boot");
    assert!(both.contains("--bootstrap-dir"), "{both}");
}

#[test]
fn secure_mode_is_the_default_and_listens_anywhere() {
    let o = start("start --dbpath /d");
    assert!(!o.insecure && !o.insecure_allow_remote);
    let o = start(
        "start --bind_ip 0.0.0.0 --peer-listen 0.0.0.0:1 --admin-listen 0.0.0.0:2 --advertise db1",
    );
    assert_eq!(o.bind_ip, "0.0.0.0");
    assert!(bad("start --insecure-allow-remote").contains("--insecure"));
}

/// The address the other nodes call this one at (`--advertise`, `teseladb.advertise`): the IP of
/// `--peer-listen` by default, a name or an IP otherwise, and needed when `--peer-listen` listens
/// on every address, since no node can call 0.0.0.0 and the second would join with the name of
/// the first.
#[test]
fn advertise_names_the_node_to_its_peers() {
    assert_eq!(start("start --insecure").advertise, None);
    assert_eq!(
        start("start --peer-listen 0.0.0.0:1 --advertise tdb-n1").advertise.as_deref(),
        Some("tdb-n1")
    );
    assert_eq!(
        start("start --peer-listen [::]:1 --advertise=fd00::5").advertise.as_deref(),
        Some("fd00::5")
    );
    let yaml = teseladb_server::config::yaml_args(b"teseladb:\n  advertise: tdb-n1\n").unwrap();
    assert_eq!(yaml, ["--advertise=tdb-n1"]);
    for (cmd, what) in [
        (
            "start --peer-listen 0.0.0.0:27100",
            "--peer-listen 0.0.0.0:27100 listens on every address",
        ),
        ("start --peer-listen [::]:27100", "--advertise"),
        ("start --advertise=", "--advertise: expected an IP or a host name"),
        ("start --advertise tdb-n1:27100", "--advertise: the port is the one of --peer-listen"),
        ("start --advertise 0.0.0.0", "--advertise 0.0.0.0: no node can call it"),
        ("start --advertise ::", "--advertise ::: no node can call it"),
        ("start --advertise bad/name", "--advertise: expected an IP or a host name"),
    ] {
        let m = bad(cmd);
        assert!(m.contains(what), "{cmd}: {m}");
    }
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

/// A `--cpu-list` CPU the process may not run on stops the start (012 FR-006).
#[test]
fn a_cpu_the_process_may_not_use_stops_the_start() {
    let dir = std::env::temp_dir().join(format!("teseladb-cli-cpu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Process::new(env!("CARGO_BIN_EXE_teseladb"))
        .args(["start", "--insecure", "--cores", "1", "--port", "0", "--peer-listen"])
        .args(["127.0.0.1:0", "--admin-listen", "127.0.0.1:0", "--cpu-list", "1023", "--dbpath"])
        .arg(dir.join("data"))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("--cpu-list: the process may not run on CPU 1023"), "{err}");
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

/// The HTTP port of 019 (FR-002, contract `cli-base.md`): 27080 on the addresses of `--bind_ip`
/// by default, 0 closes it, and with `--insecure` its addresses are loopback too.
#[test]
fn the_http_port_and_its_addresses() {
    let o = start("start --insecure --bind_ip 127.0.0.1,localhost");
    assert_eq!((o.http_port, o.http_bind.as_str()), (27_080, "127.0.0.1,localhost"));
    let o = start("start --insecure --http-port 0 --http-bind ::1");
    assert_eq!((o.http_port, o.http_bind.as_str()), (0, "::1"));
    assert!(teseladb_server::http_config(&o).is_none());
    let o = start("start --insecure --http-port=28080 --http-bind 127.0.0.1");
    let http = teseladb_server::http_config(&o).expect("the port");
    assert_eq!(http.addrs, ["127.0.0.1:28080".parse().unwrap()]);
    assert!(http.tls.is_none());
    // The URI of `/mcp` comes from the configuration, never from a client's `Host` (FR-027).
    let advertised = teseladb_server::node_config(&o).http_advertise;
    assert_eq!(advertised.as_deref(), Some("127.0.0.1:28080"));
    let advertise = teseladb_server::http_advertise;
    assert_eq!(advertise("0.0.0.0, db.example.com", 1).as_deref(), Some("db.example.com:1"));
    assert_eq!(advertise("::1", 2).as_deref(), Some("[::1]:2"));
    assert_eq!(advertise("[::1]", 2).as_deref(), Some("[::1]:2"));
    assert_eq!(advertise("0.0.0.0,::,[::]", 3), None);
    assert_eq!(advertise("", 3), None);
    let e = bad("start --insecure --http-bind 10.0.0.1");
    assert!(e.contains("--http-bind 10.0.0.1: with --insecure only loopback"), "{e}");
    let e = bad("start --insecure --http-port 70000");
    assert!(e.contains("http-port"), "{e}");
    let file = |_: &std::path::Path| {
        Ok(b"teseladb:\n  insecure: true\n  http:\n    port: 29080\n    bind: 127.0.0.2\n".to_vec())
    };
    match parse_with(&args("start --config c.yaml"), &file) {
        Ok(Command::Start(o)) => {
            assert_eq!((o.http_port, o.http_bind.as_str()), (29_080, "127.0.0.2"))
        }
        other => panic!("{other:?}"),
    }
}

/// 013 FR-031 and FR-025: `ocspEnabled` takes `1`/`0` as well as `true`/`false`, the last one
/// given counts, another parameter leaves it on, and in FIPS mode the mechanisms named are kept as
/// given.
#[test]
fn set_parameter_ocsp_and_the_mechanisms_in_fips_mode() {
    use teseladb_auth::sasl::SaslMechanism;
    assert!(!start("start").tls.ocsp_disabled);
    assert!(start("start --setParameter ocspEnabled=0").tls.ocsp_disabled);
    let on = "start --setParameter ocspEnabled=false --setParameter ocspEnabled=1";
    assert!(!start(on).tls.ocsp_disabled);
    let o = start("start --setParameter enableTestCommands=1");
    assert!(!o.tls.ocsp_disabled);
    assert_eq!(o.transaction_lifetime_secs, 60);
    let fips = "start --tlsFIPSMode --setParameter authenticationMechanisms=SCRAM-SHA-256";
    assert_eq!(start(fips).mechanisms, vec![SaslMechanism::ScramSha256]);
}

/// Every parameter `getParameter` marks as settable at startup takes `--setParameter`, with the
/// checks of `setParameter` and the startup messages of a `refnode` 9.0.4 (006 FR-037, T067;
/// measured on 05-11-2026); the node starts with them.
#[test]
fn set_parameter_takes_every_parameter_settable_at_startup() {
    use teseladb_wire::commands::server_params::{
        CURSOR_TIMEOUT_MILLIS, HASH_INTERSECTION, MAX_OPERATION_MEMORY, QUERY_STATS_SAMPLE_RATE,
    };
    use teseladb_wire::ingress::ENABLED;
    let o = start(&format!(
        "start --insecure --setParameter {CURSOR_TIMEOUT_MILLIS}=1000 \
         --setParameter {HASH_INTERSECTION}=true --setParameter {QUERY_STATS_SAMPLE_RATE}=0.5 \
         --setParameter {ENABLED}=true"
    ));
    let s = teseladb_server::client_config(&o).sessions();
    assert_eq!(s.params.cursor_timeout_ms, 1000);
    assert!(s.hot.hash_intersection.load(std::sync::atomic::Ordering::Relaxed));
    assert!((s.params.query_stats_sample_rate() - 0.5).abs() < f64::EPSILON);
    assert_eq!(s.ingress.value(ENABLED), teseladb_bson::Value::Boolean(true));
    let bad_value = |name: &str, v: &str, why: &str| {
        assert_eq!(
            bad(&format!("start --insecure --setParameter {name}={v}")),
            format!("Bad value for parameter \"{name}\": {why}"),
            "{name}={v}"
        );
    };
    bad_value(CURSOR_TIMEOUT_MILLIS, "abc", "Did not consume whole string.");
    bad_value(HASH_INTERSECTION, "yes", "Value is not a valid boolean");
    bad_value(QUERY_STATS_SAMPLE_RATE, "x", "Invalid number");
    bad_value(
        MAX_OPERATION_MEMORY,
        "0",
        &format!("Invalid value for parameter {MAX_OPERATION_MEMORY}: 0 is not greater than 0"),
    );
    assert_eq!(
        bad("start --insecure --setParameter clusterAuthMode=x509"),
        "Cannot use --setParameter to set \"clusterAuthMode\" at startup"
    );
}

/// `teseladb admin --node <at> <args>` in this process: exit code, output and errors.
fn admin(at: &str, args: &[&str]) -> (i32, String, String) {
    let argv: Vec<String> =
        ["admin", "--node", at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

/// The output of `args`, which must succeed.
fn admin_ok(at: &str, args: &[&str]) -> String {
    let (code, out, err) = admin(at, args);
    assert_eq!(code, 0, "{args:?}: {err}");
    out
}

/// 007 T058 (FR-030, FR-033 to FR-036): `teseladb admin tenant` with each option of `limits`,
/// `access`, `usage`, `suspend` and `resume`, as text and with `--format json`, and `tenant list`
/// with the limits, the list and the state (contract `admin-cli.md`).
#[test]
fn the_tenant_subcommands_take_every_option_as_text_and_json() {
    use std::net::SocketAddr;
    use teseladb_server::node::{Node, NodeConfig};
    let cfg = NodeConfig {
        cores: Some(1),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(teseladb_raftlog::MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(std::time::Duration::from_secs(30)));
    let at = node.admin_addr().unwrap().to_string();
    assert!(admin_ok(&at, &["tenant", "create", "acme"]).starts_with("tenant acme: "));
    // Every limit, each switch and the service user in one call: the answer has them all.
    let set = [
        ("--storage-bytes", "1048576", "storageBytes\t1048576"),
        ("--max-connections", "20", "maxConnections\t20"),
        ("--ops-per-second", "100", "opsPerSecond\t100"),
        ("--max-api-keys", "3", "maxApiKeys\t3"),
        ("--max-time-ms", "30000", "maxTimeMS\t30000"),
        ("--max-concurrent-ops", "4", "maxConcurrentOps\t4"),
        ("--max-write-bytes-per-second", "65536", "maxWriteBytesPerSecond\t65536"),
        ("--max-databases", "5", "maxDatabases\t5"),
        ("--max-collections", "50", "maxCollections\t50"),
        ("--max-search-indexes", "2", "maxSearchIndexes\t2"),
        ("--http", "off", "httpEnabled\tfalse"),
        ("--mcp", "off", "mcpEnabled\tfalse"),
    ];
    let mut argv = vec!["tenant", "limits", "acme"];
    for (opt, value, _) in &set {
        argv.extend([*opt, *value]);
    }
    argv.extend(["--service-user", "svc"]);
    let out = admin_ok(&at, &argv);
    for (_, _, line) in &set {
        assert!(out.lines().any(|l| l == *line), "{line} in {out}");
    }
    assert!(out.contains("serviceUser\t") && out.contains("svc"), "{out}");
    // Without options it shows them; as JSON, one document.
    assert_eq!(admin_ok(&at, &["tenant", "limits", "acme"]), out);
    let json = admin_ok(&at, &["tenant", "limits", "acme", "--format", "json"]);
    let doc = teseladb_bson::extjson::parse(json.trim()).unwrap();
    assert_eq!(doc.get("httpEnabled"), Some(&teseladb_bson::Value::Boolean(false)), "{json}");
    // A bad value and an option of another subcommand are refused with the usage code.
    let (code, _, err) = admin(&at, &["tenant", "limits", "acme", "--http", "maybe"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("on or off"), "{err}");
    let (code, _, err) = admin(&at, &["tenant", "limits", "acme", "--allow", "10.0.0.0/8"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("does not go with this subcommand"), "{err}");
    let (code, _, err) = admin(&at, &["tenant", "limits", "acme", "--max-databases", "x"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not a whole number"), "{err}");
    // The access list: replaced, shown, cleared.
    let allow = ["tenant", "access", "acme", "--allow", "10.0.0.0/8", "--allow", "192.168.1.7"];
    assert_eq!(admin_ok(&at, &allow), "10.0.0.0/8\n192.168.1.7\n");
    assert_eq!(admin_ok(&at, &["tenant", "access", "acme"]), "10.0.0.0/8\n192.168.1.7\n");
    let json = admin_ok(&at, &["tenant", "access", "acme", "--format", "json"]);
    assert!(json.contains("allowedSources") && json.contains("192.168.1.7"), "{json}");
    // `tenant list` names each limit set, each switch off, the list and the state.
    assert_eq!(admin_ok(&at, &["tenant", "suspend", "acme"]), "tenant acme suspended\n");
    let list = admin_ok(&at, &["tenant", "list"]);
    let line = list.lines().find(|l| l.contains("acme")).unwrap_or_else(|| panic!("{list}"));
    for word in [
        "opsPerSecond=100",
        "httpEnabled=off",
        "mcpEnabled=off",
        "serviceUser=svc",
        "allowedSources=10.0.0.0/8,192.168.1.7",
        "suspended",
    ] {
        assert!(line.split('\t').any(|w| w == word), "{word} in {line}");
    }
    let json = admin_ok(&at, &["tenant", "list", "--format", "json"]);
    assert!(json.contains("\"suspended\":true") || json.contains("\"suspended\" : true"), "{json}");
    assert_eq!(admin_ok(&at, &["tenant", "resume", "acme"]), "tenant acme resumed\n");
    assert!(!admin_ok(&at, &["tenant", "list"]).contains("\tsuspended"));
    assert_eq!(admin_ok(&at, &["tenant", "access", "acme", "--clear"]), "any source\n");
    // The usage: the five fields of the contract, as text and as JSON.
    let usage = admin_ok(&at, &["tenant", "usage", "acme"]);
    for field in ["storageBytes", "connections", "opsTotal", "throttledSeconds", "lastAuth"] {
        assert!(usage.lines().any(|l| l.starts_with(&format!("{field}\t"))), "{field}: {usage}");
    }
    let json = admin_ok(&at, &["tenant", "usage", "acme", "--format", "json"]);
    assert!(teseladb_bson::extjson::parse(json.trim()).unwrap().get("opsTotal").is_some(), "{json}");
    // A tenant that does not exist: an error with its name.
    let (code, _, err) = admin(&at, &["tenant", "limits", "nobody"]);
    assert_eq!(code, 1);
    assert!(err.contains("nobody"), "{err}");
    drop(node.stop());
}
