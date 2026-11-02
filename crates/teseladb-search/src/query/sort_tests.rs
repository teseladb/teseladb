//! `sort`, `searchAfter` and `searchBefore` as `refsearch` 1.70.6 runs them (measured 02-11-2026,
//! `scratchpad/lane3k/m14.js`, whose documents and index these are). `refsearch` breaks ties with
//! RefLib's document numbers; here keys break them, so the expected orders put tied documents in
//! key order.

use teseladb_bson::{Document, Value};

use crate::definition::Definition;
use crate::indexer::Indexer;

const DAY_MS: i64 = 86_400_000;
/// 2024-01-01T00:00:00Z.
const JAN_1: i64 = 1_704_067_200_000;

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

pub(super) fn m14() -> Indexer {
    let typed = |a: &str, b: &str| -> Value {
        arr(vec![Document::new().with("type", a).into(), Document::new().with("type", b).into()])
    };
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", true).with(
            "fields",
            Document::new().with("s", typed("token", "string")).with("n", typed("number", "token")),
        ),
    );
    let date = |ms: i64| Value::DateTime(ms);
    let docs = vec![
        Document::new()
            .with("_id", 1)
            .with("t", "gato negro")
            .with("n", 5)
            .with("s", "beta")
            .with("dt", date(JAN_1 + DAY_MS))
            .with("a", arr(vec![3.into(), 9.into()])),
        Document::new()
            .with("_id", 2)
            .with("t", "gato blanco gato")
            .with("n", 2)
            .with("s", "alfa")
            .with("dt", date(JAN_1))
            .with("a", arr(vec![1.into()])),
        Document::new()
            .with("_id", 3)
            .with("t", "perro gato")
            .with("n", 5)
            .with("s", "Gamma")
            .with("a", arr(vec![])),
        Document::new()
            .with("_id", 4)
            .with("t", "gato")
            .with("s", "delta")
            .with("n", "x")
            .with("dt", date(JAN_1 - 241 * DAY_MS)),
        Document::new()
            .with("_id", 5)
            .with("t", "gato gato gato")
            .with("n", -1.5)
            .with("s", arr(vec!["b".into(), "z".into()]))
            .with("a", 7),
        Document::new().with("_id", 6).with("t", "otro"),
    ];
    let mut ix = Indexer::new(Definition::parse(&def).expect("valid"));
    ix.apply(docs.into_iter().enumerate().map(|(i, d)| (vec![i as u8 + 1], Some(d))))
        .expect("applied");
    ix
}

fn query(extra: Document) -> Document {
    let mut d =
        Document::new().with("text", Document::new().with("path", "t").with("query", "gato"));
    for (k, v) in extra.iter() {
        d.push(k, v.clone());
    }
    d
}

fn ids(ix: &Indexer, extra: Document) -> Result<Vec<u8>, String> {
    ix.search(&query(extra)).map(|f| f.iter().map(|f| f.key[0]).collect()).map_err(|e| e.0)
}

fn sort(v: Document) -> Document {
    Document::new().with("sort", v)
}

fn by(field: &str, order: i32) -> Document {
    sort(Document::new().with(field, order))
}

fn meta_score() -> Document {
    Document::new().with("$meta", "searchScore")
}

#[test]
fn sort_orders_by_indexed_values_then_keys() {
    let ix = m14();
    let cases: Vec<(Document, Vec<u8>)> = vec![
        (Document::new(), vec![5, 4, 2, 1, 3]),
        (by("n", 1), vec![5, 2, 1, 3, 4]),
        (by("n", -1), vec![4, 1, 3, 2, 5]),
        (by("s", 1), vec![3, 2, 5, 1, 4]),
        (by("s", -1), vec![5, 4, 1, 2, 3]),
        (by("dt", 1), vec![3, 5, 4, 2, 1]),
        (by("dt", -1), vec![1, 2, 4, 3, 5]),
        (by("a", 1), vec![3, 4, 2, 1, 5]),
        (by("a", -1), vec![1, 5, 2, 3, 4]),
        (sort(Document::new().with("score", meta_score())), vec![5, 4, 2, 1, 3]),
        (sort(Document::new().with("score", meta_score().with("order", 1))), vec![1, 3, 2, 4, 5]),
        (sort(Document::new().with("n", 1).with("s", -1)), vec![5, 2, 1, 3, 4]),
        (sort(Document::new().with("n", -1).with("score", meta_score())), vec![4, 1, 3, 2, 5]),
        (by("_id", -1), vec![5, 4, 3, 2, 1]),
        (by("t", 1), vec![1, 2, 3, 4, 5]),
        (by("missing", 1), vec![1, 2, 3, 4, 5]),
        (
            sort(
                Document::new()
                    .with("n", Document::new().with("order", 1).with("noData", "highest")),
            ),
            vec![5, 2, 1, 3, 4],
        ),
        (
            sort(
                Document::new()
                    .with("n", Document::new().with("order", -1).with("noData", "lowest")),
            ),
            vec![4, 1, 3, 2, 5],
        ),
        (
            sort(
                Document::new()
                    .with("dt", Document::new().with("order", 1).with("noData", "highest")),
            ),
            vec![4, 2, 1, 3, 5],
        ),
        (sort(Document::new().with("n", 1.0)), vec![5, 2, 1, 3, 4]),
        (sort(Document::new().with("n", -1_i64)), vec![4, 1, 3, 2, 5]),
    ];
    for (extra, want) in cases {
        assert_eq!(ids(&ix, extra.clone()), Ok(want), "{extra:?}");
    }
}

#[test]
fn sort_errors_match_refsearch() {
    let ix = m14();
    let err = |s: Value| ids(&ix, Document::new().with("sort", s)).unwrap_err();
    assert_eq!(err(Document::new().with("n", 0).into()), "\"sort\" sort order must be 1 or -1");
    assert_eq!(
        err(Document::new().with("n", "x").into()),
        "\"sort\" must be a Expected number or document"
    );
    assert_eq!(
        err(Document::new().with("n", Document::new().with("order", 2)).into()),
        "\"sort.order\" sort order must be 1 or -1"
    );
    assert_eq!(
        err(Document::new().with("n", Document::new().with("order", 1).with("noData", "x")).into()),
        "\"sort.noData\" must be one of [highest, lowest]"
    );
    assert_eq!(
        err(Document::new().with("n", Document::new().with("noData", 1)).into()),
        "\"sort.noData\" must be one of [highest, lowest]"
    );
    assert_eq!(
        err(Document::new().with("n", Document::new().with("$meta", "x")).into()),
        "\"sort.$meta\" must be one of [searchScore]"
    );
    assert_eq!(
        err(Document::new().into()),
        "\"sort\" Sort spec must have at least one sort field defined"
    );
    assert_eq!(err(arr(vec![])), "\"sort\" must be a document");
}

fn token(ix: &Indexer, extra: Document, key: u8) -> String {
    let found = ix.search(&query(extra)).expect("ran");
    found.into_iter().find(|f| f.key[0] == key).and_then(|f| f.token).expect("token")
}

#[test]
fn tokens_page_after_and_before() {
    let ix = m14();
    let page = |extra: Document| ids(&ix, extra);
    let plain = Document::new;
    assert_eq!(page(plain().with("searchAfter", token(&ix, plain(), 4))), Ok(vec![2, 1, 3]));
    // `searchBefore` reads the results with the sort reversed and ties still in key order, as
    // RefLib does with its document numbers: before 1 comes 3, its tie.
    assert_eq!(page(plain().with("searchBefore", token(&ix, plain(), 3))), Ok(vec![2, 4, 5]));
    assert_eq!(page(plain().with("searchBefore", token(&ix, plain(), 1))), Ok(vec![3, 2, 4, 5]));
    let n = || by("n", 1);
    assert_eq!(page(n().with("searchAfter", token(&ix, n(), 2))), Ok(vec![1, 3, 4]));
    assert_eq!(page(n().with("searchBefore", token(&ix, n(), 3))), Ok(vec![2, 5]));
    assert_eq!(page(n().with("searchBefore", token(&ix, n(), 1))), Ok(vec![3, 2, 5]));
    assert_eq!(page(by("n", -1).with("searchAfter", token(&ix, n(), 2))), Ok(vec![5]));
    // A sort's token without the sort is at score 0: nothing follows it.
    assert_eq!(page(plain().with("searchAfter", token(&ix, n(), 2))), Ok(vec![]));
    assert_eq!(page(plain().with("searchAfter", "")), Ok(vec![]));
    let s = || by("s", 1);
    assert_eq!(page(s().with("searchAfter", token(&ix, s(), 5))), Ok(vec![1, 4]));
    let d = || by("dt", 1);
    assert_eq!(page(d().with("searchAfter", token(&ix, d(), 5))), Ok(vec![4, 2, 1]));
    assert_eq!(
        page(plain().with("searchAfter", "abc")),
        Err("\"searchAfter\" Invalid format for token value".to_owned())
    );
    assert_eq!(
        page(plain().with("searchAfter", 5)),
        Err("\"searchAfter\" must be a string".to_owned())
    );
    assert_eq!(
        page(plain().with("searchAfter", "CMlJFWM8A6P+").with("searchBefore", "CMlJFWM8A6P+")),
        Err("\"searchBefore\" Invalid format for token value".to_owned())
    );
}
