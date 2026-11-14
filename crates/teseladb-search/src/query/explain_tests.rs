//! `scoreDetails` as the reference search server 1.70.6 gives it on the documents of `m14.js` (measured
//! 02-11-2026, `scratchpad/lane3k/m15.js`): each line is a node of the explanation, with its
//! depth, value and description.

use teseladb_bson::{Document, Value};

use super::sort_tests::m14;
use crate::index::Explanation;

fn flat(e: &Explanation, depth: usize, out: &mut Vec<(usize, f64, String)>) {
    out.push((depth, f64::from(e.value), e.description.clone()));
    e.details.iter().for_each(|d| flat(d, depth + 1, out));
}

fn text(q: &str) -> Document {
    Document::new().with("path", "t").with("query", q)
}

fn op(name: &str, d: Document) -> Document {
    Document::new().with(name, d)
}

type Case = (u8, &'static [(usize, f64, &'static str)]);

const CASES: [Case; 6] = [
    // {"text":{"path":"t","query":"gato negro"}}
    (
        1,
        &[
            (0, 0.809821367263794, "sum of:"),
            (1, 0.10961911082267761, "$type:string/t:gato [BM25Similarity], result of:"),
            (2, 0.10961911082267761, "score(freq=1.0), computed as boost * idf * tf from:"),
            (3, 0.24116206169128418, "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:"),
            (4, 5.0, "n, number of documents containing term"),
            (4, 6.0, "N, total number of documents with field"),
            (
                3,
                0.45454543828964233,
                "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            ),
            (4, 1.0, "freq, occurrences of term within document"),
            (4, 1.2000000476837158, "k1, term saturation parameter"),
            (4, 0.75, "b, length normalization parameter"),
            (4, 2.0, "dl, length of field"),
            (4, 2.0, "avgdl, average length of field"),
            (1, 0.7002022862434387, "$type:string/t:negro [BM25Similarity], result of:"),
            (2, 0.7002022862434387, "score(freq=1.0), computed as boost * idf * tf from:"),
            (3, 1.54044508934021, "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:"),
            (4, 1.0, "n, number of documents containing term"),
            (4, 6.0, "N, total number of documents with field"),
            (
                3,
                0.45454543828964233,
                "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            ),
            (4, 1.0, "freq, occurrences of term within document"),
            (4, 1.2000000476837158, "k1, term saturation parameter"),
            (4, 0.75, "b, length normalization parameter"),
            (4, 2.0, "dl, length of field"),
            (4, 2.0, "avgdl, average length of field"),
        ],
    ),
    // {"text":{"path":"t","query":"gato","score":{"boost":{"value":2}}}}
    (
        5,
        &[
            (0, 0.3111768364906311, "$type:string/t:gato [BM25Similarity], result of:"),
            (1, 0.3111768364906311, "score(freq=3.0), computed as boost * idf * tf from:"),
            (2, 2.0, "boost"),
            (2, 0.24116206169128418, "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:"),
            (3, 5.0, "n, number of documents containing term"),
            (3, 6.0, "N, total number of documents with field"),
            (
                2,
                0.6451612710952759,
                "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            ),
            (3, 3.0, "freq, occurrences of term within document"),
            (3, 1.2000000476837158, "k1, term saturation parameter"),
            (3, 0.75, "b, length normalization parameter"),
            (3, 3.0, "dl, length of field"),
            (3, 2.0, "avgdl, average length of field"),
        ],
    ),
    // {"text":{"path":"t","query":"gato","score":{"constant":{"value":3}}}}
    // Every document ties: the reference search server gives its first document number first (4), here key 1.
    (1, &[(0, 3.0, "ConstantScore($type:string/t:gato) [BM25Similarity], result of:")]),
    // {"compound":{"must":[{"text":{"path":"t","query":"gato"}}],"should":[{"text":{"path":"t","query":"negro"}}],"filter":[{"exists":{"path":"n"}}]}}
    (
        1,
        &[
            (0, 0.809821367263794, "sum of:"),
            (1, 0.0, "match on required clause, product of:"),
            (2, 0.0, "# clause"),
            (2, 1.0, "$meta/fieldNames:n"),
            (1, 0.10961911082267761, "$type:string/t:gato [BM25Similarity], result of:"),
            (2, 0.10961911082267761, "score(freq=1.0), computed as boost * idf * tf from:"),
            (3, 0.24116206169128418, "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:"),
            (4, 5.0, "n, number of documents containing term"),
            (4, 6.0, "N, total number of documents with field"),
            (
                3,
                0.45454543828964233,
                "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            ),
            (4, 1.0, "freq, occurrences of term within document"),
            (4, 1.2000000476837158, "k1, term saturation parameter"),
            (4, 0.75, "b, length normalization parameter"),
            (4, 2.0, "dl, length of field"),
            (4, 2.0, "avgdl, average length of field"),
            (1, 0.7002022862434387, "$type:string/t:negro [BM25Similarity], result of:"),
            (2, 0.7002022862434387, "score(freq=1.0), computed as boost * idf * tf from:"),
            (3, 1.54044508934021, "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:"),
            (4, 1.0, "n, number of documents containing term"),
            (4, 6.0, "N, total number of documents with field"),
            (
                3,
                0.45454543828964233,
                "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            ),
            (4, 1.0, "freq, occurrences of term within document"),
            (4, 1.2000000476837158, "k1, term saturation parameter"),
            (4, 0.75, "b, length normalization parameter"),
            (4, 2.0, "dl, length of field"),
            (4, 2.0, "avgdl, average length of field"),
        ],
    ),
    // {"exists":{"path":"n"}}
    // Every document ties: the reference search server gives its first document number first (4), here key 1.
    (1, &[(0, 1.0, "ConstantScore($meta/fieldNames:n) [BM25Similarity], result of:")]),
    // {"wildcard":{"path":"t","query":"gat*","allowAnalyzedField":true}}
    // Every document ties: the reference search server gives its first document number first (4), here key 1.
    (1, &[(0, 1.0, "$type:string/t:gat*")]),
];

#[test]
fn score_details_explain_as_the_reference() {
    let ix = m14();
    let compound = Document::new()
        .with("must", Value::Array(vec![op("text", text("gato")).into()]))
        .with("should", Value::Array(vec![op("text", text("negro")).into()]))
        .with("filter", Value::Array(vec![op("exists", Document::new().with("path", "n")).into()]));
    let specs = [
        op("text", text("gato negro")),
        op(
            "text",
            text("gato")
                .with("score", Document::new().with("boost", Document::new().with("value", 2))),
        ),
        op(
            "text",
            text("gato")
                .with("score", Document::new().with("constant", Document::new().with("value", 3))),
        ),
        op("compound", compound),
        op("exists", Document::new().with("path", "n")),
        op(
            "wildcard",
            Document::new()
                .with("path", "t")
                .with("query", "gat*")
                .with("allowAnalyzedField", true),
        ),
    ];
    for (spec, (key, want)) in specs.into_iter().zip(CASES) {
        let found = ix.search(&spec.clone().with("scoreDetails", true)).expect("ran");
        let first = found.first().expect("found");
        assert_eq!(first.key, vec![key], "{spec:?}");
        let mut got = Vec::new();
        flat(first.details.as_ref().expect("details"), 0, &mut got);
        let want: Vec<(usize, f64, String)> =
            want.iter().map(|(d, v, s)| (*d, *v, (*s).to_owned())).collect();
        assert_eq!(got, want, "{spec:?}");
    }
    // Without the option, or with it off, no explanation.
    let plain = ix.search(&op("text", text("gato"))).expect("ran");
    assert!(plain.iter().all(|f| f.details.is_none()));
    let off = ix.search(&op("text", text("gato")).with("scoreDetails", false)).expect("ran");
    assert!(off.iter().all(|f| f.details.is_none()));
    let wrong = ix.meta(&op("text", text("gato")).with("scoreDetails", 1)).unwrap_err();
    assert_eq!(wrong.0, "\"scoreDetails\" must be a boolean");
}
