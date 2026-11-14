//! Pipelines against what a `refrouter` 9.0.4 gave for each one on 31-10 and 01-11-2026 (010 T008,
//! T010).
//! Each file of `tests/agg/` holds one request per line — a pipeline, or `{coll, pipeline}` — a
//! tab, and the result: `=> ` and the documents in canonical extended JSON, or `!! `, the error
//! code, a tab and the message. The pipelines read collection `c` (`tests/agg/c.json`) unless
//! they name `o` (`tests/agg/o.json`).
//!
//! The order of `$group`'s results, of a partitioned `$densify` and of the elements of
//! `$addToSet`, `$setUnion` and `$graphLookup` is not defined; a pipeline with them (and without a
//! later `$sort`) compares its results (and those arrays) as sets. A request with `coll: 1` is an
//! `aggregate: 1`, which reads no collection; `options.collation` is the command's collation.

use std::rc::Rc;

use teseladb_bson::{Document, RawDoc, RawValue, Value, extjson};
use teseladb_exec::collation::Collation;
use teseladb_exec::expr::{Ctx, Expr, Scope, truthy};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, GeoQuery, MqlError, NoExtensions, TextQuery};

const C: &str = include_str!("agg/c.json");
const O: &str = include_str!("agg/o.json");

/// The extensions of the tests: `$expr` and projections from the expression engine, the rest
/// as a node without the other engines.
struct TestExtensions;

impl Extensions for TestExtensions {
    fn expr(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<bool, MqlError> {
        let d = Document::from(doc);
        Ok(truthy(&Expr::parse(e)?.eval(&d, &Scope::new(d.clone(), vars))?))
    }

    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool, MqlError> {
        NoExtensions.where_js(code, doc)
    }

    fn text(&self, query: &TextQuery, doc: RawDoc<'_>) -> Result<bool, MqlError> {
        NoExtensions.text(query, doc)
    }

    fn geo(&self, query: &GeoQuery, value: RawValue<'_>) -> Result<bool, MqlError> {
        NoExtensions.geo(query, value)
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn meta(&self, name: &str, doc: RawDoc<'_>) -> Result<Option<Value>, MqlError> {
        NoExtensions.meta(name, doc)
    }

    fn eval(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>, MqlError> {
        let d = Document::from(doc);
        Expr::parse(e)?.eval(&d, &Scope::new(d.clone(), vars))
    }

    fn update_pipeline(
        &self,
        p: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document, MqlError> {
        NoExtensions.update_pipeline(p, doc, vars)
    }
}

/// The two collections, in memory.
struct TestEnv {
    random: std::cell::Cell<u64>,
    /// The request's `collation` option.
    collation: Collation,
}

/// The collation of a request's `options`, the binary order without one.
fn collation_of(options: Option<RawValue<'_>>) -> Collation {
    let Some(RawValue::Document(o)) = options else { return Collation::default() };
    let Some(RawValue::Document(c)) = o.get("collation") else { return Collation::default() };
    match teseladb_collation::parse_collation(c).expect("a collation") {
        teseladb_collation::ParsedCollation::Simple => Collation::default(),
        teseladb_collation::ParsedCollation::Collation(c, _) => Collation::new(Rc::new(c)),
    }
}

fn docs_of(text: &str) -> Vec<Document> {
    let holder = extjson::parse(&format!(r#"{{"d":{text}}}"#)).expect("the collection");
    match holder.get("d") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("not a document: {other:?}"),
            })
            .collect(),
        other => panic!("not an array: {other:?}"),
    }
}

impl Env for TestEnv {
    fn ext(&self) -> &dyn Extensions {
        &TestExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        Ok(match name {
            "c" => docs_of(C),
            "o" => docs_of(O),
            _ => Vec::new(),
        })
    }

    fn collation(&self) -> Collation {
        self.collation.clone()
    }

    fn random(&self) -> f64 {
        let s = self.random.get().wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        self.random.set(s);
        (s >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

/// Results as one document, `{r: [...]}`, with arrays sorted (`sets`) and the top level sorted
/// (`unordered`) when their order is not defined.
fn written(mut docs: Vec<Value>, unordered: bool, sets: bool) -> String {
    if sets {
        docs = docs.into_iter().map(sorted_arrays).collect();
    }
    let mut texts: Vec<String> =
        docs.into_iter().map(|d| canonical(&Document::new().with("v", d))).collect();
    if unordered {
        texts.sort();
    }
    texts.join(",")
}

fn sorted_arrays(v: Value) -> Value {
    match v {
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, x) in d.iter() {
                out.push(k, sorted_arrays(x.clone()));
            }
            Value::Document(out)
        }
        Value::Array(items) => {
            let mut items: Vec<Value> = items.into_iter().map(sorted_arrays).collect();
            items.sort_by_key(|x| canonical(&Document::new().with("v", x.clone())));
            Value::Array(items)
        }
        other => other,
    }
}

/// What TeselaDB gives for request `spec`, and RefDb's result `want`, written alike.
fn compare(spec: &str, want: &str) -> Option<String> {
    let holder =
        extjson::parse(&format!(r#"{{"s":{spec}}}"#)).expect("json").to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&holder).expect("a document");
    let (coll, pipeline, options) = match raw.get("s").expect("s") {
        RawValue::Array(a) => ("c".to_owned(), a, None),
        RawValue::Document(d) => {
            // `coll: 1` is an `aggregate: 1`, which reads no collection.
            let coll = match d.get("coll") {
                Some(RawValue::String(s)) => s.to_owned(),
                Some(_) => String::new(),
                None => "c".to_owned(),
            };
            match d.get("pipeline") {
                Some(RawValue::Array(a)) => (coll, a, d.get("options")),
                _ => panic!("no pipeline in {spec}"),
            }
        }
        _ => panic!("bad request {spec}"),
    };
    let stages: Vec<RawValue<'_>> = pipeline.iter().collect();
    let env = TestEnv { random: std::cell::Cell::new(7), collation: collation_of(options) };
    // An `aggregate` on a collection, as each measurement ran.
    let result =
        Pipeline::parse_command(&stages, &Ctx::checked([]), !coll.is_empty()).and_then(|p| {
            let input = if coll.is_empty() { Vec::new() } else { env.collection(&coll)? };
            if !coll.is_empty() {
                p.check_collection_plan()?;
            }
            p.run(input, &env)
        });
    let sorts_after = spec.rfind("\"$sort\"").unwrap_or(0)
        > spec.rfind("\"$group\"").unwrap_or(0).max(spec.rfind("\"$sortByCount\"").unwrap_or(0));
    let unordered =
        (spec.contains("\"$group\"") || spec.contains("\"$sortByCount\"")) && !sorts_after;
    // The order of `$graphLookup`'s arrays and of a partitioned `$densify` is not defined.
    let sets =
        spec.contains("$addToSet") || spec.contains("$setUnion") || spec.contains("$graphLookup");
    let unordered = unordered || (spec.contains("$densify") && spec.contains("partitionByFields"));
    let got = match result {
        Ok(docs) => format!(
            "=> {}",
            written(
                docs.into_iter().map(Value::Document).collect(),
                unordered || spec.contains("$sortByCount"),
                sets
            )
        ),
        // The measurements keep the message after its last "caused by", as the shell shows it.
        Err(e) => {
            let message = e.message.rsplit(" :: caused by :: ").next().unwrap_or_default();
            format!("!! {}\t{message}", e.code.code)
        }
    };
    let want = match want.strip_prefix("=> ") {
        Some(json) => {
            let holder = extjson::parse(&format!(r#"{{"r":{json}}}"#)).expect("json");
            let items = match holder.get("r") {
                Some(Value::Array(items)) => items.clone(),
                _ => Vec::new(),
            };
            format!("=> {}", written(items, unordered || spec.contains("$sortByCount"), sets))
        }
        None => want.to_owned(),
    };
    (got != want).then(|| format!("{spec}\n   got  {got}\n   want {want}"))
}

fn check(table: &str) {
    let wrong: Vec<String> = table
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let (spec, want) = line.split_once('\t').expect("a request and its result");
            compare(spec, want)
        })
        .collect();
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}

#[test]
fn core_stages_match_refdb() {
    check(teseladb_names::expand_static(include_str!("agg/core.out")));
}

#[test]
fn grouping_sorting_and_granularity_match_refdb() {
    check(include_str!("agg/group.out"));
}

#[test]
fn window_functions_match_refdb() {
    check(include_str!("agg/window.out"));
}

#[test]
fn densify_fill_and_graph_lookup_match_refdb() {
    check(include_str!("agg/fill.out"));
}

/// Collation in comparisons, groups and sorts (010 FR-014).
#[test]
fn collation_matches_refdb() {
    check(include_str!("agg/collation.out"));
}

/// How `$group` and its accumulators keep equal values, as SBE (the last one) or the classic
/// engine (the first one) does: the operators and stages that send a pipeline to the classic
/// engine (010 FR-004).
#[test]
fn ties_follow_the_engine_refdb_chooses() {
    check(include_str!("agg/sbe.out"));
}

/// The metadata of each document (010 T046, with 011): `$score` sets it, the stages that change,
/// filter, reorder, unwind or join documents keep it, those that group drop it, `$meta` reads
/// it and `$sort` sorts by it.
#[test]
fn metadata_follows_the_documents_as_refdb() {
    check(include_str!("agg/meta.out"));
}

/// `$sort` by `{$meta: "randVal"}` without a `$sample` before it, as a `refrouter` 9.0.4 runs it
/// (`agg/rand_val.out`): the documents keep their order, and a stage that reshapes them after
/// the sort, with no `$limit` between, fails with 16410 on the sort key's computed field.
#[test]
fn sort_by_random_value_matches_refdb() {
    check(include_str!("agg/rand_val.out"));
}

/// Each `$meta` name in `$project`, `$addFields`, `$sort`, `$expr` and an accumulator of a
/// pipeline that sets no metadata: the names a stage must set fail with 40218 when the pipeline
/// is parsed, the others read their default, and `$sort` takes only the sortable ones (31138).
#[test]
fn meta_names_without_their_stage_match_refdb() {
    check(include_str!("agg/meta_names.out"));
}

/// `$rankFusion` and `$scoreFusion` (010 T042): their scores, their order and their errors.
#[test]
fn fusion_stages_match_refdb() {
    check(include_str!("agg/fusion.out"));
}

/// The arguments of `$function` and `$accumulator` (010 T012), checked before any JavaScript
/// runs; this node has no engine.
#[test]
fn javascript_arguments_match_refdb() {
    check(include_str!("agg/js.out"));
}

/// The errors of each stage's and accumulator's arguments (010 FR-001).
#[test]
fn argument_errors_match_refdb() {
    check(include_str!("agg/errors.out"));
}

/// Parses pipeline `json` (an array) as an `aggregate` on a collection.
fn pipeline(json: &str) -> Result<Pipeline, MqlError> {
    let holder = extjson::parse(&format!(r#"{{"p":{json}}}"#)).expect("json");
    let bytes = holder.to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(a)) = raw.get("p") else { panic!("not an array: {json}") };
    let stages: Vec<RawValue<'_>> = a.iter().collect();
    Pipeline::parse(&stages, &Ctx::checked([]))
}

/// What a pipeline reads and where it runs, as the server asks before running it.
#[test]
fn pipelines_say_what_they_read_and_whether_they_stay_in_sbe() {
    let p = pipeline(r#"[{"$match":{}},{"$group":{"_id":"$g"}}]"#).expect("valid");
    assert!(p.reads_collection() && p.runs_in_sbe() && !p.is_empty());
    let p = pipeline(r#"[{"$collStats":{}}]"#).expect("valid");
    assert!(!p.reads_collection() && !p.runs_in_sbe());
    // The options RefDb 9.0.4 takes (a decimal scale rounded, unknown options ignored).
    pipeline(
        r#"[{"$collStats":{"storageStats":{"scale":{"$numberDecimal":"0.9"},"verbose":true,
            "bogus":1},"latencyStats":{"histograms":true},"count":{},"queryExecStats":{}}}]"#,
    )
    .expect("valid");
    pipeline(r#"[{"$collStats":{"storageStats":{"scale":{"$numberLong":"2"}}}}]"#).expect("valid");
    pipeline(r#"[{"$collStats":{"storageStats":{"scale":2.0}}}]"#).expect("valid");
    let p = pipeline(r#"[{"$documents":[]}]"#).expect("valid");
    assert!(!p.reads_collection() && !p.runs_in_sbe());
    let p = pipeline(r#"[{"$project":{"z":{"$toString":"$n"}}}]"#).expect("valid");
    assert!(p.reads_collection() && !p.runs_in_sbe());
    assert!(pipeline("[]").expect("valid").is_empty());
}

/// An environment without the node's own stages refuses them.
#[test]
fn an_environment_without_system_stages_refuses_them() {
    let env = TestEnv { random: std::cell::Cell::new(7), collation: Collation::default() };
    let p = pipeline(r#"[{"$collStats":{}}]"#).expect("valid");
    let e = p.run(Vec::new(), &env).expect_err("no system stages");
    assert_eq!(e.code.code, 2);
    assert_eq!(format!("{:?}", Collation::default()), "Collation(simple)");
    let c = collation_of(Some(RawValue::Document(
        RawDoc::from_bytes(
            &extjson::parse(r#"{"collation":{"locale":"es"}}"#)
                .expect("json")
                .to_bytes()
                .expect("small"),
        )
        .expect("a document"),
    )));
    assert_eq!(format!("{c:?}"), "Collation(collator)");
}

/// The search stages on a deployment without search (measured on a `refrouter` 9.0.4).
#[test]
fn search_stages_without_search_match_refdb() {
    check(teseladb_names::expand_static(include_str!("agg/search.out")));
}

/// A search stage that is not first, as a deployment with search refuses it (measured on
/// `refsearch` 1.70.6): `$search` runs as `$_internalSearchRefSearchRemote`.
#[test]
fn search_stages_go_first() {
    for (stage, name) in [
        ("$search", teseladb_names::STAGE_INTERNAL_SEARCH_REMOTE),
        ("$searchMeta", "$searchMeta"),
        ("$vectorSearch", "$vectorSearch"),
    ] {
        let spec = format!(r#"[{{"$match":{{}}}},{{"{stage}":{{}}}}]"#);
        let want = format!("!! 40602\t{name} is only valid as the first stage in a pipeline");
        assert_eq!(compare(&spec, &want), None);
    }
}

/// The random values `$sample` gives the documents it takes, as a `refrouter` 9.0.4 does (measured
/// 01-11-2026: `[{$sample: {size: 2}}, {$project: {r: {$meta: "randVal"}}}]` gave 0.749 and then
/// 0.659): a double in [0, 1) for each, descending through the sample, which a later `$sort` and
/// `$group` read.
#[test]
fn sample_gives_each_document_its_random_value() {
    let env = TestEnv { random: std::cell::Cell::new(7), collation: Collation::default() };
    let run = |json: &str| -> Vec<Document> {
        let holder = extjson::parse(&format!(r#"{{"p":{json}}}"#)).expect("pipeline");
        let bytes = holder.to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&bytes).expect("a document");
        let Some(RawValue::Array(a)) = raw.get("p") else { panic!("{json}") };
        let stages: Vec<RawValue<'_>> = a.iter().collect();
        let p = Pipeline::parse_command(&stages, &Ctx::checked([]), true).expect(json);
        p.run(env.collection("c").expect("c"), &env).expect(json)
    };
    let values = |docs: &[Document]| -> Vec<f64> {
        docs.iter()
            .map(|d| match d.get("r") {
                Some(Value::Double(r)) => *r,
                other => panic!("{other:?} in {d:?}"),
            })
            .collect()
    };
    let sampled = run(r#"[{"$sample":{"size":5}},{"$project":{"r":{"$meta":"randVal"}}}]"#);
    let r = values(&sampled);
    assert_eq!(r.len(), 5);
    assert!(r.iter().all(|x| (0.0..1.0).contains(x)), "{r:?}");
    assert!(r.windows(2).all(|w| w[0] >= w[1]), "{r:?}");
    let all = run(r#"[{"$sample":{"size":20}},{"$project":{"r":{"$meta":"randVal"}}}]"#);
    assert_eq!(all.len(), 8, "a sample larger than the collection takes all of it");
    let resorted = run(
        r#"[{"$sample":{"size":8}},{"$sort":{"k":{"$meta":"randVal"}}},{"$project":{"r":{"$meta":"randVal"}}}]"#,
    );
    let r = values(&resorted);
    assert!(r.windows(2).all(|w| w[0] >= w[1]), "{r:?}");
    let grouped = run(
        r#"[{"$sample":{"size":3}},{"$group":{"_id":null,"r":{"$push":{"$meta":"randVal"}}}}]"#,
    );
    let Some(Value::Array(pushed)) = grouped[0].get("r") else { panic!("{grouped:?}") };
    assert_eq!(pushed.len(), 3);
    assert!(pushed.iter().all(|v| matches!(v, Value::Double(_))), "{pushed:?}");
}
