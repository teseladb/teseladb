//! The options of an index as the planner sees them (009 T015, FR-008, FR-009): a stored
//! specification gives a catalog index that is unique, sparse, partial or hidden as it says; a
//! hidden index is not chosen and a `hint` to it is refused as RefDb refuses one to an index
//! that is not there.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::planner_api::{Hint, IndexCatalog, IndexState};
use teseladb_index::spec::catalog_index;
use teseladb_keys::Hlc;

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn spec(options: &[(&str, Value)]) -> Vec<u8> {
    let mut d =
        Document::new().with("v", 2).with("key", Document::new().with("a", 1)).with("name", "a_1");
    for (k, v) in options {
        d.push(*k, v.clone());
    }
    d.to_bytes().expect("bytes")
}

#[test]
fn a_specification_gives_its_options_to_the_planner() {
    let partial = Value::Document(Document::new().with("a", Document::new().with("$gt", 1)));
    let bytes = spec(&[
        ("unique", Value::Boolean(true)),
        ("hidden", Value::Boolean(true)),
        ("partialFilterExpression", partial),
        ("teselaBuckets", Value::Int32(4)),
    ]);
    let i = catalog_index(RawDoc::from_bytes(&bytes).expect("doc"), 3, 4, IndexState::Building)
        .expect("a valid specification")
        .expect("not a text index");
    assert_eq!((i.id, i.name.as_str()), (3, "a_1"));
    assert_eq!((i.unique, i.sparse, i.partial, i.hidden), (true, false, true, true));
    assert_eq!(i.def.buckets, 4);
    assert_eq!(i.key_pattern, Document::new().with("a", 1).to_bytes().expect("bytes"));
    let plain = spec(&[("sparse", Value::Boolean(true))]);
    let p = catalog_index(RawDoc::from_bytes(&plain).expect("doc"), 4, 0, IndexState::Ready(ts(2)))
        .expect("valid")
        .expect("not text");
    assert_eq!((p.unique, p.sparse, p.partial, p.hidden), (false, true, false, false));
    let text = Document::new()
        .with("key", Document::new().with("_fts", "text").with("_ftsx", 1))
        .with("name", "t")
        .to_bytes()
        .expect("bytes");
    let t = catalog_index(RawDoc::from_bytes(&text).expect("doc"), 5, 0, IndexState::Building);
    assert_eq!(t.map(|t| t.is_none()), Ok(true), "011 keeps text indexes");
    let nameless = Document::new().with("key", Document::new().with("a", 1)).to_bytes().expect("b");
    assert!(
        catalog_index(RawDoc::from_bytes(&nameless).expect("doc"), 6, 0, IndexState::Building)
            .is_err()
    );
    let keyless = Document::new().with("name", "x").to_bytes().expect("bytes");
    assert!(
        catalog_index(RawDoc::from_bytes(&keyless).expect("doc"), 7, 0, IndexState::Building)
            .is_err()
    );
}

#[test]
fn a_hidden_index_is_never_chosen_nor_hinted() {
    let hidden = spec(&[("hidden", Value::Boolean(true))]);
    let i =
        catalog_index(RawDoc::from_bytes(&hidden).expect("doc"), 3, 0, IndexState::Ready(ts(1)))
            .expect("valid")
            .expect("not text");
    let catalog = IndexCatalog { tenant: 0, collection: 1, indexes: vec![i] };
    assert_eq!(catalog.usable(ts(5)).count(), 0);
    let refused = catalog.resolve_hint(Hint::Name("a_1"), ts(5)).map(|i| i.id);
    assert_eq!(refused.map_err(|(code, _)| code), Err(2));
}
