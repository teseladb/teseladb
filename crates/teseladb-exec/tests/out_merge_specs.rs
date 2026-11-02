//! The arguments of `$out` and `$merge` and what `$merge` does with each document (010 FR-012),
//! with the codes a `refrouter` 9.0.4 gives (the same cases run through the protocol in the tests of
//! `teseladb-wire`).

use teseladb_bson::{Document, RawValue, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::out_merge::{
    Merge, MergeAction, Out, WhenMatched, WhenNotMatched, Write,
};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

struct NoEnv;

impl Env for NoEnv {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
        Ok(Vec::new())
    }

    fn random(&self) -> f64 {
        0.5
    }
}

/// The value of `text`, held in a document so that it can be read raw.
fn with_raw<T>(text: &str, f: impl FnOnce(RawValue<'_>) -> T) -> T {
    let holder = extjson::parse(&format!(r#"{{"v":{text}}}"#)).expect("valid JSON");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    f(raw.get("v").expect("the value"))
}

fn out(text: &str) -> Result<Out, i32> {
    with_raw(text, |v| Out::parse(v).map_err(|e| e.code.code))
}

fn merge(text: &str) -> Result<Merge, i32> {
    with_raw(text, |v| Merge::parse(v, &Ctx::default()).map_err(|e| e.code.code))
}

fn doc(text: &str) -> Document {
    extjson::parse(text).expect("a document")
}

#[test]
fn out_takes_a_name_or_a_database_and_collection() {
    assert_eq!(out(r#""t""#), Ok(Out { db: None, coll: "t".to_owned() }));
    let full = out(r#"{"db":"d","coll":"t","timeseries":{}}"#).expect("valid");
    assert_eq!((full.db.as_deref(), full.coll.as_str()), (Some("d"), "t"));
    assert!(full.check("x").is_ok());
    for (text, code) in [
        (r#"{"db":1,"coll":"t"}"#, 14),
        (r#"{"db":"d","coll":2}"#, 14),
        (r#"{"db":"d","coll":"t","x":1}"#, 40415),
        (r#"{"coll":"t"}"#, 40414),
        (r#"{"db":"d"}"#, 40414),
        ("7", 16990),
        (r#""system.x""#, 17385),
    ] {
        assert_eq!(out(text).map(|_| ()), Err(code), "{text}");
    }
    let empty = out(r#""""#).expect("parses");
    assert_eq!(empty.check("d").map_err(|e| e.code.code), Err(73));
    let empty = out(r#"{"db":"o","coll":""}"#).expect("parses");
    assert!(empty.check("d").expect_err("empty").message.contains(", o"));
}

#[test]
fn merge_reads_its_target_keys_and_modes() {
    let m = merge(r#""t""#).expect("valid");
    assert_eq!((m.coll.as_str(), m.on_fields()), ("t", vec!["_id".to_owned()]));
    let m = merge(
        r#"{"into":{"db":"d","coll":"t"},"on":["a","b"],"whenMatched":"replace","whenNotMatched":"discard","let":{"x":1}}"#,
    )
    .expect("valid");
    assert_eq!(m.db.as_deref(), Some("d"));
    assert_eq!(m.on_fields(), ["a", "b"]);
    assert_eq!(m.when_not_matched, WhenNotMatched::Discard);
    assert!(matches!(m.when_matched, WhenMatched::Replace));
    for mode in ["keepExisting", "merge", "fail"] {
        let text = format!(r#"{{"into":"t","on":"a","whenMatched":"{mode}"}}"#);
        assert!(merge(&text).is_ok(), "{mode}");
    }
    let p =
        merge(r#"{"into":"t","whenMatched":[{"$set":{"y":"$$new.y"}}],"whenNotMatched":"fail"}"#)
            .expect("valid");
    assert!(matches!(p.when_matched, WhenMatched::Pipeline(..)));
    for (text, code) in [
        ("1", 14),
        (r#""""#, 5786800),
        (r#"{"into":{"db":"d"}}"#, 5786801),
        (r#"{"into":2}"#, 51178),
        (r#"{"into":"system.t"}"#, 31319),
        (r#"{"into":"t","on":[1]}"#, 51134),
        (r#"{"into":"t","on":[]}"#, 51187),
        (r#"{"into":"t","on":3}"#, 51186),
        (r#"{"into":"t","whenMatched":"x"}"#, 2),
        (r#"{"into":"t","whenMatched":4}"#, 51191),
        (r#"{"into":"t","whenNotMatched":"x"}"#, 2),
        (r#"{"into":"t","whenNotMatched":5}"#, 14),
        (r#"{"into":"t","let":6}"#, 14),
        (r#"{"into":"t","x":1}"#, 40415),
        (r#"{"on":"a"}"#, 40414),
        (r#"{"into":"t","whenMatched":"fail","whenNotMatched":"discard"}"#, 51181),
        (r#"{"into":"t","whenMatched":"keepExisting","whenNotMatched":"fail"}"#, 51181),
    ] {
        assert_eq!(merge(text).map(|_| ()), Err(code), "{text}");
    }
}

#[test]
fn merge_acts_on_each_document_by_its_modes() {
    let env = NoEnv;
    let new = doc(r#"{"_id":1,"a":2}"#);
    let old = doc(r#"{"_id":9,"a":1,"b":3}"#);
    let act = |text: &str, existing: Option<&Document>| {
        merge(text).expect("valid").action(existing, new.clone(), &env)
    };
    assert_eq!(act(r#""t""#, None), Ok(MergeAction::Insert(new.clone())));
    let discard = r#"{"into":"t","whenNotMatched":"discard"}"#;
    assert_eq!(act(discard, None), Ok(MergeAction::Skip));
    let fail = r#"{"into":"t","whenMatched":"replace","whenNotMatched":"fail"}"#;
    assert_eq!(act(fail, None).map_err(|e| e.code.code), Err(13113));
    assert_eq!(act(fail, Some(&old)), Ok(MergeAction::Replace(new.clone())));
    // A replacement without `_id` takes the one of the document it replaces.
    let no_id = doc(r#"{"a":5}"#);
    let replaced = merge(fail).expect("valid").action(Some(&old), no_id, &env);
    assert_eq!(replaced, Ok(MergeAction::Replace(doc(r#"{"_id":9,"a":5}"#))));
    let keep = r#"{"into":"t","whenMatched":"keepExisting"}"#;
    assert_eq!(act(keep, Some(&old)), Ok(MergeAction::Skip));
    let dup = r#"{"into":"t","whenMatched":"fail"}"#;
    assert_eq!(act(dup, Some(&old)), Ok(MergeAction::Duplicate));
    assert_eq!(
        act(r#""t""#, Some(&old)),
        Ok(MergeAction::Replace(doc(r#"{"_id":1,"a":2,"b":3}"#)))
    );
    let pipeline =
        r#"{"into":"t","let":{"k":"$a"},"whenMatched":[{"$set":{"c":"$$k","d":"$$new._id"}}]}"#;
    assert_eq!(
        act(pipeline, Some(&old)),
        Ok(MergeAction::Replace(doc(r#"{"_id":9,"a":1,"b":3,"c":2,"d":1}"#)))
    );
    let bad = r#"{"into":"t","whenMatched":[{"$match":{}}]}"#;
    assert_eq!(act(bad, Some(&old)).map_err(|e| e.code.code), Err(72));
    let empty = r#"{"into":"t","whenMatched":[{"$unset":"_id"},{"$project":{"_id":0}}]}"#;
    assert!(act(empty, Some(&old)).is_ok());
}

fn stages(text: &str) -> Result<Pipeline, i32> {
    with_raw(text, |v| {
        let RawValue::Array(a) = v else { panic!("not a pipeline") };
        let list: Vec<RawValue<'_>> = a.iter().collect();
        Pipeline::parse(&list, &Ctx::default()).map_err(|e| e.code.code)
    })
}

#[test]
fn a_writing_stage_is_last_and_never_in_a_sub_pipeline() {
    let p = stages(r#"[{"$match":{}},{"$out":"t"}]"#).expect("valid");
    assert_eq!(p.write().map(Write::name), Some("$out"));
    let p = stages(r#"[{"$merge":"t"}]"#).expect("valid");
    assert_eq!(p.write().map(Write::name), Some("$merge"));
    assert!(stages(r#"[{"$match":{}}]"#).expect("valid").write().is_none());
    assert_eq!(stages(r#"[{"$out":"t"},{"$match":{}}]"#).map(|_| ()), Err(40601));
    let lookup = r#"[{"$lookup":{"from":"o","pipeline":[{"$out":"x"}],"as":"j"}}]"#;
    assert_eq!(stages(lookup).map(|_| ()), Err(51047));
    let union = r#"[{"$unionWith":{"coll":"o","pipeline":[{"$merge":"x"}]}}]"#;
    assert_eq!(stages(union).map(|_| ()), Err(31441));
}
