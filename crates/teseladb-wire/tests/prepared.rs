//! An evaluation prepared ahead of the range's core (005 FR-035, T069): the connection's thread
//! evaluates a mutation on the versions it reads, and the range takes that evaluation when the
//! versions in force are byte for byte the same; otherwise it evaluates the mutation itself.
//! What the range proposes is the same either way.

use teseladb_hlc::Hlc;
use teseladb_range::host::Mutation;
use teseladb_wire::commands::mutation::{
    Bound, DocMutation, EvaluatedBy, Image, MutationResult, Rules, Sent, evaluated_counts,
};
use teseladb_bson::{Document, Value};

type Version = Option<(u8, Vec<u8>)>;

fn seen(current: &[Version]) -> Vec<Option<(u8, &[u8])>> {
    current.iter().map(|c| c.as_ref().map(|(t, v)| (*t, v.as_slice()))).collect()
}

fn at(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn update(update: Document, rules: Rules) -> Bound {
    Bound {
        mutation: DocMutation::Update {
            filter: Document::new(),
            update: Value::Document(update),
            array_filters: None,
            multi: true,
            upsert: None,
            image: Image::None,
            rules,
        },
        keys: vec![vec![1], vec![2]],
    }
}

fn insert(docs: Vec<Document>, rules: Rules) -> Bound {
    let keys = (1..=docs.len() as u8).map(|k| vec![k]).collect();
    Bound { mutation: DocMutation::Insert { docs, ordered: true, rules }, keys }
}

/// The versions two documents have once inserted, as a range stores them.
fn stored(docs: Vec<Document>) -> Vec<Version> {
    let b = insert(docs, Rules::default());
    let out = b.eval(&[None, None], at(1)).unwrap();
    let documents = out.puts.iter().filter(|p| p.value_type != teseladb_range::HISTORY_VALUE);
    documents.map(|p| Some((p.value_type, p.value.clone()))).collect()
}

fn two() -> Vec<Version> {
    stored(vec![
        Document::new().with("_id", 1).with("n", 1),
        Document::new().with("_id", 2).with("n", 5),
    ])
}

#[test]
fn a_prepared_evaluation_is_what_the_range_computes_and_spares_it_the_mutation() {
    let b = update(Document::new().with("$inc", Document::new().with("n", 1)), Rules::default());
    let current = two();
    let prepared = b.prepare(&b.keys, current.clone()).expect("no mark in what it writes");
    let sent = Sent::new(b.encode().unwrap(), Some(prepared));
    let taken = || evaluated_counts()[EvaluatedBy::Prepared as usize];
    let before = taken();
    let got = sent.eval_with(&b.keys, &seen(&current), at(9)).unwrap();
    assert_eq!(got, b.eval_with(&b.keys, &seen(&current), at(9)).unwrap());
    assert_eq!(MutationResult::decode(&got.result).unwrap().n_modified, 2);
    assert!(sent.reads(&got).is_empty());
    assert!(!sent.decoded(), "the range never looked at the mutation");
    assert!(taken() > before, "counted as taken");
}

#[test]
fn a_version_that_changed_since_sends_the_range_to_the_mutation() {
    let b = update(Document::new().with("$inc", Document::new().with("n", 1)), Rules::default());
    let current = two();
    let prepared = b.prepare(&b.keys, current.clone()).unwrap();
    let sent = Sent::new(b.encode().unwrap(), Some(prepared));
    // Another write reached the second document first.
    let other = stored(vec![
        Document::new().with("_id", 1).with("n", 1),
        Document::new().with("_id", 2).with("n", 6),
    ]);
    let count = |how: EvaluatedBy| evaluated_counts()[how as usize];
    let (again, core) = (count(EvaluatedBy::Again), count(EvaluatedBy::Core));
    let got = sent.eval_with(&b.keys, &seen(&other), at(9)).unwrap();
    assert_eq!(got, b.eval_with(&b.keys, &seen(&other), at(9)).unwrap());
    assert!(sent.decoded());
    assert!(count(EvaluatedBy::Again) > again, "counted as evaluated again");
    // A write that came without an evaluation is counted apart.
    let plain = Sent::new(b.encode().unwrap(), None);
    assert_eq!(plain.eval_with(&b.keys, &seen(&other), at(9)).unwrap(), got);
    assert!(count(EvaluatedBy::Core) > core);
    // A document that went, or other keys than the ones it was prepared on.
    let gone = vec![current[0].clone(), None];
    let sent = Sent::new(b.encode().unwrap(), b.prepare(&b.keys, current.clone()));
    let got = sent.eval_with(&b.keys, &seen(&gone), at(9)).unwrap();
    assert_eq!(MutationResult::decode(&got.result).unwrap().n_modified, 1);
    let more = [b.keys.clone(), vec![vec![3]]].concat();
    let all = [current.clone(), vec![None]].concat();
    let sent = Sent::new(b.encode().unwrap(), b.prepare(&b.keys, current));
    let got = sent.eval_with(&more, &seen(&all), at(9)).unwrap();
    assert_eq!(got, b.eval_with(&more, &seen(&all), at(9)).unwrap());
    assert!(sent.decoded());
}

#[test]
fn what_depends_on_the_mark_of_the_write_is_not_prepared() {
    let z = Value::Timestamp { time: 0, increment: 0 };
    let current = two();
    // `$currentDate` writes the time of the write.
    let dated = Document::new().with("$currentDate", Document::new().with("t", true));
    let b = update(dated, Rules::default());
    assert!(b.prepare(&b.keys, current.clone()).is_none());
    let sent = Sent::new(b.encode().unwrap(), None);
    let got = sent.eval_with(&b.keys, &seen(&current), at(7_000)).unwrap();
    let doc = Document::from(teseladb_bson::RawDoc::from_bytes(&got.puts[0].value).unwrap());
    assert_eq!(doc.get("t"), Some(&Value::DateTime(7_000)));
    // An empty timestamp the server fills, in an insert and in a replacement.
    let b = insert(vec![Document::new().with("_id", 1).with("t", z.clone())], Rules::default());
    assert!(b.prepare(&b.keys, vec![None]).is_none());
    let b = update(Document::new().with("t", z.clone()), Rules::default());
    assert!(b.prepare(&b.keys, current.clone()).is_none());
    // The same empty timestamp under `$set` stays as it is: nothing of the mark is written.
    let b = update(Document::new().with("$set", Document::new().with("t", z)), Rules::default());
    assert!(b.prepare(&b.keys, current).is_some());
}

#[test]
fn the_result_of_a_retryable_write_takes_the_mark_the_range_gives_it() {
    let retry = teseladb_changestream::meta::RetryInfo {
        lsid: 7,
        uid: [0; 32],
        txn_number: 3,
        stmt_id: 0,
        prev: None,
        image: 0,
    };
    let rules = Rules { retry: Some(retry), ..Rules::default() };
    let b = update(Document::new().with("$inc", Document::new().with("n", 1)), rules);
    let current = two();
    let sent = Sent::new(b.encode().unwrap(), b.prepare(&b.keys, current.clone()));
    for mark in [at(11), at(12)] {
        let got = sent.eval_with(&b.keys, &seen(&current), mark).unwrap();
        assert_eq!(MutationResult::decode(&got.result).unwrap().mark, Some(mark));
        assert_eq!(got, b.eval_with(&b.keys, &seen(&current), mark).unwrap());
    }
    assert!(!sent.decoded());
}

#[test]
fn a_refusal_is_prepared_as_any_result_and_bytes_that_do_not_decode_fail_in_the_range() {
    // A duplicate `_id`: the statement answers with its error and writes nothing.
    let b = insert(vec![Document::new().with("_id", 1)], Rules::default());
    let current = vec![two().remove(0)];
    let prepared = b.prepare(&b.keys, current.clone()).expect("a refusal is a result");
    let sent = Sent::new(b.encode().unwrap(), Some(prepared));
    let got = sent.eval_with(&b.keys, &seen(&current), at(9)).unwrap();
    assert!(got.puts.is_empty());
    assert_eq!(MutationResult::decode(&got.result).unwrap().errors.len(), 1);
    assert_eq!(got, b.eval_with(&b.keys, &seen(&current), at(9)).unwrap());
    let broken = Sent::new(vec![1, 2, 3], None);
    assert!(broken.eval_with(&b.keys, &seen(&current), at(9)).is_err());
    assert!(broken.eval(&seen(&current), at(9)).is_err());
    assert!(broken.reads(&got).is_empty());
}
