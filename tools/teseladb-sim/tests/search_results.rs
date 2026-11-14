//! The search results checker (011 T004, SC-002, SC-006) over histories built by hand: a search
//! that answers what one segment over the committed documents answers passes; one below its
//! mark, one that misses or keeps a document, and one with another score are reported.

use teseladb_bson::Document;
use teseladb_hlc::Hlc;
use teseladb_sim::checkers::search_results::{SearchLog, Write, check, expected};

fn ts(ms: u64) -> Hlc {
    Hlc { physical: ms, logical: 0 }
}

fn text(t: &str) -> Option<Document> {
    Some(Document::new().with("t", t))
}

fn query(word: &str) -> Document {
    Document::new().with("text", Document::new().with("query", word).with("path", "t"))
}

fn writes() -> Vec<Write> {
    vec![
        Write { key: b"a".to_vec(), ts: ts(10), doc: text("alfa beta") },
        Write { key: b"b".to_vec(), ts: ts(20), doc: text("alfa alfa") },
        Write { key: b"a".to_vec(), ts: ts(30), doc: text("gama beta") },
        Write { key: b"c".to_vec(), ts: ts(40), doc: None },
    ]
}

fn log(target: u64, applied: u64, word: &str, found: Vec<(Vec<u8>, u32)>) -> SearchLog {
    SearchLog { target: ts(target), applied: ts(applied), query: query(word), vector: false, found }
}

#[test]
fn the_oracle_takes_the_newest_version_at_the_mark() {
    let w = writes();
    let at_25: Vec<Vec<u8>> =
        expected(&w, ts(25), &query("alfa"), false).into_iter().map(|f| f.0).collect();
    assert_eq!(at_25, [b"b".to_vec(), b"a".to_vec()]);
    let at_35: Vec<Vec<u8>> =
        expected(&w, ts(35), &query("alfa"), false).into_iter().map(|f| f.0).collect();
    assert_eq!(at_35, [b"b".to_vec()]);
    assert!(expected(&w, ts(5), &query("alfa"), false).is_empty());
}

#[test]
fn an_answer_as_the_oracle_passes() {
    let w = writes();
    let right = log(25, 35, "alfa", expected(&w, ts(35), &query("alfa"), false));
    assert_eq!(check(&w, &[right]), Ok(()));
}

#[test]
fn an_answer_below_its_mark_is_reported() {
    let w = writes();
    let early = log(35, 25, "alfa", expected(&w, ts(25), &query("alfa"), false));
    let v = check(&w, &[early]).unwrap_err();
    assert!(v.detail.contains("below its mark"), "{v:?}");
}

#[test]
fn a_stale_document_or_another_score_is_reported() {
    let w = writes();
    // The old version of `a` still answers at 35.
    let stale = log(35, 35, "alfa", expected(&w, ts(25), &query("alfa"), false));
    assert!(check(&w, &[stale]).is_err());
    let mut scored = expected(&w, ts(35), &query("alfa"), false);
    scored[0].1 += 1;
    assert!(check(&w, &[log(35, 35, "alfa", scored)]).is_err());
}

#[test]
fn without_a_search_it_is_a_liveness_violation() {
    let v = check(&writes(), &[]).unwrap_err();
    assert!(v.detail.contains("no search"), "{v:?}");
}
