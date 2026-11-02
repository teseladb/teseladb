//! The scores and matches of a RefDb 9.0.4 text index measured on 02-11-2026
//! (`scratchpad/lane3m/m33.js` and `m37.js`).

use super::*;
use teseladb_bson::extjson;

fn parse(json: &str) -> Document {
    extjson::parse(json).expect("json")
}

/// The `_id` and score of each document of `docs` that `search` matches, in `_id` order.
fn run(index: &TextIndex, docs: &[Document], search: &str, s: Sensitivity) -> Vec<(i32, f64)> {
    let q = TextQuery::parse(search, index.language, s);
    docs.iter()
        .filter_map(|d| {
            let Some(Value::Int32(id)) = d.get("_id") else { panic!("{d:?}") };
            index.score(&q, d).map(|sc| (*id, sc))
        })
        .collect()
}

fn plain() -> Sensitivity {
    Sensitivity::default()
}

fn close(have: &[(i32, f64)], want: &[(i32, f64)]) -> bool {
    have.len() == want.len()
        && have.iter().zip(want).all(|(h, w)| h.0 == w.0 && (h.1 - w.1).abs() < 1e-12)
}

#[test]
fn scores_follow_counts_lengths_weights_and_arrays() {
    let index = TextIndex::from_spec(&parse(
        r#"{"weights": {"t": 1, "u": 3}, "default_language": "english", "language_override": "language"}"#,
    ))
    .expect("spec");
    let docs: Vec<Document> = [
        r#"{"_id": 1, "t": "the cat sat"}"#,
        r#"{"_id": 2, "t": "cat cat cat dog"}"#,
        r#"{"_id": 3, "t": "cats running quickly", "u": "cat"}"#,
        r#"{"_id": 4, "t": "Cat"}"#,
        r#"{"_id": 5, "t": "dog bird fish cat cat"}"#,
        r#"{"_id": 6, "t": ["cat one", "two cat cat"]}"#,
        r#"{"_id": 7, "t": "café Café CAFE cafe"}"#,
        r#"{"_id": 8, "t": "running runner runs ran"}"#,
    ]
    .iter()
    .map(|j| parse(j))
    .collect();
    let cases: Vec<(&str, Sensitivity, Vec<(i32, f64)>)> = vec![
        (
            "cat",
            plain(),
            vec![
                (1, 0.75),
                (2, 1.53125),
                (3, 3.966666666666667),
                (4, 1.1),
                (5, 1.0499999999999998),
                (6, 2.0),
            ],
        ),
        ("Cat", Sensitivity { case: true, diacritic: false }, vec![(4, 1.1)]),
        ("cafe", plain(), vec![(7, 1.875)]),
        ("café", Sensitivity { case: false, diacritic: true }, vec![(7, 1.875)]),
        ("run", plain(), vec![(3, 0.6666666666666666), (8, 1.125)]),
        (
            "cat dog",
            plain(),
            vec![(1, 0.75), (2, 2.15625), (3, 3.966666666666667), (4, 1.1), (5, 1.65), (6, 2.0)],
        ),
        ("\"cat dog\"", plain(), vec![(2, 2.15625)]),
        ("cat -dog", plain(), vec![(1, 0.75), (3, 3.966666666666667), (4, 1.1), (6, 2.0)]),
        ("the", plain(), vec![]),
    ];
    for (search, s, want) in cases {
        let have = run(&index, &docs, search, s);
        assert!(close(&have, &want), "{search}: {have:?} != {want:?}");
    }
}

#[test]
fn queries_read_terms_phrases_negations_and_sensitivity_as_measured() {
    let index = TextIndex::from_spec(&parse(
        r#"{"weights": {"t": 1, "a.b": 5, "u": 2}, "default_language": "english", "language_override": "lang"}"#,
    ))
    .expect("spec");
    let docs: Vec<Document> = [
        r#"{"_id": 1, "t": "The Quick brown fox", "l": "english"}"#,
        r#"{"_id": 2, "t": "quick-brown dog"}"#,
        r#"{"_id": 3, "t": "Café au lait, CAFÉ noir"}"#,
        r#"{"_id": 4, "t": "el perro corre rápido", "lang": "spanish"}"#,
        r#"{"_id": 5, "t": "Fox fox FOX"}"#,
        r#"{"_id": 6, "a": {"b": "nested fox"}}"#,
        r#"{"_id": 7, "t": "brown", "u": "quick fox"}"#,
        r#"{"_id": 8, "t": "don't stop"}"#,
        r#"{"_id": 9, "t": "İstanbul ışık"}"#,
        r#"{"_id": 10, "t": "12 345 v2 x1y"}"#,
    ]
    .iter()
    .map(|j| parse(j))
    .collect();
    let third = 0.6666666666666666;
    let fox = vec![(1, third), (5, 1.75), (6, 3.75), (7, 1.5)];
    let case = Sensitivity { case: true, diacritic: false };
    let diacritic = Sensitivity { case: false, diacritic: true };
    let cases: Vec<(&str, Sensitivity, Vec<(i32, f64)>)> = vec![
        ("quick", plain(), vec![(1, third), (2, third), (7, 1.5)]),
        ("\"quick brown\"", plain(), vec![(1, 1.3333333333333333)]),
        ("\"Quick brown\"", plain(), vec![(1, 1.3333333333333333)]),
        ("quick -brown", plain(), vec![]),
        ("-brown", plain(), vec![]),
        ("-\"quick brown\" fox", plain(), vec![(5, 1.75), (6, 3.75), (7, 1.5)]),
        (
            "\"quick\" \"brown\"",
            plain(),
            vec![(1, 1.3333333333333333), (2, 1.3333333333333333), (7, 2.6)],
        ),
        ("fox \"brown fox\"", plain(), vec![(1, 1.3333333333333333)]),
        ("\"brown", plain(), vec![(1, third), (2, third), (7, 1.1)]),
        ("\\\"quick\\\"", plain(), vec![]),
        (
            "quick\\-brown",
            plain(),
            vec![(1, 1.3333333333333333), (2, 1.3333333333333333), (7, 2.6)],
        ),
        ("--quick", plain(), vec![]),
        ("- quick", plain(), vec![(1, third), (2, third), (7, 1.5)]),
        ("\"\" fox", plain(), fox.clone()),
        ("fox -", plain(), fox.clone()),
        ("cafe", plain(), vec![(3, 1.0499999999999998)]),
        ("CAFE", plain(), vec![(3, 1.0499999999999998)]),
        ("café", plain(), vec![(3, 1.0499999999999998)]),
        ("perro", plain(), vec![]),
        ("corre", plain(), vec![(4, third)]),
        ("correr", plain(), vec![]),
        ("nested", plain(), vec![(6, 3.75)]),
        ("istanbul", plain(), vec![]),
        ("İstanbul", plain(), vec![(9, 0.75)]),
        ("ışık", plain(), vec![(9, 0.75)]),
        ("isik", plain(), vec![]),
        ("12", plain(), vec![(10, 0.625)]),
        ("x1y", plain(), vec![(10, 0.625)]),
        ("don't", plain(), vec![]),
        ("dont", plain(), vec![]),
        ("stop", plain(), vec![(8, 1.0)]),
        ("\"don't stop\"", plain(), vec![(8, 1.0)]),
        ("fox fox", plain(), fox.clone()),
        ("FOX", plain(), fox.clone()),
        ("Fox", case, vec![(5, 1.75)]),
        ("FOX", case, vec![(5, 1.75)]),
        ("fox", case, fox.clone()),
        ("\"Fox fox\"", case, vec![(5, 1.75)]),
        ("quick", case, vec![(2, third), (7, 1.5)]),
        ("cafe", diacritic, vec![]),
        ("café", diacritic, vec![(3, 1.0499999999999998)]),
        ("CAFÉ", diacritic, vec![(3, 1.0499999999999998)]),
        ("\"café au\"", diacritic, vec![(3, 1.65)]),
        ("\"cafe au\"", diacritic, vec![]),
    ];
    for (search, s, want) in cases {
        let have = run(&index, &docs, search, s);
        assert!(close(&have, &want), "{search} {s:?}: {have:?} != {want:?}");
    }
    // `$language` changes how the query reads, not the documents.
    let spanish = Language::named("spanish").expect("spanish");
    let q = TextQuery::parse("corre", spanish, plain());
    assert_eq!(index.score(&q, &docs[3]), Some(third));
}

#[test]
fn unknown_languages_are_refused() {
    assert_eq!(
        Language::named("xx"),
        Err(TextError {
            code: 2,
            message: "unsupported language: \"xx\" for text index version 3".to_owned()
        })
    );
    assert_eq!(Language::named("none").map(Language::name), Ok("none"));
    let bad = parse(r#"{"weights": {"t": 1}, "default_language": "xx"}"#);
    assert!(TextIndex::from_spec(&bad).is_err());
}
