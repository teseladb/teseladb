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
    let mut out = ids_in_order(r);
    out.sort_unstable();
    out
}

/// The `_id` of the first batch's documents, in the order of the answer.
fn ids_in_order(r: &Document) -> Vec<i32> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    let out: Vec<i32> = b
        .iter()
        .map(|d| match d {
            Value::Document(d) => match field(d, "_id") {
                Value::Int32(n) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
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
fn an_in_of_21_ids_keeps_the_keys_examined_of_9_0_4() {
    let mut m = loaded(300);
    let wanted: Vec<i32> = (0..21).map(|i| i * 7).collect();
    let list = wanted.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
    let filter = format!(r#"{{"_id":{{"$in":[{list}]}}}}"#);
    let r = run(&mut m, &format!(r#"{{"find":"c","filter":{filter}}}"#));
    assert_eq!(ids(&r), wanted, "{r:?}");
    // 21 values and an entry after each, as 9.0.4 counts them.
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

/// The trips to the ranges `cmd` makes on `m`: a read of several spans at once is one trip.
fn trips(m: &mut Mem, cmd: &str) -> (Document, usize) {
    m.span_reads = 0;
    m.spans_batched = 0;
    let (r, scans) = range_reads(m, cmd);
    // `range_reads` counts every read of a span; a read of several spans counts once.
    (r, scans - m.spans_batched + m.span_reads)
}

#[test]
fn an_in_of_ids_reads_its_values_and_the_entries_after_them_in_one_trip() {
    let mut m = loaded(300);
    let wanted: Vec<i32> = (0..21).map(|i| i * 7).collect();
    let list = wanted.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
    let find = format!(r#"{{"find":"c","filter":{{"_id":{{"$in":[{list}]}}}}}}"#);
    run(&mut m, &find);
    let (r, trips) = trips(&mut m, &find);
    assert_eq!(ids(&r), wanted, "{r:?}");
    assert_eq!(trips, 1, "the values and the stretches after them, read together");
}

#[test]
fn an_in_of_ids_reads_each_bucket_once_for_the_stretches_between_its_values() {
    let mut m = loaded(300);
    let wanted: Vec<i32> = (0..21).map(|i| i * 7).collect();
    let list = wanted.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
    let find = format!(r#"{{"find":"c","filter":{{"_id":{{"$in":[{list}]}}}}}}"#);
    run(&mut m, &find);
    let (r, trips) = trips(&mut m, &find);
    assert_eq!(ids(&r), wanted, "{r:?}");
    assert_eq!(trips, 1);
    // The 21 values, one read of each of the 16 buckets for the stretches between them and one
    // probe of each bucket for the stretch after the last (010 T054). Before, a probe of each
    // bucket for each stretch: 21 + 21 × 16 = 357.
    assert!(m.spans_batched <= 21 + 16 + 16, "{} spans", m.spans_batched);
}

/// The `keysExamined` of `find` with `_id` in `values` (and `sort` on `_id` when `backward`) over
/// the `_id` 0 to `count - 1`, counted as 9.0.4 counts a scan of single values: each value there,
/// and one entry after each value when an `_id` lies between it and the next value the scan reads
/// (or past the last, in the scan's direction).
fn keys_examined_by_hand(count: i32, values: &[i32], backward: bool) -> i32 {
    let mut order: Vec<i32> = values.to_vec();
    order.sort_unstable();
    order.dedup();
    if backward {
        order.reverse();
    }
    let there = |v: i32| (0..count).contains(&v);
    let mut keys = 0;
    for (i, &v) in order.iter().enumerate() {
        keys += i32::from(there(v));
        let between = match (order.get(i + 1), backward) {
            (Some(&n), false) => (v + 1..n).any(there),
            (Some(&n), true) => (n + 1..v).any(there),
            (None, false) => (v + 1..count).any(there),
            (None, true) => (0..v).any(there),
        };
        keys += i32::from(between);
    }
    keys
}

#[test]
fn an_in_of_ids_counts_the_entries_after_its_values_as_9_0_4_over_any_spread() {
    let count = 2000;
    let mut m = loaded(count);
    let cases: Vec<Vec<i32>> = vec![
        // Far apart: each bucket has more entries between the values than one read takes.
        (0..21).map(|i| i * 90).collect(),
        // Values that are not there, before, among and after the documents.
        vec![-5, 3, 1999, 2500, 7000],
        // Next to each other: no entry between them.
        vec![10, 11, 12, 13, 14],
        // The last document and past it.
        vec![1998, 1999, 2000],
        // Dense and sparse together.
        vec![0, 1, 2, 500, 501, 1500, 1999],
    ];
    for values in &cases {
        for backward in [false, true] {
            let list = values.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
            let sort = if backward { r#","sort":{"_id":-1}"# } else { "" };
            let find = format!(r#"{{"find":"c","filter":{{"_id":{{"$in":[{list}]}}}}{sort}}}"#);
            let explain =
                run(&mut m, &format!(r#"{{"explain":{find},"verbosity":"executionStats"}}"#));
            assert_eq!(
                field(&explain, "executionStats.totalKeysExamined"),
                &Value::Int32(keys_examined_by_hand(count, values, backward)),
                "{values:?} backward {backward}: {explain:?}"
            );
            // `ids` gives them sorted.
            let mut there: Vec<i32> =
                values.iter().copied().filter(|v| (0..count).contains(v)).collect();
            there.sort_unstable();
            assert_eq!(ids(&run(&mut m, &find)), there, "{values:?} backward {backward}");
        }
    }
}

#[test]
fn an_in_of_far_apart_ids_asks_the_open_stretches_in_a_second_trip() {
    let mut m = loaded(2000);
    let wanted: Vec<i32> = (0..21).map(|i| i * 90).collect();
    let list = wanted.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
    let find = format!(r#"{{"find":"c","filter":{{"_id":{{"$in":[{list}]}}}}}}"#);
    run(&mut m, &find);
    let (r, trips) = trips(&mut m, &find);
    assert_eq!(ids(&r), wanted, "{r:?}");
    // Each bucket holds about 110 entries between 0 and 1800, more than its read takes (43):
    // the stretches past its last entry go in a second trip.
    assert_eq!(trips, 2);
}

#[test]
fn an_id_equality_with_a_sort_reads_its_document_alone() {
    let mut m = loaded(2_000);
    let scans = |m: &mut Mem| {
        let r = run(m, r#"{"serverStatus":1}"#);
        field(&r, "metrics.queryExecutor.collectionScans.total").clone()
    };
    let before = scans(&mut m);
    // YCSB's read: the sort and the limit change nothing on one document. A `refnode` 9.0.4
    // answers the four through the index of `_id` and counts no collection scan (measured on
    // 08-11-2026); before, each read every document, 11 ms a read on 20.000 documents.
    let cases = [
        (r#"{"find":"c","filter":{"_id":2},"sort":{"_id":1},"limit":1}"#, vec![2]),
        (r#"{"find":"c","filter":{"_id":3},"sort":{"a":-1}}"#, vec![3]),
        (r#"{"find":"c","filter":{"_id":3,"a":5},"sort":{"a":1}}"#, vec![]),
        (r#"{"find":"c","filter":{"_id":{"$eq":4}},"sort":{"b":1},"skip":1}"#, vec![]),
    ];
    for (find, want) in cases {
        m.scanned_rows = 0;
        let r = run(&mut m, find);
        assert_eq!(ids(&r), want, "{find}: {r:?}");
        assert!(m.scanned_rows < 10, "{find}: {} rows read", m.scanned_rows);
    }
    // The sort key a projection asks for still comes from the sort.
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"_id":1},"sort":{"a":1},"projection":{"k":{"$meta":"sortKey"}}}"#,
    );
    assert_eq!(field(&r, "cursor.firstBatch.0.k"), &Value::Array(vec![Value::Int32(1)]), "{r:?}");
    assert_eq!(scans(&mut m), before);
}

#[test]
fn an_ascending_id_read_with_a_limit_reads_that_many_of_each_bucket() {
    let mut m = loaded(2_000);
    // YCSB's scan and a `findOne` of the first `_id`: the documents go by the hash of their
    // `_id` into 16 buckets, each in `_id` order, so the first `n` of the collection are among
    // the first `n` of each bucket. Before, the read took every document of the range from each
    // bucket and sorted them, 13 ms on 20.000 documents against 1,5 ms in a `refnode` 9.0.4.
    let cases = [
        (
            r#"{"find":"c","filter":{"_id":{"$gte":100}},"sort":{"_id":1},"limit":5}"#,
            vec![100, 101, 102, 103, 104],
        ),
        (r#"{"find":"c","filter":{},"sort":{"_id":1},"limit":1}"#, vec![0]),
        (
            r#"{"find":"c","filter":{"_id":{"$gt":1990}},"sort":{"_id":1},"limit":20}"#,
            (1991..2000).collect(),
        ),
    ];
    for (find, want) in cases {
        m.scanned_rows = 0;
        let r = run(&mut m, find);
        assert_eq!(ids(&r), want, "{find}: {r:?}");
        assert!(m.scanned_rows < 200, "{find}: {} rows read", m.scanned_rows);
    }
}

#[test]
fn a_descending_id_read_with_a_limit_reads_from_the_end_of_each_bucket() {
    let mut m = loaded(2_000);
    // H-003: `find().sort({_id: -1}).limit(1)` read the whole collection and turned it round,
    // 22 ms on 20.000 documents against 0,6 ms in a `refnode` 9.0.4. Each bucket is now read from
    // its end down and the buckets merged from the highest `_id`.
    let cases = [
        (r#"{"find":"c","filter":{},"sort":{"_id":-1},"limit":1}"#, vec![1999]),
        (
            r#"{"find":"c","filter":{"_id":{"$lte":100}},"sort":{"_id":-1},"limit":5}"#,
            vec![100, 99, 98, 97, 96],
        ),
        (
            r#"{"find":"c","filter":{"_id":{"$lt":10}},"sort":{"_id":-1},"limit":20}"#,
            (0..10).rev().collect(),
        ),
        (
            r#"{"aggregate":"c","pipeline":[{"$sort":{"_id":-1}},{"$limit":3}],"cursor":{}}"#,
            vec![1999, 1998, 1997],
        ),
    ];
    for (cmd, want) in cases {
        m.scanned_rows = 0;
        let r = run(&mut m, cmd);
        assert_eq!(ids_in_order(&r), want, "{cmd}: {r:?}");
        assert!(m.scanned_rows < 200, "{cmd}: {} rows read", m.scanned_rows);
    }
}

#[test]
fn a_write_by_id_with_a_sort_reads_its_document_alone() {
    let mut m = loaded(2_000);
    // An `update` and a `findAndModify` by `_id` with a sort change that document, as on a
    // `refnode` 9.0.4; before, each read every document to pick the first by the sort (27 to 31
    // ms on 20.000 documents against 7 to 9 ms).
    m.scanned_rows = 0;
    let r = run(
        &mut m,
        r#"{"update":"c","updates":[{"q":{"_id":5},"u":{"$set":{"x":1}},"sort":{"a":1}}]}"#,
    );
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    assert!(m.scanned_rows < 10, "update: {} rows read", m.scanned_rows);
    m.scanned_rows = 0;
    let r = run(
        &mut m,
        r#"{"findAndModify":"c","query":{"_id":6},"sort":{"_id":-1},"update":{"$set":{"x":2}},"new":true}"#,
    );
    assert_eq!(field(&r, "value.x"), &Value::Int32(2), "{r:?}");
    assert!(m.scanned_rows < 10, "findAndModify: {} rows read", m.scanned_rows);
    let r = run(&mut m, r#"{"find":"c","filter":{"x":{"$exists":true}}}"#);
    assert_eq!(ids(&r), vec![5, 6], "{r:?}");
}

#[test]
fn an_id_range_gives_the_documents_it_read_and_asks_the_buckets_in_one_trip() {
    let mut m = loaded(2_000);
    // An entry of the index of `_id` is its document: the read keeps it instead of reading each
    // document again, and asks the 16 buckets together. Before, YCSB-E's scan read each of its
    // documents a second time, one trip each (14,7 ms against 0,12 ms in a `refnode` 9.0.4).
    m.scanned_rows = 0;
    m.span_reads = 0;
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":{"$gte":100}},"sort":{"_id":1},"limit":5}"#);
    assert_eq!(ids(&r), vec![100, 101, 102, 103, 104], "{r:?}");
    assert!(m.scanned_rows <= 16 * 5, "{} rows read", m.scanned_rows);
    assert_eq!(m.span_reads, 1, "trips to the buckets");
}

#[test]
fn a_long_id_scan_merges_the_buckets_and_reads_little_more_than_it_gives() {
    let mut m = loaded(2_000);
    // YCSB-E's scan asks for up to 100 documents. Before, the read began with 16 entries of each
    // bucket and, short, read everything again with 64 and then 256: some 5.400 rows for 100
    // documents, 11,7 ms against 0,115 ms in a `refnode` 9.0.4.
    m.scanned_rows = 0;
    let r =
        run(&mut m, r#"{"find":"c","filter":{"_id":{"$gte":100}},"sort":{"_id":1},"limit":100}"#);
    assert_eq!(ids(&r), (100..200).collect::<Vec<_>>(), "{r:?}");
    assert!(m.scanned_rows < 200, "{} rows read for 100 documents", m.scanned_rows);
    // Near the end of the collection some buckets run out: the merge still gives every document.
    m.scanned_rows = 0;
    let r =
        run(&mut m, r#"{"find":"c","filter":{"_id":{"$gte":1950}},"sort":{"_id":1},"limit":100}"#);
    assert_eq!(ids(&r), (1950..2000).collect::<Vec<_>>(), "{r:?}");
}
