//! The profiler (010 T032, FR-015): `profile`'s replies and errors against what a reference replica
//! set member 9.0.4 gave on 01-11-2026, the documents a profiled database gets in `system.profile`,
//! and the «Slow query» lines of the log.

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_wire::commands::profile::SLOW_QUERY_ID;
use teseladb_wire::commands::{NodeSessions, Shared, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const DB: &str = "pf";

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

fn request(cmd: Document) -> Request {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut c = Connection::new(9);
    c.receive(&bytes);
    c.next_request().unwrap().unwrap()
}

/// Runs `cmd` on database `db` through the dispatcher, as a client's command.
fn run(m: &mut Mem, db: &str, cmd: Document) -> Document {
    let shared: Shared = std::mem::take(&mut m.shared);
    let mut conn = Connection::new(1);
    let r = match dispatch_with(
        &mut conn,
        &request(cmd.with("$db", db)),
        &info(),
        Some((m, &shared)),
    ) {
        Action::Reply(mut d) => {
            d.remove("$clusterTime");
            d.remove("operationTime");
            d
        }
        other => panic!("{other:?}"),
    };
    m.shared = shared;
    r
}

fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    m
}

fn json(text: &str) -> Document {
    extjson::parse(text).expect("json")
}

/// Each command of the measured sequence and the reply of a reference replica set member 9.0.4
/// (`ok` and the numbers as the shell printed them, compared by value).
const SEQUENCE: &str = r#"{"profile":-1}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":2}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":-1}	{"was":2,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":0}	{"was":2,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":1,"slowms":0,"sampleRate":1}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":-1}	{"was":1,"slowms":0,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":0,"slowms":100}	{"was":1,"slowms":0,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":5}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":-2}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":1.5}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":2,"slowms":-1}	{"was":1,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":-1}	{"was":2,"slowms":-1,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":0,"slowms":100}	{"was":2,"slowms":-1,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":2,"slowms":1.7}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":-1}	{"was":2,"slowms":1,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":0,"slowms":100}	{"was":2,"slowms":1,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":0,"slowms":100,"sampleRate":0.5}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":-1}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":0.5,"ok":1}
{"profile":0,"sampleRate":1}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":0.5,"ok":1}
{"profile":2,"filter":{"op":"query"}}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"note":"When a filter expression is set, slowms and sampleRate are not used for profiling and slow-query log lines.","ok":1}
{"profile":-1}	{"was":2,"slowms":100,"slowinprogms":5000,"sampleRate":1,"filter":{"op":{"$eq":"query"}},"note":"When a filter expression is set, slowms and sampleRate are not used for profiling and slow-query log lines.","ok":1}
{"profile":0,"filter":"unset"}	{"was":2,"slowms":100,"slowinprogms":5000,"sampleRate":1,"filter":{"op":{"$eq":"query"}},"note":"When a filter expression is set, slowms and sampleRate are not used for profiling and slow-query log lines.","ok":1}
{"profile":-1}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":0,"slowinprogms":3}	{"was":0,"slowms":100,"slowinprogms":5000,"sampleRate":1,"ok":1}
{"profile":-1}	{"was":0,"slowms":100,"slowinprogms":3,"sampleRate":1,"ok":1}
{"profile":{"$numberLong":"1"}}	{"was":0,"slowms":100,"slowinprogms":3,"sampleRate":1,"ok":1}
{"profile":{"$numberDecimal":"2"}}	{"was":1,"slowms":100,"slowinprogms":3,"sampleRate":1,"ok":1}
{"profile":0}	{"was":2,"slowms":100,"slowinprogms":3,"sampleRate":1,"ok":1}
{"profile":2,"sampleRate":-0.1}	!! 2	'sampleRate' must be between 0.0 and 1.0 inclusive
{"profile":0,"sampleRate":2}	!! 2	'sampleRate' must be between 0.0 and 1.0 inclusive
{"profile":1,"filter":{"bogus":{"$foo":1}}}	!! 2	unknown operator: $foo
{"profile":0,"filter":1}	!! 2	Expected an object, or the string 'unset'.
{"profile":0,"filter":{"$where":"1"}}	!! 2	$where is not allowed in this context
{"profile":0,"sampleRate":"a"}	!! 14	BSON field 'profile.sampleRate' is the wrong type 'string', expected types '[int, double, decimal, long]'
{"profile":"a"}	!! 14	BSON field 'profile.profile' is the wrong type 'string', expected types '[int, double, decimal, long]'
{"profile":true}	!! 14	BSON field 'profile.profile' is the wrong type 'bool', expected types '[int, double, decimal, long]'
{"profile":0,"slowms":"a"}	!! 14	BSON field 'profile.slowms' is the wrong type 'string', expected types '[int, double, decimal, long]'"#;

/// A number as a double, any other value as it is.
fn loose(v: &Value) -> Value {
    match v {
        Value::Int32(n) => Value::Double(f64::from(*n)),
        Value::Int64(n) => Value::Double(*n as f64),
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, x) in d.iter() {
                out.push(k, loose(x));
            }
            Value::Document(out)
        }
        other => other.clone(),
    }
}

#[test]
fn profile_replies_and_errors_match_a_reference_member() {
    let mut m = node();
    let mut wrong = Vec::new();
    for line in SEQUENCE.lines() {
        let (cmd, want) = line.split_once('\t').expect("a command and its reply");
        let got = run(&mut m, DB, json(cmd));
        let same = match want.strip_prefix("!! ") {
            Some(err) => {
                let (code, msg) = err.split_once('\t').expect("a code and a message");
                got.get("code") == Some(&Value::Int32(code.parse().unwrap()))
                    && got.get("errmsg") == Some(&Value::from(msg))
            }
            None => loose(&Value::Document(got.clone())) == loose(&Value::Document(json(want))),
        };
        if !same {
            wrong.push(format!("{cmd}\n   got  {got:?}\n   want {want}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The documents of `pf.system.profile`.
fn profiled(m: &mut Mem) -> Vec<Document> {
    let r = run(m, DB, Document::new().with("find", "system.profile"));
    match field(&r, "cursor.firstBatch") {
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn level_two_writes_each_command_in_system_profile() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":1,"a":1},{"_id":2,"a":2}]}"#));
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{"a":{"$gt":1}}}"#));
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":9}]}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let docs = profiled(&mut m);
    let ops: Vec<Option<&Value>> = docs.iter().map(|d| d.get("op")).collect();
    assert_eq!(ops, [Some(&Value::from("query")), Some(&Value::from("insert"))], "{docs:?}");
    let find = &docs[0];
    assert_eq!(find.get("ns"), Some(&Value::from("pf.c")));
    assert_eq!(find.get("nreturned"), Some(&Value::Int32(1)));
    assert_eq!(find.get("cursorExhausted"), Some(&Value::Boolean(true)));
    assert_eq!(find.get("protocol"), Some(&Value::from("op_msg")));
    assert!(matches!(find.get("command"), Some(Value::Document(c)) if c.get("find").is_some()));
    assert!(matches!(find.get("ts"), Some(Value::DateTime(_))));
    assert_eq!(docs[1].get("ninserted"), Some(&Value::Int32(1)));
    // The collection is capped at 1 MiB, as the profiler creates it.
    let r = run(&mut m, DB, json(r#"{"listCollections":1,"filter":{"name":"system.profile"}}"#));
    let Value::Array(colls) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let Some(Value::Document(c)) = colls.first() else { panic!("{r:?}") };
    let Some(Value::Document(options)) = c.get("options") else { panic!("{c:?}") };
    assert_eq!(options.get("capped"), Some(&Value::Boolean(true)));
    assert_eq!(options.get("size").and_then(teseladb_wire::types::number), Some(1_048_576.0));
}

/// The write of a command's profile reads the indexes of `system.profile`: it ends with the command,
/// or a server would hold every change of the indexes back until the connection's next command
/// (009 FR-006; seen as `createIndexes` failing after 30 s while another connection sat idle).
#[test]
fn the_profile_of_a_command_ends_with_it() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":1}]}"#));
    assert_eq!(profiled(&mut m).len(), 1, "the insert was profiled");
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":2}]}"#));
    assert_eq!(m.open_index_reads, 0, "no read of the indexes outlives its command");
}

#[test]
fn level_one_profiles_and_logs_only_slow_commands() {
    let mut m = node();
    m.run(DB, json(r#"{"insert":"c","documents":[{"_id":1}]}"#));
    run(&mut m, DB, doc(&[("profile", Value::Int32(1)), ("slowms", Value::Int32(50))]));
    // Fast: neither profiled nor logged.
    run(&mut m, DB, json(r#"{"find":"c"}"#));
    // Slow: each read of the range moves the clock 80 ms.
    m.tick = 80;
    run(&mut m, DB, json(r#"{"find":"c","filter":{"_id":1}}"#));
    m.tick = 0;
    run(&mut m, DB, json(r#"{"profile":0,"slowms":100}"#));
    let docs = profiled(&mut m);
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert!(matches!(docs[0].get("millis"), Some(Value::Int64(ms)) if *ms >= 80));
    let slow: Vec<&Document> =
        m.logged.iter().filter(|(id, _, _)| *id == SLOW_QUERY_ID).map(|(_, _, a)| a).collect();
    assert_eq!(slow.len(), 1, "{:?}", m.logged);
    assert_eq!(slow[0].get("ns"), Some(&Value::from("pf.c")));
    assert_eq!(slow[0].get("type"), Some(&Value::from("command")));
}

#[test]
fn a_filter_decides_what_is_profiled() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":1}]}"#));
    run(&mut m, DB, json(r#"{"profile":2,"filter":{"op":"insert"}}"#));
    run(&mut m, DB, json(r#"{"find":"c"}"#));
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":2}]}"#));
    run(&mut m, DB, json(r#"{"profile":0,"filter":"unset"}"#));
    let docs = profiled(&mut m);
    let ops: Vec<Option<&Value>> = docs.iter().map(|d| d.get("op")).collect();
    assert_eq!(ops, [Some(&Value::from("insert"))], "{docs:?}");
    // Without a level the database has no profile, and another database has its own.
    let r = run(&mut m, "other", json(r#"{"profile":-1}"#));
    assert_eq!(r.get("was"), Some(&Value::Int32(0)));
}

/// The fields of a `system.profile` document that TeselaDB has no counterpart of: the reference
/// storage engine's locks and queues, the classic engine's planner and the priority port (DV-038).
const NOT_OURS: [&str; 9] = [
    "isFromPriorityPortConnection",
    // The test's connection sends no application name.
    "appName",
    "queryShapeHash",
    "planRanker",
    "locks",
    "flowControl",
    "queues",
    "totalOplogSlotDurationMicros",
    "cpuNanos",
];

/// The plan of each read in `system.profile` (010 FR-015), against what a reference replica set
/// member 9.0.4 wrote on 01-11-2026 (`tests/profile/plans.out`): the fields in its order, the
/// counters of what each read examined, its `planSummary` and the `execStats` of the plan as it
/// ran. The hashes are TeselaDB's; `cpuNanos` comes from the server's thread clock, which this node
/// in tests has not.
#[test]
fn profiled_reads_show_their_plan() {
    let mut m = node();
    run(
        &mut m,
        DB,
        json(r#"{"insert":"c","documents":[{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3,"a":3}]}"#),
    );
    run(&mut m, DB, json(r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#));
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{"a":{"$gt":1}}}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{"b":1}}"#));
    run(&mut m, DB, json(r#"{"update":"c","updates":[{"q":{"_id":2},"u":{"$set":{"b":1}}}]}"#));
    run(&mut m, DB, json(r#"{"delete":"c","deletes":[{"q":{"a":3},"limit":1}]}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let ours = profiled(&mut m);
    let theirs: Vec<Document> = teseladb_names::expand_static(include_str!("profile/plans.out"))
        .lines()
        .map(|l| extjson::parse(l).expect("a profile document"))
        .filter(|d| d.get("op") != Some(&Value::from("command")))
        .collect();
    assert_eq!(ours.len(), theirs.len(), "{ours:?}");
    for (got, want) in ours.iter().zip(&theirs) {
        let keys = |d: &Document| -> Vec<String> {
            d.iter()
                .map(|(k, _)| k.to_owned())
                .filter(|k| !NOT_OURS.contains(&k.as_str()))
                .collect()
        };
        let op = got.get("op").cloned();
        // The write statements of `update` and `delete` keep their own shape (006).
        if op == Some(Value::from("query")) {
            assert_eq!(keys(got), keys(want), "{got:?}");
        }
        for k in ["keysExamined", "docsExamined"] {
            let n = |d: &Document| d.get(k).and_then(teseladb_wire::types::number);
            assert_eq!(n(got), n(want), "{k} of {op:?}: {got:?}");
        }
        let stage = |d: &Document| match d.get("execStats") {
            Some(Value::Document(s)) => s.get("stage").cloned(),
            _ => None,
        };
        if op == Some(Value::from("query")) {
            assert_eq!(got.get("planSummary"), want.get("planSummary"), "{got:?}");
            assert_eq!(stage(got), stage(want), "{got:?}");
            for k in ["queryHash", "planCacheKey", "planCacheShapeHash"] {
                assert!(
                    matches!(got.get(k), Some(Value::String(h)) if h.len() == 8),
                    "{k}: {got:?}"
                );
            }
        }
    }
}

/// Passwords stay out of `system.profile` and the log: a reference replica set member 9.0.4
/// profiles no `createUser` nor `updateUser` (measured 01-11-2026 at level 2), and a slow one's log
/// line shows `pwd` as `"xxx"`.
#[test]
fn passwords_are_neither_profiled_nor_logged() {
    let mut m = node();
    // A filter that every operation meets profiles and logs each one.
    run(&mut m, DB, json(r#"{"profile":2,"filter":{}}"#));
    run(&mut m, DB, json(r#"{"createUser":"u1","pwd":"secret-1","roles":[]}"#));
    run(&mut m, DB, json(r#"{"updateUser":"u1","pwd":"secret-2"}"#));
    run(&mut m, DB, json(r#"{"profile":0,"filter":"unset"}"#));
    let docs = profiled(&mut m);
    assert!(docs.is_empty(), "{docs:?}");
    let logged = format!("{:?}", m.logged);
    assert!(!logged.contains("secret-"), "{logged}");
    let slow: Vec<&Document> =
        m.logged.iter().filter(|(id, _, _)| *id == SLOW_QUERY_ID).map(|(_, _, a)| a).collect();
    assert_eq!(slow.len(), 2, "{logged}");
    for a in slow {
        assert_eq!(field(a, "command.pwd"), &Value::from("xxx"), "{a:?}");
    }
}

/// What the profiler counts of each operation, as a reference replica set member 9.0.4 writes it at
/// level 2 (measured 01-11-2026 with an index `a_1`): `opid`, one batch per `find` and `getMore`,
/// the index keys each write inserted and deleted (`_id`'s among them), the documents an update
/// upserted, the framework of each read and how long its planning took.
#[test]
fn profiled_operations_count_what_they_did() {
    let mut m = node();
    run(
        &mut m,
        DB,
        json(r#"{"insert":"c","documents":[{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3,"a":3}]}"#),
    );
    run(&mut m, DB, json(r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#));
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{"a":{"$gte":1}}}"#));
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":4,"a":4}]}"#));
    run(
        &mut m,
        DB,
        json(r#"{"update":"c","updates":[{"q":{"_id":9},"u":{"$set":{"a":9}},"upsert":true}]}"#),
    );
    run(&mut m, DB, json(r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"a":10}}}]}"#));
    run(&mut m, DB, json(r#"{"delete":"c","deletes":[{"q":{"_id":2},"limit":1}]}"#));
    run(
        &mut m,
        DB,
        json(r#"{"aggregate":"c","pipeline":[{"$match":{"a":{"$gt":0}}}],"cursor":{}}"#),
    );
    run(&mut m, DB, json(r#"{"aggregate":"c","pipeline":[{"$group":{"_id":"$a"}}],"cursor":{}}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let docs = profiled(&mut m);
    let fields = ["opid", "nBatches", "keysInserted", "keysDeleted", "nUpserted", "queryFramework"];
    let got: Vec<String> = docs
        .iter()
        .map(|d| {
            let shown: Vec<String> = fields
                .iter()
                .filter_map(|k| {
                    d.get(k).map(|v| match (*k, v) {
                        ("opid", Value::Int32(n)) if *n > 0 => "opid".to_owned(),
                        (_, Value::String(s)) => format!("{k}={s}"),
                        (_, v) => {
                            format!("{k}={}", teseladb_wire::types::number(v).unwrap_or(-1.0))
                        }
                    })
                })
                .collect();
            let planned = matches!(d.get("planningTimeMicros"), Some(Value::Int64(n)) if *n >= 0);
            format!(
                "{} {} {planned}",
                d.get("op").map(|v| format!("{v:?}")).unwrap_or_default(),
                shown.join(",")
            )
        })
        .collect();
    let want = [
        r#"String("query") opid,nBatches=1,queryFramework=classic true"#,
        r#"String("insert") opid,keysInserted=2 false"#,
        r#"String("update") opid,keysInserted=2,nUpserted=1 true"#,
        r#"String("update") opid,keysInserted=1,keysDeleted=1,nUpserted=0 true"#,
        r#"String("remove") opid,keysDeleted=2 true"#,
        r#"String("command") opid,queryFramework=classic true"#,
        r#"String("command") opid,queryFramework=sbe true"#,
    ];
    assert_eq!(got, want, "{docs:?}");
}

/// `system.profile` has no index of `_id` (measured on a reference replica set member 9.0.4,
/// 01-11-2026): `listIndexes` lists none, `collStats` counts none and `listCollections` shows no
/// `idIndex`.
#[test]
fn system_profile_has_no_id_index() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{}}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let r = run(&mut m, DB, json(r#"{"listIndexes":"system.profile"}"#));
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(Vec::new()), "{r:?}");
    let r = run(&mut m, DB, json(r#"{"collStats":"system.profile"}"#));
    assert_eq!(r.get("nindexes").and_then(teseladb_wire::types::number), Some(0.0), "{r:?}");
    assert_eq!(r.get("totalIndexSize").and_then(teseladb_wire::types::number), Some(0.0));
    assert_eq!(r.get("indexSizes"), Some(&Value::Document(Document::new())), "{r:?}");
    let r = run(&mut m, DB, json(r#"{"listCollections":1,"filter":{"name":"system.profile"}}"#));
    let Value::Array(colls) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let Some(Value::Document(c)) = colls.first() else { panic!("{r:?}") };
    assert!(c.get("idIndex").is_none(), "{c:?}");
}

/// `$indexStats` counts the reads and the writes whose plan scanned each index, those by one `_id`
/// among them (measured on a reference replica set member 9.0.4, 01-11-2026: `_id_` 2 and `a_1` 2
/// after these statements).
#[test]
fn index_stats_count_reads_and_writes() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":1,"a":1},{"_id":2,"a":2}]}"#));
    run(&mut m, DB, json(r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#));
    run(&mut m, DB, json(r#"{"update":"c","updates":[{"q":{"a":1},"u":{"$set":{"b":1}}}]}"#));
    run(&mut m, DB, json(r#"{"delete":"c","deletes":[{"q":{"a":2},"limit":1}]}"#));
    run(&mut m, DB, json(r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"b":2}}}]}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{"_id":1}}"#));
    let pipeline = r#"[{"$indexStats":{}},{"$project":{"_id":0,"name":1,"o":"$accesses.ops"}},{"$sort":{"name":1}}]"#;
    let r = run(
        &mut m,
        DB,
        json(&format!(r#"{{"aggregate":"c","pipeline":{pipeline},"cursor":{{}}}}"#)),
    );
    let want = json(
        r#"{"b":[{"name":"_id_","o":{"$numberLong":"2"}},{"name":"a_1","o":{"$numberLong":"2"}}]}"#,
    );
    assert_eq!(field(&r, "cursor.firstBatch"), want.get("b").expect("batch"), "{r:?}");
}

/// A `getMore` is profiled with the command that opened its cursor, that cursor's id and the
/// framework that runs it; the command that leaves its cursor open gives the id too (measured on a
/// reference replica set member 9.0.4, 01-11-2026: a `$group` runs on the slot-based engine, a
/// `find` on the classic one).
#[test]
fn get_more_is_profiled_with_its_originating_command() {
    let mut m = node();
    run(
        &mut m,
        DB,
        json(r#"{"insert":"c","documents":[{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3,"a":3}]}"#),
    );
    run(&mut m, DB, json(r#"{"profile":2}"#));
    let group =
        json(r#"{"aggregate":"c","pipeline":[{"$group":{"_id":"$a"}}],"cursor":{"batchSize":1}}"#);
    let r = run(&mut m, DB, group);
    let agg_id = field(&r, "cursor.id").clone();
    run(&mut m, DB, Document::new().with("getMore", agg_id.clone()).with("collection", "c"));
    let r = run(&mut m, DB, json(r#"{"find":"c","batchSize":1}"#));
    let find_id = field(&r, "cursor.id").clone();
    run(&mut m, DB, Document::new().with("getMore", find_id.clone()).with("collection", "c"));
    run(&mut m, DB, json(r#"{"find":"c","batchSize":10}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let docs = profiled(&mut m);
    let shown: Vec<String> = docs
        .iter()
        .map(|d| {
            let keys: Vec<&str> = d.iter().map(|(k, _)| k).collect();
            let origin = match d.get("originatingCommand") {
                Some(Value::Document(o)) => o.iter().next().map_or("", |(k, _)| k).to_owned(),
                _ => "-".to_owned(),
            };
            let id = match d.get("cursorid") {
                Some(v) if *v == agg_id => "agg",
                Some(v) if *v == find_id => "find",
                Some(_) => "other",
                None => "-",
            };
            let framework = match d.get("queryFramework") {
                Some(Value::String(s)) => s.as_str(),
                _ => "-",
            };
            format!("{:?} {origin} {id} {framework} {:?}", d.get("op"), &keys[..keys.len().min(5)])
        })
        .collect();
    let s = |v: &str| v.to_owned();
    let want = [
        s(r#"Some(String("command")) - agg sbe ["op", "ns", "command", "opid", "cursorid"]"#),
        s(
            r#"Some(String("getmore")) aggregate agg sbe ["op", "ns", "command", "originatingCommand", "opid"]"#,
        ),
        s(r#"Some(String("query")) - find classic ["op", "ns", "command", "opid", "cursorid"]"#),
        s(
            r#"Some(String("getmore")) find find classic ["op", "ns", "command", "originatingCommand", "opid"]"#,
        ),
        s(r#"Some(String("query")) - - classic ["op", "ns", "command", "opid", "keysExamined"]"#),
    ];
    assert_eq!(shown, want, "{docs:?}");
    // What the `getMore` of the pipeline read was read when it opened (measured: 0 and 0).
    assert_eq!(docs[1].get("docsExamined"), Some(&Value::Int64(0)));
}

/// How each read's plan was chosen, as a reference replica set member 9.0.4 profiled it (measured
/// 02-11-2026): the first two reads of a shape compete (`fromMultiPlanner`), the next take the
/// cached plan (`fromPlanCache`), each `getMore` repeats its cursor's, and a cached plan that works
/// ten times its entry's works is dropped (`replanned` with its `replanReason`).
#[test]
fn profiled_reads_say_how_their_plan_was_chosen() {
    let mut m = node();
    let docs: Vec<String> =
        (0..50).map(|i| format!(r#"{{"_id":{i},"a":{},"b":{i}}}"#, i % 5)).collect();
    run(&mut m, DB, json(&format!(r#"{{"insert":"c","documents":[{}]}}"#, docs.join(","))));
    run(
        &mut m,
        DB,
        json(
            r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"},{"key":{"b":1},"name":"b_1"}]}"#,
        ),
    );
    run(&mut m, DB, json(r#"{"profile":2}"#));
    for _ in 0..3 {
        let r =
            run(&mut m, DB, json(r#"{"find":"c","filter":{"a":3,"b":{"$gt":45}},"batchSize":1}"#));
        let id = field(&r, "cursor.id").clone();
        run(&mut m, DB, Document::new().with("getMore", id).with("collection", "c"));
    }
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let more: Vec<String> =
        (100..2100).map(|i| format!(r#"{{"_id":{i},"a":4,"b":{i}}}"#)).collect();
    run(&mut m, DB, json(&format!(r#"{{"insert":"c","documents":[{}]}}"#, more.join(","))));
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{"a":3,"b":{"$gt":45}}}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let how: Vec<(String, Vec<String>)> = profiled(&mut m)
        .iter()
        .filter(|d| matches!(d.get("op"), Some(Value::String(o)) if o == "query" || o == "getmore"))
        .map(|d| {
            let op = match d.get("op") {
                Some(Value::String(o)) => o.clone(),
                _ => String::new(),
            };
            let flags = d
                .iter()
                .map(|(k, _)| k.to_owned())
                .filter(|k| k.starts_with("from") || k.starts_with("replan"))
                .collect();
            (op, flags)
        })
        .collect();
    let flags = |f: &[&str]| f.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let multi = flags(&["fromMultiPlanner"]);
    let cached = flags(&["fromPlanCache"]);
    let want = vec![
        ("query".to_owned(), multi.clone()),
        ("getmore".to_owned(), multi.clone()),
        ("query".to_owned(), multi.clone()),
        ("getmore".to_owned(), multi.clone()),
        ("query".to_owned(), cached.clone()),
        ("getmore".to_owned(), cached),
        ("query".to_owned(), flags(&["fromMultiPlanner", "replanned", "replanReason"])),
    ];
    assert_eq!(how, want);
    let last = profiled(&mut m).pop().expect("the replanned read");
    assert_eq!(
        last.get("replanReason"),
        Some(&Value::from(
            "cached plan was less efficient than expected: expected trial execution to take 5 \
             works but it took at least 50 works"
        )),
        "{last:?}"
    );
}

/// A command that fails is profiled with its error and without what it would have returned; a write
/// refused for a duplicate key with what it inserted (measured on a reference replica set member
/// 9.0.4 on 03-11-2026).
#[test]
fn failed_commands_are_profiled_with_their_error() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":1,"a":1}]}"#));
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"find":"c","filter":{"$bad":1}}"#));
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":1}]}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let docs = profiled(&mut m);
    assert_eq!(docs.len(), 2, "{docs:?}");
    let find = &docs[0];
    let msg = "unknown top level operator: $bad. If you have a field name that starts with a '$' \
               symbol, consider using $getField or $setField.";
    assert_eq!(find.get("op"), Some(&Value::from("query")));
    assert_eq!(find.get("ok"), Some(&Value::Double(0.0)));
    assert_eq!(find.get("errMsg"), Some(&Value::from(msg)));
    assert_eq!(find.get("errName"), Some(&Value::from("BadValue")));
    assert_eq!(find.get("errCode"), Some(&Value::Int32(2)));
    assert_eq!(find.get("nreturned"), None);
    let insert = &docs[1];
    assert_eq!(insert.get("ninserted"), Some(&Value::Int32(0)), "{insert:?}");
    assert_eq!((insert.get("ok"), insert.get("keysInserted")), (None, None), "{insert:?}");
}

/// The key counts of a write that wrote no key are not shown (measured on a reference replica set
/// member 9.0.4 on 03-11-2026).
#[test]
fn writes_of_no_key_show_no_key_counts() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"insert":"c","documents":[{"_id":1,"a":1}]}"#));
    run(&mut m, DB, json(r#"{"profile":2}"#));
    run(&mut m, DB, json(r#"{"delete":"c","deletes":[{"q":{"_id":5},"limit":1}]}"#));
    run(&mut m, DB, json(r#"{"update":"c","updates":[{"q":{"_id":5},"u":{"$set":{"a":1}}}]}"#));
    run(&mut m, DB, json(r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"a":1}}}]}"#));
    run(&mut m, DB, json(r#"{"findAndModify":"c","query":{"_id":9},"update":{"$set":{"a":1}}}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let docs = profiled(&mut m);
    assert_eq!(docs.len(), 4, "{docs:?}");
    for d in &docs {
        assert_eq!((d.get("keysInserted"), d.get("keysDeleted")), (None, None), "{d:?}");
    }
    assert_eq!(docs[0].get("ndeleted"), Some(&Value::Int32(0)));
    assert_eq!(docs[3].get("nUpserted"), Some(&Value::Int64(0)), "{:?}", docs[3]);
}

/// `filter: "unset"` removes the profiler's filter; another string is refused (measured on a
/// the reference replica set member 9.0.4 on 03-11-2026).
#[test]
fn a_filter_is_unset_by_name() {
    let mut m = node();
    run(&mut m, DB, json(r#"{"profile":0,"filter":{"op":"query"}}"#));
    let r = run(&mut m, DB, json(r#"{"profile":-1}"#));
    assert_eq!(field(&r, "filter"), &json(r#"{"op":{"$eq":"query"}}"#).into(), "{r:?}");
    let r = run(&mut m, DB, json(r#"{"profile":0,"filter":"unset"}"#));
    assert!(r.get("filter").is_some(), "the reply shows the filter before: {r:?}");
    let r = run(&mut m, DB, json(r#"{"profile":-1}"#));
    assert_eq!((r.get("filter"), r.get("note")), (None, None), "{r:?}");
    let r = run(&mut m, DB, json(r#"{"profile":0,"filter":"other"}"#));
    assert_eq!(field(&r, "code"), &Value::Int32(2), "{r:?}");
    assert_eq!(field(&r, "errmsg"), &Value::from("Expected an object, or the string 'unset'."));
}

/// A collection scan stops at its limit (H-012 of the native tests): a `find` with `limit: 1`
/// over 200 documents that all match examines one, as a `COLLSCAN` stops at its limit, instead of
/// reading a thousand documents to give one.
#[test]
fn a_collection_scan_stops_at_its_limit() {
    let mut m = node();
    let docs: Vec<String> = (0..200).map(|i| format!(r#"{{"_id":{i},"a":5}}"#)).collect();
    run(&mut m, DB, json(&format!(r#"{{"insert":"c","documents":[{}]}}"#, docs.join(","))));
    run(&mut m, DB, json(r#"{"profile":2}"#));
    let r = run(&mut m, DB, json(r#"{"find":"c","filter":{"a":5},"limit":1}"#));
    run(&mut m, DB, json(r#"{"profile":0}"#));
    let Value::Array(found) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(found.len(), 1);
    let profiled = profiled(&mut m);
    let examined = profiled[0].get("docsExamined").cloned();
    assert_eq!(examined, Some(Value::Int64(1)), "{profiled:?}");
}
