//! Reads the planner answers with the right documents whatever plan wins: indexes it cannot plan
//! with (hashed, partial, sparse, with a collation, hidden), `$or` with a clause no index serves,
//! hash intersections stopped by a limit or a batch, and plans tried against more documents than
//! the trial reads.

mod common;

use common::{Mem, doc, field};
use teseladb_meta::indexes::{IndexEntry, IndexState};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("p", json(cmd))
}

fn ids(r: &Document) -> Vec<i32> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    let mut out: Vec<i32> = b
        .iter()
        .map(|d| match d {
            Value::Document(d) => match field(d, "_id") {
                Value::Int32(n) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// `count` documents: `_id` i, `a` = i mod 10, `b` = i mod 7, `s` a letter.
fn loaded(count: i32) -> Mem {
    let mut m = Mem::new();
    let docs: Vec<String> = (0..count)
        .map(|i| {
            let s = ["x", "Y", "z"][(i % 3) as usize];
            format!(r#"{{"_id":{i},"a":{},"b":{},"s":"{s}"}}"#, i % 10, i % 7)
        })
        .collect();
    for chunk in docs.chunks(1000) {
        let r = run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, chunk.join(",")));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    m
}

fn index(m: &mut Mem, spec: &str) {
    let r = run(m, &format!(r#"{{"createIndexes":"c","indexes":[{spec}]}}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{spec}: {r:?}");
}

fn expected(count: i32, keep: impl Fn(i32) -> bool) -> Vec<i32> {
    (0..count).filter(|i| keep(*i)).collect()
}

#[test]
fn indexes_the_planner_cannot_use_still_give_the_matches() {
    let mut m = loaded(70);
    index(&mut m, r#"{"key":{"a":"hashed"},"name":"ah"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"bp","partialFilterExpression":{"b":{"$gt":3}}}"#);
    index(&mut m, r#"{"key":{"s":1},"name":"ss","sparse":true}"#);
    index(&mut m, r#"{"key":{"s":1,"a":1},"name":"sc","collation":{"locale":"en","strength":2}}"#);
    index(&mut m, r#"{"key":{"a":1,"b":1},"name":"ab","hidden":true}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":3}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 3));
    let r = run(&mut m, r#"{"find":"c","filter":{"b":{"$gt":5}}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 7 > 5));
    let r = run(&mut m, r#"{"find":"c","filter":{"s":"z","a":{"$lt":2}}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 3 == 2 && i % 10 < 2));
    let r = run(&mut m, r#"{"find":"c","filter":{"a":1,"b":1}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 1 && i % 7 == 1));
}

#[test]
fn or_with_a_clause_no_index_serves_scans_the_collection() {
    let mut m = loaded(70);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"$or":[{"a":2},{"s":"Y"}]}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 2 || i % 3 == 1));
    let r =
        run(&mut m, r#"{"explain":{"find":"c","filter":{"a":2}},"verbosity":"allPlansExecution"}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

#[test]
fn hash_intersections_stop_at_a_limit_or_a_batch() {
    let mut m = loaded(700);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let set = doc(&[
        ("setParameter", Value::Int32(1)),
        ("internalQueryPlannerEnableHashIntersection", Value::Boolean(true)),
    ]);
    let r = m.run("admin", set);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let all = expected(700, |i| i % 10 == 4 && i % 7 == 2);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":4,"b":2}}"#);
    assert_eq!(ids(&r), all);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":4,"b":2},"limit":3}"#);
    let got = ids(&r);
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|i| all.contains(i)), "{got:?}");
    let r = run(&mut m, r#"{"find":"c","filter":{"a":{"$gte":4},"b":{"$lte":2}},"batchSize":2}"#);
    assert_eq!(ids(&r).len(), 2);
    let r = run(&mut m, r#"{"count":"c","query":{"a":{"$gte":4},"b":{"$lte":2}}}"#);
    let want = expected(700, |i| i % 10 >= 4 && i % 7 <= 2).len();
    assert_eq!(field(&r, "n"), &Value::Int32(i32::try_from(want).unwrap_or(i32::MAX)));
}

/// With more documents than a trial reads, the plans stop at the trial's budget and the winner
/// goes on to the end.
#[test]
fn trials_stop_at_their_budget_on_a_large_collection() {
    let mut m = loaded(12_000);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let r = run(&mut m, r#"{"count":"c","query":{"a":{"$gte":0},"b":{"$gte":0}}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(12_000), "{r:?}");
    // Both scans read every key and nothing matches: the trial ends at its budget.
    let r = run(&mut m, r#"{"find":"c","filter":{"a":{"$gte":0},"b":{"$gte":0},"s":"w"}}"#);
    assert_eq!(ids(&r), Vec::<i32>::new());
    let r = run(&mut m, r#"{"find":"c","filter":{"a":9,"b":6},"projection":{"_id":1}}"#);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(b.len(), 101);
}

/// Seeds index `id` named `name` on `key` of collection `c` straight into the node's catalog,
/// with `buckets` and `state` and no entries.
fn seeded(m: &mut Mem, id: u32, key: Document, buckets: u8, state: IndexState) {
    let collection = m.catalog.collections.values().next().expect("c").id;
    let spec =
        Document::new().with("key", key).with("name", format!("i{id}")).with("v", 2).to_bytes();
    let entry = IndexEntry {
        id,
        collection,
        name: format!("i{id}"),
        spec: spec.expect("spec"),
        buckets,
        state,
    };
    m.indexes.last_index = m.indexes.last_index.max(id);
    m.indexes.indexes.insert(id, entry);
}

/// An index still being built, one with hash buckets and a `text` index are left out of the plans:
/// the matches come from the collection, not from an index without entries.
#[test]
fn indexes_being_built_bucketed_or_text_are_not_planned() {
    let mut m = loaded(70);
    seeded(&mut m, 100, Document::new().with("a", 1), 0, IndexState::Building);
    seeded(&mut m, 101, Document::new().with("b", 1), 4, IndexState::Ready);
    index(&mut m, r#"{"key":{"s":"text"},"name":"st"}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":3}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 3));
    let r = run(&mut m, r#"{"find":"c","filter":{"b":2}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 7 == 2));
    let r = run(&mut m, r#"{"find":"c","filter":{"s":"x"}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 3 == 0));
}

/// An `$or` whose clauses both keep a filter on the fetched document gives once a document both
/// clauses find.
#[test]
fn or_clauses_with_filters_give_a_shared_document_once() {
    let mut m = loaded(300);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"$or":[{"a":1,"s":"x"},{"b":1,"s":"x"}]},"batchSize":300}"#,
    );
    let want = expected(300, |i| (i % 10 == 1 || i % 7 == 1) && i % 3 == 0);
    assert!(want.contains(&141), "a document both clauses find");
    assert_eq!(ids(&r), want);
}

/// Plans that sort in memory stop at the trial's budget too; the winner then sorts everything.
#[test]
fn sorting_plans_stop_at_the_trial_budget() {
    let mut m = loaded(3000);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"a":{"$gte":0},"b":{"$gte":0}},"sort":{"s":1,"_id":1},
            "batchSize":3000,"projection":{"_id":1}}"#,
    );
    let got = ids(&r);
    assert_eq!(got.len(), 3000);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    // "Y" sorts before "x" and "z": the first document is the first with `i % 3 == 1`.
    assert_eq!(b.first(), Some(&Value::Document(doc(&[("_id", Value::Int32(1))]))));
}

/// A range of `_id` over more documents than a read of the index of `_id` takes at once in a
/// bucket reads every bucket to its end.
#[test]
fn id_ranges_read_every_chunk_of_every_bucket() {
    let mut m = loaded(6000);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"_id":{"$gte":10}},"batchSize":6000,"projection":{"_id":1}}"#,
    );
    assert_eq!(ids(&r), expected(6000, |i| i >= 10));
}

/// Two clauses of an `$or` on one index use it once (measured on a `refnode` 9.0.4 on
/// 02-11-2026): `$indexStats` counts one access, and the clauses' documents come in their
/// clauses' order.
#[test]
fn or_clauses_on_one_index_count_one_access() {
    let mut m = loaded(50);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"$or":[{"a":1,"s":"x"},{"a":{"$gt":8}}]},"projection":{"_id":1}}"#,
    );
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let order: Vec<&Value> = b
        .iter()
        .map(|d| match d {
            Value::Document(d) => field(d, "_id"),
            other => panic!("{other:?}"),
        })
        .collect();
    let want: Vec<Value> = [21, 9, 19, 29, 39, 49].into_iter().map(Value::Int32).collect();
    assert_eq!(order, want.iter().collect::<Vec<_>>());
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$indexStats":{}},{"$match":{"name":"a_1"}}],"cursor":{}}"#,
    );
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let Some(Value::Document(a)) = b.first() else { panic!("{r:?}") };
    assert_eq!(field(a, "accesses.ops"), &Value::Int64(1), "{r:?}");
}

/// A unique index keeps its document's primary key in the entry's value: a plan on it finds the
/// documents (it failed with "an index entry without its document").
#[test]
fn a_unique_index_plan_finds_its_documents() {
    let mut m = loaded(20);
    index(&mut m, r#"{"key":{"b":1,"_id":1},"name":"b_id","unique":true}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"b":3}}"#);
    assert_eq!(ids(&r), expected(20, |i| i % 7 == 3));
    let r = run(&mut m, r#"{"find":"c","filter":{"b":{"$gte":5}},"sort":{"b":1}}"#);
    assert_eq!(ids(&r), expected(20, |i| i % 7 >= 5));
}

/// A wildcard index is no plan of the planner's: the query takes the index of its field, as a
/// `refnode` 9.0.4 picks `a_1` next to `$**_1` (measured on 02-11-2026).
#[test]
fn a_wildcard_index_next_to_a_field_index_leaves_the_field_index() {
    let mut m = loaded(30);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"$**":1},"name":"$**_1"}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":5}}"#);
    assert_eq!(ids(&r), expected(30, |i| i % 10 == 5));
    let r = run(&mut m, r#"{"explain":{"find":"c","filter":{"a":5}}}"#);
    let plan = field(&r, "queryPlanner.winningPlan.shards.0.winningPlan.inputStage.indexName");
    assert_eq!(plan, &Value::from("a_1"), "{r:?}");
}

/// A `distinct` whose interval runs to the end of the index reads no key after it: two keys
/// for `{a: {$gte: 5}}` over 1, 3, 5, 5, 7 (measured on a `refnode` 9.0.4 on 02-11-2026).
#[test]
fn a_distinct_scan_to_the_end_of_the_index_reads_no_key_after_it() {
    let mut m = Mem::new();
    let docs =
        r#"[{"_id":1,"a":1},{"_id":2,"a":3},{"_id":3,"a":5},{"_id":4,"a":5},{"_id":5,"a":7}]"#;
    let r = run(&mut m, &format!(r#"{{"insert":"c","documents":{docs}}}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    for (q, keys) in [(r#"{"$gte":5}"#, 2), (r#"{"$lte":3}"#, 3)] {
        let cmd = format!(
            r#"{{"explain":{{"distinct":"c","key":"a","query":{{"a":{q}}}}},"verbosity":"executionStats"}}"#
        );
        let r = run(&mut m, &cmd);
        let got = field(&r, "executionStats.totalKeysExamined");
        assert_eq!(got, &Value::Int32(keys), "{q}: {r:?}");
        assert_eq!(field(&r, "executionStats.nReturned"), &Value::Int32(2), "{q}");
    }
}

/// The trial ends at the round where a plan reached its end (RefDb alternates the plans and
/// stops there): a plan that would read up to the budget is not run past that round. Measured on
/// the internal cluster of TeselaDB Cloud, a plan of a selective compound index that ended at 9
/// documents waited for a plan of a broad index to read 157.000 entries, 20 s per planning.
#[test]
fn a_trial_reads_no_further_than_the_round_where_a_plan_ended() {
    use std::cell::Cell;
    use std::rc::Rc;

    let mut m = loaded(12_000);
    index(&mut m, r#"{"key":{"b":1},"name":"aa"}"#);
    index(&mut m, r#"{"key":{"a":1,"s":1},"name":"zz"}"#);
    let scans = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&scans);
    m.on_scan = Some(Box::new(move || seen.set(seen.get() + 1)));
    // `zz` finds nothing at once; `aa` reads every key and nothing matches.
    let r = run(&mut m, r#"{"find":"c","filter":{"a":9,"s":"w","b":{"$gte":0}}}"#);
    assert_eq!(ids(&r), Vec::<i32>::new());
    // The trial's least budget is 10.000 works; before, `b_1` read them all (10.054 reads).
    assert!(scans.get() < 1_000, "{} reads", scans.get());
}

/// `count` sessions as a sign-in store keeps them: `subject` shared by 30 of them, a `family_id`
/// of its own and an `expires_at` in the future, with an index on each of the three.
fn sessions(count: i32) -> Mem {
    let mut m = Mem::new();
    let docs: Vec<String> = (0..count)
        .map(|i| {
            format!(
                r#"{{"_id":{i},"subject":"u{}","family_id":"f{i}","expires_at":{{"$date":"2100-01-01T00:00:00Z"}}}}"#,
                i % 30
            )
        })
        .collect();
    for chunk in docs.chunks(1000) {
        let r = run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, chunk.join(",")));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    index(&mut m, r#"{"key":{"subject":1},"name":"subject_1"}"#);
    index(&mut m, r#"{"key":{"family_id":1},"name":"family_id_1"}"#);
    index(&mut m, r#"{"key":{"expires_at":1},"name":"expires_at_1","expireAfterSeconds":0}"#);
    m
}

/// The session check of a sign-in store: one session by its subject and its family, open and
/// not expired, of which the read wants one.
const OPEN_SESSION: &str = r#"{"subject":"u7","family_id":"f7","revoked_at":{"$exists":false},"expires_at":{"$gt":{"$date":"2026-11-07T00:00:00Z"}}}"#;

#[test]
fn a_read_with_a_limit_ends_its_trial_at_the_limit_and_keeps_its_cached_plan() {
    let mut m = sessions(6_000);
    let find = format!(r#"{{"find":"c","filter":{OPEN_SESSION},"limit":1,"singleBatch":true}}"#);
    let count = format!(r#"{{"count":"c","query":{OPEN_SESSION},"limit":1}}"#);
    // Each read competes its plans or takes the cached one; none reads the range of
    // `expires_at_1` (every session) nor counts the collection for the trial's budget. RefDb
    // 9.0.4 ends the trial at min(limit, 101) results and keeps a cached plan that gives them.
    // A trial gives each of the three plans its first round of 128 works, so a read with its
    // fetches stays under 400 rows; before, a `findOne` read 55.889.
    for round in 0..4 {
        m.scanned_rows = 0;
        let r = run(&mut m, &find);
        assert_eq!(ids(&r), vec![7], "{r:?}");
        assert!(m.scanned_rows < 400, "find, round {round}: {} rows read", m.scanned_rows);
        m.scanned_rows = 0;
        let r = run(&mut m, &count);
        assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
        assert!(m.scanned_rows < 400, "count, round {round}: {} rows read", m.scanned_rows);
    }
    // After the first read the plan comes from the cache: its first batch of 16 entries of the
    // subject, its document and the catalog, not a trial of every plan nor the 200 sessions of
    // the subject.
    m.scanned_rows = 0;
    run(&mut m, &find);
    assert!(m.scanned_rows < 40, "cached find: {} rows read", m.scanned_rows);
    let r = run(&mut m, r#"{"aggregate":"c","pipeline":[{"$planCacheStats":{}}],"cursor":{}}"#);
    let Value::Array(entries) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert!(
        entries.iter().all(
            |e| matches!(e, Value::Document(d) if field(d, "isActive") == &Value::Boolean(true))
        ),
        "a plan that gives its limit stays cached: {r:?}"
    );
}

/// The reads of the range `cmd` makes on `m`, each of them a trip to a range's thread on a node.
fn range_reads(m: &mut Mem, cmd: &str) -> (Document, usize) {
    let reads = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = std::rc::Rc::clone(&reads);
    m.on_scan = Some(Box::new(move || counted.set(counted.get() + 1)));
    let r = run(m, cmd);
    m.on_scan = None;
    (r, reads.get())
}

#[test]
fn a_find_by_one_id_reads_one_bucket_with_its_document() {
    let mut m = loaded(300);
    let find = r#"{"find":"c","filter":{"_id":42}}"#;
    run(&mut m, find);
    // The cached plan: the document of 42 in the bucket of its hash, read once. Before, the
    // value was looked for in each of the 16 buckets and its document read again.
    let (r, reads) = range_reads(&mut m, find);
    assert_eq!(ids(&r), vec![42], "{r:?}");
    assert_eq!(reads, 1);
}

#[test]
fn an_in_of_21_ids_reads_each_value_once() {
    let mut m = loaded(300);
    let wanted: Vec<i32> = (0..21).map(|i| i * 7).collect();
    let list = wanted.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
    let filter = format!(r#"{{"_id":{{"$in":[{list}]}}}}"#);
    let find = format!(r#"{{"find":"c","filter":{filter}}}"#);
    run(&mut m, &find);
    let (r, reads) = range_reads(&mut m, &find);
    assert_eq!(ids(&r), wanted, "{r:?}");
    // One read for each value with its document (408 before, with every bucket and a second
    // read of each document), and the probes of the 21 stretches after them for an entry, which
    // `keysExamined` counts as in 9.0.4: 21 values and an entry after each.
    assert!(reads <= 21 + 21 * 3, "{reads} reads of the range");
    let explain = run(
        &mut m,
        &format!(r#"{{"explain":{{"find":"c","filter":{filter}}},"verbosity":"executionStats"}}"#),
    );
    assert_eq!(
        field(&explain, "executionStats.totalKeysExamined"),
        &Value::Int32(42),
        "{explain:?}"
    );
    assert_eq!(
        field(&explain, "executionStats.totalDocsExamined"),
        &Value::Int32(21),
        "{explain:?}"
    );
}
