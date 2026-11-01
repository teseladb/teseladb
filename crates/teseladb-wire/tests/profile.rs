//! The profiler (010 T032, FR-015): `profile`'s replies and errors against what a `refnode`
//! 9.0.4 gave on 01-11-2026, the documents a profiled database gets in `system.profile`, and the
//! «Slow query» lines of the log.

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

/// Each command of the measured sequence and the reply of a `refnode` 9.0.4 (`ok` and the
/// numbers as the shell printed them, compared by value).
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
fn profile_replies_and_errors_match_a_refnode() {
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

/// The fields of a `system.profile` document that TeselaDB has no counterpart of: RefEngine's
/// locks and queues, the classic engine's planner and the priority port (DV-038).
const NOT_OURS: [&str; 13] = [
    "isFromPriorityPortConnection",
    // The test's connection sends no application name.
    "appName",
    "opid",
    "nBatches",
    "queryShapeHash",
    "queryFramework",
    "planRanker",
    "locks",
    "flowControl",
    "queues",
    "planningTimeMicros",
    "totalOplogSlotDurationMicros",
    "cpuNanos",
];

/// The plan of each read in `system.profile` (010 FR-015), against what a `refnode` 9.0.4 wrote
/// on 01-11-2026 (`tests/profile/plans.out`): the fields in its order, the counters of what each
/// read examined, its `planSummary` and the `execStats` of the plan as it ran. The hashes are
/// TeselaDB's; `cpuNanos` comes from the server's thread clock, which this node in tests has not.
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
    let theirs: Vec<Document> = include_str!("profile/plans.out")
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

/// Passwords stay out of `system.profile` and the log: a `refnode` 9.0.4 profiles no
/// `createUser` nor `updateUser` (measured 01-11-2026 at level 2), and a slow one's log line
/// shows `pwd` as `"xxx"`.
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
