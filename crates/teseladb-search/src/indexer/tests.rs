//! The segments of an index give what one index with all the documents gives: the same scores
//! bit for bit (the statistics of the whole index), the same order and the same count, through
//! splits, merges, replacements and deletions.

use teseladb_bson::{Document, Value};

use super::*;

const TEXTS: [&str; 8] = [
    "gato",
    "gato perro",
    "gato gato perro raton",
    "perro perro perro",
    "raton queso queso queso queso queso queso",
    "el gato y el perro y el raton y el queso y la casa y el jardin y la calle",
    "gato gato gato gato gato gato gato gato gato gato",
    "casa",
];

fn definition() -> Definition {
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    Definition::parse(&def).expect("valid")
}

fn doc(i: usize) -> Document {
    Document::new().with("_id", i as i32).with("t", TEXTS[i])
}

fn changes() -> Vec<(Vec<u8>, Option<Document>)> {
    (0..TEXTS.len()).map(|i| (vec![i as u8], Some(doc(i)))).collect()
}

fn bounds(cuts: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut edges: Vec<Vec<u8>> = vec![Vec::new()];
    edges.extend(cuts.iter().map(|c| vec![*c]));
    edges.push(Vec::new());
    edges.windows(2).map(|w| (w[0].clone(), w[1].clone())).collect()
}

fn text(q: &str) -> Document {
    Document::new().with("text", Document::new().with("query", q).with("path", "t"))
}

fn phrase(q: &str) -> Document {
    Document::new().with("phrase", Document::new().with("query", q).with("path", "t"))
}

fn scores(found: &[Found]) -> Vec<(u8, u32)> {
    found.iter().map(|f| (f.key[0], f.score.to_bits())).collect()
}

fn whole() -> MemIndex {
    let mut one = MemIndex::new(definition());
    for (k, d) in changes() {
        one.put(&k, &d.expect("doc")).expect("indexed");
    }
    one
}

fn reference(spec: &Document) -> Vec<(u8, u32)> {
    let hits = crate::query::search::run(&whole(), spec).expect("ran");
    hits.iter().map(|h| (h.key[0], h.score.to_bits())).collect()
}

#[test]
fn segments_score_as_one_index() {
    let mut ix = Indexer::new(definition());
    ix.reshape(&bounds(&[2, 5])).expect("reshaped");
    ix.apply(changes()).expect("applied");
    assert_eq!(ix.segments(), 3);
    assert_eq!(ix.len(), TEXTS.len());
    for spec in [text("gato"), text("perro queso"), phrase("gato perro")] {
        assert_eq!(scores(&ix.search(&spec).expect("ran")), reference(&spec), "{spec:?}");
    }
}

#[test]
fn one_segment_scores_with_its_own_statistics() {
    let mut ix = Indexer::new(definition());
    ix.apply(changes()).expect("applied");
    assert_eq!(ix.segments(), 1);
    assert_eq!(scores(&ix.search(&text("gato")).expect("ran")), reference(&text("gato")));
}

#[test]
fn a_split_and_a_merge_keep_the_documents_and_the_scores() {
    let mut ix = Indexer::new(definition());
    ix.apply(changes()).expect("applied");
    ix.reshape(&bounds(&[1, 4, 6])).expect("split");
    assert_eq!(ix.segments(), 4);
    assert_eq!(ix.len(), TEXTS.len());
    assert_eq!(scores(&ix.search(&text("raton")).expect("ran")), reference(&text("raton")));
    ix.reshape(&bounds(&[4])).expect("merged");
    assert_eq!(ix.segments(), 2);
    assert_eq!(scores(&ix.search(&text("raton")).expect("ran")), reference(&text("raton")));
    ix.reshape(&[]).expect("no ranges");
    assert_eq!(ix.segments(), 1);
    assert_eq!(ix.len(), TEXTS.len());
}

#[test]
fn replacements_and_deletions_change_the_results() {
    let mut ix = Indexer::new(definition());
    ix.reshape(&bounds(&[4])).expect("reshaped");
    ix.apply(changes()).expect("applied");
    let replaced = Document::new().with("_id", 0).with("t", "queso");
    ix.apply(vec![(vec![0], Some(replaced.clone())), (vec![6], None)]).expect("applied");
    assert_eq!(ix.len(), TEXTS.len() - 1);
    let found = ix.search(&text("gato")).expect("ran");
    assert!(found.iter().all(|f| f.key != vec![0] && f.key != vec![6]), "{found:?}");
    let queso = ix.search(&text("queso")).expect("ran");
    assert_eq!(queso.iter().find(|f| f.key == vec![0]).map(|f| &f.doc), Some(&replaced));
    // Deleting what is not there changes nothing.
    ix.apply(vec![(vec![99], None)]).expect("applied");
    assert_eq!(ix.len(), TEXTS.len() - 1);
}

#[test]
fn meta_counts_across_segments() {
    let mut ix = Indexer::new(definition());
    ix.reshape(&bounds(&[3])).expect("reshaped");
    ix.apply(changes()).expect("applied");
    let gato = ix.meta(&text("gato")).expect("counted");
    assert_eq!(gato.get("count"), Some(&Value::Document(Document::new().with("lowerBound", 5i64))));
    let total = text("perro").with("count", Document::new().with("type", "total"));
    let perro = ix.meta(&total).expect("counted");
    assert_eq!(perro.get("count"), Some(&Value::Document(Document::new().with("total", 4i64))));
    assert!(ix.meta(&text("x").with("count", 5)).is_err());
}

#[test]
fn errors_of_the_query_come_back_with_no_documents() {
    let mut ix = Indexer::new(definition());
    ix.reshape(&bounds(&[3])).expect("reshaped");
    let bad = Document::new().with("text", Document::new().with("query", "a"));
    assert!(ix.search(&bad).is_err());
    assert!(ix.meta(&bad).is_err());
    assert!(ix.is_empty());
}

#[test]
fn a_document_that_cannot_be_analyzed_is_an_error() {
    let definition = Definition { analyzer: Some("reflib.nothing".to_owned()), ..definition() };
    let mut ix = Indexer::new(definition);
    assert!(ix.apply(changes()).is_err());
    ix.reshape(&bounds(&[3])).expect("empty segments reshape");
}

#[test]
fn keys_fall_in_their_range() {
    assert!(in_range(b"b", b"a", b"c"));
    assert!(!in_range(b"c", b"a", b"c"));
    assert!(in_range(b"z", b"a", b""));
    assert!(!in_range(b"0", b"a", b""));
}

/// Three replicas analyze the same log each on its own, with other batches and with the split
/// at another moment, and end with the same segments (FR-011).
#[test]
fn three_replicas_build_the_same_segments() {
    let all = changes();
    let mut log = all.clone();
    log.push((vec![2], Some(doc(5))));
    log.push((vec![6], None));
    log.push((vec![9], Some(doc(1))));
    let mut replicas = vec![Indexer::new(definition()); 3];
    // One batch, then the split.
    replicas[0].apply(log.clone()).expect("applied");
    replicas[0].reshape(&bounds(&[4])).expect("split");
    // The split first, then one change at a time.
    replicas[1].reshape(&bounds(&[4])).expect("split");
    for c in log.clone() {
        replicas[1].apply([c]).expect("applied");
    }
    // Two batches around the split.
    let (a, b) = log.split_at(5);
    replicas[2].apply(a.to_vec()).expect("applied");
    replicas[2].reshape(&bounds(&[4])).expect("split");
    replicas[2].apply(b.to_vec()).expect("applied");
    let first = replicas[0].contents();
    assert_eq!(first.len(), 2);
    for r in &replicas {
        assert_eq!(r.analyzers_version(), crate::analysis::ANALYZERS_VERSION);
        assert_eq!(r.contents(), first);
        assert_eq!(
            scores(&r.search(&text("gato perro")).expect("ran")),
            scores(&replicas[0].search(&text("gato perro")).expect("ran"))
        );
    }
}

#[test]
fn merges_and_memory_are_counted() {
    let mut ix = Indexer::new(definition());
    assert_eq!((ix.merges(), ix.memory_bytes()), (0, 0));
    ix.apply(changes()).expect("applied");
    let one = ix.memory_bytes();
    assert!(one > 0);
    // Splitting the one range rebuilds its segment; keeping the ranges rebuilds nothing.
    ix.reshape(&bounds(&[4])).expect("reshaped");
    assert_eq!(ix.merges(), 1);
    ix.reshape(&bounds(&[4])).expect("reshaped");
    assert_eq!(ix.merges(), 1);
    ix.reshape(&bounds(&[])).expect("reshaped");
    assert_eq!(ix.merges(), 3);
    assert_eq!(ix.memory_bytes(), one);
}

/// A segment whose range stays is kept as it is, a key outside every segment is left out, and
/// files that do not chain their ranges do not load.
#[test]
fn kept_segments_stray_keys_and_unchained_files() {
    let mut ix = Indexer::new(definition());
    assert_eq!(ix.definition().search_analyzer, None);
    ix.apply(changes()).expect("applied");
    ix.reshape(&bounds(&[3])).expect("split");
    ix.reshape(&bounds(&[3, 5])).expect("the first range stays");
    assert_eq!((ix.segments(), ix.len()), (3, TEXTS.len()));
    ix.reshape(&[(vec![2], Vec::new())]).expect("narrowed");
    ix.apply([(vec![0], Some(doc(0)))]).expect("a stray key is left out");
    assert_eq!(ix.len(), TEXTS.len() - 2);
    ix.reshape(&bounds(&[5])).expect("split again");
    let mark = Hlc { physical: 7, logical: 0 };
    let mut files = ix.dump(mark);
    files.reverse();
    assert!(Indexer::load(definition(), &files).is_err());
}

/// Every segment gets the synonym documents, read again only when they change.
#[test]
fn synonyms_reach_every_segment() {
    let def = teseladb_bson::extjson::parse(&format!(
        r#"{{"mappings":{{"dynamic":true}},"synonyms":[{{"name":"sy","analyzer":"{}","source":{{"collection":"syn"}}}}]}}"#,
        teseladb_names::ANALYZER_STANDARD
    ))
    .expect("JSON");
    let mut ix = Indexer::new(Definition::parse(&def).expect("valid"));
    ix.apply(changes()).expect("applied");
    ix.reshape(&bounds(&[4])).expect("split");
    let minino = Document::new().with(
        "text",
        Document::new().with("query", "minino").with("path", "t").with("synonyms", "sy"),
    );
    let gato = Document::new()
        .with("mappingType", "equivalent")
        .with("synonyms", Value::Array(vec!["gato".into(), "minino".into()]));
    ix.set_synonyms("sy", std::slice::from_ref(&gato)).expect("set");
    let found = ix.search(&minino).expect("search").len();
    assert_eq!(found, 5);
    ix.set_synonyms("sy", std::slice::from_ref(&gato)).expect("unchanged");
    assert_eq!(ix.search(&minino).expect("search").len(), found);
    ix.set_synonyms("sy", &[]).expect("emptied");
    assert!(ix.search(&minino).expect("search").is_empty());
}
