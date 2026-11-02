//! Scores of the `text` operator as `refsearch` 1.70.6 gives them on a 9.0.4 replica set
//! (measured 01-11-2026 on the corpus below, index `{mappings: {dynamic: true}}`).

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

fn index() -> MemIndex {
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    for (i, t) in TEXTS.iter().enumerate() {
        let u = if i % 2 == 1 { "gato" } else { "perro casa" };
        let doc = Document::new().with("_id", i as i32).with("t", *t).with("u", u);
        idx.put(&[i as u8], &doc).expect("indexed");
    }
    idx
}

fn scores(hits: Vec<Hit>) -> Vec<(u8, f64)> {
    hits.into_iter().map(|h| (h.key[0], f64::from(h.score))).collect()
}

fn text(idx: &MemIndex, q: &str, paths: &[&str], boost: f32) -> Vec<(u8, f64)> {
    let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
    scores(idx.text(&[q.to_owned()], &paths, boost).expect("scored"))
}

#[test]
fn single_terms_score_as_refsearch() {
    let idx = index();
    assert_eq!(
        text(&idx, "gato", &["t"], 1.0),
        [
            (6, 0.41735297441482544),
            (0, 0.33963894844055176),
            (2, 0.33963894844055176),
            (1, 0.30779778957366943),
            (5, 0.11452940106391907)
        ]
    );
    assert_eq!(
        text(&idx, "perro", &["t"], 1.0),
        [
            (3, 0.5545177459716797),
            (1, 0.43321695923805237),
            (2, 0.36481431126594543),
            (5, 0.1611970067024231)
        ]
    );
    assert_eq!(
        text(&idx, "queso", &["t"], 1.0),
        [(4, 1.0456602573394775), (5, 0.2978915572166443)]
    );
    let casa = text(&idx, "casa", &["u"], 1.0);
    assert_eq!(casa.len(), 4);
    assert!(casa.iter().all(|(_, s)| *s == 0.27725890278816223), "{casa:?}");
}

#[test]
fn terms_paths_and_boosts_add_up_as_refsearch() {
    let idx = index();
    assert_eq!(
        text(&idx, "gato perro", &["t"], 1.0),
        [
            (1, 0.7410147190093994),
            (2, 0.7044532299041748),
            (3, 0.5545177459716797),
            (6, 0.41735297441482544),
            (0, 0.33963894844055176),
            (5, 0.27572640776634216)
        ]
    );
    let both = [
        (1, 0.6726120710372925),
        (5, 0.4793437123298645),
        (6, 0.41735297441482544),
        (3, 0.36481431126594543),
        (7, 0.36481431126594543),
        (0, 0.33963894844055176),
        (2, 0.33963894844055176),
    ];
    assert_eq!(text(&idx, "gato", &["t", "u"], 1.0), both);
    let all = idx.paths("*");
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    assert_eq!(all, ["t", "u"]);
    assert_eq!(text(&idx, "gato", &all, 1.0), both);
    assert_eq!(
        text(&idx, "gato", &["t"], 3.0),
        [
            (6, 1.2520588636398315),
            (0, 1.0189168453216553),
            (2, 1.0189168453216553),
            (1, 0.9233934283256531),
            (5, 0.3435882329940796)
        ]
    );
}

#[test]
fn documents_are_replaced_and_deleted() {
    let mut idx = index();
    assert_eq!(idx.len(), 8);
    idx.delete(&[6]);
    idx.delete(&[42]);
    assert_eq!(idx.len(), 7);
    assert!(text(&idx, "gato", &["t"], 1.0).iter().all(|(k, _)| *k != 6));
    let doc = Document::new().with("t", "perro");
    idx.put(&[0], &doc).expect("indexed");
    assert!(text(&idx, "gato", &["t"], 1.0).iter().all(|(k, _)| *k != 0));
    assert!(text(&idx, "perro", &["t"], 1.0).iter().any(|(k, _)| *k == 0));
    assert!(!idx.is_empty());
}

#[test]
fn mappings_decide_which_strings_are_indexed() {
    let ty = |t: &str| Document::new().with("type", t);
    let def = Document::new().with(
        "mappings",
        Document::new().with(
            "fields",
            Document::new()
                .with("t", ty("string").with("analyzer", "reflib.keyword"))
                .with("s", ty("document").with("dynamic", true)),
        ),
    );
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    let doc = Document::new()
        .with("t", Value::Array(vec!["Hola Mundo".into(), "adios".into()]))
        .with("s", Document::new().with("x", "Gato"))
        .with("other", "gato");
    idx.put(b"a", &doc).expect("indexed");
    assert_eq!(idx.paths("*"), ["s.x", "t"]);
    assert_eq!(text(&idx, "Hola Mundo", &["t"], 1.0).len(), 1);
    assert!(text(&idx, "hola", &["t"], 1.0).is_empty());
    assert_eq!(text(&idx, "gato", &["s.x"], 1.0).len(), 1);
    assert!(text(&idx, "gato", &["other"], 1.0).is_empty());
}

/// The grams of an `autocomplete` word (measured on `refsearch` 1.70.6, 02-11-2026: the five
/// documents of `search-op-autocomplete` sum 296 grams, `avgdl` 59.2): a word shorter than
/// `minGrams` is kept whole, one longer than `maxGrams` gives its first `maxGrams` characters
/// once more.
#[test]
fn autocomplete_keeps_words_shorter_than_the_shortest_gram() {
    let edge = Grams::new(2, 7, true, "edgeGram");
    assert_eq!(edge.of("a"), ["a"]);
    assert_eq!(edge.of("fox"), ["fo", "fox"]);
    assert_eq!(edge.of("jumping"), ["ju", "jum", "jump", "jumpi", "jumpin", "jumping"]);
    assert_eq!(edge.of("jumping fox").last().map(String::as_str), Some("jumping"));
    let right = Grams::new(2, 3, true, "rightEdgeGram");
    assert_eq!(right.of("x"), ["x"]);
    assert_eq!(right.of("abcd"), ["dc", "dcb", "dcb"]);
}
