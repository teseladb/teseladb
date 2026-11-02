//! The bucket catalog of a time series through sequences of inserts (015 FR-010, T025), as a
//! `refnode` 9.0.4 counted them on 02-11-2026: each step gives the counters of
//! `collStats.timeseries` that moved and by how much, and `numActiveBuckets` after it.

mod common;

use std::collections::BTreeMap;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const H: i64 = 3_600_000;
const ACTIVE: &str = "numActiveBuckets";

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

/// A time series `name` with a `2dsphere` index on `loc`, and its counters so far.
struct Series {
    name: &'static str,
    seen: BTreeMap<String, i64>,
}

fn series(m: &mut Mem, name: &'static str) -> Series {
    let create =
        format!(r#"{{"create":"{name}","timeseries":{{"timeField":"t","metaField":"m"}}}}"#);
    let r = m.run("g", json(&create));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let index = format!(
        r#"{{"createIndexes":"{name}","indexes":[{{"key":{{"loc":"2dsphere"}},"name":"g"}}]}}"#
    );
    let r = m.run("g", json(&index));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    Series { name, seen: BTreeMap::new() }
}

/// A measurement at `T0 + t` ms of meta value `meta`; `bad` puts a point out of bounds in `loc`.
fn at(t: i64, meta: i32, bad: bool) -> String {
    let x = if bad { 1000 } else { 1 };
    format!(
        r#"{{"t":{{"$date":{{"$numberLong":"{}"}}}},"m":{meta},"loc":{{"type":"Point","coordinates":[{x},1]}}}}"#,
        T0 + t
    )
}

fn ok(t: i64, meta: i32) -> String {
    at(t, meta, false)
}

fn bad(t: i64, meta: i32) -> String {
    at(t, meta, true)
}

impl Series {
    /// Inserts `docs`, checks `n` and the places of the errors, then that the counters moved by
    /// `moved` exactly and that `active` buckets are active.
    fn step(
        &mut self,
        m: &mut Mem,
        (docs, ordered): (&[String], bool),
        (n, errors): (i32, &[i32]),
        moved: &[(&str, i64)],
        active: i64,
    ) {
        let cmd = format!(
            r#"{{"insert":"{}","ordered":{ordered},"documents":[{}]}}"#,
            self.name,
            docs.join(",")
        );
        let r = m.run("g", json(&cmd));
        assert_eq!(field(&r, "n"), &Value::Int32(n), "{r:?}");
        let at: Vec<i32> = match r.get("writeErrors") {
            Some(Value::Array(e)) => e
                .iter()
                .map(|e| match e {
                    Value::Document(e) => match field(e, "index") {
                        Value::Int32(i) => *i,
                        other => panic!("{other:?}"),
                    },
                    other => panic!("{other:?}"),
                })
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(at, errors, "{r:?}");
        let r = m.run("g", doc(&[("collStats", Value::from(self.name))]));
        let Value::Document(ts) = field(&r, "timeseries") else { panic!("{r:?}") };
        let mut got = BTreeMap::new();
        let mut now = BTreeMap::new();
        for (k, v) in ts.iter() {
            let v = match v {
                Value::Int32(v) => i64::from(*v),
                Value::Int64(v) => *v,
                _ => continue,
            };
            if k.starts_with("avg") {
                continue;
            }
            let was = self.seen.get(k).copied().unwrap_or(0);
            if v != was {
                got.insert(k.to_owned(), v - was);
            }
            now.insert(k.to_owned(), v);
        }
        let want: BTreeMap<String, i64> =
            moved.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
        assert_eq!(got, want, "{cmd}");
        assert_eq!(now.get(ACTIVE).copied(), Some(active), "{cmd}");
        self.seen = now;
    }
}

const COUNT: &str = "bucketCount";
const INSERTS: &str = "numBucketInserts";
const UPDATES: &str = "numBucketUpdates";
const META: &str = "numBucketsOpenedDueToMetadata";
const BY_COUNT: &str = "numBucketsClosedDueToCount";
const FORWARD: &str = "numBucketsClosedDueToTimeForward";
const ARCHIVED: &str = "numBucketsArchivedDueToTimeBackward";
const REOPENED: &str = "numBucketsReopened";
const FETCHED: &str = "numBucketsFetched";
const QUERIED: &str = "numBucketsQueried";
const FAILED: &str = "numBucketQueriesFailed";
const COMMITS: &str = "numCommits";
const MEASUREMENTS: &str = "numMeasurementsCommitted";

/// T025 (a) and (b): a lookup lands on the first bucket in index order, a full one closes by
/// count and a new bucket opens; time forward opens a new bucket without a lookup; in one write
/// the windows go in time order and the one the write used closes when it fetches a later one.
#[test]
fn a_lookup_lands_on_the_first_bucket_of_its_window_in_index_order() {
    let mut m = Mem::new();
    let mut s = series(&mut m, "a");
    let many: Vec<String> = (0..1005).map(|i| ok(i, 1)).collect();
    s.step(
        &mut m,
        (&many, true),
        (1005, &[]),
        &[
            (COUNT, 2),
            (ACTIVE, 1),
            (INSERTS, 2),
            (META, 1),
            (BY_COUNT, 1),
            (COMMITS, 2),
            (MEASUREMENTS, 1005),
            (FAILED, 1),
        ],
        1,
    );
    let one = [(COUNT, 1), (INSERTS, 1), (FORWARD, 1), (COMMITS, 1), (MEASUREMENTS, 1)];
    s.step(&mut m, (&[ok(2 * H, 1)], true), (1, &[]), &one, 1);
    s.step(&mut m, (&[ok(3 * H, 1)], true), (1, &[]), &one, 1);
    s.step(
        &mut m,
        (&[ok(1, 1)], true),
        (1, &[]),
        &[
            (COUNT, 1),
            (ACTIVE, 1),
            (INSERTS, 1),
            (BY_COUNT, 1),
            (COMMITS, 1),
            (MEASUREMENTS, 1),
            (ARCHIVED, 1),
            (REOPENED, 1),
            (QUERIED, 1),
        ],
        2,
    );
    s.step(&mut m, (&[ok(2 * H + 5, 1)], true), (1, &[]), &one, 2);
    s.step(
        &mut m,
        (&[ok(3 * H + 5, 1), ok(5, 1)], true),
        (2, &[]),
        &[
            (UPDATES, 2),
            (FORWARD, 1),
            (COMMITS, 2),
            (MEASUREMENTS, 2),
            (ARCHIVED, 1),
            (REOPENED, 2),
            (FETCHED, 1),
            (QUERIED, 1),
        ],
        2,
    );
    s.step(
        &mut m,
        (&[ok(6, 1)], true),
        (1, &[]),
        &[(ACTIVE, 1), (UPDATES, 1), (COMMITS, 1), (MEASUREMENTS, 1), (REOPENED, 1), (QUERIED, 1)],
        3,
    );
    s.step(
        &mut m,
        (&[ok(2 * H + 6, 1)], true),
        (1, &[]),
        &[
            (UPDATES, 1),
            (COMMITS, 1),
            (MEASUREMENTS, 1),
            (ARCHIVED, 1),
            (REOPENED, 1),
            (FETCHED, 1),
        ],
        3,
    );
    let r = m.run("g", json(r#"{"count":"a"}"#));
    assert_eq!(field(&r, "n"), &Value::Int32(1013), "{r:?}");
}

/// A window left behind by a lookup goes to the archive at the next insert of its meta value,
/// and is taken back at once when that insert goes to it; a write that used a window closes it
/// when it moves to another one it reopens.
#[test]
fn a_displaced_window_is_archived_at_the_next_insert() {
    let mut m = Mem::new();
    let mut s = series(&mut m, "d");
    s.step(
        &mut m,
        (&[ok(0, 1)], true),
        (1, &[]),
        &[
            (COUNT, 1),
            (ACTIVE, 1),
            (INSERTS, 1),
            (META, 1),
            (COMMITS, 1),
            (MEASUREMENTS, 1),
            (FAILED, 1),
        ],
        1,
    );
    s.step(
        &mut m,
        (&[ok(2 * H, 1)], true),
        (1, &[]),
        &[(COUNT, 1), (INSERTS, 1), (FORWARD, 1), (COMMITS, 1), (MEASUREMENTS, 1)],
        1,
    );
    let query =
        [(ACTIVE, 1), (UPDATES, 1), (COMMITS, 1), (MEASUREMENTS, 1), (REOPENED, 1), (QUERIED, 1)];
    s.step(&mut m, (&[ok(1, 1)], true), (1, &[]), &query, 2);
    s.step(
        &mut m,
        (&[ok(2, 1), ok(2 * H + 2, 1)], true),
        (2, &[]),
        &[
            (ACTIVE, -1),
            (UPDATES, 2),
            (FORWARD, 1),
            (COMMITS, 2),
            (MEASUREMENTS, 2),
            (ARCHIVED, 1),
            (REOPENED, 1),
            (FETCHED, 1),
        ],
        1,
    );
    s.step(&mut m, (&[ok(3, 1)], true), (1, &[]), &query, 2);
    s.step(
        &mut m,
        (&[ok(2 * H + 3, 1)], true),
        (1, &[]),
        &[(UPDATES, 1), (COMMITS, 1), (MEASUREMENTS, 1), (ARCHIVED, 1)],
        2,
    );
    let mut s = series(&mut m, "e");
    s.step(
        &mut m,
        (&[ok(2 * H, 1), ok(0, 1)], true),
        (2, &[]),
        &[
            (COUNT, 2),
            (ACTIVE, 1),
            (INSERTS, 2),
            (META, 1),
            (FORWARD, 1),
            (COMMITS, 2),
            (MEASUREMENTS, 2),
            (FAILED, 1),
        ],
        1,
    );
    s.step(
        &mut m,
        (&[ok(2 * H + 1, 1)], true),
        (1, &[]),
        &[(UPDATES, 1), (COMMITS, 1), (MEASUREMENTS, 1)],
        1,
    );
}

/// T025 (c): an ordered insert with a refused measurement is tried whole and retried one
/// measurement at a time, so its lookups count twice and each measurement before the refused
/// one is its own commit; the bucket of the refused one leaves the catalog.
#[test]
fn a_failed_ordered_insert_looks_up_twice_and_drops_the_failed_bucket() {
    let mut m = Mem::new();
    let mut s = series(&mut m, "c");
    let docs = [ok(0, 1), ok(1, 2), bad(2, 1), ok(3, 2), ok(4, 1)];
    s.step(
        &mut m,
        (&docs, true),
        (2, &[2]),
        &[
            (COUNT, 2),
            (ACTIVE, 1),
            (INSERTS, 2),
            (META, 2),
            (COMMITS, 2),
            (MEASUREMENTS, 2),
            (FAILED, 4),
        ],
        1,
    );
    let docs = [ok(5, 1), ok(6, 2), ok(7, 1), ok(8, 3)];
    s.step(
        &mut m,
        (&docs, true),
        (4, &[]),
        &[
            (COUNT, 1),
            (ACTIVE, 2),
            (INSERTS, 1),
            (UPDATES, 2),
            (META, 1),
            (COMMITS, 3),
            (MEASUREMENTS, 4),
            (REOPENED, 1),
            (QUERIED, 1),
            (FAILED, 1),
        ],
        3,
    );
    let mut s = series(&mut m, "c1");
    s.step(
        &mut m,
        (&[ok(0, 1), ok(1, 1), bad(2, 1)], true),
        (2, &[2]),
        &[
            (COUNT, 1),
            (INSERTS, 1),
            (UPDATES, 1),
            (META, 1),
            (COMMITS, 2),
            (MEASUREMENTS, 2),
            (FAILED, 2),
        ],
        0,
    );
    let mut s = series(&mut m, "c2");
    s.step(&mut m, (&[bad(0, 1), ok(3, 2)], true), (0, &[0]), &[(FAILED, 3)], 0);
    let fresh = [
        (COUNT, 1),
        (ACTIVE, 1),
        (INSERTS, 1),
        (META, 1),
        (COMMITS, 1),
        (MEASUREMENTS, 1),
        (FAILED, 1),
    ];
    s.step(&mut m, (&[ok(50, 1)], true), (1, &[]), &fresh, 1);
    let mut s = series(&mut m, "c3");
    s.step(
        &mut m,
        (&[ok(0, 1), ok(1, 2)], true),
        (2, &[]),
        &[
            (COUNT, 2),
            (ACTIVE, 2),
            (INSERTS, 2),
            (META, 2),
            (COMMITS, 2),
            (MEASUREMENTS, 2),
            (FAILED, 2),
        ],
        2,
    );
    s.step(
        &mut m,
        (&[ok(1, 1), ok(1, 2), bad(2, 1)], true),
        (2, &[2]),
        &[(ACTIVE, -1), (UPDATES, 2), (COMMITS, 2), (MEASUREMENTS, 2), (REOPENED, 2), (QUERIED, 2)],
        1,
    );
}

/// The aborted try counts the lookups it makes and the open buckets it finds full, not the
/// buckets it opens itself.
#[test]
fn the_aborted_try_counts_lookups_and_full_buckets_only() {
    let mut m = Mem::new();
    let mut s = series(&mut m, "f");
    let many: Vec<String> = (0..1000).map(|i| ok(i, 1)).collect();
    s.step(
        &mut m,
        (&many, true),
        (1000, &[]),
        &[
            (COUNT, 1),
            (ACTIVE, 1),
            (INSERTS, 1),
            (META, 1),
            (COMMITS, 1),
            (MEASUREMENTS, 1000),
            (FAILED, 1),
        ],
        1,
    );
    s.step(
        &mut m,
        (&[ok(1001, 1), bad(1002, 1)], true),
        (1, &[1]),
        &[
            (COUNT, 1),
            (ACTIVE, -1),
            (INSERTS, 1),
            (BY_COUNT, 2),
            (COMMITS, 1),
            (MEASUREMENTS, 1),
            (REOPENED, 1),
            (QUERIED, 1),
        ],
        0,
    );
    let mut s = series(&mut m, "g");
    s.step(
        &mut m,
        (&[ok(0, 1), ok(2 * H, 1), bad(2, 1)], true),
        (2, &[2]),
        &[
            (COUNT, 2),
            (ACTIVE, 1),
            (INSERTS, 2),
            (META, 1),
            (FORWARD, 1),
            (COMMITS, 2),
            (MEASUREMENTS, 2),
            (REOPENED, 1),
            (QUERIED, 1),
            (FAILED, 2),
        ],
        1,
    );
    s.step(
        &mut m,
        (&[ok(2 * H + 1, 1)], true),
        (1, &[]),
        &[(UPDATES, 1), (COMMITS, 1), (MEASUREMENTS, 1), (ARCHIVED, 1)],
        1,
    );
    let mut s = series(&mut m, "h");
    s.step(
        &mut m,
        (&[ok(2 * H, 1), bad(0, 1)], true),
        (1, &[1]),
        &[
            (COUNT, 1),
            (ACTIVE, 1),
            (INSERTS, 1),
            (META, 1),
            (COMMITS, 1),
            (MEASUREMENTS, 1),
            (ARCHIVED, 1),
            (FAILED, 3),
        ],
        1,
    );
    s.step(
        &mut m,
        (&[ok(2 * H + 1, 1)], true),
        (1, &[]),
        &[(UPDATES, 1), (COMMITS, 1), (MEASUREMENTS, 1), (REOPENED, 1), (FETCHED, 1)],
        1,
    );
}

/// Unordered, a failed bucket counts its lookup and leaves the catalog.
#[test]
fn an_unordered_failed_bucket_counts_its_lookup_and_leaves_the_catalog() {
    let mut m = Mem::new();
    let mut s = series(&mut m, "u");
    s.step(
        &mut m,
        (&[ok(0, 1), bad(2, 1), ok(3, 2)], false),
        (1, &[0, 1]),
        &[
            (COUNT, 1),
            (ACTIVE, 1),
            (INSERTS, 1),
            (META, 1),
            (COMMITS, 1),
            (MEASUREMENTS, 1),
            (FAILED, 2),
        ],
        1,
    );
    let mut s = series(&mut m, "v");
    s.step(
        &mut m,
        (&[ok(0, 1)], true),
        (1, &[]),
        &[
            (COUNT, 1),
            (ACTIVE, 1),
            (INSERTS, 1),
            (META, 1),
            (COMMITS, 1),
            (MEASUREMENTS, 1),
            (FAILED, 1),
        ],
        1,
    );
    s.step(
        &mut m,
        (&[bad(1, 1), ok(2, 2)], false),
        (1, &[0]),
        &[(COUNT, 1), (INSERTS, 1), (META, 1), (COMMITS, 1), (MEASUREMENTS, 1), (FAILED, 1)],
        1,
    );
    s.step(
        &mut m,
        (&[ok(3, 1)], true),
        (1, &[]),
        &[(ACTIVE, 1), (UPDATES, 1), (COMMITS, 1), (MEASUREMENTS, 1), (REOPENED, 1), (QUERIED, 1)],
        2,
    );
}
