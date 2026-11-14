//! The kinds of collection the catalog keeps (015 T001, FR-001, FR-003, FR-006, FR-009): a
//! collection's stored options say whether it is a view, capped, a time series or clustered, with
//! the options of each, as `create` stored them on a reference router 9.0.4 (measured 30-10-2026).

use teseladb_bson::{Document, Value};
use teseladb_meta::collection_types::{CollectionKind, Granularity, TimeSeries};

fn kind(options: Document) -> CollectionKind {
    CollectionKind::of(&options.to_bytes().expect("bytes")).expect("valid options")
}

#[test]
fn stored_options_give_the_kind() {
    assert_eq!(kind(Document::new()), CollectionKind::Normal);
    let view = kind(Document::new().with("viewOn", "base").with(
        "pipeline",
        Value::Array(vec![Value::Document(Document::new().with("$match", Document::new()))]),
    ));
    let CollectionKind::View { view_on, pipeline, collation } = &view else { panic!("{view:?}") };
    assert_eq!((view_on.as_str(), pipeline.len(), collation), ("base", 1, &None));
    assert_eq!(view.type_name(), "view");
    let capped = kind(Document::new().with("capped", true).with("size", 1000).with("max", 10));
    assert_eq!(capped, CollectionKind::Capped { size: 1000, max: Some(10) });
    assert_eq!(capped.type_name(), "collection");
    let unlimited = kind(Document::new().with("capped", true).with("size", 1000i64));
    assert_eq!(unlimited, CollectionKind::Capped { size: 1000, max: None });
    let ts = kind(
        Document::new().with("expireAfterSeconds", 60i64).with(
            "timeseries",
            Document::new()
                .with("timeField", "t")
                .with("metaField", "m")
                .with("granularity", "hours")
                .with("bucketMaxSpanSeconds", 2_592_000)
                .with("fixedBucketing", true),
        ),
    );
    assert_eq!(
        ts,
        CollectionKind::TimeSeries(TimeSeries {
            time_field: "t".into(),
            meta_field: Some("m".into()),
            granularity: Some(Granularity::Hours),
            bucket_max_span_seconds: 2_592_000,
            bucket_rounding_seconds: None,
            expire_after_seconds: Some(60),
        })
    );
    assert_eq!(ts.type_name(), "timeseries");
    let custom = kind(
        Document::new().with(
            "timeseries",
            Document::new()
                .with("timeField", "t")
                .with("bucketRoundingSeconds", 100)
                .with("bucketMaxSpanSeconds", 100),
        ),
    );
    let CollectionKind::TimeSeries(t) = custom else { panic!() };
    assert_eq!((t.granularity, t.bucket_rounding_seconds), (None, Some(100)));
    let clustered = kind(
        Document::new()
            .with(
                "clusteredIndex",
                Document::new()
                    .with("v", 2)
                    .with("key", Document::new().with("_id", 1))
                    .with("name", "x")
                    .with("unique", true),
            )
            .with("expireAfterSeconds", 5i64),
    );
    assert_eq!(
        clustered,
        CollectionKind::Clustered { name: "x".into(), expire_after_seconds: Some(5) }
    );
    assert_eq!(clustered.type_name(), "collection");
}

#[test]
fn granularities_have_their_names_and_spans() {
    for (g, name, span) in [
        (Granularity::Seconds, "seconds", 3_600),
        (Granularity::Minutes, "minutes", 86_400),
        (Granularity::Hours, "hours", 2_592_000),
    ] {
        assert_eq!((g.name(), g.max_span_seconds()), (name, span));
        assert_eq!(Granularity::of(name), Some(g));
    }
    assert_eq!(Granularity::of("days"), None);
}

#[test]
fn options_the_catalog_never_stores_are_refused() {
    let bad = |d: Document| CollectionKind::of(&d.to_bytes().expect("bytes")).is_err();
    assert!(bad(Document::new().with("viewOn", 1)));
    assert!(bad(Document::new().with("viewOn", "b").with("pipeline", 1)));
    assert!(bad(Document::new().with("capped", true)));
    assert!(bad(Document::new().with("capped", true).with("size", "x")));
    assert!(bad(Document::new().with("timeseries", Document::new())));
    assert!(bad(Document::new()
        .with("timeseries", Document::new().with("timeField", "t").with("granularity", "x"))));
    assert!(bad(Document::new().with("timeseries", 1)));
    assert!(bad(Document::new().with("clusteredIndex", true)));
    assert!(CollectionKind::of(&[1, 2]).is_err(), "not BSON");
}
