//! The planner's view of indexes (009 T026, FR-006, FR-009, FR-015): usable indexes by time and
//! visibility, `hint`, key intervals with buckets and hashed fields, and the cell intervals of a
//! `2dsphere` query, compared with `explain` on refnode 7.0.45 (image `ref:7.0`) and again on 9.0.4
//! (image `ref:9.0`, same bounds), 30-10-2026.

use teseladb_index::geo::geojson::{Geometry, parse};
use teseladb_index::geo::region::GeometryRegion;
use teseladb_index::keygen::{FieldKind, IndexDef, IndexField, hashed_key, index_entries};
use teseladb_index::planner_api::{
    BAD_VALUE, CatalogIndex, HINT_NOT_FOUND, Hint, IndexCatalog, IndexState, geo_field_bounds,
    geo_intervals,
};
use teseladb_keys::{Direction, FieldBounds, Hlc, Interval, KeyRange, PrimaryKey};
use teseladb_bson::{Document, RawDoc, RawValue, extjson};

/// `explain` of `{loc: {$geoWithin: {$geometry: <triangle (0,0) (3,6) (6,1)>}}}`.
const POLYGON_BOUNDS: &[&str] = &[
    "1152921504606846977,1155173304420532223",
    "1155173304420532225,1155736254373953535",
    "1156017729350664192,1156017729350664192",
    "1156017729350664193,1156158466839019519",
    "1156158466839019521,1156299204327374847",
    "1156299204327374848,1156299204327374848",
    "1156299204327374849,1156862154280796159",
    "1156862154280796161,1157425104234217471",
    "1157425104234217472,1157425104234217472",
    "1157425104234217473,1157988054187638783",
    "1157988054187638785,1158551004141060095",
    "1158551004141060096,1158551004141060096",
    "1158551004141060097,1158691741629415423",
    "1158832479117770752,1158832479117770752",
    "1158973216606126081,1159113954094481407",
    "1159113954094481409,1159676904047902719",
    "1159676904047902721,1160239854001324031",
    "1160239854001324033,1160802803954745343",
    "1160802803954745344,1160802803954745344",
    "1166432303488958464,1166432303488958464",
    "1169810003209486336,1169810003209486336",
    "1169810003209486337,1170372953162907647",
    "1170935903116328960,1170935903116328960",
    "1183320802091597824,1183320802091597824",
    "1183602277068308480,1183602277068308480",
    "1183672645812486144,1183672645812486144",
    "1183690237998530560,1183690237998530560",
    "1183690237998530561,1183699034091552767",
    "1184446701998440448,1184446701998440448",
    "1185009651951861761,1185572601905283071",
    "1185572601905283072,1185572601905283072",
    "1185572601905283073,1186135551858704383",
    "1224979098644774912,1224979098644774912",
];
/// `explain` of `{loc: {$geoIntersects: {$geometry: {type: "Point", coordinates: [1, 2]}}}}`.
const POINT_BOUNDS: &[&str] = &[
    "1153765929536978944,1153765929536978944",
    "1153836298281156608,1153836298281156608",
    "1153884676792778752,1153884676792778752",
    "1153886944535511040,1153886944535511040",
    "1153886961715380224,1153886961715380224",
    "1153886974600282112,1153886974600282112",
    "1153886977821507584,1153886977821507584",
    "1153886977998716928,1153886977998716928",
    "1153886977999503360,1153886977999503360",
    "1153886977999699968,1153886977999699968",
    "1153886977999732737,1153886977999765503",
    "1153886978001862656,1153886978001862656",
    "1153886978006056960,1153886978006056960",
    "1153886978022834176,1153886978022834176",
    "1153886978089943040,1153886978089943040",
    "1153887150693941248,1153887150693941248",
    "1153887975327662080,1153887975327662080",
    "1153889074839289856,1153889074839289856",
    "1154047404513689600,1154047404513689600",
    "1157425104234217472,1157425104234217472",
    "1170935903116328960,1170935903116328960",
    "1224979098644774912,1224979098644774912",
];

fn geometry(text: &str) -> Geometry {
    let bytes = extjson::parse(&format!("{{\"v\": {text}}}")).unwrap().to_bytes().unwrap();
    parse(RawDoc::from_bytes(&bytes).unwrap().get("v").unwrap()).unwrap()
}

fn measured(bounds: &[&str]) -> Vec<(i64, i64)> {
    bounds
        .iter()
        .map(|b| {
            let (lo, hi) = b.split_once(',').unwrap();
            (lo.parse().unwrap(), hi.parse().unwrap())
        })
        .collect()
}

#[test]
fn geo_query_intervals_as_explain_shows_them() {
    let triangle =
        geometry("{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [3, 6], [6, 1], [0, 0]]]}");
    assert_eq!(geo_intervals(&GeometryRegion::new(&triangle)), measured(POLYGON_BOUNDS));
    let point = geometry("{\"type\": \"Point\", \"coordinates\": [1, 2]}");
    assert_eq!(geo_intervals(&GeometryRegion::new(&point)), measured(POINT_BOUNDS));
}

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn pattern(d: Document) -> Vec<u8> {
    d.to_bytes().unwrap()
}

fn index(
    id: u32,
    name: &str,
    fields: Vec<IndexField>,
    buckets: u8,
    state: IndexState,
) -> CatalogIndex {
    CatalogIndex {
        id,
        name: name.to_string(),
        key_pattern: pattern(Document::default().with(name, 1i32)),
        def: IndexDef { fields, buckets, sparse: false, unique: false },
        unique: false,
        sparse: false,
        partial: false,
        hidden: false,
        multikey: false,
        multikey_paths: Vec::new(),
        state,
    }
}

fn catalog() -> IndexCatalog {
    let a = IndexField::ordered("a", Direction::Ascending);
    let mut hidden = index(
        3,
        "c",
        vec![IndexField::ordered("c", Direction::Ascending)],
        0,
        IndexState::Ready(ts(1)),
    );
    hidden.hidden = true;
    IndexCatalog {
        tenant: 0,
        collection: 7,
        indexes: vec![
            index(1, "a", vec![a.clone()], 0, IndexState::Ready(ts(10))),
            index(
                2,
                "b",
                vec![IndexField::ordered("b", Direction::Descending)],
                4,
                IndexState::Ready(ts(1)),
            ),
            hidden,
            index(
                4,
                "d",
                vec![IndexField::special("d", FieldKind::Hashed)],
                0,
                IndexState::Ready(ts(1)),
            ),
            index(5, "e", vec![a], 0, IndexState::Building),
            index(
                6,
                "f",
                vec![IndexField::ordered("f", Direction::Ascending)],
                0,
                IndexState::Dropping,
            ),
        ],
    }
}

#[test]
fn usable_indexes_and_hints() {
    let c = catalog();
    let names = |t| c.usable(ts(t)).map(|i| i.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(5), ["b", "d"], "a is ready at 10; c hidden; e building; f dropping");
    assert_eq!(names(10), ["a", "b", "d"]);
    assert_eq!(c.resolve_hint(Hint::Name("a"), ts(10)).map(|i| i.id), Ok(1));
    let key = pattern(Document::default().with("b", 1i32));
    assert_eq!(c.resolve_hint(Hint::KeyPattern(&key), ts(10)).map(|i| i.id), Ok(2));
    for name in ["c", "e", "nope"] {
        assert_eq!(c.resolve_hint(Hint::Name(name), ts(10)), Err((BAD_VALUE, HINT_NOT_FOUND)));
    }
    assert_eq!(c.resolve_hint(Hint::Name("a"), ts(5)), Err((BAD_VALUE, HINT_NOT_FOUND)));
}

fn point(v: RawValue<'_>) -> Interval<'_> {
    Interval { low: v, low_inclusive: true, high: v, high_inclusive: true }
}

fn contains(ranges: &[KeyRange], key: &[u8]) -> bool {
    ranges
        .iter()
        .any(|r| r.start.as_slice() <= key && r.end.as_ref().is_none_or(|e| key < e.as_slice()))
}

/// The entry of `{<field>: v}` in index `i` of `catalog`.
fn entry(c: &IndexCatalog, i: &CatalogIndex, field: &str, v: i32) -> Vec<u8> {
    let bytes = Document::default().with("_id", 1i32).with(field, v).to_bytes().unwrap();
    let doc = RawDoc::from_bytes(&bytes).unwrap();
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let e = index_entries(c.tenant, c.collection, i.id, &i.def, doc, &pk, None).unwrap();
    e[0].0.as_bytes().to_vec()
}

#[test]
fn bounds_with_buckets_and_hashes() {
    let c = catalog();
    let b = &c.indexes[1];
    // Points on every field of a bucketed index: one interval each, under its bucket.
    let filter =
        FieldBounds { fields: vec![vec![point(RawValue::Int32(5)), point(RawValue::Int32(9))]] };
    let ranges = c.bounds(b, &filter, None);
    assert_eq!(ranges.len(), 2);
    assert!(contains(&ranges, &entry(&c, b, "b", 5)) && contains(&ranges, &entry(&c, b, "b", 9)));
    assert!(!contains(&ranges, &entry(&c, b, "b", 6)));
    // A real interval scans it under every bucket.
    let between = Interval {
        low: RawValue::Int32(1),
        low_inclusive: true,
        high: RawValue::Int32(7),
        high_inclusive: false,
    };
    let ranges = c.bounds(b, &FieldBounds { fields: vec![vec![between]] }, None);
    assert_eq!(ranges.len(), 4);
    assert!(contains(&ranges, &entry(&c, b, "b", 6)) && !contains(&ranges, &entry(&c, b, "b", 7)));
    // Hashed: an equality becomes the hash; anything else scans the whole index.
    let d = &c.indexes[3];
    let ranges = c.bounds(d, &FieldBounds { fields: vec![vec![point(RawValue::Int32(3))]] }, None);
    assert_eq!(ranges.len(), 1);
    assert!(contains(&ranges, &entry(&c, d, "d", 3)) && !contains(&ranges, &entry(&c, d, "d", 4)));
    let ranges = c.bounds(d, &FieldBounds { fields: vec![vec![between]] }, None);
    assert!(contains(&ranges, &entry(&c, d, "d", 4)) && contains(&ranges, &entry(&c, d, "d", 3)));
    assert_ne!(hashed_key(RawValue::Int32(3)), hashed_key(RawValue::Int32(4)));
    // An unbucketed ordered index.
    let a = &c.indexes[0];
    let ranges = c.bounds(a, &FieldBounds { fields: vec![vec![between]] }, None);
    assert!(contains(&ranges, &entry(&c, a, "a", 1)) && !contains(&ranges, &entry(&c, a, "a", 7)));
}

#[test]
fn geo_bounds_find_the_entries_of_located_documents() {
    let c = IndexCatalog {
        tenant: 0,
        collection: 9,
        indexes: vec![index(
            1,
            "loc",
            vec![IndexField::special("loc", FieldKind::Sphere)],
            0,
            IndexState::Ready(ts(1)),
        )],
    };
    let i = &c.indexes[0];
    let triangle =
        geometry("{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [3, 6], [6, 1], [0, 0]]]}");
    let ranges = c.bounds(
        i,
        &FieldBounds { fields: vec![geo_field_bounds(&GeometryRegion::new(&triangle))] },
        None,
    );
    let located = |lng: i32, lat: i32| {
        let text = format!("{{\"_id\": 1, \"loc\": [{lng}, {lat}]}}");
        let bytes = extjson::parse(&text).unwrap().to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let pk = PrimaryKey::id(RawValue::Int32(1));
        index_entries(c.tenant, c.collection, i.id, &i.def, doc, &pk, None).unwrap()[0]
            .0
            .as_bytes()
            .to_vec()
    };
    assert!(contains(&ranges, &located(3, 2)), "inside the triangle");
    assert!(!contains(&ranges, &located(40, 40)), "far away");
}
