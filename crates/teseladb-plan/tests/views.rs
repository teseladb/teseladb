//! Views (015 T004, FR-001) against what the reference router 9.0.4 answered on 31-10-2026: the
//! checks of a definition (stages, writing and change-stream stages, also in sub-pipelines,
//! collation of the views it reads, cycles and depth) and the resolution of a query on a view.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value, extjson};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_plan::stages::{self, Reach, STAGES};
use teseladb_plan::views::{
    Definition, FACET_WRITES, GRAPH_CONTAINS_CYCLE, LOOKUP_WRITES, OPTION_NOT_SUPPORTED_ON_VIEW,
    Resolved, UNION_WITH_WRITES, VIEW_DEPTH_LIMIT_EXCEEDED, ViewError, check_definition,
    check_pipeline, resolve,
};

fn stages_of(json: &str) -> Vec<Vec<u8>> {
    let d = extjson::parse(&format!("{{\"p\":{json}}}")).expect("json");
    let Some(Value::Array(a)) = d.get("p") else { panic!("not an array") };
    a.iter()
        .map(|s| match s {
            Value::Document(s) => s.to_bytes().expect("bytes"),
            _ => panic!("a stage that is not a document"),
        })
        .collect()
}

fn fr() -> Vec<u8> {
    Document::new().with("locale", "fr").to_bytes().expect("bytes")
}

fn view(on: &str, pipeline: &str, collation: Option<Vec<u8>>) -> CollectionKind {
    CollectionKind::View { view_on: on.to_owned(), pipeline: stages_of(pipeline), collation }
}

fn err(code: i32, message: &str) -> Result<(), ViewError> {
    Err(ViewError { code, message: message.to_owned() })
}

#[test]
fn the_stage_list_is_sorted_and_knows_what_writes() {
    assert!(STAGES.windows(2).all(|w| w[0].name < w[1].name));
    assert_eq!(stages::stage("$out").map(|s| s.reach), Some(Reach::Writes));
    assert_eq!(stages::stage("$changeStream").map(|s| s.reach), Some(Reach::ChangeStream));
    assert_eq!(stages::stage("$match").map(|s| s.reach), Some(Reach::Reads));
    assert_eq!(stages::stage("$nope"), None);
    assert_eq!(stages::stage("$listCatalog").map(|s| s.reach), Some(Reach::Reads));
    assert_eq!(
        stages::named(&["$mergeCursors"]).map_err(|e| (e.code, e.message)),
        Err((stages::INTERNAL_STAGE, "$mergeCursors' is not allowed in user requests".to_owned()))
    );
}

#[test]
fn view_pipelines_are_checked_as_the_reference_router_checks_them() {
    let ns = "m6.v";
    let cases: Vec<(&str, Result<(), ViewError>)> = vec![
        (r#"[{"$match":{"a":1}},{"$geoNear":{"near":[0,0],"distanceField":"d"}}]"#, Ok(())),
        (r#"[{"$unionWith":"z"},{"$indexStats":{}}]"#, Ok(())),
        (r#"[{"$nope":1}]"#, err(40_324, "Unrecognized pipeline stage name: '$nope'")),
        (
            r#"[{"$match":{},"$project":{"a":1}}]"#,
            err(40_323, "A pipeline stage specification object must contain exactly one field."),
        ),
        (
            r#"[{"$match":{}},{"$out":"x"}]"#,
            err(
                OPTION_NOT_SUPPORTED_ON_VIEW,
                "Invalid pipeline for view m6.v :: caused by :: The aggregation stage $out in location 1 of the pipeline cannot be used in the view definition of m6.v because it writes to disk",
            ),
        ),
        (
            r#"[{"$merge":"y"}]"#,
            err(
                OPTION_NOT_SUPPORTED_ON_VIEW,
                "Invalid pipeline for view m6.v :: caused by :: The aggregation stage $merge in location 0 of the pipeline cannot be used in the view definition of m6.v because it writes to disk",
            ),
        ),
        (
            r#"[{"$changeStream":{}}]"#,
            err(
                OPTION_NOT_SUPPORTED_ON_VIEW,
                "Invalid pipeline for view m6.v :: caused by :: $changeStream cannot be used in a view definition",
            ),
        ),
        (
            r#"[{"$facet":{"a":[{"$out":"x"}]}}]"#,
            err(
                FACET_WRITES,
                "Invalid pipeline for view m6.v :: caused by :: $out is not allowed to be used within a $facet stage",
            ),
        ),
        (
            r#"[{"$facet":{"a":[{"$changeStream":{}}]}}]"#,
            err(
                FACET_WRITES,
                "Invalid pipeline for view m6.v :: caused by :: $_internalChangeStreamOplogMatch is not allowed to be used within a $facet stage",
            ),
        ),
        (
            r#"[{"$unionWith":{"coll":"z","pipeline":[{"$out":"x"}]}}]"#,
            err(
                UNION_WITH_WRITES,
                "Invalid pipeline for view m6.v :: caused by :: $out is not allowed within a $unionWith's sub-pipeline",
            ),
        ),
        (
            r#"[{"$lookup":{"from":"z","as":"q","pipeline":[{"$merge":"w"}]}}]"#,
            err(
                LOOKUP_WRITES,
                "Invalid pipeline for view m6.v :: caused by :: $merge is not allowed within a $lookup's sub-pipeline",
            ),
        ),
        (
            r#"[{"$lookup":{"from":"z","as":"q","pipeline":[{"$changeStream":{}}]}}]"#,
            err(
                LOOKUP_WRITES,
                "Invalid pipeline for view m6.v :: caused by :: $_internalChangeStreamOplogMatch is not allowed within a $lookup's sub-pipeline",
            ),
        ),
        (
            r#"[{"$lookup":{"from":"z","as":"q","pipeline":[{"$nope":1}]}}]"#,
            err(40_324, "Unrecognized pipeline stage name: '$nope'"),
        ),
    ];
    for (pipeline, want) in cases {
        assert_eq!(check_pipeline(ns, &stages_of(pipeline)), want, "{pipeline}");
    }
}

/// A catalog of views by name.
fn catalog(views: &[(&str, CollectionKind)]) -> BTreeMap<String, CollectionKind> {
    views.iter().map(|(n, k)| ((*n).to_owned(), k.clone())).collect()
}

fn check(
    cat: &BTreeMap<String, CollectionKind>,
    name: &str,
    on: &str,
    pipeline: &str,
    collation: Option<&[u8]>,
) -> Result<(), ViewError> {
    let pipeline = stages_of(pipeline);
    let def = Definition { name, view_on: on, pipeline: &pipeline, collation };
    check_definition("m6", &def, |n| cat.get(n).cloned())
}

#[test]
fn a_view_reads_views_of_its_collation_only() {
    let cat = catalog(&[("fr", view("b", "[]", Some(fr()))), ("b", CollectionKind::Normal)]);
    let conflict = |name: &str| {
        err(
            OPTION_NOT_SUPPORTED_ON_VIEW,
            &format!("View m6.{name} has conflicting collation with view m6.fr"),
        )
    };
    assert_eq!(check(&cat, "onfr", "fr", "[]", None), conflict("onfr"));
    assert_eq!(
        check(&cat, "lk", "b", r#"[{"$lookup":{"from":"fr","as":"x","pipeline":[]}}]"#, None),
        conflict("lk")
    );
    assert_eq!(check(&cat, "lk3", "b", r#"[{"$unionWith":"fr"}]"#, None), conflict("lk3"));
    assert_eq!(
        check(&cat, "lk4", "b", r#"[{"$unionWith":{"coll":"fr","pipeline":[]}}]"#, None),
        conflict("lk4")
    );
    let graph = r#"[{"$graphLookup":{"from":"fr","startWith":"$a","connectFromField":"a","connectToField":"b","as":"c"}}]"#;
    assert_eq!(check(&cat, "g", "b", graph, None), conflict("g"));
    let f = fr();
    assert_eq!(
        check(&cat, "lk2", "b", r#"[{"$lookup":{"from":"fr","as":"x","pipeline":[]}}]"#, Some(&f)),
        Ok(())
    );
    assert_eq!(check(&cat, "missing", "nothere", "[]", None), Ok(()));
    // A `$lookup` without a collection and a `$unionWith` of `$documents` read no view.
    let no_from = r#"[{"$lookup":{"as":"x","pipeline":[{"$documents":[]}]}},{"$unionWith":{"pipeline":[{"$documents":[]}]}}]"#;
    assert_eq!(check(&cat, "docs", "b", no_from, None), Ok(()));
}

#[test]
fn a_view_that_reaches_itself_is_a_cycle() {
    let mut views = vec![("d0", view("b", "[]", None))];
    let names: Vec<String> = (0..6).map(|i| format!("d{i}")).collect();
    for i in 1..6 {
        views.push((names[i].as_str(), view(&names[i - 1], "[]", None)));
    }
    let cat = catalog(&views);
    assert_eq!(
        check(&cat, "d0", "d5", "[]", None),
        err(
            GRAPH_CONTAINS_CYCLE,
            "View cycle detected: m6.d0 => m6.d0 => m6.d1 => m6.d2 => m6.d3 => m6.d4 => m6.d5 => m6.d0"
        )
    );
    assert_eq!(
        check(&cat, "self", "self", "[]", None),
        err(GRAPH_CONTAINS_CYCLE, "View cycle detected: m6.self => m6.self => m6.self")
    );
    assert_eq!(
        check(&cat, "cy", "b", r#"[{"$lookup":{"from":"cy","as":"x","pipeline":[]}}]"#, None),
        err(GRAPH_CONTAINS_CYCLE, "View cycle detected: m6.cy => m6.cy => m6.cy")
    );
}

/// Views `d0` (on `b`) to `d{n-1}`, each on the one before.
fn chain(n: usize) -> BTreeMap<String, CollectionKind> {
    (0..n)
        .map(|i| {
            let on = if i == 0 { "b".to_owned() } else { format!("d{}", i - 1) };
            (format!("d{i}"), view(&on, &format!(r#"[{{"$match":{{"n":{i}}}}}]"#), None))
        })
        .collect()
}

#[test]
fn a_definition_stands_on_at_most_twenty_views() {
    let cat = chain(20);
    assert_eq!(check(&cat, "d19", "d18", "[]", None), Ok(()));
    let too_deep = err(VIEW_DEPTH_LIMIT_EXCEEDED, "View depth limit exceeded; maximum depth is 20");
    assert_eq!(check(&cat, "d20", "d19", "[]", None), too_deep);
    assert_eq!(
        check(&cat, "lk", "b", r#"[{"$lookup":{"from":"d19","as":"x","pipeline":[]}}]"#, None),
        too_deep
    );
    assert_eq!(check(&cat, "d21", "d20", "[]", None), Ok(()));
}

#[test]
fn a_query_resolves_the_chain_innermost_first() {
    let cat = chain(3);
    let got = resolve("d2", |n| Ok::<_, ViewError>(cat.get(n).cloned())).expect("resolves");
    let want = Resolved {
        collection: "b".to_owned(),
        pipeline: stages_of(r#"[{"$match":{"n":0}},{"$match":{"n":1}},{"$match":{"n":2}}]"#),
        collation: None,
    };
    assert_eq!(got, Some(want));
    assert_eq!(resolve("b", |n| Ok::<_, ViewError>(cat.get(n).cloned())), Ok(None));
    let with_collation = catalog(&[("fr", view("b", "[]", Some(fr())))]);
    let fr_view =
        resolve("fr", |n| Ok::<_, ViewError>(with_collation.get(n).cloned())).expect("resolves");
    assert_eq!(fr_view.and_then(|r| r.collation), Some(fr()));
}

#[test]
fn a_query_refuses_a_chain_of_twenty_views() {
    let cat = chain(20);
    assert_eq!(
        resolve("d19", |n| Ok::<_, ViewError>(cat.get(n).cloned())),
        Err(ViewError {
            code: VIEW_DEPTH_LIMIT_EXCEEDED,
            message: "View depth too deep or view cycle detected; maximum depth is 20".to_owned(),
        })
    );
    assert!(resolve("d18", |n| Ok::<_, ViewError>(cat.get(n).cloned())).is_ok());
}

#[test]
fn a_stage_that_is_not_bson_is_refused() {
    let bad = vec![vec![1, 2, 3]];
    assert_eq!(check_pipeline("m6.v", &bad).map_err(|e| e.code), Err(OPTION_NOT_SUPPORTED_ON_VIEW));
    let cat = catalog(&[]);
    let def = Definition { name: "v", view_on: "b", pipeline: &bad, collation: None };
    assert!(check_definition("m6", &def, |n| cat.get(n).cloned()).is_err());
}
