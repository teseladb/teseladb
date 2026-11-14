//! The `parsedQuery` of `explain` (010 FR-003): each filter of `tests/parsed/explain.out`, a tab
//! and what a reference router 9.0.4 wrote back for it on 01-11-2026, in canonical extended JSON:
//! the clauses the reference server settles or joins, `$in` in order, and `$jsonSchema` in the
//! reference server's internal operators.

use teseladb_bson::{Document, RawDoc, extjson};
use teseladb_mql::{Filter, FilterUse};

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

#[test]
fn parsed_queries_match_the_reference() {
    let mut wrong = Vec::new();
    for line in teseladb_names::expand_static(include_str!("parsed/explain.out"))
        .lines()
        .filter(|l| !l.is_empty())
    {
        let (filter, want) = line.split_once('\t').expect("a filter and its parsed form");
        let bytes = extjson::parse(filter).expect("a filter").to_bytes().expect("small");
        let parsed =
            Filter::parse(RawDoc::from_bytes(&bytes).expect("a document"), FilterUse::Query)
                .expect("a valid filter");
        let got = canonical(&parsed.parsed_query());
        let want = canonical(&extjson::parse(want).expect("a parsed form"));
        if got != want {
            wrong.push(format!("{filter}\n   got  {got}\n   want {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}

/// Clauses the reference server joins or translates beyond the measured filters.
#[test]
fn joined_and_translated_clauses() {
    for (filter, want) in [
        (
            r#"{"$or":[{"a":1},{"b":2},{"a":3},{"c":{"$regex":"x","$options":"i"}}]}"#,
            r#"{"$or":[{"b":{"$eq":2}},{"c":{"$regex":"x","$options":"i"}},{"a":{"$in":[1,3]}}]}"#,
        ),
        (
            r#"{"a":{"$in":[{"$regularExpression":{"pattern":"x","options":"i"}}]}}"#,
            r#"{"a":{"$regex":"x","$options":"i"}}"#,
        ),
        (
            r#"{"$jsonSchema":{"properties":{"a":{"bsonType":"string","minimum":1}}}}"#,
            r#"{"$or":[{"a":{"$not":{"$exists":true}}},{"a":{"$_internalSchemaType":[2]}}]}"#,
        ),
        (
            r#"{"$jsonSchema":{"properties":{"a":{"type":"number"}}}}"#,
            r#"{"$or":[{"a":{"$not":{"$exists":true}}},{"a":{"$_internalSchemaType":["number"]}}]}"#,
        ),
        (
            r#"{"$jsonSchema":{"additionalProperties":false,"patternProperties":{"^x":{}}}}"#,
            r#"{"$_internalSchemaAllowedProperties":{"properties":[],"namePlaceholder":"i","patternProperties":[{"regex":{"$regularExpression":{"pattern":"^x","options":""}},"expression":{"$alwaysTrue":1}}],"otherwise":{"$alwaysFalse":1}}}"#,
        ),
    ] {
        let bytes = extjson::parse(filter).expect("a filter").to_bytes().expect("small");
        let parsed =
            Filter::parse(RawDoc::from_bytes(&bytes).expect("a document"), FilterUse::Query)
                .expect("a valid filter");
        assert_eq!(
            canonical(&parsed.parsed_query()),
            canonical(&extjson::parse(want).expect("form")),
            "{filter}"
        );
    }
}
