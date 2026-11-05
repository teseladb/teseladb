//! Diagnostic commands (FR-023): the shapes, types and messages a `refrouter` 9.0.4 gives (measured
//! on 30-10-2026), with the data the node reports.

mod common;

use common::{Mem, doc, field};
use teseladb_wire::commands::diagnostics::{
    ANNOUNCED_VERSION, ConnectionCounts, HostFacts, LogTail, NetworkCounts, OpCounters,
    ProcessFacts, ResourceUse, ServerCertificate, listed_names,
};
use teseladb_wire::commands::{is_data_command, lock};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{
    Action, ClusterTime, NodeInfo, Topology, accepts, command_names, dispatch_with,
};
use teseladb_wire::msg::OpMsg;
use teseladb_bson::{Document, Value};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => panic!("no error in {r:?}"),
    }
}

fn keys(v: &Value) -> Vec<String> {
    match v {
        Value::Document(d) => d.iter().map(|(k, _)| k.to_owned()).collect(),
        other => panic!("{other:?}"),
    }
}

fn facts() -> ProcessFacts {
    ProcessFacts {
        port: 27018,
        pid: 42,
        start_ms: 1_700_000_000_000,
        now_ms: 1_700_000_007_361,
        argv: vec!["teseladb".into(), "--port".into(), "27018".into()],
        parsed: Document::new().with("net", Document::new().with("port", 27018)),
        build: "0.1.0".into(),
        build_target: ("aarch64".into(), "linux".into()),
        host: HostFacts {
            hostname: "h1".into(),
            cpu_arch: "aarch64".into(),
            num_cores: 12,
            num_cores_available: 12,
            mem_size_mb: 19_969,
            page_size: 4096,
            num_pages: 5_112_267,
            max_open_files: 1_048_576,
            os_type: "Linux".into(),
            os_name: "Ubuntu".into(),
            os_version: "26.04".into(),
            kernel_version: "7.0.14".into(),
            version_string: "Linux version 7.0.14".into(),
        },
        connections: ConnectionCounts {
            current: 3,
            available: 999_997,
            total_created: 664,
            rejected: 0,
            active: 2,
        },
        network: NetworkCounts {
            bytes_in: 10,
            bytes_out: 20,
            physical_bytes_in: 9,
            physical_bytes_out: 19,
            requests: 5,
            tls_handshakes: 4,
            tls_handshake_ms: 12,
            tls_handshake_micros: 12_500,
        },
        resources: ResourceUse {
            resident_bytes: 121 << 20,
            virtual_bytes: 3641 << 20,
            user_time_us: 10,
            system_time_us: 6,
            max_resident_kb: 130_880,
            threads: 44,
            ..ResourceUse::default()
        },
        fs_used: 1 << 30,
        fs_total: 4 << 30,
        log: LogTail { total: 3, lines: vec!["a".into(), "b".into()] },
        startup_warnings: LogTail { total: 1, lines: vec!["w".into()] },
        server_certificate: None,
    }
}

fn node() -> Mem {
    Mem { facts: facts(), ..Mem::new() }
}

#[test]
fn build_info_announces_9_0_with_teselas_build() {
    let mut m = node();
    let r = m.run("db", doc(&[("buildInfo", 1.into())]));
    assert_eq!(field(&r, "version"), &s(ANNOUNCED_VERSION));
    assert_eq!(
        field(&r, "versionArray"),
        &Value::Array([9, 0, 4, 0].iter().map(|n| Value::Int32(*n)).collect())
    );
    assert_eq!(field(&r, "gitVersion"), &s("0.1.0"));
    assert_eq!(field(&r, "buildEnvironment.target_os"), &s("linux"));
    assert_eq!(field(&r, "maxBsonObjectSize"), &Value::Int64(16 * 1024 * 1024));
    assert_eq!(field(&r, "bits"), &Value::Int32(64));
    assert_eq!(field(&r, "sysinfo"), &s("deprecated"));
    assert_eq!(
        keys(&Value::Document(r.clone())),
        [
            "version",
            "versionArray",
            "gitVersion",
            "modules",
            "allocator",
            "javascriptEngine",
            "sysinfo",
            "buildEnvironment",
            "bits",
            "debug",
            "maxBsonObjectSize",
            "opentelemetry",
            "ok"
        ]
    );
}

#[test]
fn host_info_describes_the_host() {
    let mut m = node();
    let r = m.run("admin", doc(&[("hostInfo", 1.into())]));
    assert_eq!(field(&r, "system.hostname"), &s("h1:27018"));
    assert_eq!(field(&r, "system.currentTime"), &Value::DateTime(1_700_000_007_361));
    assert_eq!(field(&r, "system.memSizeMB"), &Value::Int64(19_969));
    assert_eq!(field(&r, "system.numCores"), &Value::Int32(12));
    assert_eq!(field(&r, "os.type"), &s("Linux"));
    assert_eq!(field(&r, "extra.pageSize"), &Value::Int64(4096));
    assert_eq!(field(&r, "extra.numPages"), &Value::Int32(5_112_267));
    assert_eq!(keys(field(&r, "os")), ["type", "name", "version"]);
}

#[test]
fn server_status_counts_and_hides_the_sections_asked() {
    let mut m = node();
    m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(vec![d(&[]), d(&[])]))]));
    let r = m.run("admin", doc(&[("serverStatus", 1.into())]));
    assert_eq!(field(&r, "host"), &s("h1:27018"));
    assert_eq!(field(&r, "process"), &s("refrouter"));
    assert_eq!(field(&r, "pid"), &Value::Int64(42));
    assert_eq!(field(&r, "uptime"), &Value::Double(7.0));
    assert_eq!(field(&r, "uptimeMillis"), &Value::Int64(7361));
    assert_eq!(field(&r, "connections.current"), &Value::Int32(3));
    assert_eq!(field(&r, "connections.exhaustHello"), &Value::Int64(0));
    assert_eq!(field(&r, "network.numRequests"), &Value::Int64(5));
    assert_eq!(field(&r, "mem.resident"), &Value::Int32(121));
    assert_eq!(field(&r, "extra_info.threads"), &Value::Int64(44));
    // `run` of the tests skips the dispatch, which counts; the counters come from the table.
    assert_eq!(field(&r, "opcounters.insert"), &Value::Int64(0));
    // The sections in a `refrouter` 9.0.4's order, without those of DV-038.
    assert_eq!(
        keys(&Value::Document(r.clone())),
        [
            "host",
            "version",
            "process",
            "pid",
            "uptime",
            "uptimeMillis",
            "uptimeEstimate",
            "localTime",
            "asserts",
            "connections",
            "defaultRWConcern",
            "extra_info",
            "health",
            "internalTransactions",
            "logicalSessionRecordCache",
            "network",
            "opLatencies",
            "opWorkingTime",
            "opcounters",
            "security",
            "transactions",
            "transportSecurity",
            "mem",
            "metrics",
            "ok"
        ]
    );
    // The keys are TLS versions with a dot: read without splitting the path.
    let Some(Value::Document(tls)) = r.get("transportSecurity") else {
        panic!("transportSecurity")
    };
    assert_eq!(tls.get("1.3"), Some(&Value::Int64(4)));
    assert_eq!(tls.get("1.2"), Some(&Value::Int64(0)));
    assert_eq!(field(&r, "metrics.network.totalIngressTLSConnections"), &Value::Int64(4));
    assert_eq!(field(&r, "metrics.network.totalIngressTLSHandshakeTimeMillis"), &Value::Int64(12));
    assert_eq!(
        field(&r, "metrics.network.averageTimeToCompletedTLSHandshakeMicros"),
        &Value::Double(3125.0)
    );
    assert_eq!(field(&r, "defaultRWConcern.defaultWriteConcern.w"), &s("majority"));
    assert_eq!(field(&r, "defaultRWConcern.defaultWriteConcern.wtimeout"), &Value::Int32(0));
    assert_eq!(field(&r, "health.state"), &s("Ok"));
    assert_eq!(field(&r, "health.enteredStateAtTime"), &Value::DateTime(1_700_000_000_000));
    assert_eq!(field(&r, "transactions.commitTypes.twoPhaseCommit.initiated"), &Value::Int64(0));
    assert_eq!(field(&r, "internalTransactions.started"), &Value::Int64(0));
    assert_eq!(field(&r, "metrics.commands.<UNKNOWN>"), &Value::Int64(0));
    assert_eq!(field(&r, "metrics.commands.update.arrayFilters"), &Value::Int64(0));
    assert_eq!(field(&r, "metrics.commands.collMod.validationLevel.strict"), &Value::Int64(0));
    assert_eq!(
        field(&r, "security.authentication.mechanisms.PLAIN"),
        &d(&[(
            "egress",
            d(&[
                (
                    "speculativeAuthenticate",
                    d(&[("total", Value::Int64(0)), ("successful", Value::Int64(0))])
                ),
                ("authenticate", d(&[("total", Value::Int64(0)), ("successful", Value::Int64(0))])),
            ])
        )])
    );
    let r = m.run(
        "admin",
        doc(&[("serverStatus", 1.into()), ("mem", 0.into()), ("asserts", false.into())]),
    );
    assert!(r.get("mem").is_none() && r.get("asserts").is_none());
    assert!(r.get("network").is_some());
    let mut c = OpCounters::default();
    let many = |n: usize| Value::Array(vec![d(&[]); n]);
    c.count("insert", &doc(&[("insert", s("c")), ("documents", many(3))]), false);
    c.count("update", &doc(&[("update", s("c")), ("updates", many(2))]), false);
    c.count("delete", &doc(&[("delete", s("c")), ("deletes", many(1))]), true);
    c.count("insert", &doc(&[("insert", s("c"))]), false);
    for name in ["find", "getMore", "aggregate", "ping"] {
        c.count(name, &doc(&[(name, 1.into())]), false);
    }
    let want = OpCounters {
        insert: 3,
        query: 1,
        update: 2,
        delete: 1,
        getmore: 1,
        command: 1,
        aggregate: 1,
        user_asserts: 1,
    };
    assert_eq!(c, want);
}

#[test]
fn connection_status_has_no_users_and_a_new_uuid_each_time() {
    let mut m = node();
    let r = m.run("db", doc(&[("connectionStatus", 1.into())]));
    assert_eq!(keys(field(&r, "authInfo")), ["authenticatedUsers", "authenticatedUserRoles"]);
    let first = field(&r, "uuid").clone();
    assert!(matches!(&first, Value::Binary { subtype: 4, bytes } if bytes.len() == 16));
    let r = m.run("db", doc(&[("connectionStatus", 1.into()), ("showPrivileges", true.into())]));
    assert_eq!(
        keys(field(&r, "authInfo")),
        ["authenticatedUsers", "authenticatedUserRoles", "authenticatedUserPrivileges"]
    );
    assert_ne!(field(&r, "uuid"), &first);
}

#[test]
fn command_line_and_logs_only_on_admin() {
    let mut m = node();
    let r = m.run("admin", doc(&[("getCmdLineOpts", 1.into())]));
    assert_eq!(field(&r, "argv.1"), &s("--port"));
    assert_eq!(field(&r, "parsed.net.port"), &Value::Int32(27018));
    for cmd in ["getCmdLineOpts", "getLog"] {
        let r = m.run("db", doc(&[(cmd, s("*"))]));
        assert_eq!(code(&r), 13);
        let msg = format!("{cmd} may only be run against the admin database.");
        assert_eq!(field(&r, "errmsg"), &s(&msg));
    }
    let r = m.run("admin", doc(&[("getLog", s("*"))]));
    assert_eq!(field(&r, "names"), &Value::Array(vec![s("global"), s("startupWarnings")]));
    let mut quiet = node();
    quiet.facts.startup_warnings.total = 0;
    let r = quiet.run("admin", doc(&[("getLog", s("*"))]));
    assert_eq!(field(&r, "names"), &Value::Array(vec![s("global")]));
    let r = m.run("admin", doc(&[("getLog", s("global"))]));
    assert_eq!(field(&r, "totalLinesWritten"), &Value::Int32(3));
    assert_eq!(field(&r, "log"), &Value::Array(vec![s("a"), s("b")]));
    let r = m.run("admin", doc(&[("getLog", s("startupWarnings"))]));
    assert_eq!(field(&r, "log"), &Value::Array(vec![s("w")]));
    let r = m.run("admin", doc(&[("getLog", s("nosuch"))]));
    assert_eq!((code(&r), field(&r, "errmsg")), (96, &s("No log named 'nosuch'")));
    let r = m.run("admin", doc(&[("getLog", 1.into())]));
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field 'getLog.getLog' is the wrong type 'int', expected type 'string'")
    );
}

#[test]
fn db_and_collection_stats_count_documents_and_bytes() {
    let mut m = node();
    let r = m.run("nosuchdb", doc(&[("dbStats", 1.into())]));
    assert_eq!(field(&r, "collections"), &Value::Int32(0));
    assert_eq!(field(&r, "fsTotalSize"), &Value::Double(0.0));
    assert_eq!(field(&r, "avgObjSize"), &Value::Double(0.0));
    let docs: Vec<Value> = (0..10).map(|i| d(&[("_id", i.into()), ("pad", s("xxxx"))])).collect();
    m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(docs))]));
    m.run("db", doc(&[("create", s("cap")), ("capped", true.into()), ("size", 4096.into())]));
    let r = m.run("db", doc(&[("dbStats", 1.into())]));
    assert_eq!(field(&r, "db"), &s("db"));
    assert_eq!(field(&r, "collections"), &Value::Int32(2));
    assert_eq!(field(&r, "objects"), &Value::Int32(10));
    // Each document is 28 bytes: `{_id: <int>, pad: "xxxx"}`.
    assert_eq!(field(&r, "dataSize"), &Value::Double(280.0));
    assert_eq!(field(&r, "avgObjSize"), &Value::Double(28.0));
    assert_eq!(field(&r, "indexes"), &Value::Int32(2));
    assert_eq!(field(&r, "indexSize"), &Value::Double(8192.0));
    assert_eq!(field(&r, "fsTotalSize"), &Value::Double(4_294_967_296.0));
    let r = m.run("db", doc(&[("dbStats", 1.into()), ("scale", 1024.into())]));
    assert_eq!(field(&r, "indexSize"), &Value::Double(8.0));
    assert_eq!(field(&r, "scaleFactor"), &Value::Int32(1024));
    let r = m.run("db", doc(&[("dbStats", 1.into()), ("scale", 0.into())]));
    assert_eq!(field(&r, "errmsg"), &s("Scale must be greater than zero"));
    let r = m.run("db", doc(&[("dbStats", 1.into()), ("scale", s("x"))]));
    assert_eq!(
        field(&r, "errmsg"),
        &s(
            "BSON field 'dbStats.scale' is the wrong type 'string', expected types '[int, decimal, double, long]'"
        )
    );
    let r = m.run("db", doc(&[("collStats", s("c"))]));
    assert_eq!(field(&r, "ns"), &s("db.c"));
    assert_eq!(field(&r, "count"), &Value::Int32(10));
    assert_eq!(field(&r, "size"), &Value::Int32(280));
    assert_eq!(field(&r, "capped"), &Value::Boolean(false));
    assert_eq!(field(&r, "indexSizes._id_"), &Value::Int32(4096));
    assert_eq!(field(&r, "avgObjSize"), &Value::Double(28.0));
    let r = m.run("db", doc(&[("collStats", s("c")), ("scale", 1024.into())]));
    assert_eq!(field(&r, "totalIndexSize"), &Value::Int32(4));
    let r = m.run("db", doc(&[("collStats", s("cap"))]));
    assert_eq!(
        (field(&r, "capped"), field(&r, "count")),
        (&Value::Boolean(true), &Value::Int32(0))
    );
    let r = m.run("db", doc(&[("collStats", s("nosuch"))]));
    assert_eq!(field(&r, "nindexes"), &Value::Int32(0));
    assert!(r.get("capped").is_none());
    for (arg, code_of, msg) in [
        (s(""), 73, "Invalid namespace specified 'db'"),
        (Value::Int32(5), 73, "Collection name has invalid type int"),
    ] {
        let r = m.run("db", doc(&[("collStats", arg)]));
        assert_eq!((code(&r), field(&r, "errmsg")), (code_of, &s(msg)));
    }
}

#[test]
fn list_commands_names_what_the_node_answers() {
    let mut m = node();
    let r = m.run("db", doc(&[("listCommands", 1.into())]));
    let names = keys(field(&r, "commands"));
    // Every command the dispatch takes and no other, in byte order as a `refrouter` lists them.
    let mut want: Vec<String> = command_names(false).into_iter().map(str::to_owned).collect();
    want.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(names, want);
    assert_eq!(
        field(&r, "commands.find"),
        &d(&[
            ("help", s("query for documents")),
            ("requiresAuth", true.into()),
            ("requiresAuthzChecks", true.into()),
            ("secondaryOk", false.into()),
            ("adminOnly", false.into()),
            ("secondaryOverrideOk", true.into()),
            ("apiVersions", Value::Array(vec![s("1")])),
            ("deprecatedApiVersions", Value::Array(Vec::new())),
        ])
    );
    let Value::String(help) = field(&r, "commands.setParameter.help") else { panic!() };
    assert_eq!(
        help,
        "set administrative option(s)\n{ setParameter:1, <param>:<value> }\nsupported:\n  auditAuthorizationSuccess\n  clientCursorMonitorFrequencySecs\n  clusterAuthMode\n  cursorTimeoutMillis\n  ingressConnectionEstablishmentBurstCapacitySecs\n  ingressConnectionEstablishmentMaxQueueDepth\n  ingressConnectionEstablishmentRateLimiterBypass\n  ingressConnectionEstablishmentRateLimiterEnabled\n  ingressConnectionEstablishmentRatePerSec\n  internalQueryMaxMemoryUsageBytesPerOperation\n  internalQueryMaxSingleExpressionMemoryUsageBytes\n  internalQueryPlannerEnableHashIntersection\n  internalQueryStatsRateLimit\n  internalQueryStatsSampleRate\n  maxConcurrentMultiDocumentTransactions\n  redactClientLogData\n  transactionLifetimeLimitSeconds\n  useInternalAuthzForX509\n"
    );
    let Value::String(help) = field(&r, "commands.getParameter.help") else { panic!() };
    assert!(help.contains("  enableTestCommands\n") && help.ends_with("to get everything\n"));
    let mut tests = node();
    {
        let mut g = lock(&tests.shared);
        let plain = std::mem::take(&mut *g);
        *g = plain.with_test_commands();
    }
    let r = tests.run("db", doc(&[("listCommands", 1.into())]));
    assert!(field(&r, "commands.configureFailPoint.adminOnly") == &Value::Boolean(true));
}

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

fn reply(conn: &mut Connection, m: &mut Mem, cmd: Document) -> Document {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", "admin").to_bytes().unwrap());
    let mut c = Connection::new(9);
    c.receive(&bytes);
    let req: Request = c.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let r = match dispatch_with(conn, &req, &info(), Some((m, &shared))) {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    };
    m.shared = shared;
    r
}

#[test]
fn whatsmyuri_answers_the_client_address_and_the_dispatch_counts() {
    let mut m = node();
    let mut conn = Connection::new(9);
    conn.peer = Some("10.0.0.7:5000".into());
    let r = reply(&mut conn, &mut m, doc(&[("whatsmyuri", 1.into())]));
    assert_eq!(field(&r, "you"), &s("10.0.0.7:5000"));
    reply(&mut conn, &mut m, doc(&[("ping", 1.into())]));
    reply(&mut conn, &mut m, doc(&[("find", s("c"))]));
    reply(&mut conn, &mut m, doc(&[("find", 5.into())]));
    let r = reply(&mut conn, &mut m, doc(&[("serverStatus", 1.into())]));
    assert_eq!(field(&r, "pid"), &Value::Int64(42), "the facts reach through the op table");
    assert_eq!(field(&r, "opcounters.query"), &Value::Int64(2));
    assert_eq!(field(&r, "opcounters.command"), &Value::Int64(2));
    assert_eq!(field(&r, "asserts.user"), &Value::Int32(1));
    let mut lone = Connection::new(3);
    let bytes = OpMsg::encode(
        1,
        0,
        0,
        &doc(&[("whatsmyuri", 1.into()), ("$db", s("admin"))]).to_bytes().unwrap(),
    );
    let mut c = Connection::new(9);
    c.receive(&bytes);
    let req = c.next_request().unwrap().unwrap();
    let Action::Reply(r) = dispatch_with::<Mem>(&mut lone, &req, &info(), None) else { panic!() };
    assert_eq!(field(&r, "you"), &s(""));
}

#[test]
fn repl_set_get_status_answers_as_a_refrouter() {
    let mut m = node();
    let r = m.run("admin", doc(&[("replSetGetStatus", 1.into())]));
    assert_eq!(
        r,
        doc(&[
            ("info", s("refrouter")),
            ("ok", Value::Double(0.0)),
            ("errmsg", s("replSetGetStatus is not supported through refrouter")),
        ])
    );
    let r = m.run("db", doc(&[("replSetGetStatus", 1.into())]));
    assert_eq!(code(&r), 13);
    assert!(listed_names().any(|n| n == "replSetGetStatus"));
}

#[test]
fn commands_answer_to_refdbs_other_names() {
    let mut m = node();
    m.run(
        "db",
        doc(&[("insert", s("c")), ("documents", Value::Array(vec![d(&[("_id", 1.into())])]))]),
    );
    let r = m.run("admin", doc(&[("buildinfo", 1.into())]));
    assert_eq!(field(&r, "version"), &s(ANNOUNCED_VERSION));
    let r = m.run("db", doc(&[("dbstats", 1.into())]));
    assert_eq!(field(&r, "collections"), &Value::Int32(1));
    let r = m.run("db", doc(&[("collstats", s("c"))]));
    assert_eq!(field(&r, "count"), &Value::Int32(1));
    let r = m.run("db", doc(&[("datasize", 5.into())]));
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field 'dataSize.datasize' is the wrong type 'int', expected type 'string'")
    );
    // `setParameter` may run on a secondary, as RefDb lists it.
    let r = m.run("admin", doc(&[("listCommands", 1.into())]));
    assert_eq!(field(&r, "commands.setParameter.secondaryOk"), &Value::Boolean(true));
    assert!(is_data_command("buildinfo") && !is_data_command("BUILDINFO"));
}

#[test]
fn server_status_counts_what_the_dispatch_ran() {
    let mut m = node();
    let mut conn = Connection::new(9);
    conn.hello.app_name = Some("app".into());
    reply(&mut conn, &mut m, doc(&[("find", s("c"))]));
    reply(&mut conn, &mut m, doc(&[("find", s("c")), ("apiVersion", s("1"))]));
    reply(&mut conn, &mut m, doc(&[("noSuchCommand", 1.into())]));
    let docs = Value::Array(vec![d(&[("_id", 1.into())]), d(&[("_id", 1.into())])]);
    reply(
        &mut conn,
        &mut m,
        doc(&[("insert", s("c")), ("documents", docs), ("ordered", false.into())]),
    );
    let stmt =
        d(&[("q", d(&[])), ("u", Value::Array(vec![d(&[("$set", d(&[("a", 1.into())]))])]))]);
    let filtered = d(&[
        ("q", d(&[])),
        ("u", d(&[("$set", d(&[("b.$[x]", 1.into())]))])),
        ("arrayFilters", Value::Array(vec![d(&[("x", 1.into())])])),
    ]);
    reply(
        &mut conn,
        &mut m,
        doc(&[("update", s("c")), ("updates", Value::Array(vec![stmt, filtered]))]),
    );
    let validator = d(&[("$jsonSchema", d(&[]))]);
    reply(
        &mut conn,
        &mut m,
        doc(&[("create", s("v")), ("validator", validator), ("validationLevel", s("moderate"))]),
    );
    let mut lone = Connection::new(10);
    let r = reply(&mut lone, &mut m, doc(&[("serverStatus", 1.into())]));
    assert_eq!(field(&r, "metrics.commands.find.total"), &Value::Int64(2));
    assert_eq!(field(&r, "metrics.commands.find.failed"), &Value::Int64(0));
    assert_eq!(field(&r, "metrics.commands.<UNKNOWN>"), &Value::Int64(1));
    assert_eq!(field(&r, "metrics.commands.update.pipeline"), &Value::Int64(1));
    assert_eq!(field(&r, "metrics.commands.update.arrayFilters"), &Value::Int64(1));
    assert_eq!(field(&r, "metrics.commands.create.validator.jsonSchema"), &Value::Int64(1));
    assert_eq!(field(&r, "metrics.commands.create.validationLevel.moderate"), &Value::Int64(1));
    assert_eq!(field(&r, "metrics.operation.insertFailedDueToDuplicateKeyError"), &Value::Int64(1));
    assert_eq!(field(&r, "metrics.apiVersions.app"), &Value::Array(vec![s("1"), s("default")]));
    assert_eq!(field(&r, "opLatencies.reads.ops"), &Value::Int64(2));
    assert_eq!(field(&r, "opLatencies.writes.ops"), &Value::Int64(2));
    assert_eq!(field(&r, "opLatencies.commands.ops"), &Value::Int64(2));
    assert_eq!(field(&r, "opWorkingTime.reads.ops"), &Value::Int64(2));
    // Only the update read the collection, and a `refnode` 9.0.4 counts the scan of a statement
    // that ends (the pipeline's), not of one that fails while it updates (the `arrayFilters`
    // one, on documents without `b`; measured): both `find`s ran before the insert created it.
    assert_eq!(field(&r, "metrics.queryExecutor.collectionScans.total"), &Value::Int64(1));
    assert_eq!(field(&r, "logicalSessionRecordCache.activeSessionsCount"), &Value::Int32(0));
}

#[test]
fn status_counters_fill_their_sections() {
    use teseladb_wire::commands::status::{OpKind, StatusCounters};
    let mut c = StatusCounters::default();
    assert_eq!(OpKind::of("getMore"), OpKind::Read);
    assert_eq!(OpKind::of("bulkWrite"), OpKind::Write);
    assert_eq!(OpKind::of("ping"), OpKind::Command);
    c.killed_max_time = (2, 1);
    c.wtime = (3, 40);
    c.wtimeouts = 1;
    c.cursors_timed_out = 5;
    c.sasl_supported_mechs = 2;
    c.authentication_attempt("SCRAM-SHA-256", false);
    c.authentication_succeeded("SCRAM-SHA-256", false);
    c.authentication_attempt("REFDB-X509", true);
    c.authentication_time(250);
    c.txn.open = 3;
    c.txn.active = 1;
    c.txn.internal = [1, 2, 3, 4];
    let op = Value::Document(c.operation_section());
    assert_eq!(field_of(&op, "killedDueToMaxTimeMSExpired"), &Value::Int64(2));
    assert_eq!(field_of(&op, "killedDueToDefaultMaxTimeMSExpired"), &Value::Int64(1));
    assert_eq!(field_of(&op, "interrupt.totalChecks"), &Value::Int64(0));
    let wc = Value::Document(c.write_concern_section());
    assert_eq!(field_of(&wc, "wtime.totalMillis"), &Value::Int64(40));
    assert_eq!(field_of(&wc, "wtimeouts"), &Value::Int64(1));
    let cur = Value::Document(c.cursor_section(7));
    assert_eq!(field_of(&cur, "timedOut"), &Value::Int64(5));
    assert_eq!(field_of(&cur, "open.total"), &Value::Int64(7));
    let sec = Value::Document(c.security());
    let scram = "authentication.mechanisms.SCRAM-SHA-256.ingress.authenticate";
    assert_eq!(field_of(&sec, &format!("{scram}.successful")), &Value::Int64(1));
    let x509 = "authentication.mechanisms.REFDB-X509.ingress.speculativeAuthenticate.total";
    assert_eq!(field_of(&sec, x509), &Value::Int64(1));
    assert_eq!(
        field_of(&sec, "authentication.totalIngressAuthenticationTimeMicros"),
        &Value::Int64(250)
    );
    assert_eq!(field_of(&sec, "authentication.saslSupportedMechsReceived"), &Value::Int64(2));
    let t = Value::Document(c.transactions());
    assert_eq!(field_of(&t, "currentInactive"), &Value::Int64(2));
    let it = Value::Document(c.internal_transactions());
    assert_eq!(field_of(&it, "succeeded"), &Value::Int64(4));
    let sessions = Value::Document(c.session_cache(4));
    assert_eq!(field_of(&sessions, "sessionCatalogSize"), &Value::Int32(4));
    let ops = Value::Array(vec![
        d(&[("update", 0.into()), ("updateMods", Value::Array(vec![]))]),
        d(&[("update", 0.into()), ("updateMods", d(&[])), ("arrayFilters", Value::Array(vec![]))]),
    ]);
    c.command("bulkWrite", &doc(&[("bulkWrite", 1.into()), ("ops", ops)]), 3, false, true);
    let create = doc(&[
        ("create", s("v")),
        ("validator", d(&[("a", 1.into())])),
        ("validationLevel", s("strict")),
    ]);
    c.command("create", &create, 1, true, true);
    c.command("aggregate", &doc(&[("aggregate", s("c"))]), 1, false, true);
    let cmds =
        Value::Document(c.commands_section(["create", "bulkWrite", "aggregate"].into_iter()));
    assert_eq!(field_of(&cmds, "bulkWrite.pipeline"), &Value::Int64(1));
    assert_eq!(field_of(&cmds, "bulkWrite.arrayFilters"), &Value::Int64(1));
    assert_eq!(field_of(&cmds, "create.failed"), &Value::Int64(1));
    assert_eq!(field_of(&cmds, "create.validator.total"), &Value::Int64(1));
    assert_eq!(field_of(&cmds, "create.validator.jsonSchema"), &Value::Int64(0));
    assert_eq!(field_of(&cmds, "create.validationLevel.strict"), &Value::Int64(1));
    assert_eq!(field_of(&cmds, "aggregate.withExtension.succeeded"), &Value::Int64(0));
    let lat = Value::Document(c.latencies());
    assert_eq!(field_of(&lat, "writes.latency"), &Value::Int64(3));
    assert_eq!(field_of(&lat, "reads.ops"), &Value::Int64(1));
}

fn field_of<'a>(v: &'a Value, path: &str) -> &'a Value {
    match v {
        Value::Document(d) => field(d, path),
        other => panic!("{other:?}"),
    }
}

#[test]
fn only_commands_that_never_wait_stay_on_the_client_worker() {
    use teseladb_wire::dispatch::answers_without_waiting;
    let request = |cmd: Document| {
        let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", "admin").to_bytes().unwrap());
        let mut c = Connection::new(9);
        c.receive(&bytes);
        c.next_request().unwrap().unwrap()
    };
    for cmd in ["ping", "whatsmyuri", "hello", "isMaster", "ismaster"] {
        assert!(answers_without_waiting(&request(doc(&[(cmd, 1.into())]))), "{cmd}");
    }
    let mechs = doc(&[("hello", 1.into()), ("saslSupportedMechs", s("admin.u"))]);
    let spec = doc(&[("hello", 1.into()), ("speculativeAuthenticate", d(&[]))]);
    for cmd in [mechs, spec, doc(&[("find", s("c"))]), doc(&[("serverStatus", 1.into())])] {
        assert!(!answers_without_waiting(&request(cmd.clone())), "{cmd:?}");
    }
}

/// `getDiagnosticData` gives the node's sample on `admin` (010 FR-024, T041): `start`, the
/// parts of a `refnode` 9.0.4 TeselaDB has, each with its own `start` and `end`, and `end`; on
/// another database it is 13, as on a `refrouter` 9.0.4 (measured 01-11-2026).
#[test]
fn get_diagnostic_data_samples_the_node() {
    let mut m = node();
    let r = m.run("admin", doc(&[("getDiagnosticData", 1.into())]));
    assert_eq!(keys(&Value::Document(r.clone())), ["data", "ok"]);
    assert_eq!(keys(field(&r, "data")), ["start", "serverStatus", "connPoolStats", "end"]);
    let status = keys(field(&r, "data.serverStatus"));
    assert_eq!(status.first().map(String::as_str), Some("start"));
    assert_eq!(status.get(1).map(String::as_str), Some("host"));
    assert_eq!(status.last().map(String::as_str), Some("end"));
    assert!(!status.contains(&"ok".to_owned()));
    assert!(matches!(field(&r, "data.connPoolStats.start"), Value::DateTime(_)));
    // What the node writes each second outside any command is the same sample.
    let shared = std::mem::take(&mut m.shared);
    let sample = teseladb_wire::commands::diagnostic_data::capture(&mut m, &shared).unwrap();
    m.shared = shared;
    assert_eq!(keys(&Value::Document(sample)), ["start", "serverStatus", "connPoolStats", "end"]);
    let r = m.run("db", doc(&[("getDiagnosticData", 1.into())]));
    assert_eq!(code(&r), 13);
    assert_eq!(
        field(&r, "errmsg"),
        &s("getDiagnosticData may only be run against the admin database.")
    );
}

/// A node with TLS names its certificate in `security`, and the average TLS handshake goes first
/// in `metrics.network`; without handshakes there is no average (measured on a `refrouter` 9.0.4
/// with `--tlsMode allowTLS` on 03-11-2026).
#[test]
fn server_status_shows_the_server_certificate_and_the_tls_handshakes() {
    let cert = ServerCertificate { subject: "CN=localhost".into(), expires_ms: 1_793_976_060_000 };
    let mut m =
        Mem { facts: ProcessFacts { server_certificate: Some(cert), ..facts() }, ..Mem::new() };
    let r = m.run("admin", doc(&[("serverStatus", 1.into())]));
    let Some(Value::Document(sec)) = r.get("security") else { panic!("{r:?}") };
    let names: Vec<&str> = sec.iter().map(|(k, _)| k).collect();
    assert_eq!(
        names,
        ["authentication", "SSLServerSubjectName", "SSLServerCertificateExpirationDate"]
    );
    assert_eq!(sec.get("SSLServerSubjectName"), Some(&s("CN=localhost")));
    assert_eq!(
        sec.get("SSLServerCertificateExpirationDate"),
        Some(&Value::DateTime(1_793_976_060_000))
    );
    let Value::Document(net) = field(&r, "metrics.network") else { panic!("{r:?}") };
    let names: Vec<&str> = net.iter().map(|(k, _)| k).collect();
    assert_eq!(
        names,
        [
            "averageTimeToCompletedTLSHandshakeMicros",
            "totalIngressTLSConnections",
            "totalIngressTLSHandshakeTimeMillis"
        ]
    );
    let mut plain = ProcessFacts { server_certificate: None, ..facts() };
    plain.network.tls_handshakes = 0;
    let mut m = Mem { facts: plain, ..Mem::new() };
    let r = m.run("admin", doc(&[("serverStatus", 1.into())]));
    let Some(Value::Document(sec)) = r.get("security") else { panic!("{r:?}") };
    assert_eq!(sec.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["authentication"]);
    let Value::Document(net) = field(&r, "metrics.network") else { panic!("{r:?}") };
    assert!(net.get("averageTimeToCompletedTLSHandshakeMicros").is_none(), "{net:?}");
}

/// The table of `listCommands` and the dispatch name the same commands: each one the dispatch
/// takes has its entry, and each entry is one the dispatch takes, `configureFailPoint` with
/// `enableTestCommands`; the table goes in byte order of the names, as a `refrouter` 9.0.4 lists
/// them.
#[test]
fn list_commands_names_what_the_dispatch_takes() {
    let table: Vec<&str> = listed_names().collect();
    let mut dispatched = command_names(true);
    dispatched.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(table, dispatched);
    for name in &table {
        assert!(accepts(name, true, true), "{name} is listed but not taken");
    }
    assert!(!accepts("configureFailPoint", true, false));
    // The aliases are taken under the name `listCommands` gives.
    assert!(accepts("findandmodify", true, false) && accepts("ismaster", false, false));
    assert!(!table.contains(&"findandmodify") && !table.contains(&"ismaster"));
}
