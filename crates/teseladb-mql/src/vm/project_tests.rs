//! Projections on the register machine against the tree evaluator (010 FR-031): a table of
//! projections on the edges (`_id`, dotted paths through arrays, `$slice`, `$elemMatch`, `$`,
//! `$meta`, computed fields), then a seeded fuzz of projections on documents as they come, with
//! their tables of offsets (003 FR-021) and with tables numbered by a dictionary (003 FR-022).

use teseladb_bson::{Document, RawDoc, Value};

use super::tests::{FIELDS, Rng, all_names, doc, widen};
use crate::ext::NoExtensions;
use crate::matcher::MatchContext;
use crate::projection::{Projection, ProjectionContext};

/// The tree evaluator's and the machine's answers for `doc` agree: the same document, or errors
/// with the same code.
fn agree(p: &Projection, doc: RawDoc<'_>, position: Option<usize>, spec: &Document) {
    let ext = NoExtensions;
    let sort = [Value::Int32(1)];
    let key = Document::new().with("k", 1);
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position,
        sort_key: Some(&sort),
        index_key: Some(&key),
    };
    let tree = p.apply_by_tree(doc, &ctx);
    let vm = p.apply(doc, &ctx);
    match (tree, vm) {
        (Ok(t), Ok(v)) => assert_eq!(
            v.to_bytes().expect("small"),
            t.to_bytes().expect("small"),
            "{spec:?} on {:?}: {v:?} against {t:?}",
            Document::from(doc)
        ),
        (Err(t), Err(v)) => assert_eq!(v.code, t.code, "{spec:?}: {v:?} against {t:?}"),
        (t, v) => panic!("{spec:?} on {:?}: {v:?} against {t:?}", Document::from(doc)),
    }
}

/// `doc` as it comes, with its table of offsets and with a table numbered by a dictionary of a
/// part of its names.
fn stored_forms(bson: &[u8]) -> Vec<Vec<u8>> {
    use teseladb_keys::fields::FieldNumbers;
    let mut names = Vec::new();
    all_names(RawDoc::from_bytes(bson).expect("valid"), &mut names);
    let numbers = FieldNumbers::from_names(3, names.iter().step_by(2).map(Vec::as_slice));
    vec![
        bson.to_vec(),
        teseladb_keys::stored::encode(bson.to_vec()),
        teseladb_keys::stored::encode_numbered(bson.to_vec(), &numbers, None),
    ]
}

/// Checks projection `spec` on every document of `docs` (in its stored forms) when it parses.
fn check(spec: &Document, docs: &[Vec<u8>]) -> bool {
    let bytes = spec.to_bytes().expect("small");
    let Ok(p) = Projection::parse(RawDoc::from_bytes(&bytes).expect("valid")) else {
        return false;
    };
    for d in docs {
        for kept in stored_forms(d) {
            let raw = teseladb_keys::stored::raw(&kept).expect("stored");
            for position in [None, Some(0), Some(1), Some(5)] {
                agree(&p, raw, position, spec);
            }
        }
    }
    true
}

fn op(name: &str, v: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, v.into()))
}

fn edge_docs() -> Vec<Vec<u8>> {
    let items = |n: i32| Value::Array((0..n).map(Value::Int32).collect());
    let subs = Value::Array(vec![
        Value::Document(Document::new().with("b", 1).with("c", 2)),
        Value::Int32(3),
        Value::Array(vec![Value::Document(Document::new().with("b", 4))]),
        Value::Document(Document::new().with("c", 5)),
    ]);
    [
        Document::new().with("_id", 1).with("a", items(5)).with("b", "x"),
        Document::new().with("_id", 2).with("a", subs).with("c", Document::new().with("b", 1)),
        Document::new().with("a", Document::new().with("b", items(3)).with("c", 1)).with("_id", 3),
        Document::new().with("_id", Value::Null).with("a", 7),
        Document::new(),
    ]
    .iter()
    .map(|d| d.to_bytes().expect("small"))
    .collect()
}

#[test]
fn the_edges_of_projections_match_the_tree() {
    let docs = edge_docs();
    let specs = vec![
        Document::new().with("a", 1),
        Document::new().with("a", 0),
        Document::new().with("_id", 0).with("a.b", 1),
        Document::new().with("a.b", 0),
        Document::new().with("a.c", 1).with("b", 1),
        Document::new().with("_id", 1),
        Document::new().with("_id", 0),
        Document::new().with("a", op("$slice", 2)),
        Document::new().with("a", op("$slice", -2)).with("b", 1),
        Document::new().with("a", op("$slice", Value::Array(vec![1.into(), 2.into()]))),
        Document::new().with("a", op("$slice", Value::Array(vec![(-2).into(), 1.into()]))),
        Document::new().with("a.b", op("$slice", 1)),
        Document::new().with("a", op("$elemMatch", Document::new().with("$gt", 1))),
        Document::new().with("a", op("$elemMatch", Document::new().with("b", 4))).with("b", 0),
        Document::new().with("a", op("$elemMatch", Document::new().with("$gte", 3))).with("b", 1),
        Document::new().with("a.$", 1),
        Document::new().with("a.b.$", 1),
        Document::new().with("a.$", 1).with("b", 1),
        Document::new().with("s", op("$meta", "sortKey")).with("a", 1),
        Document::new().with("k", op("$meta", "indexKey")).with("a", 0),
        Document::new().with("t", op("$meta", "textScore")),
        Document::new().with("c", op("$literal", 1)).with("a", 1),
        Document::new().with("a.c", op("$add", Value::Array(vec![1.into(), 2.into()]))),
    ];
    for spec in &specs {
        assert!(check(spec, &docs), "{spec:?} parses");
    }
}

/// A random projection: an inclusion or an exclusion of dotted paths, with a `$slice`, an
/// `$elemMatch`, a `$` or a `$meta` now and then.
fn projection(rng: &mut Rng) -> Document {
    let inclusion = rng.below(2) == 0;
    let flag = Value::Int32(i32::from(inclusion));
    let mut out = Document::new();
    if rng.below(3) == 0 {
        out.insert("_id", Value::Int32(rng.below(2) as i32));
    }
    for _ in 0..1 + rng.below(3) {
        let n = 1 + rng.below(3);
        let path = (0..n).map(|_| *rng.pick(&FIELDS)).collect::<Vec<_>>().join(".");
        let v = match rng.below(10) {
            0 => op("$slice", Value::Int32(rng.below(5) as i32 - 2)),
            1 => op("$slice", Value::Array(vec![(rng.below(5) as i32 - 2).into(), 2.into()])),
            2 if !path.contains('.') => {
                op("$elemMatch", Document::new().with("$gte", Value::Int32(rng.below(3) as i32)))
            }
            3 if inclusion => {
                out.insert(format!("{path}.$"), Value::Int32(1));
                continue;
            }
            4 => op("$meta", *rng.pick(&["sortKey", "indexKey", "textScore"])),
            _ => flag.clone(),
        };
        out.insert(path, v);
    }
    out
}

#[test]
fn seeded_fuzz_of_projections_matches_the_tree() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut docs: Vec<Vec<u8>> =
        (0..40).map(|_| doc(&mut rng, 0).to_bytes().expect("small")).collect();
    // Documents of more fields, which get a table of offsets.
    docs.extend((0..20).map(|_| widen(&doc(&mut rng, 0)).to_bytes().expect("small")));
    let mut parsed = 0;
    for _ in 0..2000 {
        if check(&projection(&mut rng), &docs) {
            parsed += 1;
        }
    }
    assert!(parsed > 1000, "only {parsed} projections parsed");
}

#[test]
fn a_projection_compiles_to_64_bit_steps_by_level() {
    let spec = Document::new().with("a.b", 1).with("c", op("$slice", 2)).to_bytes().expect("small");
    let p = Projection::parse(RawDoc::from_bytes(&spec).expect("valid")).expect("parses");
    // The document's level (`_id`, `a`, `c`), then the level of `a` (`b`).
    assert_eq!(
        p.listing(),
        vec![
            "Level 0 0 #3",
            "Keep 0 0 #0",
            "Descend 0 1 #1",
            "Slice 0 0 #3",
            "Level 0 0 #1",
            "Keep 0 0 #2"
        ]
    );
    assert_eq!(std::mem::size_of::<super::project::Step>(), 8);
    // A computed field ends the program with the step that hands it to the extensions.
    let spec = Document::new().with("s", op("$meta", "sortKey")).to_bytes().expect("small");
    let p = Projection::parse(RawDoc::from_bytes(&spec).expect("valid")).expect("parses");
    assert_eq!(p.listing().last().map(String::as_str), Some("Ext 0 0 #0"));
}

#[test]
fn a_projection_of_a_known_specification_comes_from_the_cache() {
    let spec = Document::new().with("cached", 1).to_bytes().expect("small");
    let a = Projection::parse(RawDoc::from_bytes(&spec).expect("valid")).expect("parses");
    let b = Projection::parse(RawDoc::from_bytes(&spec).expect("valid")).expect("parses");
    assert!(a.same_program(&b));
}
