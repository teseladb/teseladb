//! `updateDescription` (014 FR-009): the shape a reference router 9.0.4 gives (measured on 31-10),
//! and the bytes kept with the version read back.

use teseladb_bson::{Document, Value};
use teseladb_changestream::update_desc::{DescError, PathPart, UpdateDescription};

fn keys(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

/// `{$set: {"o.0.x": 5, "s.0.a": 7}}` on `{o: {"0": {x: 1}}, s: [{a: 1}]}`: only the path through the
/// field named "0" is ambiguous.
fn measured() -> UpdateDescription {
    UpdateDescription {
        updated: vec![("o.0.x".into(), Value::Int32(5)), ("s.0.a".into(), Value::Int32(7))],
        removed: vec!["gone".into()],
        truncated: vec![("arr".into(), 2)],
        disambiguated: vec![(
            "o.0.x".into(),
            vec![
                PathPart::Field("o".into()),
                PathPart::Field("0".into()),
                PathPart::Field("x".into()),
            ],
        )],
        pipeline: false,
    }
}

#[test]
fn the_description_has_the_reference_fields_and_the_paths_only_with_expanded_events() {
    let d = measured().to_document(true);
    assert_eq!(
        keys(&d),
        ["updatedFields", "removedFields", "truncatedArrays", "disambiguatedPaths"]
    );
    let plain = measured().to_document(false);
    assert_eq!(keys(&plain), ["updatedFields", "removedFields", "truncatedArrays"]);
    let Some(Value::Array(t)) = d.get("truncatedArrays") else { panic!("{d:?}") };
    let Value::Document(t) = &t[0] else { panic!("{t:?}") };
    assert_eq!(keys(t), ["field", "newSize"]);
    assert_eq!(t.get("newSize"), Some(&Value::Int32(2)));
    let Some(Value::Document(p)) = d.get("disambiguatedPaths") else { panic!("{d:?}") };
    assert_eq!(keys(p), ["o.0.x"]);
    // With nothing ambiguous the paths are an empty document, as the reference server writes them.
    let empty = UpdateDescription::default().to_document(true);
    assert_eq!(empty.get("disambiguatedPaths"), Some(&Value::Document(Document::new())));
}

#[test]
fn the_stored_bytes_read_back() {
    let mut d = measured();
    d.disambiguated.push((
        "s.1.2".into(),
        vec![PathPart::Field("s".into()), PathPart::Index(1), PathPart::Field("2".into())],
    ));
    assert_eq!(UpdateDescription::decode(&d.encode().unwrap()), Ok(d));
    let empty = UpdateDescription::default();
    assert_eq!(UpdateDescription::decode(&empty.encode().unwrap()), Ok(empty));
}

#[test]
fn damaged_bytes_are_refused() {
    assert!(matches!(UpdateDescription::decode(b"\x01"), Err(DescError::Bson(_))));
    let mut wrong = Document::new();
    wrong.push("updatedFields", 1);
    assert_eq!(
        UpdateDescription::decode(&wrong.to_bytes().unwrap()),
        Err(DescError::Shape("updatedFields"))
    );
    let bad = |field: &str, value: Value| {
        let mut d = measured().to_document(true);
        d.insert(field, value);
        UpdateDescription::decode(&d.to_bytes().unwrap())
    };
    assert_eq!(bad("removedFields", Value::Int32(1)), Err(DescError::Shape("removedFields")));
    assert_eq!(
        bad("removedFields", vec![Value::Int32(1)].into()),
        Err(DescError::Shape("removedFields"))
    );
    assert_eq!(
        bad("truncatedArrays", vec![Value::Int32(1)].into()),
        Err(DescError::Shape("truncatedArrays"))
    );
    let mut neg = Document::new();
    neg.push("field", "a");
    neg.push("newSize", -1);
    assert_eq!(
        bad("truncatedArrays", vec![Value::Document(neg)].into()),
        Err(DescError::Shape("newSize"))
    );
    let mut no_size = Document::new();
    no_size.push("field", "a");
    assert_eq!(
        bad("truncatedArrays", vec![Value::Document(no_size)].into()),
        Err(DescError::Shape("truncatedArrays"))
    );
    let mut paths = Document::new();
    paths.push("a.0", 3);
    assert_eq!(
        bad("disambiguatedPaths", Value::Document(paths)),
        Err(DescError::Shape("disambiguatedPaths"))
    );
    let mut paths = Document::new();
    paths.push("a.0", vec![Value::Boolean(true)]);
    assert_eq!(
        bad("disambiguatedPaths", Value::Document(paths)),
        Err(DescError::Shape("disambiguatedPaths"))
    );
    let mut paths = Document::new();
    paths.push("a.0", vec![Value::Int32(-2)]);
    assert_eq!(
        bad("disambiguatedPaths", Value::Document(paths)),
        Err(DescError::Shape("disambiguatedPaths"))
    );
    assert_eq!(
        bad("disambiguatedPaths", Value::Int32(0)),
        Err(DescError::Shape("disambiguatedPaths"))
    );
}

#[test]
fn the_descriptions_of_a_write_read_back_by_document() {
    assert!(UpdateDescription::encode_all(&[]).unwrap().is_empty());
    assert_eq!(UpdateDescription::decode_all(&[]), Ok(Vec::new()));
    let all = vec![(b"k1".to_vec(), measured()), (b"k2".to_vec(), UpdateDescription::default())];
    let bytes = UpdateDescription::encode_all(&all).unwrap();
    assert_eq!(UpdateDescription::decode_all(&bytes), Ok(all));
    // Cut short anywhere, or with a description that is not one.
    for cut in [1, 3, 5, 7, bytes.len() - 1] {
        assert!(UpdateDescription::decode_all(&bytes[..cut]).is_err(), "{cut}");
    }
    let mut bad = vec![1, 0, 0, 0, b'k', 5, 0, 0, 0];
    bad.extend_from_slice(&Document::new().to_bytes().unwrap());
    assert!(UpdateDescription::decode_all(&bad).is_err());
}
