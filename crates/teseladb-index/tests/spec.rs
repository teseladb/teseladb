//! The key definition of an index from its specification as the catalog keeps it (009 T007,
//! FR-001, FR-013, FR-014): what each value of `key` stores, the projection of a wildcard index,
//! `sparse`, the grid of a `2d` index and the buckets; a `text` index has no keys here (011 builds
//! it).

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::geo::planar::{DEFAULT_GRID, Grid};
use teseladb_index::keygen::wildcard::WildcardProjection;
use teseladb_index::keygen::{FieldKind, IndexDef, IndexField};
use teseladb_index::spec::{index_def, is_text};
use teseladb_keys::Direction;

fn def(spec: Document, buckets: u8) -> Option<IndexDef> {
    let bytes = spec.to_bytes().unwrap();
    index_def(RawDoc::from_bytes(&bytes).unwrap(), buckets).unwrap()
}

fn spec(key: Document) -> Document {
    Document::new().with("key", key).with("name", "i")
}

#[test]
fn numbers_give_the_order_and_strings_the_kind() {
    let d = def(spec(Document::new().with("a", 1).with("b", -1.0).with("c", Value::Int64(-3))), 0);
    let want = vec![
        IndexField::ordered("a", Direction::Ascending),
        IndexField::ordered("b", Direction::Descending),
        IndexField::ordered("c", Direction::Descending),
    ];
    assert_eq!(d, Some(IndexDef { fields: want, buckets: 0, sparse: false, unique: false }));
    let d = def(spec(Document::new().with("h", "hashed").with("g", "2dsphere")), 4).unwrap();
    assert_eq!(d.fields[0], IndexField::special("h", FieldKind::Hashed));
    assert_eq!(d.fields[1], IndexField::special("g", FieldKind::Sphere));
    assert_eq!(d.buckets, 4);
}

#[test]
fn a_2d_index_takes_its_grid() {
    let d = def(spec(Document::new().with("p", "2d")), 0).unwrap();
    assert_eq!(d.fields[0].kind, FieldKind::Plane(DEFAULT_GRID));
    let with =
        spec(Document::new().with("p", "2d")).with("bits", 20).with("min", -500).with("max", 500.5);
    let d = def(with, 0).unwrap();
    assert_eq!(d.fields[0].kind, FieldKind::Plane(Grid { bits: 20, min: -500.0, max: 500.5 }));
}

#[test]
fn wildcard_keys_take_their_projection_and_sparse_is_kept() {
    let d = def(spec(Document::new().with("$**", 1)), 0).unwrap();
    assert_eq!(
        d.fields,
        vec![IndexField::wildcard("$**", Direction::Ascending, WildcardProjection::all())]
    );
    let projected = spec(Document::new().with("$**", -1))
        .with("wildcardProjection", Document::new().with("a", 1));
    let d = def(projected, 0).unwrap();
    let a = Document::new().with("a", 1).to_bytes().unwrap();
    let want = WildcardProjection::parse(RawDoc::from_bytes(&a).unwrap(), &|_| false).unwrap();
    assert_eq!(d.fields[0], IndexField::wildcard("$**", Direction::Descending, want));
    let d = def(spec(Document::new().with("a.$**", 1)), 0).unwrap();
    assert_eq!(d.fields[0].kind, FieldKind::Wildcard(WildcardProjection::subtree("a")));
    let sparse = spec(Document::new().with("a", 1)).with("sparse", true);
    assert!(def(sparse, 0).unwrap().sparse);
}

#[test]
fn text_indexes_have_no_keys_here_and_bad_specs_are_errors() {
    let text = spec(Document::new().with("s", "text").with("a", 1));
    assert_eq!(def(text.clone(), 0), None);
    let bytes = text.to_bytes().unwrap();
    assert!(is_text(RawDoc::from_bytes(&bytes).unwrap()));
    let plain = spec(Document::new().with("a", 1)).to_bytes().unwrap();
    assert!(!is_text(RawDoc::from_bytes(&plain).unwrap()));
    for bad in [
        Document::new().with("name", "i"),
        spec(Document::new()),
        spec(Document::new().with("a", "bogus")),
        spec(Document::new().with("a", true)),
        spec(Document::new().with("$**", 1)).with("wildcardProjection", 3),
    ] {
        let bytes = bad.to_bytes().unwrap();
        assert!(index_def(RawDoc::from_bytes(&bytes).unwrap(), 0).is_err(), "{bad:?}");
    }
}

/// `sparse` and `unique` given as numbers: any number but 0 sets them.
#[test]
fn a_number_other_than_zero_sets_an_option() {
    let key = Document::new().with("a", 1);
    let d = def(spec(key.clone()).with("sparse", 1).with("unique", 0.0), 0).unwrap();
    assert!(d.sparse && !d.unique);
    let d = def(spec(key).with("sparse", 0).with("unique", 2_i64), 0).unwrap();
    assert!(!d.sparse && d.unique);
}
