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
const IGNORED: [&str; 33] = [
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
    check(teseladb_names::expand_static(include_str!("explain/explain.out")));
}

/// `explain` of `aggregate` (`tests/explain/aggregate.out`): the query plan of the leading
/// stages, the slot-based engine's when a `$group` or an equality `$lookup` follows them.
#[test]
fn explain_of_aggregate_answers_as_refdb() {
    check(teseladb_names::expand_static(include_str!("explain/aggregate.out")));
}

/// Pipelines whose leading `$match` is one `_id` equality (`tests/explain/agg_express.out`,
/// recorded on a native `refrouter` 9.0.4 on 09-11-2026, H-011 of the native tests): RefDb reads
/// them by the express path (`EXPRESS_IXSCAN`, or `IDHACK` under a projection it cannot take),
/// with its sort and limit folded in, a simple projection in the stage and the rest of the
/// pipeline after a `$cursor`, never in the slot-based engine.
#[test]
fn explain_of_aggregate_by_id_answers_as_refdb() {
    check(teseladb_names::expand_static(include_str!("explain/agg_express.out")));
}

/// `count` and `distinct` by one `_id` equality (`tests/explain/id_express.out`, recorded on a
/// native `refrouter` 9.0.4 on 09-11-2026, H-011 of the native tests): `COUNT` over `IDHACK` and
/// `EXPRESS_IXSCAN`, without the planner, and their answers.
#[test]
fn count_and_distinct_by_id_answer_as_refdb() {
    check(teseladb_names::expand_static(include_str!("explain/id_express.out")));
}

/// More pipelines (`tests/explain/aggregate2.out`): the forms of the stages after the plan and
/// the stages RefDb moves before others.
#[test]
fn explain_of_more_pipelines_answers_as_refdb() {
    check(teseladb_names::expand_static(include_str!("explain/aggregate2.out")));
}

/// The filters of `find` as RefDb plans them (`tests/explain/filters.out`): each `$expr`
/// optimised, with the clauses it implies, a constant one settling the filter, `$in` and
/// disjunctions rewritten, `$jsonSchema` translated.
#[test]
fn explain_of_filters_answers_as_refdb() {
    check(teseladb_names::expand_static(include_str!("explain/filters.out")));
}

/// `explain` of reads and writes on views (`tests/explain/views.out`, 015 FR-001): the
/// explain of the aggregation each read becomes on the view's collection; its commands run in
/// order on one node, the views first.
#[test]
fn explain_on_views_answers_as_refdb() {
    check_in(teseladb_names::expand_static(include_str!("explain/views.out")), false);
}

/// `explain` with indexes (`tests/explain/plans.out`, 010 FR-001): the plan the planner chooses,
/// the ones it rejects and what the winner counts; the indexes come first.
#[test]
fn explain_of_index_plans_answers_as_refdb() {
    check_in(teseladb_names::expand_static(include_str!("explain/plans.out")), false);
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
    check_in(teseladb_names::expand_static(include_str!("explain/plan_more.out")), false);
}

/// More `explain` with indexes (`tests/explain/plans_more.out`, T018): counts with a skip and a
/// limit, a descending index and an `_id` that is not there, `distinct` by its index, covered
/// and uncovered projections, a collation, and writes that take one document.
#[test]
fn more_index_plans_answer_as_refdb() {
    check_in(teseladb_names::expand_static(include_str!("explain/plans_more.out")), false);
}

/// Plans that intersect index scans (`tests/explain/hash_*.out`, 010 FR-001, T016), measured
/// with `internalQueryPlannerEnableHashIntersection` on the shard: each file on a node of its
/// own with the parameter set.
#[test]
fn hash_intersection_plans_answer_as_refdb() {
    for file in [
        teseladb_names::expand_static(include_str!("explain/hash_ix2.out")),
        teseladb_names::expand_static(include_str!("explain/hash_ix3.out")),
        teseladb_names::expand_static(include_str!("explain/hash_ix4.out")),
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
    check_in(teseladb_names::expand_static(include_str!("explain/edges.out")), false);
}

/// `aggregate` whose leading `$match` reads through an index (`tests/explain/agg_index.out`,
/// 010 FR-001): the planner's plan under the stages after it, with the plans it rejected.
#[test]
fn aggregate_reads_through_index_plans_as_refdb() {
    check_in(teseladb_names::expand_static(include_str!("explain/agg_index.out")), false);
}

/// The index of `_id` as one more candidate of the planner (`tests/explain/plan_id.out`, 010
/// FR-001, T019): `find`, `count`, `distinct`, the writes and `aggregate` read `_id` ranges, `$in`
/// and sorts on `_id` through it, in its order, and it competes with the other indexes.
#[test]
fn id_index_plans_answer_as_refdb() {
    check_in(teseladb_names::expand_static(include_str!("explain/plan_id.out")), false);
}

/// A rooted `$or` (`tests/explain/or_*.out`, 010 FR-001, T019): its clauses on the leading field
/// of one index read it once over their joined intervals, a scan of several intervals examines
/// the key that ends each one, and the `SUBPLAN` stands over the stages of the whole read
/// (limit, sort, projection, skip) and under `COUNT`, `UPDATE` and `DELETE`.
#[test]
fn or_plans_answer_as_refdb() {
    for file in [
        teseladb_names::expand_static(include_str!("explain/or_join.out")),
        teseladb_names::expand_static(include_str!("explain/or_join2.out")),
        teseladb_names::expand_static(include_str!("explain/or_subplan.out")),
    ] {
        check_in(file, false);
    }
}

/// `distinct` and `findAndModify` with a query through the planner
/// (`tests/explain/plan_distinct.out`, T019): a scan of the key's index after fields the query
/// fixes reads one key per value (`DISTINCT_SCAN`), `findAndModify` takes the first document of
/// the plan or of its sort, and a skip goes under a fetch without filter.
#[test]
fn distinct_and_find_and_modify_plans_answer_as_refdb() {
    check_in(teseladb_names::expand_static(include_str!("explain/plan_distinct.out")), false);
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

/// The same replies on collections kept in `_id` order, the layout a node creates once every
/// node reads it (D-153): RefDb's index of `_id` is in order too, so the plans and their
/// counters are the same (H-011 of the native tests: `$in` counted fewer keys on that layout,
/// which only the hash-bucket layout of these tests ran).
#[test]
fn explain_answers_as_refdb_on_collections_in_id_order() {
    let files = [
        (teseladb_names::expand_static(include_str!("explain/explain.out")), true),
        (teseladb_names::expand_static(include_str!("explain/aggregate.out")), true),
        (teseladb_names::expand_static(include_str!("explain/agg_express.out")), true),
        (teseladb_names::expand_static(include_str!("explain/id_express.out")), true),
        (teseladb_names::expand_static(include_str!("explain/aggregate2.out")), true),
        (teseladb_names::expand_static(include_str!("explain/filters.out")), true),
        (teseladb_names::expand_static(include_str!("explain/views.out")), false),
        (teseladb_names::expand_static(include_str!("explain/plans.out")), false),
        (include_str!("explain/plan_reads.out"), false),
        (include_str!("explain/shards.out"), false),
        (include_str!("explain/plan_writes.out"), false),
        (teseladb_names::expand_static(include_str!("explain/plan_more.out")), false),
        (teseladb_names::expand_static(include_str!("explain/plans_more.out")), false),
        (teseladb_names::expand_static(include_str!("explain/edges.out")), false),
        (teseladb_names::expand_static(include_str!("explain/agg_index.out")), false),
        (teseladb_names::expand_static(include_str!("explain/plan_id.out")), false),
        (teseladb_names::expand_static(include_str!("explain/plan_distinct.out")), false),
        (include_str!("explain/shards2.out"), false),
        (teseladb_names::expand_static(include_str!("explain/plan_checked.out")), false),
    ];
    for (file, fresh) in files {
        let mut m = Mem::new();
        m.ordered_ids = true;
        load(&mut m);
        check_on(&mut m, file, fresh);
    }
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
            let ordered = m.ordered_ids;
            *m = Mem::new();
            m.ordered_ids = ordered;
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

/// The fields of a plan cache entry that belong to the moment or to RefDb's own sizes and
/// solution hash, and the work counters of each stage, left out of the comparison; the plan
/// cache hashes are compared (H-028).
const CACHE_VOLATILE: [&str; 19] = [
    "totalDataSizeSorted",
    "timeOfCreation",
    "estimatedSizeBytes",
    "solutionHash",
    "host",
    "shard",
    "executionTimeMillisEstimate",
    "works",
    "advanced",
    "needTime",
    "needYield",
    "saveState",
    "restoreState",
    "isEOF",
    "seeks",
    "peakTrackedMemBytes",
    "operationTime",
    "$clusterTime",
    "multiKeyPaths",
];

/// `v` without [`CACHE_VOLATILE`], except an entry's own `works`, and with a `refrouter`'s part
/// of shard `sh1` as the node's `node1` (D-102).
fn cache_normal(v: &Value, entry: bool) -> Value {
    match v {
        Value::Document(d) => {
            let is_entry = d.get("cachedPlan").is_some();
            let mut out = Document::new();
            for (k, x) in d.iter() {
                if k == "sh1" && matches!(x, Value::Document(p) if p.get("ok").is_some()) {
                    out.push("node1", Document::new().with("ok", 1.0));
                    continue;
                }
                if CACHE_VOLATILE.contains(&k) && !(k == "works" && is_entry) {
                    continue;
                }
                let x = if k == "ok" { Value::Double(1.0) } else { x.clone() };
                out.push(k, cache_normal(&x, entry || is_entry));
            }
            Value::Document(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| cache_normal(i, entry)).collect()),
        Value::Int64(n) if !entry => Value::Int64(*n),
        other => other.clone(),
    }
}

/// The plan cache (`tests/explain/plan_cache*.out`, 010 FR-002, T017): a read whose plans
/// competed leaves an inactive entry of its shape that the next read activates, `explain`
/// leaves none, `planCacheClear` drops one shape or all, building an index drops them, and
/// `$planCacheStats` lists them with their plan and trials, as a `refrouter` 9.0.4 does.
#[test]
fn plan_cache_answers_as_refdb() {
    for file in [
        include_str!("explain/plan_cache.out"),
        teseladb_names::expand_static(include_str!("explain/plan_cache2.out")),
    ] {
        let mut m = Mem::new();
        load(&mut m);
        let mut wrong = Vec::new();
        let mut text = String::new();
        let mut lines: Vec<String> = Vec::new();
        for line in file.lines() {
            if line.starts_with("{\"") && !text.is_empty() {
                lines.push(std::mem::take(&mut text));
            }
            if !text.is_empty() {
                text.push_str("\\n");
            }
            text.push_str(line);
        }
        lines.push(text);
        for line in lines.iter().filter(|l| !l.is_empty()) {
            let (cmd, want) = line.split_once('\t').expect("a command and its reply");
            let c = extjson::parse(cmd).expect("command");
            let name = c.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default();
            let got = match teseladb_wire::generic::parse(&c, &name) {
                Err(e) => e.to_document(),
                Ok(_) => m.run(DB, c),
            };
            if cmd.starts_with("{\"explain\"") {
                let want = match want.strip_prefix("=> ") {
                    Some(json) => shown(&extjson::parse(json).expect("reply")),
                    None => want.to_owned(),
                };
                let got = shown(&got);
                if got != want {
                    wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
                }
                continue;
            }
            let got = match (got.get("code"), got.get("codeName"), got.get("errmsg")) {
                (Some(Value::Int32(code)), Some(Value::String(n)), Some(Value::String(e))) => {
                    format!("!! {code}\t{n}\t{e}")
                }
                _ => canonical_value(&cache_normal(&Value::Document(got), false)),
            };
            let want = want
                .replace("sh1/lc12-sh1:27018", "node1/:0")
                .replace("\"shards\":{\"sh1\"", "\"shards\":{\"node1\"");
            let want = match want.strip_prefix("=> ") {
                Some(json) => canonical_value(&cache_normal(
                    &Value::Document(extjson::parse(json).expect("reply")),
                    false,
                )),
                None => want.to_owned(),
            };
            if got != want {
                wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
            }
        }
        assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
    }
}

fn canonical_value(v: &Value) -> String {
    match v {
        Value::Document(d) => format!("=> {}", canonical(d)),
        other => format!("{other:?}"),
    }
}

/// Scans whose fields after a range keep their intervals, and `$exists` over a whole index,
/// against a `refrouter` 9.0.4 (`plan_checked.out`, 010 FR-001): each key the bounds checker lands
/// on is examined, the keys outside make it seek.
#[test]
fn scans_that_check_their_keys_answer_as_refdb() {
    check_in(teseladb_names::expand_static(include_str!("explain/plan_checked.out")), false);
}

/// A `distinct` that reads one key per value reads the key after its interval when one is
/// there, and none past the end of the index (measured on a `refnode` 9.0.4 on 02-11-2026 over
/// `g` = "a", "b", "a", "c", 5 with an index on `g`: the values, keys read and documents read).
#[test]
fn a_distinct_scan_reads_the_key_after_its_interval_only_when_there_is_one() {
    let mut m = Mem::new();
    let docs = r#"[{"_id":1,"g":"a"},{"_id":2,"g":"b"},{"_id":3,"g":"a"},{"_id":4,"g":"c"},{"_id":5,"g":5}]"#;
    let r = m.run(DB, extjson::parse(&format!(r#"{{"insert":"kd","documents":{docs}}}"#)).unwrap());
    assert_eq!(field(&r, "n"), &Value::Int32(5), "{r:?}");
    let index = r#"{"createIndexes":"kd","indexes":[{"key":{"g":1},"name":"g_1"}]}"#;
    m.run(DB, extjson::parse(index).unwrap());
    for (query, want) in [
        ("{}", (4, 4, 0)),
        (r#"{"g":{"$gte":"a"}}"#, (3, 3, 0)),
        (r#"{"g":{"$lt":"b"}}"#, (1, 2, 0)),
        (r#"{"g":{"$gte":"b"}}"#, (2, 2, 0)),
    ] {
        let cmd = format!(
            r#"{{"explain":{{"distinct":"kd","key":"g","query":{query}}},"verbosity":"executionStats"}}"#
        );
        let r = m.run(DB, extjson::parse(&cmd).unwrap());
        let n = |path: &str| match field(&r, path) {
            Value::Int32(n) => i64::from(*n),
            Value::Int64(n) => *n,
            other => panic!("{path}: {other:?} in {r:?}"),
        };
        let got = (
            n("executionStats.nReturned"),
            n("executionStats.totalKeysExamined"),
            n("executionStats.totalDocsExamined"),
        );
        assert_eq!(got, want, "{query}: {r:?}");
    }
}

/// A document both scans of an index intersection hold leaves the intersection only when it also
/// matches the rest of the filter (007 T037).
#[test]
fn an_intersection_filters_the_documents_both_scans_hold() {
    let mut m = Mem::new();
    let set = doc(&[
        ("setParameter", Value::Int32(1)),
        ("internalQueryPlannerEnableHashIntersection", Value::Boolean(true)),
    ]);
    let r = m.run("admin", set);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let docs: Vec<Value> = (0..4)
        .map(|i| {
            Value::Document(doc(&[
                ("_id", Value::Int32(i)),
                ("a", Value::Int32(1)),
                ("b", Value::Int32(1)),
                ("c", Value::Int32(i)),
            ]))
        })
        .collect();
    m.run("x", doc(&[("insert", Value::from("i")), ("documents", Value::Array(docs))]));
    for key in ["a", "b"] {
        let index = doc(&[
            ("key", Value::Document(doc(&[(key, Value::Int32(1))]))),
            ("name", Value::from(key)),
        ]);
        let cmd = doc(&[
            ("createIndexes", Value::from("i")),
            ("indexes", Value::Array(vec![Value::Document(index)])),
        ]);
        m.run("x", cmd);
    }
    let filter = doc(&[("a", Value::Int32(1)), ("b", Value::Int32(1)), ("c", Value::Int32(2))]);
    let r = m.run("x", doc(&[("find", Value::from("i")), ("filter", Value::Document(filter))]));
    let Value::Array(found) = field(&r, "cursor.firstBatch").clone() else { panic!("{r:?}") };
    assert_eq!(found.len(), 1, "{r:?}");
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(2), "{r:?}");
}

/// The stages of an execution plan, as `stage` with `keysExamined`, `docsExamined`, `nReturned`
/// and `skipAmount`, nested by `inputStage`.
fn stages_of(v: &Value) -> String {
    let Value::Document(d) = v else { return String::new() };
    let mut out = format!("{:?}", d.get("stage"));
    for k in ["keysExamined", "docsExamined", "nReturned", "skipAmount"] {
        if let Some(n) = d.get(k).and_then(teseladb_wire::types::number) {
            out.push_str(&format!(" {k}={n}"));
        }
    }
    if let Some(input) = d.get("inputStage") {
        out.push_str(&format!(" < {}", stages_of(input)));
    }
    out
}

/// An `_id` equality that finds nothing examines no key; one on a capped collection reads the
/// index too; with a collation and strings, or a skip and a collation, it reads the collection
/// (measured on a `refnode` 9.0.4 on 03-11-2026).
#[test]
fn id_equalities_of_explained_finds() {
    let mut m = Mem::new();
    let run = |m: &mut Mem, cmd: &str| m.run("ex", extjson::parse(cmd).expect("command"));
    run(&mut m, r#"{"create":"cap","capped":true,"size":4096}"#);
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":1}]}"#);
    run(&mut m, r#"{"insert":"s","documents":[{"_id":"a"},{"_id":"b"}]}"#);
    let cases = [
        (
            r#"{"find":"cap","filter":{"_id":99}}"#,
            r#"Some(String("EXPRESS_IXSCAN")) keysExamined=0 docsExamined=0 nReturned=0"#,
        ),
        (
            r#"{"find":"cap","filter":{"_id":1}}"#,
            r#"Some(String("EXPRESS_IXSCAN")) keysExamined=1 docsExamined=1 nReturned=1"#,
        ),
        (
            r#"{"find":"s","filter":{"_id":"zz"}}"#,
            r#"Some(String("EXPRESS_IXSCAN")) keysExamined=0 docsExamined=0 nReturned=0"#,
        ),
        (
            r#"{"find":"s","filter":{"_id":"a"},"skip":1,"collation":{"locale":"en"}}"#,
            r#"Some(String("SKIP")) nReturned=0 skipAmount=1 < Some(String("COLLSCAN")) docsExamined=2 nReturned=1"#,
        ),
        (
            r#"{"find":"s","filter":{"_id":"a"},"collation":{"locale":"en"}}"#,
            r#"Some(String("COLLSCAN")) docsExamined=2 nReturned=1"#,
        ),
    ];
    for (find, want) in cases {
        let cmd = format!(r#"{{"explain":{find},"verbosity":"executionStats"}}"#);
        let r = run(&mut m, &cmd);
        let stages = field(&r, "executionStats.executionStages.shards.0.executionStages");
        assert_eq!(stages_of(stages), want, "{find}: {r:?}");
    }
}

#[test]
fn explain_gives_the_planning_microseconds_and_a_long_child_time() {
    // H-028: a `refnode` 9.0.4 gives `optimizationTimeMicros` right after the milliseconds and the
    // router's `totalChildMillis` as a long (measured on 09-11-2026).
    let mut m = Mem::new();
    let insert = extjson::parse(r#"{"insert":"c","documents":[{"_id":1}]}"#).unwrap();
    m.run(DB, insert);
    let cmd = r#"{"explain":{"find":"c","filter":{"_id":{"$gt":0}}},"verbosity":"executionStats"}"#;
    let r = m.run(DB, extjson::parse(cmd).unwrap());
    let shard = field(&r, "queryPlanner.winningPlan.shards.0").clone();
    let Value::Document(shard) = shard else { panic!("{r:?}") };
    let keys: Vec<&str> = shard.iter().map(|(k, _)| k).collect();
    let at = keys.iter().position(|k| *k == "optimizationTimeMillis").expect("milliseconds");
    assert_eq!(keys.get(at + 1), Some(&"optimizationTimeMicros"), "{keys:?}");
    let child = field(&r, "executionStats.executionStages.totalChildMillis");
    assert!(matches!(child, Value::Int64(_)), "{child:?}");
}

#[test]
fn explain_echoes_the_session_and_cluster_time_it_was_sent() {
    // H-028: a `refrouter` 9.0.4 echoes `lsid` and `$clusterTime` of `explain` after the command's own
    // fields and before `$db`, except in an `aggregate` (measured on 09-11-2026).
    let mut m = Mem::new();
    let lsid = r#"{"id":{"$binary":{"base64":"AAAAAAAAAAAAAAAAAAAAAA==","subType":"04"}}}"#;
    let time = r#"{"clusterTime":{"$timestamp":{"t":1,"i":1}},"signature":{"hash":{"$binary":{"base64":"AAAAAAAAAAAAAAAAAAAAAAAAAAA=","subType":"00"}},"keyId":0}}"#;
    let cmd = format!(
        r#"{{"explain":{{"find":"c","filter":{{"a":1}}}},"lsid":{lsid},"$clusterTime":{time}}}"#
    );
    let r = m.run(DB, extjson::parse(&cmd).unwrap());
    let Value::Document(echo) = field(&r, "command").clone() else { panic!("{r:?}") };
    let keys: Vec<&str> = echo.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["find", "filter", "lsid", "$clusterTime", "$db"], "{r:?}");
    let agg =
        format!(r#"{{"explain":{{"aggregate":"c","pipeline":[],"cursor":{{}}}},"lsid":{lsid}}}"#);
    let r = m.run(DB, extjson::parse(&agg).unwrap());
    let Value::Document(echo) = field(&r, "command").clone() else { panic!("{r:?}") };
    assert!(echo.get("lsid").is_none(), "{echo:?}");
}
