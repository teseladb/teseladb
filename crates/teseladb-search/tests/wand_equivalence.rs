//! Block-Max WAND gives the first `k` documents of evaluating the whole query (011 FR-030,
//! T058): the same keys, scores bit for bit, sequence tokens, score details and highlights, on
//! one segment and on several (statistics of every segment), for `text` alone and as the
//! `must` of a `compound` whose `filter` and `mustNot` give the candidates.

use teseladb_bson::{Document, Value};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::{Found, Indexer};

/// Words with many shared and some rare ones, so that scores tie and blocks differ.
const WORDS: [&str; 24] = [
    "gato", "perro", "raton", "queso", "casa", "jardin", "calle", "rio", "monte", "mar", "sol",
    "luna", "arbol", "flor", "piedra", "nube", "viento", "fuego", "agua", "tierra", "rojo", "azul",
    "verde", "negro",
];

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }
}

fn definition() -> Definition {
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    Definition::parse(&def).expect("valid")
}

/// A document of a few words, skewed towards the first ones, with a number and a tag.
fn document(rng: &mut Rng, i: u32) -> Document {
    let words = 1 + rng.below(12);
    let text: Vec<&str> =
        (0..words).map(|_| WORDS[(rng.below(24) * rng.below(24) / 24) as usize]).collect();
    let title: Vec<&str> = (0..1 + rng.below(3)).map(|_| WORDS[rng.below(24) as usize]).collect();
    Document::new()
        .with("_id", i as i32)
        .with("t", text.join(" "))
        .with("title", title.join(" "))
        .with("n", rng.below(100) as i32)
        .with("tag", WORDS[rng.below(4) as usize])
}

/// An index of `docs` documents with keys in an order other than their arrival, split at
/// `cuts` into segments, and some documents replaced and deleted.
fn index(rng: &mut Rng, docs: u32, cuts: &[u8]) -> Indexer {
    let mut ix = Indexer::new(definition());
    let mut edges: Vec<Vec<u8>> = vec![Vec::new()];
    edges.extend(cuts.iter().map(|c| vec![*c]));
    edges.push(Vec::new());
    let bounds: Vec<(Vec<u8>, Vec<u8>)> =
        edges.windows(2).map(|w| (w[0].clone(), w[1].clone())).collect();
    ix.reshape(&bounds).expect("segments");
    // Two bytes, the first spread over the cuts.
    let key = |i: u32| (((u64::from(i) * 2_654_435_761) % 65_521) as u16).to_be_bytes().to_vec();
    let changes: Vec<(Vec<u8>, Option<Document>)> =
        (0..docs).map(|i| (key(i), Some(document(rng, i)))).collect();
    ix.apply(changes).expect("indexed");
    let later: Vec<(Vec<u8>, Option<Document>)> = (0..docs / 10)
        .map(|_| {
            let i = rng.below(u64::from(docs)) as u32;
            (key(i), (rng.below(2) == 0).then(|| document(rng, i)))
        })
        .collect();
    ix.apply(later).expect("changed");
    ix
}

fn text(rng: &mut Rng) -> Document {
    let words: Vec<&str> = (0..1 + rng.below(4)).map(|_| WORDS[rng.below(24) as usize]).collect();
    let path: Value = match rng.below(3) {
        0 => "t".into(),
        1 => vec![Value::from("t"), Value::from("title")].into(),
        _ => "title".into(),
    };
    let mut op = Document::new().with("query", words.join(" ")).with("path", path);
    if rng.below(3) == 0 {
        let boost = Document::new().with("value", 1.5 + rng.below(4) as f64);
        op.push("score", Document::new().with("boost", boost));
    }
    Document::new().with("text", op)
}

fn spec(rng: &mut Rng) -> Document {
    let mut spec = match rng.below(3) {
        0 => text(rng),
        _ => {
            let range = Document::new()
                .with("range", Document::new().with("path", "n").with("gte", rng.below(60) as i32));
            let tag = Document::new().with(
                "equals",
                Document::new().with("path", "tag").with("value", WORDS[rng.below(4) as usize]),
            );
            let mut compound = Document::new().with("must", vec![Value::from(text(rng))]);
            compound.push("filter", vec![Value::from(range)]);
            if rng.below(2) == 0 {
                compound.push("mustNot", vec![Value::from(tag)]);
            }
            Document::new().with("compound", compound)
        }
    };
    if rng.below(4) == 0 {
        spec.push("scoreDetails", true);
    }
    if rng.below(5) == 0 {
        spec.push("highlight", Document::new().with("path", "t"));
    }
    spec
}

/// What a result is made of, scores as their bits.
fn shape(found: &[Found]) -> Vec<(Vec<u8>, u32, Option<String>, String, String, Document)> {
    found
        .iter()
        .map(|f| {
            let details = format!("{:?}", f.details);
            let highlights = format!("{:?}", f.highlights);
            (f.key.clone(), f.score.to_bits(), f.token.clone(), details, highlights, f.doc.clone())
        })
        .collect()
}

#[test]
fn the_best_k_are_the_first_k_of_the_whole_query() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut compared = 0;
    for round in 0..24u32 {
        let cuts: &[u8] = match round % 3 {
            0 => &[],
            1 => &[128],
            _ => &[40, 120, 200],
        };
        let ix = index(&mut rng, 300 + round * 60, cuts);
        for _ in 0..25 {
            let spec = spec(&mut rng);
            let whole = ix.search(&spec).expect("searched");
            for k in [1, 2, 7, 10, 64, 1000] {
                let mut top = ix.rank(&spec, Some(k)).expect("ranked");
                for f in &mut top {
                    f.doc = ix.document_of(&f.key).cloned().unwrap_or_default();
                }
                let first: Vec<Found> = whole.iter().take(k).cloned().collect();
                assert_eq!(shape(&top), shape(&first), "{spec:?} k {k}");
                compared += 1;
            }
        }
    }
    assert_eq!(compared, 24 * 25 * 6);
}

#[test]
fn a_sort_or_a_token_reads_every_result() {
    let mut rng = Rng(7);
    let ix = index(&mut rng, 400, &[100]);
    assert!(ix.segments() == 2 && ix.len() > 300);
    let sorted = text(&mut rng).with("sort", Document::new().with("n", 1));
    let whole = ix.search(&sorted).expect("searched");
    let top = ix.rank(&sorted, Some(3)).expect("ranked");
    assert_eq!(top.len(), whole.len());
    assert!(top.iter().all(|f| f.doc.is_empty()));
    let after = text(&mut rng).with("searchAfter", whole[0].token.clone().unwrap_or_default());
    assert_eq!(ix.rank(&after, Some(1)).map(|f| f.len()), ix.search(&after).map(|f| f.len()));
}
