//! What the reference server adds beside an `$expr` before it plans (010 FR-003, T018), measured on
//! a reference router 9.0.4 on 01-11-2026 (`parsedQuery` and the filter of the scan in `explain`):
//! the clauses an optimised expression implies on its paths, and the scan's filter without the
//! `$_internalExpr…` ones. The optimised expressions are given here as the reference server wrote
//! them.

use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_mql::{
    Filter, FilterUse, MatchContext, NoExtensions, OptimizedExpr, optimize_exprs, scan_filter,
};

fn doc(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

/// The parsed query and the scan filter of `filter`, whose `$expr`s optimise to `form` (`true`,
/// `false` or the expression).
fn rewritten(filter: &str, form: &str) -> (String, String) {
    let form = doc(&format!(r#"{{"e":{form}}}"#)).get("e").cloned().expect("e");
    let mut optimize = |_: &Value| {
        Ok(match &form {
            Value::Boolean(true) => OptimizedExpr::True,
            Value::Boolean(false) => OptimizedExpr::False,
            other => OptimizedExpr::Expr(other.clone()),
        })
    };
    let out = optimize_exprs(&doc(filter), &mut optimize).expect("rewritten");
    let bytes = out.to_bytes().expect("small");
    let parsed = Filter::parse(RawDoc::from_bytes(&bytes).expect("doc"), FilterUse::Query)
        .expect("a filter")
        .parsed_query();
    (canonical(&parsed), canonical(&scan_filter(&parsed)))
}

#[test]
fn an_expr_implies_clauses_on_its_paths() {
    let cases: [(&str, &str, &str, &str); 14] = [
        (
            r#"{"$expr":1}"#,
            r#"{"$eq":["$g",{"$const":"a"}]}"#,
            r#"{"$and":[{"$expr":{"$eq":["$g",{"$const":"a"}]}},{"g":{"$_internalExprEq":"a"}}]}"#,
            r#"{"$and":[{"$expr":{"$eq":["$g",{"$const":"a"}]}}]}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$gt":[{"$const":1},"$n"]}"#,
            r#"{"$and":[{"$expr":{"$gt":[{"$const":1},"$n"]}},{"n":{"$_internalExprLt":1}}]}"#,
            r#"{"$and":[{"$expr":{"$gt":[{"$const":1},"$n"]}}]}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$lte":[{"$const":1},"$n"]}"#,
            r#"{"$and":[{"$expr":{"$lte":[{"$const":1},"$n"]}},{"n":{"$_internalExprGte":1}}]}"#,
            r#"{"$and":[{"$expr":{"$lte":[{"$const":1},"$n"]}}]}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$gte":["$$ROOT.o.k",{"$const":1}]}"#,
            r#"{"$and":[{"$expr":{"$gte":["$$ROOT.o.k",{"$const":1}]}},{"o.k":{"$_internalExprGte":1}}]}"#,
            r#"{"$and":[{"$expr":{"$gte":["$$ROOT.o.k",{"$const":1}]}}]}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$lt":["$n",{"$const":1}]}"#,
            r#"{"$and":[{"$expr":{"$lt":["$n",{"$const":1}]}},{"n":{"$_internalExprLt":1}}]}"#,
            r#"{"$and":[{"$expr":{"$lt":["$n",{"$const":1}]}}]}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$eq":["$g",{"$const":[1]}]}"#,
            r#"{"$expr":{"$eq":["$g",{"$const":[1]}]}}"#,
            r#"{"$expr":{"$eq":["$g",{"$const":[1]}]}}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$eq":["$g","$n"]}"#,
            r#"{"$expr":{"$eq":["$g","$n"]}}"#,
            r#"{"$expr":{"$eq":["$g","$n"]}}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$or":[{"$and":[{"$eq":["$g",{"$const":"a"}]},{"$eq":["$s",{"$const":"x"}]}]},{"$in":["$n",{"$const":[2,1]}]}]}"#,
            r#"{"$and":[{"$or":[{"$and":[{"g":{"$_internalExprEq":"a"}},{"s":{"$_internalExprEq":"x"}}]},{"n":{"$in":[1,2]}}]},{"$expr":{"$or":[{"$and":[{"$eq":["$g",{"$const":"a"}]},{"$eq":["$s",{"$const":"x"}]}]},{"$in":["$n",{"$const":[2,1]}]}]}}]}"#,
            r#"{"$and":[{"$expr":{"$or":[{"$and":[{"$eq":["$g",{"$const":"a"}]},{"$eq":["$s",{"$const":"x"}]}]},{"$in":["$n",{"$const":[2,1]}]}]}}]}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$or":[{"$eq":["$g",{"$const":"a"}]},{"$ne":["$n",{"$const":1}]}]}"#,
            r#"{"$expr":{"$or":[{"$eq":["$g",{"$const":"a"}]},{"$ne":["$n",{"$const":1}]}]}}"#,
            r#"{"$expr":{"$or":[{"$eq":["$g",{"$const":"a"}]},{"$ne":["$n",{"$const":1}]}]}}"#,
        ),
        (
            r#"{"$expr":1}"#,
            r#"{"$in":["$g",{"$const":[1,null]}]}"#,
            r#"{"$expr":{"$in":["$g",{"$const":[1,null]}]}}"#,
            r#"{"$expr":{"$in":["$g",{"$const":[1,null]}]}}"#,
        ),
        (
            r#"{"$expr":1,"$and":[{"n":1}]}"#,
            r#"{"$and":[{"$eq":["$g",{"$const":"a"}]},{"$ne":["$n",1]},"$x"]}"#,
            r#"{"$and":[{"n":{"$eq":1}},{"$expr":{"$and":[{"$eq":["$g",{"$const":"a"}]},{"$ne":["$n",1]},"$x"]}},{"g":{"$_internalExprEq":"a"}}]}"#,
            r#"{"$and":[{"n":{"$eq":1}},{"$expr":{"$and":[{"$eq":["$g",{"$const":"a"}]},{"$ne":["$n",1]},"$x"]}}]}"#,
        ),
        (
            r#"{"$nor":[{"$expr":1}]}"#,
            r#"{"$eq":["$g",{"$const":"a"}]}"#,
            r#"{"$nor":[{"$and":[{"$expr":{"$eq":["$g",{"$const":"a"}]}},{"g":{"$_internalExprEq":"a"}}]}]}"#,
            r#"{"$nor":[{"$and":[{"$expr":{"$eq":["$g",{"$const":"a"}]}},{"g":{"$_internalExprEq":"a"}}]}]}"#,
        ),
        (r#"{"$or":[{"$expr":1},{"g":"a"}]}"#, "true", r#"{}"#, r#"{}"#),
        (
            r#"{"$and":[{"$expr":1},{"g":"a"}]}"#,
            "false",
            r#"{"$alwaysFalse":1}"#,
            r#"{"$alwaysFalse":1}"#,
        ),
    ];
    for (filter, form, parsed, scan) in cases {
        let got = rewritten(filter, form);
        assert_eq!(got.0, canonical(&doc(parsed)), "{filter} {form}");
        assert_eq!(got.1, canonical(&doc(scan)), "{filter} {form}");
    }
}

/// What is not a clause stays as given (the filter's parser refuses it).
#[test]
fn entries_that_are_no_clause_stay() {
    let mut optimize = |_: &Value| Ok(OptimizedExpr::True);
    let out = optimize_exprs(&doc(r#"{"$or":[1]}"#), &mut optimize).expect("rewritten");
    assert_eq!(canonical(&out), canonical(&doc(r#"{"$or":[1]}"#)));
}

#[test]
fn a_lone_always_true_filters_nothing() {
    assert_eq!(canonical(&scan_filter(&doc(r#"{"$alwaysTrue":1}"#))), "{}");
}

/// `$_internalExprEq` and the other comparisons an `$expr` implies, where the reference server
/// evaluates them (measured on 9.0.4): under `$nor` and in an index's partial filter they compare
/// the whole value, an array on the path matches, a missing value equals nothing and sorts before
/// every value; at the top of a query they filter nothing.
#[test]
fn internal_comparisons_match_as_measured() {
    let ctx = MatchContext { collation: None, ext: &NoExtensions, vars: None };
    let cases = [
        (r#"{"$nor":[{"g":{"$_internalExprEq":1}}]}"#, FilterUse::Query, r#"{"g":1}"#, false),
        (r#"{"$nor":[{"g":{"$_internalExprEq":1}}]}"#, FilterUse::Query, r#"{"h":1}"#, true),
        (r#"{"$nor":[{"g.k":{"$_internalExprEq":1}}]}"#, FilterUse::Query, r#"{"g":[]}"#, false),
        (r#"{"$nor":[{"g":{"$_internalExprLt":0}}]}"#, FilterUse::Query, r#"{"h":1}"#, false),
        (r#"{"$nor":[{"g":{"$_internalExprGte":0}}]}"#, FilterUse::Query, r#"{"h":1}"#, true),
        (r#"{"g":{"$_internalExprEq":1}}"#, FilterUse::Query, r#"{"g":2}"#, true),
        (r#"{"g":{"$_internalExprEq":1}}"#, FilterUse::Index, r#"{"g":2}"#, false),
        (r#"{"g":{"$_internalExprGt":5,"$lt":9}}"#, FilterUse::Query, r#"{"g":1}"#, true),
        (r#"{"$or":[{"g":{"$_internalExprEq":1}},{"h":1}]}"#, FilterUse::Query, r#"{"g":2}"#, true),
        (
            r#"{"$or":[{"$and":[{"g":1},{"g":{"$_internalExprEq":1}}]},{"h":1}]}"#,
            FilterUse::Query,
            r#"{"g":2}"#,
            false,
        ),
        (
            r#"{"g":{"$elemMatch":{"$_internalExprEq":"z"}}}"#,
            FilterUse::Query,
            r#"{"g":[["a"]]}"#,
            true,
        ),
    ];
    for (f, usage, d, want) in cases {
        let bytes = doc(f).to_bytes().expect("small");
        let filter =
            Filter::parse(RawDoc::from_bytes(&bytes).expect("doc"), usage).expect("a filter");
        let b = doc(d).to_bytes().expect("small");
        let got = filter.matches(RawDoc::from_bytes(&b).expect("doc"), &ctx).expect("matched");
        assert_eq!(got, want, "{f} on {d}");
    }
    let array = doc(r#"{"g":{"$_internalExprEq":[1,2]}}"#).to_bytes().expect("small");
    let e = Filter::parse(RawDoc::from_bytes(&array).expect("doc"), FilterUse::Query)
        .expect_err("an array operand");
    assert_eq!(e.message, "$_internalExprEq cannot be used to compare to type: array");
}

/// What an optimised expression that implies nothing looks like, and the scan filter of nested
/// conjunctions.
#[test]
fn expressions_that_imply_nothing() {
    for form in [
        r#"{"$literal":1}"#,
        r#"{"$eq":["$g",{"$const":1},{"$const":2}]}"#,
        r#"{"$in":["$g"]}"#,
        r#"{"$in":["$g",{"$const":5}]}"#,
        r#"{"$eq":["$$x",{"$const":1}]}"#,
        r#"{"$eq":["$g",{"$const":1,"x":2}]}"#,
        r#"{"$eq":["$g",5]}"#,
    ] {
        let (parsed, _) = rewritten(r#"{"$expr":1}"#, form);
        assert_eq!(parsed, canonical(&doc(&format!(r#"{{"$expr":{form}}}"#))), "{form}");
    }
    let parsed = doc(
        r#"{"$and":[{"$and":[{"g":{"$_internalExprEq":1}}]},{"$or":[{"a":{"$eq":1}}]},{"h":{"$eq":1,"$_internalExprEq":2}},{"$x":1},{}]}"#,
    );
    assert_eq!(
        canonical(&scan_filter(&parsed)),
        canonical(&doc(
            r#"{"$and":[{"$or":[{"a":{"$eq":1}}]},{"h":{"$eq":1,"$_internalExprEq":2}},{"$x":1},{}]}"#
        ))
    );
}
