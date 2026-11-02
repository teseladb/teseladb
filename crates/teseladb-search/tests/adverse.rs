//! Adverse cases of 011 T046, measured on `refsearch` 1.70.6 with `refnode` 9.0.4 on 02-11-2026
//! (`scratchpad/lane3p/m51.js` to `m53.js`): enormous texts and values, queries that expand into
//! many terms and option values out of bounds.

use teseladb_bson::{Document, extjson};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;
use teseladb_search::query::search::SearchError;

/// Value lengths of the measured documents, around the limits of `refsearch`.
const LENGTHS: [usize; 11] = [100, 255, 256, 1000, 8191, 8192, 8193, 16383, 16384, 32765, 32766];

fn index(definition: &str, docs: Vec<Document>) -> Indexer {
    let def = Definition::parse(&extjson::parse(definition).expect("JSON")).expect("definition");
    let mut ix = Indexer::new(def);
    let changes =
        docs.into_iter().enumerate().map(|(i, d)| (vec![u8::try_from(i).expect("few")], Some(d)));
    ix.apply(changes).expect("applied");
    ix
}

fn ids(ix: &Indexer, query: &Document) -> Result<Vec<i64>, SearchError> {
    let mut out: Vec<i64> = ix
        .search(query)?
        .iter()
        .map(|f| match f.doc.get("_id") {
            Some(teseladb_bson::Value::Int64(n)) => *n,
            other => panic!("an _id: {other:?}"),
        })
        .collect();
    out.sort_unstable();
    Ok(out)
}

fn scores(ix: &Indexer, query: &Document) -> Result<Vec<f32>, SearchError> {
    Ok(ix.search(query)?.iter().map(|f| f.score).collect())
}

fn q(json: &str) -> Document {
    extjson::parse(json).expect("query JSON")
}

#[test]
fn token_values_keep_their_first_8191_characters_and_keyword_terms_their_bytes() {
    let docs = LENGTHS
        .iter()
        .map(|&n| {
            Document::new()
                .with("_id", i64::try_from(n).expect("small"))
                .with("k", "k".repeat(n))
                .with("t", "t".repeat(n))
                .with("u", "é".repeat(n))
        })
        .collect();
    let ix = index(
        r#"{"mappings": {"dynamic": true, "fields": {"k": {"type": "token"},
            "t": {"type": "string", "analyzer": "reflib.keyword"}, "u": {"type": "token"}}}}"#,
        docs,
    );
    let long: Vec<i64> = vec![8191, 8192, 8193, 16383, 16384, 32765, 32766];
    for &n in &LENGTHS {
        let k = Document::new()
            .with("equals", Document::new().with("path", "k").with("value", "k".repeat(n)));
        let t = Document::new()
            .with("text", Document::new().with("path", "t").with("query", "t".repeat(n)));
        let p = Document::new()
            .with("phrase", Document::new().with("path", "t").with("query", "t".repeat(n)));
        let u = Document::new()
            .with("equals", Document::new().with("path", "u").with("value", "é".repeat(n)));
        let n64 = i64::try_from(n).expect("small");
        let token = match n {
            ..8191 => vec![n64],
            8191 => long.clone(),
            _ => Vec::new(),
        };
        assert_eq!(ids(&ix, &k).expect("k"), token, "token of {n}");
        assert_eq!(ids(&ix, &t).expect("t"), vec![n64], "keyword of {n}");
        assert_eq!(ids(&ix, &p).expect("p"), vec![n64], "phrase of {n}");
        match n {
            // The query term is over 32 766 bytes in UTF-8.
            16384.. => {
                let e = ids(&ix, &u).expect_err("immense term");
                assert_eq!(e.0, "Path 'u' needs to be indexed as token");
            }
            _ => assert_eq!(ids(&ix, &u).expect("u"), token, "accented token of {n}"),
        }
    }
    let over = Document::new()
        .with("equals", Document::new().with("path", "k").with("value", "k".repeat(32767)));
    assert_eq!(ids(&ix, &over).expect_err("immense").0, "Path 'k' needs to be indexed as token");
    let all = ids(&ix, &q(r#"{"exists": {"path": "k"}}"#)).expect("exists");
    assert_eq!(all.len(), LENGTHS.len());
}

#[test]
fn keyword_terms_over_32766_bytes_are_not_indexed() {
    let docs = vec![
        Document::new().with("_id", 5_i64).with("t", "z".repeat(40000)),
        Document::new().with("_id", 4_i64).with("k", "x"),
    ];
    let ix = index(
        r#"{"mappings": {"dynamic": true, "fields": {"k": {"type": "token"},
            "t": {"type": "string", "analyzer": "reflib.keyword"}}}}"#,
        docs,
    );
    let tz = Document::new()
        .with("text", Document::new().with("path", "t").with("query", "z".repeat(40000)));
    assert_eq!(ids(&ix, &tz).expect("tz"), Vec::<i64>::new());
    assert_eq!(ids(&ix, &q(r#"{"exists": {"path": "t"}}"#)).expect("exists"), vec![5]);
    let wk = ids(&ix, &q(r#"{"wildcard": {"path": "k", "query": "k*"}}"#)).expect_err("analyzed");
    assert_eq!(
        wk.0,
        "Field k is analyzed. Use a keyword analyzed field or set allowAnalyzedField to true."
    );
}

#[test]
fn enormous_texts_and_expanding_queries_score_as_refsearch() {
    let words: Vec<String> = (0..3000_u32).map(|i| format!("w{}x", radix36(i))).collect();
    let big = format!("{} b{}", "a".repeat(300), "c".repeat(40000));
    let docs = vec![
        Document::new().with("_id", 1_i64).with("t", big).with("k", "k".repeat(40000)),
        Document::new().with("_id", 2_i64).with("t", words.join(" ")).with("k", "small"),
        Document::new().with("_id", 3_i64).with("t", "aaaa bbbb").with("k", "small2"),
    ];
    let ix = index(r#"{"mappings": {"dynamic": true, "fields": {"k": {"type": "token"}}}}"#, docs);
    let text =
        |s: &str| Document::new().with("text", Document::new().with("path", "t").with("query", s));
    // Tokens of more than 255 characters are cut into pieces of 255.
    assert_eq!(scores(&ix, &text(&"a".repeat(255))).expect("a255"), [0.685_974_96]);
    assert_eq!(scores(&ix, &text(&"a".repeat(300))).expect("a300"), [1.371_949_9]);
    assert_eq!(scores(&ix, &text(&"a".repeat(45))).expect("a45"), [0.685_974_96]);
    assert_eq!(scores(&ix, &text(&"c".repeat(255))).expect("c255"), [0.978_116_8]);
    let wild = q(r#"{"wildcard": {"path": "t", "query": "w*x", "allowAnalyzedField": true}}"#);
    assert_eq!(ids(&ix, &wild).expect("wildcard"), vec![2]);
    assert_eq!(scores(&ix, &wild).expect("wildcard"), [1.0]);
    let regex = q(r#"{"regex": {"path": "t", "query": "w.*", "allowAnalyzedField": true}}"#);
    assert_eq!(scores(&ix, &regex).expect("regex"), [1.0]);
    let fuzzy = |m: &str| {
        q(&format!(r#"{{"text": {{"path": "t", "query": "w1x", "fuzzy": {{"maxEdits": 2{m}}}}}}}"#))
    };
    assert_eq!(scores(&ix, &fuzzy(r#", "maxExpansions": 3"#)).expect("3"), [0.614_255_3]);
    assert_eq!(scores(&ix, &fuzzy("")).expect("50"), [8.862_827]);
    let e = scores(&ix, &fuzzy(r#", "maxExpansions": 100000"#)).expect_err("bounds");
    assert_eq!(e.0, "\"text.fuzzy.maxExpansions\" must be within bounds [1..1000]");
    let phrase = Document::new()
        .with("phrase", Document::new().with("path", "t").with("query", words[..500].join(" ")));
    assert_eq!(scores(&ix, &phrase).expect("phrase"), [131.626_13]);
    let e = scores(&ix, &text(&words.join(" "))).expect_err("too many clauses");
    assert_eq!(
        e.0,
        "query has expanded into too many sub-queries internally: maxClauseCount is set to 1024"
    );
    // One boolean query takes 1 024 clauses: the terms of `text`, the clauses of `compound`.
    assert_eq!(scores(&ix, &text(&words[..1024].join(" "))).expect("1024"), [269.570_3]);
    let should = |n: usize| -> Vec<teseladb_bson::Value> {
        words[..n]
            .iter()
            .map(|w| {
                Document::new()
                    .with("text", Document::new().with("path", "t").with("query", w.as_str()))
                    .into()
            })
            .collect()
    };
    let compound =
        |n: usize| Document::new().with("compound", Document::new().with("should", should(n)));
    assert_eq!(scores(&ix, &compound(1024)).expect("1024 clauses"), [269.570_3]);
    assert_eq!(scores(&ix, &compound(1025)).expect_err("1025 clauses").0, e.0);
    let ks = q(r#"{"equals": {"path": "k", "value": "small"}}"#);
    assert_eq!(ids(&ix, &ks).expect("small"), vec![2]);
}

/// The measured index of `m51.js`, with `u` on document 3 (`m60.js`).
fn clause_index(words: &[String]) -> Indexer {
    let big = format!("{} b{}", "a".repeat(300), "c".repeat(40000));
    let docs = vec![
        Document::new().with("_id", 1_i64).with("t", big).with("k", "k".repeat(40000)),
        Document::new().with("_id", 2_i64).with("t", words.join(" ")).with("k", "small"),
        Document::new()
            .with("_id", 3_i64)
            .with("t", "aaaa bbbb")
            .with("k", "small2")
            .with("u", "aaaa"),
    ];
    index(r#"{"mappings": {"dynamic": true, "fields": {"k": {"type": "token"}}}}"#, docs)
}

/// The error of `query` on `ix`, or `ok`.
fn outcome(ix: &Indexer, query: &str) -> String {
    match ix.search(&q(query)) {
        Ok(_) => "ok".to_owned(),
        Err(e) => e.0,
    }
}

#[test]
fn clauses_are_counted_after_rewriting_as_refsearch() {
    let w: Vec<String> = (0..3000_u32).map(|i| format!("w{}x", radix36(i))).collect();
    let z: Vec<String> = (0..3000_u32).map(|i| format!("zzzzzzzzz{}", radix36(i))).collect();
    let ix = clause_index(&w);
    let flat = "maxClauseCount is set to 1024";
    let nested = "Query contains too many nested clauses; maxClauseCount is set to 1024";
    let text = |words: &[String], fuzzy: bool| {
        let f = if fuzzy { r#", "fuzzy": {}"# } else { "" };
        format!(r#"{{"text": {{"path": "t", "query": "{}"{f}}}}}"#, words.join(" "))
    };
    let should = |ops: &[String]| format!(r#"{{"compound": {{"should": [{}]}}}}"#, ops.join(", "));
    // A `text` on two paths is a disjunction of one per path (m60 `p2_512`, `p2_513`).
    let two = |n: usize| {
        format!(r#"{{"text": {{"path": ["t", "u"], "query": "{}"}}}}"#, w[..n].join(" "))
    };
    assert_eq!(outcome(&ix, &two(512)), "ok");
    assert_eq!(outcome(&ix, &two(513)), flat);
    // Each fuzzy word brings its 50 expansions (m60 `fz20`, `fz21`; m64).
    let fz = |n: usize| {
        format!(
            r#"{{"text": {{"path": "t", "query": "{}", "fuzzy": {{"maxEdits": 2}}}}}}"#,
            w[..n].join(" ")
        )
    };
    assert_eq!(outcome(&ix, &fz(20)), "ok");
    assert_eq!(outcome(&ix, &fz(21)), flat);
    // A `should` takes in the disjunctions it holds: 600 and 500 terms are too many (m61 `a`),
    // 524 and 500 are not (m65 `plain2`).
    assert_eq!(outcome(&ix, &should(&[text(&w[..600], false), text(&w[600..1100], false)])), flat);
    assert_eq!(outcome(&ix, &should(&[text(&z[..524], false), text(&z[1000..1500], false)])), "ok");
    assert_eq!(outcome(&ix, &should(&[text(&z[..525], false), text(&z[1000..1500], false)])), flat);
    // The 1 000 expansions of 20 fuzzy words are some 550 different terms once taken in
    // (m65 `z450`, `z480`; m64 `w400`), and one fuzzy word with 1 001 more terms is too many.
    let fuzzy20 = text(&w[..20], true);
    assert_eq!(outcome(&ix, &should(&[fuzzy20.clone(), text(&z[..450], false)])), "ok");
    assert_eq!(outcome(&ix, &should(&[fuzzy20.clone(), text(&z[..480], false)])), flat);
    assert_eq!(outcome(&ix, &should(&[fuzzy20, text(&w[100..500], false)])), "ok");
    let one = should(&[text(&w[..1], true), text(&z[..1000], false), text(&z[1000..1001], false)]);
    assert_eq!(outcome(&ix, &one), flat);
    // Equal clauses of one level count once (m66 `dupText`; m61 `ex2`).
    assert_eq!(outcome(&ix, &should(&[text(&z[..600], false), text(&z[..600], false)])), "ok");
    let exists = |n: usize| should(&vec![r#"{"exists": {"path": "t"}}"#.to_owned(); n]);
    assert_eq!(outcome(&ix, &should(&[exists(600), exists(500)])), "ok");
    // A numeric `range` is two clauses (m66 `rangeShould`, `rangeMust`).
    let ranges = |a: usize, b: usize| {
        let r: Vec<String> =
            (a..b).map(|i| format!(r#"{{"range": {{"path": "n", "gte": {i}}}}}"#)).collect();
        should(&r)
    };
    assert_eq!(outcome(&ix, &should(&[ranges(0, 600), ranges(600, 1100)])), flat);
    let must = |m: &str, other: &str, o: &str| {
        format!(r#"{{"compound": {{"must": [{m}], "{other}": [{o}]}}}}"#)
    };
    assert_eq!(outcome(&ix, &must(&ranges(0, 600), "filter", &ranges(600, 1100))), flat);
    // With `must` or `filter` nothing is taken in, and the whole tree counts (m61 `mixed`,
    // m66 `eqMust`, `mustText`, `mustText2`, `dupMust`; m62 `zm1000`).
    let equals = |a: usize, b: usize| {
        let e: Vec<String> =
            (a..b).map(|i| format!(r#"{{"equals": {{"path": "k", "value": "v{i}"}}}}"#)).collect();
        should(&e)
    };
    assert_eq!(
        outcome(&ix, &must(&text(&w[..600], false), "filter", &text(&w[600..1100], false))),
        nested
    );
    assert_eq!(outcome(&ix, &must(&equals(0, 600), "filter", &equals(600, 1100))), nested);
    assert_eq!(
        outcome(&ix, &must(&text(&z[..600], false), "should", &text(&z[600..1100], false))),
        nested
    );
    assert_eq!(
        outcome(&ix, &must(&text(&z[..524], false), "should", &text(&z[600..1100], false))),
        "ok"
    );
    assert_eq!(
        outcome(&ix, &must(&text(&z[..600], false), "filter", &text(&z[..600], false))),
        "ok"
    );
    assert_eq!(
        outcome(&ix, &must(&text(&w[1..2], false), "should", &text(&z[..1000], false))),
        "ok"
    );
}

fn radix36(mut n: u32) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII")
}

#[test]
fn fuzzy_bounds_are_those_of_each_operator() {
    let docs = vec![Document::new().with("_id", 1_i64).with("t", "w1x w2x")];
    let ix = index(r#"{"mappings": {"dynamic": true}}"#, docs);
    let err = |op: &str, f: &str| {
        let json = format!(r#"{{"{op}": {{"path": "t", "query": "w1x", "fuzzy": {f}}}}}"#);
        ix.search(&q(&json)).map(|_| ()).map_err(|e| e.0)
    };
    for op in ["text", "autocomplete", "term"] {
        let at = format!("\"{op}.fuzzy");
        let integer = format!("{at}.maxEdits\" must be an integer");
        assert_eq!(err(op, r#"{"maxEdits": 1.5}"#), Err(integer));
        let negative = format!("{at}.prefixLength\" cannot be negative");
        assert_eq!(err(op, r#"{"prefixLength": -1}"#), Err(negative));
        let not_number = format!("{at}.maxExpansions\" must be a integer");
        assert_eq!(err(op, r#"{"maxExpansions": "a"}"#), Err(not_number));
    }
    for op in ["text", "autocomplete"] {
        let at = format!("\"{op}.fuzzy");
        for e in [0, 3] {
            let m = format!("{at}.maxEdits\" must be either 1 or 2");
            assert_eq!(err(op, &format!(r#"{{"maxEdits": {e}}}"#)), Err(m));
        }
        for x in [0, 1001] {
            let m = format!("{at}.maxExpansions\" must be within bounds [1..1000]");
            assert_eq!(err(op, &format!(r#"{{"maxExpansions": {x}}}"#)), Err(m));
        }
    }
    let text = err("text", r#"{"maxExpansions": 1000, "prefixLength": 100000}"#);
    assert_eq!(text, Ok(()));
    assert_eq!(
        err("term", r#"{"maxEdits": 3}"#),
        Err("\"term.fuzzy.maxEdits\" must be within bounds [0..2]".to_owned())
    );
    assert_eq!(
        err("term", r#"{"maxExpansions": 0}"#),
        Err("\"term.fuzzy.maxExpansions\" must be positive".to_owned())
    );
    assert_eq!(err("term", r#"{"maxEdits": 0, "maxExpansions": 1001}"#), Ok(()));
}
