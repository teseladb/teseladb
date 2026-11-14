//! 014 T015: `fullDocument: "whenAvailable" | "required"` and `fullDocumentBeforeChange` over the
//! in-memory node of `common` (FR-008): the images exist while the collection keeps them
//! (`changeStreamPreAndPostImages`), as the reference router 9.0.4 gives them (measured on 01-11).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const DB: &str = "img";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn open(m: &mut Mem, coll: &str, spec: Document) -> i64 {
    let stages = arr(vec![Document::new().with("$changeStream", spec)]);
    let cmd = Document::new()
        .with("aggregate", coll)
        .with("pipeline", stages)
        .with("cursor", Document::new());
    let r = m.run(DB, cmd);
    match field(&r, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?} in {r:?}"),
    }
}

fn get_more(m: &mut Mem, id: i64, coll: &str) -> Document {
    m.now += 1;
    m.run(DB, Document::new().with("getMore", id).with("collection", coll).with("maxTimeMS", 1))
}

fn batch(r: &Document) -> Vec<Document> {
    match field(r, "cursor.nextBatch") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?} in {r:?}"),
    }
}

fn run(m: &mut Mem, cmd: Document) -> Document {
    m.now += 1;
    let r = m.run(DB, cmd);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    r
}

fn images(on: bool) -> Value {
    Value::Document(Document::new().with("enabled", on))
}

/// Insert, update, replace and delete of document 1 of `coll`.
fn writes(m: &mut Mem, coll: &str) {
    let one = || doc(&[("_id", 1.into())]);
    run(
        m,
        Document::new()
            .with("insert", coll)
            .with("documents", arr(vec![doc(&[("_id", 1.into()), ("a", 1.into())])])),
    );
    let set = Document::new().with("$set", doc(&[("a", 2.into())]));
    run(
        m,
        Document::new()
            .with("update", coll)
            .with("updates", arr(vec![Document::new().with("q", one()).with("u", set)])),
    );
    let replace = doc(&[("b", 1.into())]);
    run(
        m,
        Document::new()
            .with("update", coll)
            .with("updates", arr(vec![Document::new().with("q", one()).with("u", replace)])),
    );
    run(
        m,
        Document::new()
            .with("delete", coll)
            .with("deletes", arr(vec![Document::new().with("q", one()).with("limit", 1)])),
    );
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

#[test]
fn a_collection_that_keeps_images_gives_them_and_one_that_does_not_gives_null() {
    let mut m = Mem::new();
    run(&mut m, Document::new().with("create", "c"));
    run(
        &mut m,
        Document::new().with("create", "p").with("changeStreamPreAndPostImages", images(true)),
    );
    let both = || {
        Document::new()
            .with("fullDocument", "whenAvailable")
            .with("fullDocumentBeforeChange", "whenAvailable")
    };
    let (c, p) = (open(&mut m, "c", both()), open(&mut m, "p", both()));
    writes(&mut m, "c");
    writes(&mut m, "p");
    let off = batch(&get_more(&mut m, c, "c"));
    assert_eq!(off.len(), 4);
    assert!(off[0].get("fullDocumentBeforeChange").is_none(), "an insert has no pre-image");
    assert_eq!(field(&off[1], "fullDocument"), &Value::Null);
    for e in &off[1..] {
        assert_eq!(field(e, "fullDocumentBeforeChange"), &Value::Null);
    }
    let on = batch(&get_more(&mut m, p, "p"));
    let keys: Vec<&str> = on[1].iter().map(|(k, _)| k).collect();
    assert_eq!(keys[keys.len() - 2..], ["fullDocumentBeforeChange", "fullDocument"]);
    assert_eq!(field(&on[1], "fullDocument"), &d(&[("_id", 1.into()), ("a", 2.into())]));
    assert_eq!(
        field(&on[1], "fullDocumentBeforeChange"),
        &d(&[("_id", 1.into()), ("a", 1.into())])
    );
    assert_eq!(
        field(&on[2], "fullDocumentBeforeChange"),
        &d(&[("_id", 1.into()), ("a", 2.into())])
    );
    assert_eq!(field(&on[2], "fullDocument"), &d(&[("_id", 1.into()), ("b", 1.into())]));
    assert_eq!(
        field(&on[3], "fullDocumentBeforeChange"),
        &d(&[("_id", 1.into()), ("b", 1.into())])
    );
    assert_eq!(field(&on[3], "documentKey._id"), &Value::Int32(1));
}

#[test]
fn required_images_that_are_missing_fail_as_on_the_reference_router() {
    let mut m = Mem::new();
    run(&mut m, Document::new().with("create", "c"));
    let post = open(&mut m, "c", Document::new().with("fullDocument", "required"));
    let pre = open(&mut m, "c", Document::new().with("fullDocumentBeforeChange", "required"));
    writes(&mut m, "c");
    let e = get_more(&mut m, post, "c");
    assert_eq!(e.get("code"), Some(&Value::Int32(47)), "{e:?}");
    let msg = match e.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    };
    // Through the reference router the shard that read the event names itself (measured on 01-11).
    let msg = msg
        .strip_prefix("Error on remote shard ")
        .and_then(|m| m.split_once(" :: caused by :: ").map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("no shard prefix: {msg}"));
    assert!(msg.starts_with("Executor error during getMore :: caused by :: Change stream was configured to require a post-image for all update events, but the post-image was not found for event: {operationType: \"update\", ns: {db: \"img\", coll: \"c\"}, clusterTime: Timestamp("), "{msg}");
    let e = get_more(&mut m, pre, "c");
    assert_eq!(e.get("code"), Some(&Value::Int32(47)), "{e:?}");
    assert!(
        matches!(e.get("errmsg"), Some(Value::String(s)) if s.contains("require a pre-image for all update, delete and replace events"))
    );
}

#[test]
fn the_images_follow_the_option_at_the_time_of_each_write() {
    let mut m = Mem::new();
    run(
        &mut m,
        Document::new().with("create", "p").with("changeStreamPreAndPostImages", images(true)),
    );
    let both = Document::new()
        .with("fullDocument", "whenAvailable")
        .with("fullDocumentBeforeChange", "whenAvailable");
    let id = open(&mut m, "p", both);
    let one = || doc(&[("_id", 2.into())]);
    let set = |n: i32| {
        Document::new()
            .with("q", one())
            .with("u", Document::new().with("$set", doc(&[("a", n.into())])))
    };
    run(
        &mut m,
        Document::new()
            .with("insert", "p")
            .with("documents", arr(vec![doc(&[("_id", 2.into()), ("a", 1.into())])])),
    );
    run(
        &mut m,
        Document::new().with("collMod", "p").with("changeStreamPreAndPostImages", images(false)),
    );
    run(&mut m, Document::new().with("update", "p").with("updates", arr(vec![set(3)])));
    run(
        &mut m,
        Document::new().with("collMod", "p").with("changeStreamPreAndPostImages", images(true)),
    );
    run(&mut m, Document::new().with("update", "p").with("updates", arr(vec![set(4)])));
    let events = batch(&get_more(&mut m, id, "p"));
    assert_eq!(events.len(), 3);
    assert_eq!(field(&events[1], "fullDocument"), &Value::Null);
    assert_eq!(field(&events[1], "fullDocumentBeforeChange"), &Value::Null);
    assert_eq!(
        field(&events[2], "fullDocumentBeforeChange"),
        &d(&[("_id", 2.into()), ("a", 3.into())])
    );
    assert_eq!(field(&events[2], "fullDocument"), &d(&[("_id", 2.into()), ("a", 4.into())]));
}

#[test]
fn the_option_is_checked_as_on_the_reference_router() {
    let mut m = Mem::new();
    m.run(DB, Document::new().with("create", "c"));
    let try_mod = |m: &mut Mem, v: Value| {
        let r =
            m.run(DB, Document::new().with("collMod", "c").with("changeStreamPreAndPostImages", v));
        (r.get("code").cloned(), r.get("errmsg").cloned())
    };
    let msg = |s: &str| Some(Value::from(s));
    assert_eq!(
        try_mod(&mut m, Value::Document(Document::new().with("enabled", "x"))),
        (
            Some(Value::Int32(14)),
            msg(
                "BSON field 'collMod.changeStreamPreAndPostImages.enabled' is the wrong type 'string', expected type 'bool'"
            )
        )
    );
    assert_eq!(
        try_mod(&mut m, Value::Document(Document::new())),
        (
            Some(Value::Int32(40414)),
            msg(
                "BSON field 'collMod.changeStreamPreAndPostImages.enabled' is missing but a required field"
            )
        )
    );
    assert_eq!(
        try_mod(&mut m, Value::Boolean(true)),
        (
            Some(Value::Int32(14)),
            msg(
                "BSON field 'collMod.changeStreamPreAndPostImages' is the wrong type 'bool', expected type 'object'"
            )
        )
    );
    let r = m.run(
        DB,
        Document::new().with("create", "q").with(
            "changeStreamPreAndPostImages",
            Value::Document(Document::new().with("enabled", true).with("x", 1)),
        ),
    );
    assert_eq!(r.get("code"), Some(&Value::Int32(40415)));
    assert_eq!(
        r.get("errmsg"),
        msg("BSON field 'create.changeStreamPreAndPostImages.x' is an unknown field.").as_ref()
    );
}

fn open_with(m: &mut Mem, coll: &str, spec: Document, rest: Vec<Document>) -> i64 {
    let mut stages = vec![Document::new().with("$changeStream", spec)];
    stages.extend(rest);
    let cmd = Document::new()
        .with("aggregate", coll)
        .with("pipeline", arr(stages))
        .with("cursor", Document::new());
    let r = m.run(DB, cmd);
    match field(&r, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?} in {r:?}"),
    }
}

#[test]
fn a_large_event_fails_whole_and_comes_in_fragments_with_the_split_stage() {
    // 014 T018 (FR-011): an update of a 9 MiB document with both images (measured on 01-11).
    let mut m = Mem::new();
    run(
        &mut m,
        Document::new().with("create", "c").with("changeStreamPreAndPostImages", images(true)),
    );
    let spec = || {
        Document::new()
            .with("fullDocument", "required")
            .with("fullDocumentBeforeChange", "required")
    };
    let plain = open_with(&mut m, "c", spec(), vec![]);
    let splitting = Document::new().with("$changeStreamSplitLargeEvent", Document::new());
    let split = open_with(&mut m, "c", spec(), vec![splitting.clone()]);
    let big = "x".repeat(9 * 1024 * 1024);
    let one = doc(&[("_id", 1.into()), ("a", big.as_str().into())]);
    run(&mut m, Document::new().with("insert", "c").with("documents", arr(vec![one])));
    let set = Document::new().with("$set", doc(&[("b", 1.into())]));
    let u = Document::new().with("q", doc(&[("_id", 1.into())])).with("u", set);
    run(&mut m, Document::new().with("update", "c").with("updates", arr(vec![u])));
    let e = get_more(&mut m, plain, "c");
    assert_eq!(e.get("code"), Some(&Value::Int32(10334)), "{:?}", e.get("errmsg"));
    let msg = match e.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    };
    // With the shard prefix the reference router 9.0.4 puts before it (`cs-split-large` step 10,
    // 04-11).
    assert!(msg.starts_with("Error on remote shard "), "{msg}");
    assert!(msg.contains(" :: caused by :: Executor error during getMore :: caused by :: Serializing Document failed :: caused by :: Size "), "{msg}");
    assert!(msg.ends_with(" exceeds maximum 16793600"), "{msg}");
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.extend(batch(&get_more(&mut m, split, "c")));
    }
    let shape: Vec<(Option<&Value>, Option<&Value>)> =
        seen.iter().map(|e| (e.get("operationType"), e.get("splitEvent"))).collect();
    assert_eq!(seen.len(), 3, "{shape:?}");
    assert_eq!(field(&seen[1], "splitEvent"), &d(&[("fragment", 1.into()), ("of", 2.into())]));
    let keys: Vec<&str> = seen[2].iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "splitEvent", "fullDocumentBeforeChange"]);
    // Resuming after the first fragment gives the second.
    let after = field(&seen[1], "_id").clone();
    // Without the stage, a fragment's token is refused when the stream opens (measured on 02-11).
    let stages =
        vec![Document::new().with("$changeStream", spec().with("resumeAfter", after.clone()))];
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", arr(stages))
        .with("cursor", Document::new());
    let r = m.run(DB, cmd);
    assert_eq!(r.get("code"), Some(&Value::Int32(280)), "{r:?}");
    let labels = Value::Array(vec![Value::from("NonResumableChangeStreamError")]);
    assert_eq!(r.get("errorLabels"), Some(&labels), "measured on 04-11: {r:?}");
    let want = "To resume from a split event, the $changeStream pipeline must include a $changeStreamSplitLargeEvent stage";
    assert_eq!(r.get("errmsg"), Some(&Value::String(want.into())));
    let resumed = open_with(&mut m, "c", spec().with("resumeAfter", after), vec![splitting]);
    let rest = batch(&get_more(&mut m, resumed, "c"));
    assert_eq!(rest.len(), 1);
    assert_eq!(field(&rest[0], "splitEvent"), &d(&[("fragment", 2.into()), ("of", 2.into())]));
}

/// A stream without `$changeStreamSplitLargeEvent` reads its events as the reference router 9.0.4
/// reads a shard's batch (measured on 04-11-2026, `cs-split-large`): in order, until the next event
/// does not fit in what is left of 16 MiB, which starts the next batch. An event too large to
/// serialize fails the batch it falls in, with the events of that batch, and the cursor is gone;
/// the batches before it come out. Before, the event before the large one came out and the
/// `getMore` after it failed.
fn too_large_stream(m: &mut Mem) -> i64 {
    // Each read of the range gives one millisecond of its history, so that each event comes in
    // a read of its own, as the node's reads of 4 MiB cut a history of large versions (014
    // FR-003): the batch of the shard goes on reading until it holds 16 MiB.
    m.changes_window_ms = 1;
    run(m, Document::new().with("create", "c").with("changeStreamPreAndPostImages", images(true)));
    let spec = Document::new()
        .with("fullDocument", "required")
        .with("fullDocumentBeforeChange", "required");
    open_with(m, "c", spec, vec![])
}

fn insert_big(m: &mut Mem, id: i32) {
    m.now += 1;
    let big = "x".repeat(9 * 1024 * 1024);
    let one = doc(&[("_id", id.into()), ("a", big.as_str().into())]);
    run(m, Document::new().with("insert", "c").with("documents", arr(vec![one])));
}

/// An update of document `id` of 9 MiB: its event, with the two images, passes 16 MiB.
fn update_big(m: &mut Mem, id: i32) {
    m.now += 1;
    let set = Document::new().with("$set", doc(&[("b", 1.into())]));
    let u = Document::new().with("q", doc(&[("_id", id.into())])).with("u", set);
    run(m, Document::new().with("update", "c").with("updates", arr(vec![u])));
}

fn one_more(m: &mut Mem, cursor: i64) -> Document {
    m.now += 1;
    let cmd = Document::new()
        .with("getMore", cursor)
        .with("collection", "c")
        .with("batchSize", 1)
        .with("maxTimeMS", 1);
    m.run(DB, cmd)
}

fn fails_too_large(r: &Document) {
    assert_eq!(r.get("code"), Some(&Value::Int32(10334)), "{r:?}");
    let msg = match r.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    };
    assert!(msg.starts_with("Error on remote shard "), "{msg}");
    assert!(
        msg.contains(" :: caused by :: Executor error during getMore :: caused by :: Serializing Document failed :: caused by :: Size "),
        "{msg}"
    );
}

#[test]
fn an_event_too_large_fails_the_shard_batch_it_falls_in() {
    let mut m = Mem::new();
    let plain = too_large_stream(&mut m);
    insert_big(&mut m, 1);
    update_big(&mut m, 1);
    // The insert and the update fall in the same batch: the first `getMore` fails.
    fails_too_large(&one_more(&mut m, plain));
    let gone = one_more(&mut m, plain);
    assert_eq!(gone.get("code"), Some(&Value::Int32(43)), "{gone:?}");
}

#[test]
fn the_batches_before_the_one_of_an_event_too_large_come_out() {
    let mut m = Mem::new();
    let plain = too_large_stream(&mut m);
    insert_big(&mut m, 1);
    // Two events of 9 MiB do not fit in one batch: the second starts the next one, with the
    // update after it, which is too large.
    insert_big(&mut m, 2);
    update_big(&mut m, 2);
    let first = batch(&one_more(&mut m, plain));
    assert_eq!(first.len(), 1);
    assert_eq!(field(&first[0], "documentKey"), &Value::Document(doc(&[("_id", 1.into())])));
    fails_too_large(&one_more(&mut m, plain));
    let gone = one_more(&mut m, plain);
    assert_eq!(gone.get("code"), Some(&Value::Int32(43)), "{gone:?}");
}

/// `changeStreamOptions.preAndPostImages.expireAfterSeconds` (007 T046, FR-019) limits how long
/// the images of an event are served: past it, `whenAvailable` gives `null` and `required` fails,
/// as once the reference server removed them; `"off"` serves them again.
#[test]
fn images_older_than_their_expiry_are_not_served() {
    let mut m = Mem::new();
    run(
        &mut m,
        Document::new().with("create", "p").with("changeStreamPreAndPostImages", images(true)),
    );
    let expire = |m: &mut Mem, v: Value| {
        let options =
            Document::new().with("preAndPostImages", Document::new().with("expireAfterSeconds", v));
        let set = Document::new()
            .with("setClusterParameter", Document::new().with("changeStreamOptions", options));
        let r = m.run("admin", set);
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    };
    expire(&mut m, Value::Int32(60));
    let both = || {
        Document::new()
            .with("fullDocument", "whenAvailable")
            .with("fullDocumentBeforeChange", "whenAvailable")
    };
    let fresh = open(&mut m, "p", both());
    let late = open(&mut m, "p", both());
    let required = open(&mut m, "p", Document::new().with("fullDocumentBeforeChange", "required"));
    writes(&mut m, "p");
    // Within the minute the images are there.
    let now = batch(&get_more(&mut m, fresh, "p"));
    assert_eq!(
        field(&now[1], "fullDocumentBeforeChange"),
        &d(&[("_id", 1.into()), ("a", 1.into())])
    );
    // Two minutes later they expired.
    m.now += 120_000;
    let later = batch(&get_more(&mut m, late, "p"));
    assert_eq!(later.len(), 4);
    for e in &later[1..] {
        assert_eq!(field(e, "fullDocumentBeforeChange"), &Value::Null);
    }
    assert_eq!(field(&later[1], "fullDocument"), &Value::Null);
    let r = get_more(&mut m, required, "p");
    assert_eq!(r.get("code"), Some(&Value::Int32(47)), "{r:?}");
}
