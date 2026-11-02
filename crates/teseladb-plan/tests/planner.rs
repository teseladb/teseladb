//! The planner (010 FR-001, T014, T016) against the plans a `refrouter` 9.0.4 chose on 01-11-2026
//! (`explain` of `find` with the indexes `{g: 1}`, `{n: 1}`, `{g: 1, n: 1}`, `{s: 1}`, `{t: 1}`,
//! `{o.k: 1}`, `{x: -1}`, and the ties of `{a: 1}`, `{b: 1}`, `{a: 1, b: 1}` in each order of
//! creation): the intervals each index scans as `explain` writes them, the clauses left for the
//! documents, the plans made and their order, and the choice by trial.

use teseladb_bson::{Document, Value, extjson};
use teseladb_plan::cost::{Trial, ranked, score, trial_works};
use teseladb_plan::logical::{
    Bound, compare_values, conjunction, conjuncts, intersect, intervals, on_path, shown_value,
    sorted, union,
};
use teseladb_plan::rules::{Access, IndexMeta, IndexScan, Request, candidates};

fn doc(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document"))
}

fn index(name: &str, key: &[(&str, i32)]) -> IndexMeta {
    IndexMeta {
        name: name.to_owned(),
        key: key.iter().map(|(f, d)| ((*f).to_owned(), *d)).collect(),
        multikey: false,
        plannable: true,
    }
}

fn catalog() -> Vec<IndexMeta> {
    vec![
        index("g_1", &[("g", 1)]),
        index("n_1", &[("n", 1)]),
        index("g_1_n_1", &[("g", 1), ("n", 1)]),
        index("s_1", &[("s", 1)]),
        IndexMeta { multikey: true, ..index("t_1", &[("t", 1)]) },
        index("o.k_1", &[("o.k", 1)]),
        index("x_-1", &[("x", -1)]),
    ]
}

/// The plans of `parsed` (sorted by `sort`) as `index name: bounds | residual`.
fn plans(parsed: &str, sort: &[(&str, i32)], indexes: &[IndexMeta]) -> Vec<String> {
    let parsed = doc(parsed);
    let sort: Vec<(String, i32)> = sort.iter().map(|(f, d)| ((*f).to_owned(), *d)).collect();
    let request = Request { parsed: Some(&parsed), sort: &sort, hint: None, intersect: false };
    candidates(&request, indexes).iter().map(|a| shown(a, indexes)).collect()
}

fn scan_text(s: &IndexScan, indexes: &[IndexMeta]) -> String {
    let index = &indexes[s.index];
    let bounds: Vec<String> = s
        .bounds
        .iter()
        .zip(&index.key)
        .map(|((f, list), (_, dir))| {
            let against = (*dir < 0) != s.backward;
            let mut items: Vec<String> = list
                .iter()
                .map(|b| if against { b.reversed().shown() } else { b.shown() })
                .collect();
            if against {
                items.reverse();
            }
            format!("{f}{items:?}")
        })
        .collect();
    let residual = s.residual.as_ref().map_or(String::new(), |r| format!(" | {}", canonical(r)));
    let order = match (s.sorts, s.backward) {
        (true, true) => " sorted backward",
        (true, false) => " sorted",
        _ => "",
    };
    format!("{}: {}{residual}{order}", index.name, bounds.join(" "))
}

fn shown(a: &Access, indexes: &[IndexMeta]) -> String {
    match a {
        Access::Collscan => "COLLSCAN".to_owned(),
        Access::Index(s) => scan_text(s, indexes),
        Access::Or(list) => {
            format!(
                "OR [{}]",
                list.iter().map(|s| scan_text(s, indexes)).collect::<Vec<_>>().join("; ")
            )
        }
        Access::AndHash { scans, filter } => {
            let names: Vec<&str> = scans.iter().map(|s| indexes[s.index].name.as_str()).collect();
            let filter = filter.as_ref().map(canonical).unwrap_or_default();
            format!("AND_HASH [{}] {filter}", names.join(", "))
        }
    }
}

/// The intersections of `parsed` with hash intersection on, in the plans' order.
fn intersections(parsed: &str, sort: &[(&str, i32)], indexes: &[IndexMeta]) -> Vec<String> {
    let parsed = doc(parsed);
    let sort: Vec<(String, i32)> = sort.iter().map(|(f, d)| ((*f).to_owned(), *d)).collect();
    let request = Request { parsed: Some(&parsed), sort: &sort, hint: None, intersect: true };
    candidates(&request, indexes)
        .iter()
        .filter(|a| matches!(a, Access::AndHash { .. }))
        .map(|a| shown(a, indexes))
        .collect()
}

/// With `internalQueryPlannerEnableHashIntersection`, a `refrouter` 9.0.4 makes one `AND_HASH` per
/// pair of indexes on different clauses (measured on 01-11-2026 with `{g: 1}`, `{n: 1}`,
/// `{s: 1}` and `{g: 1, s: 1}`): the later index first, last with a sort, none for a pair one
/// index takes alone, and the documents filtered by the streamed scan's clauses first.
#[test]
fn hash_intersections_pair_the_indexes_as_refdb() {
    let three = [index("g_1", &[("g", 1)]), index("n_1", &[("n", 1)]), index("s_1", &[("s", 1)])];
    let all = r#"{"$and":[{"g":{"$eq":"a"}},{"n":{"$eq":5}},{"s":{"$eq":"apple"}}]}"#;
    let g = r#"{"g":{"$eq":"a"}}"#;
    let n = r#"{"n":{"$eq":{"$numberInt":"5"}}}"#;
    let s = r#"{"s":{"$eq":"apple"}}"#;
    let and = |parts: &[&str]| format!(r#"{{"$and":[{}]}}"#, parts.join(","));
    assert_eq!(
        intersections(all, &[], &three),
        [
            format!("AND_HASH [n_1, g_1] {}", and(&[g, n, s])),
            format!("AND_HASH [s_1, g_1] {}", and(&[g, s, n])),
            format!("AND_HASH [s_1, n_1] {}", and(&[n, s, g])),
        ]
    );
    let four = [
        index("g_1", &[("g", 1)]),
        index("n_1", &[("n", 1)]),
        index("s_1", &[("s", 1)]),
        index("g_1_s_1", &[("g", 1), ("s", 1)]),
    ];
    let found = intersections(all, &[], &four);
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(found.contains(&format!("AND_HASH [g_1_s_1, n_1] {}", and(&[n, g, s]))), "{found:?}");
    assert!(!found.iter().any(|f| f.starts_with("AND_HASH [s_1, g_1]")), "{found:?}");
    let two = r#"{"$and":[{"g":{"$eq":"a"}},{"n":{"$eq":5}}]}"#;
    assert_eq!(
        intersections(two, &[("n", 1)], &three),
        [format!("AND_HASH [g_1, n_1] {}", and(&[g, n]))]
    );
    assert!(intersections(two, &[], &three[..1]).is_empty());
    let one = r#"{"g":{"$eq":"a"}}"#;
    assert!(intersections(one, &[], &three).is_empty());
}

#[test]
fn indexes_scan_the_intervals_refdb_shows() {
    let c = catalog();
    assert_eq!(
        plans(r#"{"g":{"$eq":"a"}}"#, &[], &c),
        [r#"g_1: g["[\"a\", \"a\"]"]"#, r#"g_1_n_1: g["[\"a\", \"a\"]"] n["[MinKey, MaxKey]"]"#,]
    );
    assert_eq!(plans(r#"{"n":{"$gt":3}}"#, &[], &c), [r#"n_1: n["(3, inf]"]"#]);
    assert_eq!(plans(r#"{"x":{"$lt":2}}"#, &[], &c), [r#"x_-1: x["(2, -inf]"]"#]);
    assert_eq!(plans(r#"{"o.k":{"$eq":1}}"#, &[], &c), [r#"o.k_1: o.k["[1, 1]"]"#]);
    assert_eq!(plans(r#"{"t":{"$eq":"red"}}"#, &[], &c), [r#"t_1: t["[\"red\", \"red\"]"]"#]);
    assert_eq!(
        plans(r#"{"$and":[{"n":{"$gte":3}},{"n":{"$lt":7}}]}"#, &[], &c),
        [r#"n_1: n["[3, 7)"]"#]
    );
    assert_eq!(
        plans(r#"{"s":{"$regex":"^a"}}"#, &[], &c),
        [r#"s_1: s["[\"a\", \"b\")", "[/^a/, /^a/]"]"#]
    );
    assert_eq!(
        plans(r#"{"g":{"$in":["a","b"]}}"#, &[], &c),
        [
            r#"g_1: g["[\"a\", \"a\"]", "[\"b\", \"b\"]"]"#,
            r#"g_1_n_1: g["[\"a\", \"a\"]", "[\"b\", \"b\"]"] n["[MinKey, MaxKey]"]"#,
        ]
    );
    assert_eq!(plans(r#"{"zz":{"$eq":1}}"#, &[], &c), ["COLLSCAN"]);
    assert_eq!(plans(r#"{}"#, &[], &c), ["COLLSCAN"]);
}

#[test]
fn the_rest_of_the_filter_waits_for_the_documents() {
    let c = catalog();
    assert_eq!(
        plans(r#"{"$and":[{"g":{"$eq":"a"}},{"n":{"$gt":3}}]}"#, &[], &c),
        [
            r#"g_1_n_1: g["[\"a\", \"a\"]"] n["(3, inf]"]"#,
            r#"n_1: n["(3, inf]"] | {"g":{"$eq":"a"}}"#,
            r#"g_1: g["[\"a\", \"a\"]"] | {"n":{"$gt":{"$numberInt":"3"}}}"#,
        ]
    );
    // `null` matches a missing field too, which no index entry decides alone.
    assert_eq!(
        plans(r#"{"g":{"$eq":null}}"#, &[], &c)[0],
        r#"g_1: g["[null, null]"] | {"g":{"$eq":null}}"#
    );
}

#[test]
fn an_index_in_the_order_of_the_sort_is_a_plan() {
    let c = catalog();
    assert_eq!(plans(r#"{}"#, &[("n", 1)], &c), [r#"n_1: n["[MinKey, MaxKey]"] sorted"#]);
    assert_eq!(
        plans(r#"{"g":{"$eq":"a"}}"#, &[("n", -1)], &c),
        [
            r#"g_1: g["[\"a\", \"a\"]"]"#,
            r#"g_1_n_1: g["[\"a\", \"a\"]"] n["[MaxKey, MinKey]"] sorted backward"#,
        ]
    );
    assert_eq!(
        plans(r#"{"g":{"$eq":"a"}}"#, &[("s", 1)], &c),
        [
            r#"g_1: g["[\"a\", \"a\"]"]"#,
            r#"g_1_n_1: g["[\"a\", \"a\"]"] n["[MinKey, MaxKey]"]"#,
            r#"s_1: s["[MinKey, MaxKey]"] | {"g":{"$eq":"a"}} sorted"#,
        ]
    );
    // An index on arrays gives no order.
    assert_eq!(plans(r#"{}"#, &[("t", 1)], &c), ["COLLSCAN"]);
    assert_eq!(plans(r#"{}"#, &[("x", 1), ("q", 1)], &c), ["COLLSCAN"]);
}

#[test]
fn a_disjunction_scans_an_index_per_clause() {
    let c = catalog();
    assert_eq!(
        plans(r#"{"$or":[{"g":{"$eq":"a"}},{"s":{"$eq":"date"}}]}"#, &[], &c),
        [r#"OR [g_1: g["[\"a\", \"a\"]"]; s_1: s["[\"date\", \"date\"]"]]"#]
    );
    assert_eq!(plans(r#"{"$or":[{"g":{"$eq":"a"}},{"zz":{"$eq":1}}]}"#, &[], &c), ["COLLSCAN"]);
    assert_eq!(plans(r#"{"$or":[{"g":{"$eq":"a"}},5]}"#, &[], &c), ["COLLSCAN"]);
}

#[test]
fn a_hint_makes_its_index_the_plan() {
    let c = catalog();
    let parsed = doc(r#"{"g":{"$eq":"a"}}"#);
    let request = Request { parsed: Some(&parsed), sort: &[], hint: Some(1), intersect: false };
    let got: Vec<String> = candidates(&request, &c).iter().map(|a| shown(a, &c)).collect();
    assert_eq!(got, [r#"n_1: n["[MinKey, MaxKey]"] | {"g":{"$eq":"a"}}"#]);
    let hidden = vec![IndexMeta { plannable: false, ..index("g_1", &[("g", 1)]) }];
    assert_eq!(plans(r#"{"g":{"$eq":"a"}}"#, &[], &hidden), ["COLLSCAN"]);
}

/// The order of the plans when their trials tie, for each order of creation.
#[test]
fn tied_plans_keep_refdb_order() {
    let single = |n: &str| index(n, &[(n, 1)]);
    let both = index("ab", &[("a", 1), ("b", 1)]);
    let names = |parsed: &str, c: &[IndexMeta]| -> Vec<String> {
        let parsed = doc(parsed);
        let request = Request { parsed: Some(&parsed), sort: &[], hint: None, intersect: false };
        candidates(&request, c)
            .iter()
            .map(|a| match a {
                Access::Index(s) => c[s.index].name.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    };
    let eq = r#"{"$and":[{"a":{"$eq":1}},{"b":{"$eq":1}}]}"#;
    for (catalog, want) in [
        (vec![single("a"), single("b"), both.clone()], ["ab", "b", "a"]),
        (vec![single("b"), single("a"), both.clone()], ["ab", "a", "b"]),
        (vec![both.clone(), single("a"), single("b")], ["ab", "a", "b"]),
        (vec![both.clone(), single("b"), single("a")], ["ab", "b", "a"]),
    ] {
        assert_eq!(names(eq, &catalog), want);
        assert_eq!(names(r#"{"a":{"$eq":1}}"#, &catalog), ["a", "ab"]);
    }
}

#[test]
fn trials_choose_as_refdb_scores() {
    let t = |works, advanced, fetches, sorts| Trial {
        works,
        advanced,
        fetches,
        sorts,
        intersects: false,
        eof: false,
    };
    // Equal trials: the plan made first.
    assert_eq!(ranked(&[t(4, 3, true, false), t(4, 3, true, false)]), [0, 1]);
    // More documents per work.
    assert_eq!(ranked(&[t(4, 2, true, false), t(3, 2, true, false)]), [1, 0]);
    // A sort costs its bonus.
    assert_eq!(ranked(&[t(4, 3, true, true), t(4, 3, true, false)]), [1, 0]);
    assert!(score(&t(0, 0, false, false)) > 1.0);
    assert_eq!(trial_works(10), 10_000);
    assert_eq!(trial_works(100_000), 30_000);
}

#[test]
fn intervals_follow_refdb() {
    assert_eq!(intervals("$eq", &Value::Array(Vec::new())), None);
    // `$exists` scans every key of a non-sparse index, `$not: {$exists: true}` its nulls; the
    // documents decide both (measured on a `refrouter` 9.0.4, 01-11-2026).
    let (b, exact) = intervals("$exists", &Value::Boolean(true)).expect("every key");
    assert_eq!((b, exact), (vec![Bound::all()], false));
    let missing = Value::Document(doc(r#"{"$exists":true}"#));
    let (b, exact) = intervals("$not", &missing).expect("the nulls");
    assert_eq!((b, exact), (vec![Bound::point(Value::Null)], false));
    assert_eq!(intervals("$not", &Value::Document(doc(r#"{"$gt":1}"#))), None);
    assert_eq!(intervals("$exists", &Value::Boolean(false)), None);
    assert_eq!(intervals("$in", &Value::Int32(1)), None);
    assert_eq!(intervals("$in", &Value::Array(vec![Value::Array(Vec::new())])), None);
    let (b, exact) = intervals("$in", &Value::Array(vec![Value::Null])).expect("an $in");
    assert!(!exact);
    assert_eq!(b, vec![Bound::point(Value::Null)]);
    assert_eq!(intervals("$gt", &Value::Boolean(true)), None);
    // The comparisons a time series' bucket filter makes on `control.min` and `control.max`
    // scan the intervals of their plain forms, the buckets deciding them (015 FR-008).
    let at = Value::DateTime(5);
    let (plain, _) = intervals("$gte", &at).expect("a $gte");
    assert_eq!(intervals("$_internalExprGte", &at), Some((plain, false)));
    let (plain, _) = intervals("$lt", &at).expect("a $lt");
    assert_eq!(intervals("$_internalExprLt", &at), Some((plain, false)));
    assert_eq!(shown_value(&Value::DateTime(i64::MIN)), "new Date(-9223372036854775808)");
    let (b, exact) = intervals("$gte", &at).expect("a $gte");
    let to = Bound {
        low: at.clone(),
        low_inclusive: true,
        high: Value::DateTime(i64::MAX),
        high_inclusive: true,
    };
    assert_eq!((b, exact), (vec![to], true));
    assert!(intervals("$_internalExprGt", &at).is_some());
    assert!(intervals("$_internalExprLte", &at).is_some());
    let (b, _) = intervals("$lte", &Value::String("m".to_owned())).expect("a bound");
    assert_eq!(b[0].shown(), r#"["", "m"]"#);
    let (b, _) = intervals("$gte", &Value::String("m".to_owned())).expect("a bound");
    assert_eq!(b[0].shown(), r#"["m", {})"#);
    let regex = Value::Regex { pattern: "a.b".to_owned(), options: String::new() };
    assert_eq!(intervals("$regex", &regex), None);
    let caseless = Value::Regex { pattern: "^a".to_owned(), options: "i".to_owned() };
    assert_eq!(intervals("$regex", &caseless), None);
    assert_eq!(intervals("$regex", &Value::Int32(1)), None);
    let (b, exact) = intervals("$eq", &regex).expect("a point");
    assert!(exact);
    assert_eq!(b[0].shown(), "[/a.b/, /a.b/]");
}

#[test]
fn values_and_intervals_are_written_as_explain_writes_them() {
    for (v, want) in [
        (Value::Double(1.5), "1.5"),
        (Value::Double(f64::NEG_INFINITY), "-inf"),
        (Value::Int64(7), "7"),
        (Value::Undefined, "undefined"),
        (Value::Boolean(true), "true"),
        (Value::Array(Vec::new()), "[]"),
        (Value::String("q\"\\".to_owned()), r#""q\"\\""#),
    ] {
        assert_eq!(shown_value(&v), want);
    }
    assert!(!shown_value(&Value::ObjectId([0; 12])).is_empty());
    let a = vec![Bound::all()];
    let b = vec![Bound {
        low: Value::Int32(5),
        low_inclusive: false,
        high: Value::Int32(9),
        high_inclusive: true,
    }];
    assert_eq!(intersect(&a, &b), b);
    assert_eq!(intersect(&b, &a), b);
    let c = vec![Bound {
        low: Value::Int32(1),
        low_inclusive: true,
        high: Value::Int32(5),
        high_inclusive: false,
    }];
    assert!(intersect(&b, &c).is_empty());
    let d = vec![Bound {
        low: Value::Int32(5),
        low_inclusive: true,
        high: Value::Int32(9),
        high_inclusive: false,
    }];
    assert_eq!(intersect(&b, &d)[0].shown(), "(5, 9)");
    let points = sorted(vec![
        Bound::point(Value::Int32(2)),
        Bound::point(Value::Int32(1)),
        Bound::point(Value::Int32(2)),
    ]);
    assert_eq!(points.len(), 2);
    assert!(compare_values(&Value::Int32(1), &Value::Int64(1)).is_eq());
}

#[test]
fn clauses_and_paths_of_a_parsed_filter() {
    assert!(conjuncts(&doc("{}")).is_empty());
    assert_eq!(conjuncts(&doc(r#"{"$and":[{"a":{"$eq":1}},5]}"#)).len(), 1);
    assert_eq!(conjunction(&[]), None);
    assert_eq!(on_path(&doc(r#"{"$expr":1}"#)), None);
    assert_eq!(on_path(&doc(r#"{"a":1}"#)), None);
    assert_eq!(on_path(&doc(r#"{"a":{"$gt":1,"$lt":3}}"#)), None);
    assert_eq!(on_path(&doc(r#"{"a":{}}"#)), None);
}

/// The branches the measured cases leave: clauses no index answers, arrays, hints over `$or`,
/// intervals that touch.
#[test]
fn fields_after_a_range_keep_their_intervals() {
    let c = catalog();
    assert_eq!(
        plans(r#"{"$and":[{"n":{"$eq":5}},{"g":{"$gt":"a"}}]}"#, &[], &c)[0],
        r#"g_1_n_1: g["(\"a\", {})"] n["[5, 5]"]"#
    );
    assert_eq!(
        plans(r#"{"$and":[{"n":{"$eq":5}},{"g":{"$exists":true}}]}"#, &[], &c)[0],
        r#"g_1_n_1: g["[MinKey, MaxKey]"] n["[5, 5]"] | {"g":{"$exists":true}}"#
    );
}

#[test]
fn existence_scans_the_whole_index() {
    let c = catalog();
    assert_eq!(
        plans(r#"{"t":{"$exists":true}}"#, &[], &c),
        [r#"t_1: t["[MinKey, MaxKey]"] | {"t":{"$exists":true}}"#]
    );
    assert_eq!(
        plans(r#"{"t":{"$not":{"$exists":true}}}"#, &[], &c),
        [r#"t_1: t["[null, null]"] | {"t":{"$not":{"$exists":true}}}"#]
    );
    assert_eq!(
        plans(r#"{"g":{"$exists":true}}"#, &[], &c),
        [
            r#"g_1: g["[MinKey, MaxKey]"] | {"g":{"$exists":true}}"#,
            r#"g_1_n_1: g["[MinKey, MaxKey]"] n["[MinKey, MaxKey]"] | {"g":{"$exists":true}}"#,
        ]
    );
}

#[test]
fn edges_of_the_rules() {
    let c = catalog();
    // A clause of its own and an operator without intervals stay for the documents.
    assert_eq!(
        plans(
            r#"{"$and":[{"$expr":{"$eq":["$g","a"]}},{"g":{"$exists":true}},{"g":{"$eq":"a"}}]}"#,
            &[],
            &c
        )[0],
        r#"g_1: g["[\"a\", \"a\"]"] | {"$and":[{"$expr":{"$eq":["$g","a"]}},{"g":{"$exists":true}}]}"#
    );
    // On arrays, a second clause on the field keeps to the first one's intervals.
    assert_eq!(
        plans(r#"{"$and":[{"t":{"$gt":"a"}},{"t":{"$lt":"m"}}]}"#, &[], &c),
        [r#"t_1: t["(\"a\", {})"] | {"t":{"$lt":"m"}}"#]
    );
    // A hint over a `$or` plans the hinted index alone.
    let parsed = doc(r#"{"$or":[{"g":{"$eq":"a"}},{"s":{"$eq":"x"}}]}"#);
    let hidden = vec![IndexMeta { plannable: false, ..index("g_1", &[("g", 1)]) }];
    let request = Request { parsed: Some(&parsed), sort: &[], hint: Some(0), intersect: false };
    assert_eq!(candidates(&request, &hidden).len(), 1);
    // A sort after a field with several intervals: the index does not give its order.
    assert_eq!(
        plans(r#"{"g":{"$in":["a","b"]}}"#, &[("n", 1)], &c),
        [
            r#"g_1: g["[\"a\", \"a\"]", "[\"b\", \"b\"]"]"#,
            r#"g_1_n_1: g["[\"a\", \"a\"]", "[\"b\", \"b\"]"] n["[MinKey, MaxKey]"]"#,
            r#"n_1: n["[MinKey, MaxKey]"] | {"g":{"$in":["a","b"]}} sorted"#,
        ]
    );
    assert_eq!(plans(r#"{"s":{"$regex":"^"}}"#, &[], &c), ["COLLSCAN"]);
    assert_eq!(plans(r#"{"s":{"$regex":"^a.b"}}"#, &[], &c), ["COLLSCAN"]);
    assert_eq!(plans(r#"{"s":{"$regex":"^a b_c-d"}}"#, &[], &c).len(), 1);
}

#[test]
fn intervals_that_touch_meet_at_their_ends() {
    let b = |l: i32, li: bool, h: i32, hi: bool| Bound {
        low: Value::Int32(l),
        low_inclusive: li,
        high: Value::Int32(h),
        high_inclusive: hi,
    };
    assert_eq!(intersect(&[b(1, true, 5, true)], &[b(1, false, 5, false)])[0].shown(), "(1, 5)");
    assert_eq!(intersect(&[b(1, true, 5, true)], &[b(5, true, 9, true)])[0].shown(), "[5, 5]");
    assert!(intersect(&[b(1, true, 5, false)], &[b(5, true, 9, true)]).is_empty());
    let kept =
        sorted(vec![b(1, true, 3, true), b(1, true, 3, true), Bound::point(Value::Int32(1))]);
    assert_eq!(kept.len(), 3);
    assert_eq!(shown_value(&Value::Document(Document::new())), "{}");
    assert_eq!(on_path(&doc(r#"{"a":{"$eq":1},"b":{"$eq":1}}"#)), None);
}

/// A `$or` whose clauses are each one predicate on the leading field of the same index scans that
/// index once, over the union of their intervals (measured on a `refrouter` 9.0.4, 01-11-2026:
/// `{_id: {$lt: 3}}` or `{_id: "x"}` reads `_id_` over `[-inf, 3)` and `["x", "x"]`); a clause of
/// more predicates keeps its own scan, and the scans follow the order of their indexes.
#[test]
fn clauses_on_the_leading_field_of_one_index_join_their_intervals() {
    let c = vec![
        index("_id_", &[("_id", 1)]),
        index("g_1", &[("g", 1)]),
        index("n_1_g_1", &[("n", 1), ("g", 1)]),
    ];
    let one = r#"{"$or":[{"_id":{"$lt":3}},{"_id":{"$eq":"x"}}]}"#;
    assert_eq!(
        plans(one, &[], &c),
        [r#"_id_: _id["[-inf, 3)", "[\"x\", \"x\"]"]"#].map(|s| format!("OR [{s}]"))
    );
    let spread = r#"{"$or":[{"_id":{"$lt":3}},{"g":{"$eq":"c"}},{"_id":{"$gt":6}}]}"#;
    assert_eq!(
        plans(spread, &[], &c),
        [r#"OR [_id_: _id["[-inf, 3)", "(6, inf]"]; g_1: g["[\"c\", \"c\"]"]]"#]
    );
    let compound = r#"{"$or":[{"n":{"$lt":1}},{"n":{"$gt":6}}]}"#;
    assert_eq!(
        plans(compound, &[], &c),
        [r#"OR [n_1_g_1: n["[-inf, 1)", "(6, inf]"] g["[MinKey, MaxKey]"]]"#]
    );
    let and = r#"{"$or":[{"$and":[{"n":{"$gt":5}},{"n":{"$lt":8}}]},{"n":{"$eq":3}}]}"#;
    assert_eq!(
        plans(and, &[], &c),
        [
            r#"OR [n_1_g_1: n["(5, 8)"] g["[MinKey, MaxKey]"]; n_1_g_1: n["[3, 3]"] g["[MinKey, MaxKey]"]]"#
        ]
    );
    let first = r#"{"$or":[{"_id":{"$lt":3}},{"$and":[{"_id":{"$eq":"x"}},{"g":{"$eq":2}}]}]}"#;
    assert_eq!(
        plans(first, &[], &c),
        [
            r#"OR [_id_: _id["[\"x\", \"x\"]"] | {"g":{"$eq":{"$numberInt":"2"}}}; _id_: _id["[-inf, 3)"]]"#
        ]
    );
    let all = r#"{"$or":[{"_id":{"$gt":2}},{"_id":{"$lt":4}}]}"#;
    assert_eq!(plans(all, &[], &c), [r#"OR [_id_: _id["[-inf, inf]"]]"#]);
    let backward = r#"{"$or":[{"_id":{"$gt":6}},{"_id":{"$eq":2}}]}"#;
    assert_eq!(
        plans(backward, &[("_id", -1)], &c),
        [r#"OR [_id_: _id["[inf, 6)", "[2, 2]"] sorted backward]"#]
    );
}

/// The union of intervals: those that meet or overlap join, the others stay apart in order.
#[test]
fn intervals_join_where_they_meet() {
    let b = |low: i32, li: bool, high: i32, hi: bool| Bound {
        low: Value::Int32(low),
        low_inclusive: li,
        high: Value::Int32(high),
        high_inclusive: hi,
    };
    let shown = |list: Vec<Bound>| list.iter().map(Bound::shown).collect::<Vec<_>>();
    assert_eq!(shown(union(&[b(1, true, 3, false)], &[b(3, true, 5, true)])), ["[1, 5]"]);
    assert_eq!(
        shown(union(&[b(1, true, 3, false)], &[b(3, false, 5, true)])),
        ["[1, 3)", "(3, 5]"]
    );
    assert_eq!(shown(union(&[b(4, true, 9, true)], &[b(1, true, 5, false)])), ["[1, 9]"]);
    assert_eq!(shown(union(&[b(1, true, 9, false)], &[b(2, true, 3, true)])), ["[1, 9)"]);
    assert_eq!(shown(union(&[b(1, true, 3, false)], &[b(1, true, 3, true)])), ["[1, 3]"]);
    assert_eq!(shown(union(&[b(5, true, 5, true)], &[b(1, true, 1, true)])), ["[1, 1]", "[5, 5]"]);
}

#[test]
fn edges_of_intervals_and_their_display() {
    let open = Bound {
        low: Value::Int32(1),
        low_inclusive: false,
        high: Value::Int32(1),
        high_inclusive: true,
    };
    assert!(!open.is_point());
    assert_eq!(shown_value(&Value::Document(Document::new())), "{}");
    assert_eq!(shown_value(&Value::Array(Vec::new())), "[]");
    // `$not` of more than `$exists`, or of `$exists: 0`, has no intervals.
    assert_eq!(intervals("$not", &Value::Document(doc(r#"{"$exists":true,"$gt":1}"#))), None);
    assert_eq!(intervals("$not", &Value::Document(doc(r#"{"$exists":0}"#))), None);
    for v in [Value::Int64(1), Value::Double(1.0), Value::String("x".to_owned())] {
        assert!(intervals("$exists", &v).is_some(), "{v:?}");
    }
    for v in [Value::Int64(0), Value::Double(0.0), Value::Null, Value::Undefined] {
        assert!(intervals("$exists", &v).is_none(), "{v:?}");
    }
    // Two intervals with the same upper end keep it in only if both do.
    let a = Bound {
        low: Value::Int32(0),
        low_inclusive: true,
        high: Value::Int32(5),
        high_inclusive: true,
    };
    let b = Bound {
        low: Value::Int32(1),
        low_inclusive: true,
        high: Value::Int32(5),
        high_inclusive: false,
    };
    assert_eq!(intersect(std::slice::from_ref(&a), std::slice::from_ref(&b))[0].shown(), "[1, 5)");
    // Intervals that meet at an end left open on both sides stay apart.
    let c = Bound {
        low: Value::Int32(5),
        low_inclusive: false,
        high: Value::Int32(9),
        high_inclusive: true,
    };
    assert_eq!(union(&[b], &[c]).len(), 2);
    let empty = Bound {
        low: Value::Int32(5),
        low_inclusive: false,
        high: Value::Int32(5),
        high_inclusive: true,
    };
    assert!(intersect(&[a], &[empty]).is_empty());
}

#[test]
fn a_trial_that_does_not_end_scores_without_its_bonus() {
    let base =
        Trial { works: 10, advanced: 5, fetches: true, sorts: false, intersects: false, eof: true };
    assert!(score(&base) > score(&Trial { eof: false, ..base }));
}

#[test]
fn a_hint_takes_no_intersection() {
    let c = catalog();
    let parsed = doc(r#"{"$and":[{"g":{"$eq":"a"}},{"s":{"$eq":"x"}}]}"#);
    let request = Request { parsed: Some(&parsed), sort: &[], hint: Some(0), intersect: true };
    let found = candidates(&request, &c);
    assert_eq!(found.len(), 1);
    assert!(matches!(found[0], Access::Index(_)));
}

#[test]
fn more_edges_of_intervals_joins_and_intersections() {
    let half = Bound {
        low: Value::Int32(1),
        low_inclusive: true,
        high: Value::Int32(1),
        high_inclusive: false,
    };
    assert!(!half.is_point());
    let d = Value::Document(doc(r#"{"a":1}"#));
    assert_ne!(shown_value(&d), "{}");
    assert_ne!(shown_value(&Value::Array(vec![Value::Int32(1)])), "[]");
    let x = Bound {
        low: Value::Int32(0),
        low_inclusive: true,
        high: Value::Int32(5),
        high_inclusive: false,
    };
    let y = Bound {
        low: Value::Int32(1),
        low_inclusive: true,
        high: Value::Int32(5),
        high_inclusive: true,
    };
    assert_eq!(intersect(std::slice::from_ref(&x), std::slice::from_ref(&y))[0].shown(), "[1, 5)");
    let after = Bound {
        low: Value::Int32(5),
        low_inclusive: false,
        high: Value::Int32(9),
        high_inclusive: true,
    };
    assert!(intersect(std::slice::from_ref(&y), std::slice::from_ref(&after)).is_empty());
    let from = Bound {
        low: Value::Int32(5),
        low_inclusive: true,
        high: Value::Int32(9),
        high_inclusive: true,
    };
    assert_eq!(union(&[x], &[from])[0].shown(), "[0, 9]");
    // The clauses of a `$or` on different indexes stay apart.
    let c = catalog();
    assert_eq!(plans(r#"{"$or":[{"g":{"$eq":"a"}},{"s":{"$eq":"x"}}]}"#, &[], &c).len(), 1);
    let parsed = doc(r#"{"$and":[{"g":{"$eq":"a"}},{"s":{"$eq":"x"}}]}"#);
    let request = Request { parsed: Some(&parsed), sort: &[], hint: None, intersect: true };
    assert!(candidates(&request, &c).iter().any(|a| matches!(a, Access::AndHash { .. })));
}
