//! The segment in memory holds and answers what it did before its terms went into a table of
//! open addressing and its lists into chunks (011 T060, FR-032): `data/index_fingerprints.txt`
//! holds a fingerprint of the contents, the statistics and the hits of each query of an index
//! built with inserts, replacements and deletes, recorded with the code of 14-11-2026 before the
//! change. Each fingerprint is printed before it is compared, so a run with `--nocapture` on
//! another version of the code writes the file anew.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};
use teseladb_search::definition::Definition;
use teseladb_search::index::MemIndex;
use teseladb_search::query::search;

const FINGERPRINTS: &str = include_str!("data/index_fingerprints.txt");

/// Documents put, replaced or deleted.
const OPERATIONS: u64 = 3000;
/// Keys the documents take, so many operations replace or delete a document.
const KEYS: u64 = 700;

const WORDS: &[&str] = &[
    "gato",
    "perro",
    "raton",
    "casa",
    "Casa",
    "árbol",
    "arbol",
    "niño",
    "ninos",
    "corre",
    "corriendo",
    "salta",
    "saltó",
    "rojo",
    "azul",
    "verde",
    "WiFi",
    "wi-fi",
    "the",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "dogs",
    "running",
    "runner",
    "3.14",
    "x_y",
    "naïve",
    "café",
    "cafe",
    "Straße",
    "strasse",
    "日本語",
    "テキスト",
    "한국어",
    "ไทย",
];

/// Dynamic mappings with a field of shingles, which stack tokens, and an `autocomplete` field.
fn definition() -> Definition {
    let shingles = Document::new()
        .with("name", "sh")
        .with("tokenizer", Document::new().with("type", "standard"))
        .with(
            "tokenFilters",
            vec![
                Value::Document(Document::new().with("type", "lowercase")),
                Value::Document(
                    Document::new()
                        .with("type", "shingle")
                        .with("minShingleSize", 2)
                        .with("maxShingleSize", 3),
                ),
            ],
        );
    let fields = Document::new()
        .with("s", Document::new().with("type", "string").with("analyzer", "sh"))
        .with(
            "a",
            Document::new()
                .with("type", "autocomplete")
                .with("tokenization", "edgeGram")
                .with("minGrams", 2)
                .with("maxGrams", 5),
        );
    let def = Document::new()
        .with("mappings", Document::new().with("dynamic", true).with("fields", fields))
        .with("analyzers", vec![Value::Document(shingles)]);
    Definition::parse(&def).expect("valid definition")
}

/// A deterministic sequence of numbers.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    fn text(&mut self) -> String {
        let n = self.below(12);
        let mut words: Vec<&str> =
            (0..n).map(|_| WORDS[self.below(WORDS.len() as u64) as usize]).collect();
        // Runs of words that the phrase queries find, with or without words between them.
        let at = self.below(n + 1) as usize;
        let run: &[&str] = match self.below(4) {
            0 => &["the", "quick", "brown", "fox"],
            1 => &["gato", "rojo", "perro"],
            2 => &["rojo", "azul", "verde"],
            _ => &[],
        };
        words.splice(at..at, run.iter().copied());
        words.join(" ")
    }
}

fn index() -> MemIndex {
    let mut rng = Rng(7);
    let mut ix = MemIndex::new(definition());
    for _ in 0..OPERATIONS {
        let key = rng.below(KEYS);
        let id = (key as u32).to_be_bytes().to_vec();
        if rng.below(5) == 0 {
            ix.delete(&id);
            continue;
        }
        let mut doc = Document::new().with("_id", key as i64).with("t", rng.text());
        if rng.below(3) == 0 {
            let values: Vec<Value> = (0..rng.below(4)).map(|_| Value::from(rng.text())).collect();
            doc = doc.with("t", values);
        }
        doc = doc.with("s", rng.text()).with("a", rng.text());
        ix.put(&id, &doc).expect("indexed");
    }
    ix
}

fn query(operator: &str, body: Document) -> Document {
    Document::new().with(operator, body)
}

fn queries() -> Vec<(&'static str, Document)> {
    let text = |q: &str, path: &str| Document::new().with("query", q).with("path", path);
    vec![
        ("text-one", query("text", text("gato", "t"))),
        ("text-many", query("text", text("gato perro the fox", "t"))),
        (
            "text-paths",
            query(
                "text",
                text("casa azul", "t").with("path", vec![Value::from("t"), Value::from("s")]),
            ),
        ),
        (
            "text-fuzzy",
            query(
                "text",
                text("gatto peroo", "t").with("fuzzy", Document::new().with("maxEdits", 1)),
            ),
        ),
        ("phrase", query("phrase", text("the quick", "t"))),
        ("phrase-slop", query("phrase", text("gato perro", "t").with("slop", 2))),
        ("phrase-shingles", query("phrase", text("rojo azul verde", "s"))),
        ("autocomplete", query("autocomplete", text("cor", "a"))),
        ("wildcard", query("wildcard", text("ca*", "t").with("allowAnalyzedField", true))),
        (
            "compound",
            query(
                "compound",
                Document::new()
                    .with("must", vec![Value::Document(query("text", text("perro", "t")))])
                    .with("should", vec![Value::Document(query("phrase", text("gato perro", "t")))])
                    .with("mustNot", vec![Value::Document(query("text", text("lazy", "t")))]),
            ),
        ),
    ]
}

/// FNV-1a of `bytes` added to `hash`.
fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }
}

fn of(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    fnv(&mut h, bytes);
    h
}

#[test]
fn the_index_holds_and_answers_what_it_did() {
    let ix = index();
    let mut found: Vec<(String, u64)> = vec![
        ("contents".to_owned(), of(format!("{:?}", ix.contents()).as_bytes())),
        ("stats".to_owned(), of(format!("{:?}", ix.stats()).as_bytes())),
        ("documents".to_owned(), ix.len() as u64),
    ];
    for (name, spec) in queries() {
        let hits = search::run(&ix, &spec).expect("query");
        let mut h = 0xcbf2_9ce4_8422_2325;
        for hit in &hits {
            fnv(&mut h, &hit.key);
            fnv(&mut h, &hit.score.to_bits().to_le_bytes());
        }
        found.push((format!("{name} ({} hits)", hits.len()), h));
    }
    for (name, hash) in &found {
        println!("{name}\t{hash:016x}");
    }
    let expected: BTreeMap<&str, &str> =
        FINGERPRINTS.lines().filter_map(|l| l.split_once('\t')).collect();
    assert_eq!(expected.len(), found.len());
    for (name, hash) in &found {
        assert_eq!(expected.get(name.as_str()), Some(&format!("{hash:016x}").as_str()), "{name}");
    }
}
