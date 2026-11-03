//! The `$search` scenarios (011 T041, FR-024, SC-003, SC-004) against `--target` or a local
//! cluster of three nodes, over `--records` documents whose text field `t` holds eight words of a
//! fixed vocabulary drawn with a Zipf law from a fixed seed, under a dynamic `default` index built
//! before the load:
//!
//! - `search-term`: a `text` query for one word, the 10 best (SC-004: p50 under 5 ms over a
//!   million documents);
//! - `search-multi`: a `compound` with two `must` words and a `should` one, the 10 best;
//! - `search-phrase`: a `phrase` of two words of one document, the 10 best;
//! - `search-fuzzy`: a `text` query with `fuzzy.maxEdits` 1 for a word with its last letter
//!   changed, the 10 best;
//! - `search-visibility`: an insert confirmed by the majority with the journal of a document with
//!   a word nobody else has, then the search for it (SC-003: found by the next search every time,
//!   with the wait for it under 10 ms at p99).
//!
//! Every query has to find something: one that finds nothing is an error.

use std::time::{Duration, Instant};

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_types::Histogram;

use super::ycsb_gen::{Rng, Zipfian};
use crate::cli::Args;
use crate::client::{Client, Target, check, doc};
use crate::report::{Report, Target as Goal};

const DB: &str = "bench";
const COLLECTION: &str = "texts";
/// Words of the vocabulary and words per document.
const VOCABULARY: u64 = 2_000;
const WORDS: usize = 8;
/// Zipf constant of the words (YCSB's).
const THETA: f64 = 0.99;
/// Results a query asks for.
const LIMIT: i32 = 10;
/// Documents per `insert` of the load.
const BATCH: usize = 500;
/// Queries measured at least, whatever the duration.
const MIN_QUERIES: u64 = 50;
/// Longest wait for the index to be `READY`.
const INDEX_READY: Duration = Duration::from_secs(3_600);
const SEED: u64 = 0x5345_4152;
/// SC-004: a term query under 5 ms at p50.
const TERM_P50_NS: f64 = 5_000_000.0;
/// SC-003: a document found by the next search every time, waited for under 10 ms at p99.
const VISIBLE_P99_NS: f64 = 10_000_000.0;

/// Word `n` of the vocabulary: letters only, so that the standard analyzer keeps it whole.
pub fn word(n: u64) -> String {
    let mut s = String::from("tes");
    let mut n = n;
    loop {
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 {
            return s;
        }
    }
}

/// The words of document `i`.
pub fn words(i: u64) -> Vec<String> {
    let mut rng = Rng::new(SEED ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let zipf = Zipfian::new(VOCABULARY, THETA);
    (0..WORDS).map(|_| word(zipf.next(&mut rng))).collect()
}

/// `w` with its last letter changed: one edit away.
pub fn misspelled(w: &str) -> String {
    let mut s: Vec<u8> = w.bytes().collect();
    // An empty word, which the vocabulary never has, becomes `a`.
    let last = s.pop().unwrap_or(b'a' - 1);
    s.push(if last == b'z' { b'y' } else { last + 1 });
    String::from_utf8(s).expect("ASCII")
}

/// Creates the dynamic `default` search index of `collection` and waits until it is `READY`.
pub fn default_index(
    c: &mut Client,
    db: &str,
    collection: &str,
    limit: Duration,
) -> Result<(), String> {
    let definition =
        doc(vec![("mappings", Value::Document(doc(vec![("dynamic", Value::Boolean(true))])))]);
    let index = doc(vec![
        ("name", Value::String("default".into())),
        ("definition", Value::Document(definition)),
    ]);
    check(&c.command(
        db,
        doc(vec![
            ("createSearchIndexes", Value::String(collection.into())),
            ("indexes", Value::Array(vec![Value::Document(index)])),
        ]),
    )?)?;
    wait_ready(c, db, collection, limit)
}

/// Waits until a search index of `collection` lists as `READY`.
pub fn wait_ready(
    c: &mut Client,
    db: &str,
    collection: &str,
    limit: Duration,
) -> Result<(), String> {
    let list = doc(vec![("listSearchIndexes", Value::String(collection.into()))]);
    let listed = crate::client::retry(limit, Duration::from_millis(250), || {
        Ok(c.command(db, list.clone())?.windows(5).any(|w| w == b"READY").then_some(()))
    })?;
    listed.map(drop).ok_or(format!("the search index of {collection} was not READY in time"))
}

fn load(c: &mut Client, records: u64) -> Result<(), String> {
    let ids: Vec<u64> = (0..records).collect();
    for chunk in ids.chunks(BATCH) {
        let docs = chunk
            .iter()
            .map(|&i| {
                let t = words(i).join(" ");
                Value::Document(doc(vec![("_id", Value::Int64(i as i64)), ("t", Value::String(t))]))
            })
            .collect();
        let insert = doc(vec![
            ("insert", Value::String(COLLECTION.into())),
            ("documents", Value::Array(docs)),
        ]);
        check(&c.command(DB, insert)?)?;
    }
    Ok(())
}

fn text(query: &str) -> Document {
    doc(vec![("query", Value::String(query.into())), ("path", Value::String("t".into()))])
}

/// The operator of query `n` of `scenario`.
fn operator(scenario: &str, n: u64, records: u64) -> Result<(&'static str, Document), String> {
    let sample = words(n % records.max(1));
    Ok(match scenario {
        "search-term" => ("text", text(&sample[0])),
        "search-multi" => {
            let must = vec![
                Value::Document(doc(vec![("text", Value::Document(text(&sample[0])))])),
                Value::Document(doc(vec![("text", Value::Document(text(&sample[1])))])),
            ];
            let should =
                vec![Value::Document(doc(vec![("text", Value::Document(text(&sample[2])))]))];
            let compound =
                doc(vec![("must", Value::Array(must)), ("should", Value::Array(should))]);
            ("compound", compound)
        }
        "search-phrase" => ("phrase", text(&format!("{} {}", sample[0], sample[1]))),
        "search-fuzzy" => {
            let mut t = text(&misspelled(&sample[0]));
            t.insert("fuzzy", Value::Document(doc(vec![("maxEdits", Value::Int32(1))])));
            ("text", t)
        }
        other => return Err(format!("not a search scenario: {other}")),
    })
}

/// The `_id`s a `$search` answers, best first.
fn search(c: &mut Client, op: &str, body: Document) -> Result<Vec<i64>, String> {
    let stage = doc(vec![(op, Value::Document(body))]);
    let pipeline = vec![
        Value::Document(doc(vec![("$search", Value::Document(stage))])),
        Value::Document(doc(vec![("$limit", Value::Int32(LIMIT))])),
        Value::Document(doc(vec![(
            "$project",
            Value::Document(doc(vec![("_id", Value::Int32(1))])),
        )])),
    ];
    let aggregate = doc(vec![
        ("aggregate", Value::String(COLLECTION.into())),
        ("pipeline", Value::Array(pipeline)),
        ("cursor", Value::Document(Document::new())),
    ]);
    let reply = c.command(DB, aggregate)?;
    check(&reply)?;
    ids(&reply)
}

/// The `_id`s of a cursor's first batch, in order.
pub fn ids(reply: &[u8]) -> Result<Vec<i64>, String> {
    let d = RawDoc::from_bytes(reply).map_err(crate::client::debug)?;
    let Some(RawValue::Document(cursor)) = d.get("cursor") else {
        return Err(format!("a reply without a cursor: {:?}", Document::from(d)));
    };
    let Some(RawValue::Array(batch)) = cursor.get("firstBatch") else {
        return Err("a cursor without a batch".into());
    };
    Ok(batch
        .iter()
        .filter_map(|e| match e {
            RawValue::Document(e) => match e.get("_id") {
                Some(RawValue::Int64(n)) => Some(n),
                _ => None,
            },
            _ => None,
        })
        .collect())
}

/// The queries of `search-term`, `search-multi`, `search-phrase` and `search-fuzzy`.
fn queries(c: &mut Client, args: &Args, report: &mut Report) -> Result<(), String> {
    let mut n = 0u64;
    // One query: how long it took.
    let mut one = |c: &mut Client| -> Result<Duration, String> {
        n += 1;
        let (op, body) = operator(&args.scenario, n, args.records)?;
        let asked = Instant::now();
        let found = search(c, op, body)?;
        let took = asked.elapsed();
        if found.is_empty() {
            return Err(format!("{} query {n} found nothing", args.scenario));
        }
        Ok(took)
    };
    super::warm_up(args.warmup, || one(c).map(drop))?;
    let mut latency = Histogram::new();
    let (start, mut measured) = (Instant::now(), 0u64);
    while start.elapsed() < args.duration || measured < MIN_QUERIES {
        latency.record(one(c)?.as_nanos() as u64);
        measured += 1;
    }
    let goal = (args.scenario == "search-term").then_some(TERM_P50_NS);
    report.latency("query", &latency, goal, None);
    Ok(())
}

/// `search-visibility`: each insert is searched for at once, and again until it is found.
fn visibility(c: &mut Client, args: &Args, report: &mut Report) -> Result<(), String> {
    let mut n = 0u64;
    // One insert: how long until a search found it, and the searches that did not.
    let mut one = |c: &mut Client| -> Result<(Duration, u32), String> {
        n += 1;
        let id = (args.records + n) as i64;
        let unique = word(VOCABULARY + n);
        let wc = doc(vec![("w", Value::String("majority".into())), ("j", Value::Boolean(true))]);
        let document = doc(vec![("_id", Value::Int64(id)), ("t", Value::String(unique.clone()))]);
        check(&c.command(
            DB,
            doc(vec![
                ("insert", Value::String(COLLECTION.into())),
                ("documents", Value::Array(vec![Value::Document(document)])),
                ("writeConcern", Value::Document(wc)),
            ]),
        )?)?;
        let written = Instant::now();
        let found = crate::client::retry(INDEX_READY, Duration::ZERO, || {
            Ok(search(c, "text", text(&unique))?.contains(&id).then_some(()))
        })?;
        let ((), tries) = found.ok_or(format!("document {id} never became visible"))?;
        Ok((written.elapsed(), tries))
    };
    super::warm_up(args.warmup, || one(c).map(drop))?;
    let mut wait = Histogram::new();
    let (start, mut first, mut measured) = (Instant::now(), 0u64, 0u64);
    while start.elapsed() < args.duration || measured < MIN_QUERIES {
        let (waited, tries) = one(c)?;
        wait.record(waited.as_nanos() as u64);
        first += u64::from(tries == 0);
        measured += 1;
    }
    report.latency("visible", &wait, None, Some(VISIBLE_P99_NS));
    report.push("visible.next", "ratio", first as f64 / measured as f64, Some(Goal::AtLeast(1.0)));
    Ok(())
}

/// Runs `args.scenario`, one of the `$search` scenarios.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let _ = c.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    load(&mut c, args.records)?;
    default_index(&mut c, DB, COLLECTION, INDEX_READY)?;
    let mut report = Report::new(&args.scenario);
    match args.scenario.as_str() {
        "search-visibility" => visibility(&mut c, args, &mut report)?,
        _ => queries(&mut c, args, &mut report)?,
    }
    report.notes.push(format!(
        "{} documents of {WORDS} words (vocabulary of {VOCABULARY}, Zipf {THETA}), limit {LIMIT}, \
         target {}",
        args.records,
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_corpus_and_the_queries_follow_the_seed() {
        assert_eq!(word(0), "tesa");
        assert_eq!(word(27), "tesbb");
        assert_eq!(words(5), words(5));
        assert_eq!(words(5).len(), WORDS);
        assert_eq!(misspelled("tesaz"), "tesay");
        assert_eq!(misspelled("tesab"), "tesac");
        assert_eq!(misspelled(""), "a");
        for s in ["search-term", "search-multi", "search-phrase", "search-fuzzy"] {
            assert!(operator(s, 3, 100).is_ok(), "{s}");
        }
        assert!(operator("search-visibility", 3, 100).is_err());
        let (op, body) = operator("search-fuzzy", 3, 100).unwrap();
        assert_eq!(op, "text");
        assert!(body.get("fuzzy").is_some());
    }

    #[test]
    fn the_ids_come_from_the_first_batch() {
        let bytes = |d: Document| d.to_bytes().unwrap();
        let batch = vec![
            Value::Document(doc(vec![("_id", Value::Int64(4))])),
            Value::Document(doc(vec![("_id", Value::Int32(5))])),
            Value::Int32(6),
        ];
        let cursor = Document::new().with("firstBatch", batch);
        assert_eq!(ids(&bytes(Document::new().with("cursor", cursor))), Ok(vec![4]));
        let no_batch = Document::new().with("cursor", Document::new());
        assert_eq!(ids(&bytes(no_batch)), Err("a cursor without a batch".into()));
        let no_cursor = ids(&bytes(Document::new().with("ok", 1.0))).unwrap_err();
        assert!(no_cursor.starts_with("a reply without a cursor"), "{no_cursor}");
        assert!(ids(b"not bson").is_err());
    }
}
