//! `explain` against a `refrouter` 9.0.4 (`tests/explain/explain.out`, 010 FR-003, T018): each
//! command, a tab and RefDb's reply, compared without what belongs to the environment or to
//! RefDb's own engine: the hosts and node names, the build, the timings, the plan hashes, the
//! work counters of each stage (`works`, `advanced`, `needTime`…) and the command's session
//! fields. A `refrouter` names its shard where TeselaDB names the node.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, RawDoc, Value, extjson};

const C: &str = include_str!("explain/c.json");
const O: &str = include_str!("aggregate/o.json");
const DB: &str = "xa";

/// The fields left out of the comparison.
const IGNORED: [&str; 36] = [
    "optimizationTimeMillis",
    "expressionEvaluationPeakMemoryBytes",
    "totalDataSizeSortedBytesEstimate",
    "totalOutputDataSizeBytes",
    "maxAccumulatorMemoryUsageBytes",
    "slotBasedPlan",
    "serverInfo",
    "host",
    "shardName",
    "connectionString",
    "queryHash",
    "planCacheShapeHash",
    "planCacheKey",
    "queryShapeHash",
    "optimizationTimeMicros",
    "executionTimeMillis",
    "executionTimeMicros",
    "executionTimeMillisEstimate",
    "totalChildMillis",
    "works",
    "advanced",
    "needTime",
    "needYield",
    "saveState",
    "restoreState",
    "isEOF",
    "seeks",
    "alreadyHasObj",
    "lsid",
    "$clusterTime",
    "multiKeyPaths",
    "operationTime",
    "runtimeConstants",
    "totalDataSizeSorted",
    "peakTrackedMemBytes",
    // The bytes an `AND_HASH` holds, by each engine's own accounting.
    "memUsage",
];

fn load(m: &mut Mem) {
    for (coll, text) in [("c", C), ("o", O)] {
        let holder = extjson::parse(&format!(r#"{{"d":{text}}}"#)).expect("documents");
        let docs = holder.get("d").cloned().expect("d");
        let r = m.run(DB, doc(&[("insert", coll.into()), ("documents", docs)]));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
}

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

/// `v` without the ignored fields; the shards of `shards` under their node names; `ok` as the
/// number 1.
fn normal(v: &Value) -> Value {
    normal_in(v, false)
}

/// [`normal`] inside a section of RefDb's slot-based engine (`engine`), whose
/// `executionStages` are that engine's own program (a divergence in the report).
fn normal_in(v: &Value, engine: bool) -> Value {
    match v {
        Value::Document(d) => {
            let engine =
                engine || matches!(d.get("explainVersion"), Some(Value::String(e)) if e == "2");
            let mut out = Document::new();
            for (k, x) in d.iter() {
                if IGNORED.contains(&k) || (engine && k == "executionStages") {
                    continue;
                }
                let x = match (k, x) {
                    ("ok", _) => Value::Double(1.0),
                    // Plans whose trials tie keep an order of RefDb's enumerator that
                    // TeselaDB does not reproduce (DV in the report): compared as a set.
                    ("rejectedPlans", Value::Array(plans)) => {
                        let mut list: Vec<(String, Value)> = plans
                            .iter()
                            .map(|p| {
                                let n = normal_in(p, engine);
                                let key = match &n {
                                    Value::Document(d) => canonical(d),
                                    _ => String::new(),
                                };
                                (key, n)
                            })
                            .collect();
                        list.sort_by(|a, b| a.0.cmp(&b.0));
                        Value::Array(list.into_iter().map(|(_, v)| v).collect())
                    }
                    // A shard's reply under `raw`, keyed `sh<N>/host:port`; TeselaDB's
                    // `node<N>/host:port` (D-102), compared without the host.
                    ("raw", Value::Document(s)) => {
                        let mut one = Document::new();
                        for (name, part) in s.iter() {
                            let shard = name.split('/').next().unwrap_or_default();
                            let node = match shard.strip_prefix("sh") {
                                Some(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
                                    format!("node{n}")
                                }
                                _ => shard.to_owned(),
                            };
                            one.push(format!("{node}/host"), normal_in(part, engine));
                        }
                        Value::Document(one)
                    }
                    // A `refrouter` keys its shards `sh<N>`; TeselaDB its nodes `node<N>` (D-102).
                    ("shards", Value::Document(s)) => {
                        let mut one = Document::new();
                        for (name, node) in s.iter() {
                            let name = match name.strip_prefix("sh") {
                                Some(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
                                    format!("node{n}")
                                }
                                _ => name.to_owned(),
                            };
                            one.push(&name, normal_in(node, engine));
                        }
                        Value::Document(one)
                    }
                    _ => normal_in(x, engine),
                };
                out.push(k, x);
            }
            Value::Document(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| normal_in(i, engine)).collect()),
        other => other.clone(),
    }
}

fn shown(r: &Document) -> String {
    match (r.get("ok"), r.get("code"), r.get("codeName"), r.get("errmsg")) {
        (
            Some(Value::Double(x)),
            Some(Value::Int32(code)),
            Some(Value::String(name)),
            Some(Value::String(msg)),
        ) if *x == 0.0 => {
            format!("!! {code}\t{name}\t{msg}")
        }
        _ => match normal(&Value::Document(r.clone())) {
            Value::Document(d) => format!("=> {}", canonical(&d)),
            _ => String::new(),
        },
    }
}

#[test]
fn explain_answers_as_refdb() {
    check(include_str!("explain/explain.out"));
}

/// `explain` of `aggregate` (`tests/explain/aggregate.out`): the query plan of the leading
/// stages, the slot-based engine's when a `$group` or an equality `$lookup` follows them.
#[test]
fn explain_of_aggregate_answers_as_refdb() {
    check(include_str!("explain/aggregate.out"));
}

/// More pipelines (`tests/explain/aggregate2.out`): the forms of the stages after the plan and
/// the stages RefDb moves before others.
#[test]
fn explain_of_more_pipelines_answers_as_refdb() {
    check(include_str!("explain/aggregate2.out"));
}

/// The filters of `find` as RefDb plans them (`tests/explain/filters.out`): each `$expr`
/// optimised, with the clauses it implies, a constant one settling the filter, `$in` and
/// disjunctions rewritten, `$jsonSchema` translated.
#[test]
fn explain_of_filters_answers_as_refdb() {
    check(include_str!("explain/filters.out"));
}

/// `explain` of reads and writes on views (`tests/explain/views.out`, 015 FR-001): the
/// explain of the aggregation each read becomes on the view's collection; its commands run in
/// order on one node, the views first.
#[test]
fn explain_on_views_answers_as_refdb() {
    check_in(include_str!("explain/views.out"), false);
}

/// `explain` with indexes (`tests/explain/plans.out`, 010 FR-001): the plan the planner chooses,
/// the ones it rejects and what the winner counts; the indexes come first.
#[test]
fn explain_of_index_plans_answers_as_refdb() {
    check_in(include_str!("explain/plans.out"), false);
}

/// Reads that an index plan answers (`tests/explain/plan_reads.out`, 010 FR-001): the
/// documents in the order of the index that wins, as a `refrouter` 9.0.4 gives them.
#[test]
fn reads_through_index_plans_answer_as_refdb() {
    check_in(include_str!("explain/plan_reads.out"), false);
}

/// Replies that name a `refrouter`'s shards (`tests/explain/shards.out`, DV-031, D-102): the
/// node in their place, as `node<N>`.
#[test]
fn shard_named_replies_answer_as_refdb() {
    check_in(include_str!("explain/shards.out"), false);
}

/// Writes, counts and `distinct` that an index plan answers (`tests/explain/plan_writes.out`,
/// 010 FR-001, T019): the document an update of one, a delete of one or a `findAndModify`
/// without a sort takes is the first the winning plan gives, also with a `hint`.
#[test]
fn writes_through_index_plans_answer_as_refdb() {
    check_in(include_str!("explain/plan_writes.out"), false);
}

/// More reads and writes through index plans (`tests/explain/plan_more.out`, T019): a hidden
/// index left out, a `$or` whose clauses find a document twice, a limit, an update of many and
/// one with a collation, and an index of more entries than a scan reads at once.
#[test]
fn more_reads_through_index_plans_answer_as_refdb() {
    check_in(include_str!("explain/plan_more.out"), false);
}

/// More `explain` with indexes (`tests/explain/plans_more.out`, T018): counts with a skip and a
/// limit, a descending index and an `_id` that is not there, `distinct` by its index, covered
/// and uncovered projections, a collation, and writes that take one document.
#[test]
fn more_index_plans_answer_as_refdb() {
    check_in(include_str!("explain/plans_more.out"), false);
}

/// Plans that intersect index scans (`tests/explain/hash_*.out`, 010 FR-001, T016), measured
/// with `internalQueryPlannerEnableHashIntersection` on the shard: each file on a node of its
/// own with the parameter set.
#[test]
fn hash_intersection_plans_answer_as_refdb() {
    for file in [
        include_str!("explain/hash_ix2.out"),
        include_str!("explain/hash_ix3.out"),
        include_str!("explain/hash_ix4.out"),
    ] {
        let mut m = Mem::new();
        load(&mut m);
        let set = doc(&[
            ("setParameter", Value::Int32(1)),
            ("internalQueryPlannerEnableHashIntersection", Value::Boolean(true)),
        ]);
        let r = m.run("admin", set);
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
        check_on(&mut m, file, false);
    }
}

/// The edges of `explain` (`tests/explain/edges.out`, T018): a `$db` inside the command, a read
/// concern given to `aggregate`, a collection that does not exist, an empty sort, `_id`
/// equalities with a skip, a collation or `multi`, a `limit` written as a double, a sort in
/// `findAndModify`, and index plans that cannot cover or count by keys alone.
#[test]
fn explain_edges_answer_as_refdb() {
    check_in(include_str!("explain/edges.out"), false);
}

/// `aggregate` whose leading `$match` reads through an index (`tests/explain/agg_index.out`,
/// 010 FR-001): the planner's plan under the stages after it, with the plans it rejected.
#[test]
fn aggregate_reads_through_index_plans_as_refdb() {
    check_in(include_str!("explain/agg_index.out"), false);
}

/// More replies that name a `refrouter`'s shards (`tests/explain/shards2.out`, DV-031, D-102): the
/// index commands' `raw`, keyed `node<N>/host:port`, and a `writeConcernError` ` at node<N>`.
#[test]
fn shard_named_parts_answer_as_refdb() {
    check_in(include_str!("explain/shards2.out"), false);
    let mut m = Mem::new();
    load(&mut m);
    let create =
        extjson::parse(r#"{"createIndexes":"c","indexes":[{"key":{"q":1},"name":"q_1"}]}"#)
            .expect("command");
    let r = m.run(DB, create);
    let Some(Value::Document(raw)) = r.get("raw") else { panic!("{r:?}") };
    let facts = &m.facts;
    let name = format!("node1/{}:{}", facts.host.hostname, facts.port);
    assert_eq!(raw.iter().map(|(k, _)| k).collect::<Vec<_>>(), [name.as_str()], "{r:?}");
}

/// `text` with the host a `refrouter` names in «cluster write results unavailable from host:port»
/// left out.
fn without_host(text: &str) -> String {
    const FROM: &str = "unavailable from ";
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(FROM) {
        out.push_str(&rest[..i + FROM.len()]);
        rest = &rest[i + FROM.len()..];
        let end = rest.find(' ').unwrap_or(rest.len());
        out.push_str("host");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn check(file: &str) {
    check_in(file, true);
}

/// Checks each command of `file` against RefDb's reply, each on a node of its own (`fresh`)
/// or all on one.
fn check_in(file: &str, fresh: bool) {
    let mut m = Mem::new();
    load(&mut m);
    check_on(&mut m, file, fresh);
}

/// [`check_in`] on node `m`.
fn check_on(m: &mut Mem, file: &str, fresh: bool) {
    let mut wrong = Vec::new();
    for line in file.lines().filter(|l| !l.is_empty()) {
        let (cmd, want) = line.split_once('\t').expect("a command and its reply");
        if fresh {
            *m = Mem::new();
            load(m);
        }
        let c = extjson::parse(cmd).expect("command");
        let name = c.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default();
        let got = match teseladb_wire::generic::parse(&c, &name) {
            Err(e) => shown(&e.to_document()),
            Ok(_) => shown(&m.run(DB, c)),
        };
        // A `refrouter` names the shard that failed; TeselaDB names the node (D-102).
        let want = want
            .replace("on shard sh1 failed", "on shard node1 failed")
            .replace("failed on: sh1 :: ", "failed on: node1 :: ")
            .replace(" at sh1\"", " at node1\"");
        // An error message's line breaks are written `\n` in the fixture.
        let want = match want.strip_prefix("=> ") {
            Some(json) => shown(&extjson::parse(json).expect("reply")),
            None => want.replace("\\n", "\n"),
        };
        let (got, want) = (without_host(&got), without_host(&want));
        if got != want {
            wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}
