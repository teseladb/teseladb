//! Custom analyzers out of their shape are refused with the message of the reference search
//! server (011 FR-006, FR-007): one case per check of `definition/analyzers.rs`, each with the
//! path of the attribute and what is wrong with it.

use teseladb_bson::extjson;
use teseladb_search::definition::Definition;

/// A definition with dynamic mappings and the analyzers of `analyzers` (a JSON array).
fn definition(analyzers: &str) -> String {
    format!(r#"{{"mappings": {{"dynamic": true}}, "analyzers": {analyzers}}}"#)
}

/// An analyzer named `a` with the standard tokenizer and the token filter `filter`.
fn with_filter(filter: &str) -> String {
    definition(&format!(
        r#"[{{"name": "a", "tokenizer": {{"type": "standard"}}, "tokenFilters": [{filter}]}}]"#
    ))
}

/// An analyzer named `a` with the tokenizer `tokenizer`.
fn with_tokenizer(tokenizer: &str) -> String {
    definition(&format!(r#"[{{"name": "a", "tokenizer": {tokenizer}}}]"#))
}

/// An analyzer named `a` with the standard tokenizer and the character filter `filter`.
fn with_char_filter(filter: &str) -> String {
    definition(&format!(
        r#"[{{"name": "a", "tokenizer": {{"type": "standard"}}, "charFilters": [{filter}]}}]"#
    ))
}

fn message(json: &str) -> String {
    let doc = extjson::parse(json).expect("JSON");
    match Definition::parse(&doc) {
        Ok(_) => "accepted".to_owned(),
        Err(e) => e.message(""),
    }
}

const T: &str = "analyzers[0].tokenizer";
const F: &str = "analyzers[0].tokenFilters[0]";
const C: &str = "analyzers[0].charFilters[0]";

#[test]
fn malformed_analyzers_are_refused_with_their_path() {
    let cases: Vec<(String, String)> = vec![
        (definition("1"), r#""analyzers" must be a array"#.into()),
        (
            definition(
                r#"[{"name": "a", "tokenizer": {"type": "keyword"}},
                   {"name": "a", "tokenizer": {"type": "keyword"}}]"#,
            ),
            r#""analyzers" cannot contain elements with the same name"#.into(),
        ),
        (
            definition(r#"[{"tokenizer": {"type": "keyword"}}]"#),
            r#""analyzers[0].name" is required"#.into(),
        ),
        (definition(r#"[{"name": "a"}]"#), format!(r#""{T}" is required"#)),
        (definition(r#"[{"name": "a", "tokenizer": 1}]"#), format!(r#""{T}" must be a document"#)),
        (
            definition(r#"[{"name": "a", "tokenizer": {"type": "keyword"}, "tokenFilters": 1}]"#),
            r#""analyzers[0].tokenFilters" must be a array"#.into(),
        ),
        (
            definition(r#"[{"name": "a", "tokenizer": {"type": "keyword"}, "other": 1}]"#),
            r#""analyzers[0]" unrecognized field "other""#.into(),
        ),
        (with_tokenizer("{}"), format!(r#""{T}.type" is required"#)),
        (
            with_tokenizer(r#"{"type": "standard", "maxTokenLength": 1.5}"#),
            format!(r#""{T}.maxTokenLength" must be an integer"#),
        ),
        (
            with_tokenizer(r#"{"type": "standard", "maxTokenLength": "x"}"#),
            format!(r#""{T}.maxTokenLength" must be a integer"#),
        ),
        (
            with_tokenizer(r#"{"type": "standard", "maxTokenLength": 3000000000}"#),
            format!(r#""{T}.maxTokenLength" overflowed, must fit in a 32 bit integer"#),
        ),
        (
            with_tokenizer(r#"{"type": "standard", "maxTokenLength": -3000000000}"#),
            format!(r#""{T}.maxTokenLength" underflowed, must fit in a 32 bit integer"#),
        ),
        (
            with_tokenizer(r#"{"type": "standard", "maxTokenLength": 0}"#),
            format!(r#""{T}.maxTokenLength" must be within bounds [1..1048576]"#),
        ),
        (
            with_tokenizer(r#"{"type": "keyword", "x": 1}"#),
            format!(r#""{T}" unrecognized field "x""#),
        ),
        (
            with_tokenizer(r#"{"type": "nGram", "minGram": 3, "maxGram": 2}"#),
            format!(r#""{T}" minGram must not be greater than maxGram"#),
        ),
        (
            with_tokenizer(r#"{"type": "edgeGram", "minGram": -1, "maxGram": 2}"#),
            format!(r#""{T}.minGram" cannot be negative"#),
        ),
        (
            with_tokenizer(r#"{"type": "edgeGram", "maxGram": 2}"#),
            format!(r#""{T}.minGram" is required"#),
        ),
        (with_tokenizer(r#"{"type": "regexSplit"}"#), format!(r#""{T}.pattern" is required"#)),
        (
            with_tokenizer(r#"{"type": "regexCaptureGroup", "pattern": "(a)", "group": 2}"#),
            format!(r#""{T}" invalid group specified: pattern only has: 1 capturing groups"#),
        ),
        (
            with_tokenizer(r#"{"type": "regexCaptureGroup", "pattern": "a", "group": -1}"#),
            format!(r#""{T}.group" cannot be negative"#),
        ),
        (with_char_filter(r#"{"type": "mapping"}"#), format!(r#""{C}.mappings" is required"#)),
        (
            with_char_filter(r#"{"type": "mapping", "mappings": 1}"#),
            format!(r#""{C}.mappings" must be a document"#),
        ),
        (
            with_char_filter(r#"{"type": "mapping", "mappings": {"a": 1}}"#),
            format!(r#""{C}.mappings.a" must be a string"#),
        ),
        (
            with_char_filter(r#"{"type": "mapping", "mappings": {}}"#),
            format!(r#""{C}.mappings" cannot be empty"#),
        ),
        (
            with_char_filter(r#"{"type": "mapping", "mappings": {"": "b"}}"#),
            format!(r#""{C}.mappings" cannot contain the empty string as a key"#),
        ),
        (
            with_char_filter(r#"{"type": "htmlStrip", "ignoredTags": [1]}"#),
            format!(r#""{C}.ignoredTags[0]" must be a string"#),
        ),
        (
            with_char_filter(r#"{"type": "htmlStrip", "ignoredTags": "b"}"#),
            format!(r#""{C}.ignoredTags" must be a array"#),
        ),
        (
            with_char_filter(r#"{"type": "persian", "x": 1}"#),
            format!(r#""{C}" unrecognized field "x""#),
        ),
        (
            with_char_filter(r#"{"type": "icuNormalize", "x": 1}"#),
            format!(r#""{C}" unrecognized field "x""#),
        ),
        (
            with_filter(r#"{"type": "asciiFolding", "originalTokens": "maybe"}"#),
            format!(r#""{F}.originalTokens" must be one of [include, omit]"#),
        ),
        (
            with_filter(r#"{"type": "nGram", "minGram": 3, "maxGram": 2}"#),
            format!(r#""{F}" minGram must not be greater than maxGram"#),
        ),
        (
            with_filter(r#"{"type": "length", "min": 5, "max": 2}"#),
            format!(r#""{F}" min must not be greater than max"#),
        ),
        (
            with_filter(r#"{"type": "limitTokenCount", "maxTokenCount": 0}"#),
            format!(r#""{F}.maxTokenCount" must be positive"#),
        ),
        (
            with_filter(r#"{"type": "regex", "pattern": "a", "replacement": "b"}"#),
            format!(r#""{F}.matches" is required"#),
        ),
        (
            with_filter(r#"{"type": "shingle", "minShingleSize": 1, "maxShingleSize": 2}"#),
            format!(r#""{F}.minShingleSize" must be within bounds [2..2147483647]"#),
        ),
        (
            with_filter(r#"{"type": "shingle", "minShingleSize": 3, "maxShingleSize": 2}"#),
            format!(r#""{F}" minShingleSize must not be greater than maxShingleSize"#),
        ),
        (with_filter(r#"{"type": "stopword"}"#), format!(r#""{F}.tokens" is required"#)),
        (
            with_filter(r#"{"type": "stopword", "tokens": []}"#),
            format!(r#""{F}.tokens" cannot be empty"#),
        ),
        (
            with_filter(r#"{"type": "wordDelimiterGraph", "protectedWords": 1}"#),
            format!(r#""{F}.protectedWords" must be a document"#),
        ),
        (
            with_filter(r#"{"type": "wordDelimiterGraph", "protectedWords": {}}"#),
            format!(r#""{F}.protectedWords.words" is required"#),
        ),
        (
            with_filter(r#"{"type": "wordDelimiterGraph", "delimiterOptions": 1}"#),
            format!(r#""{F}.delimiterOptions" must be a document"#),
        ),
        (
            with_filter(r#"{"type": "wordDelimiterGraph", "delimiterOptions": {"x": true}}"#),
            format!(r#""{F}.delimiterOptions" unrecognized field "x""#),
        ),
        (
            with_filter(r#"{"type": "lowercase", "x": 1}"#),
            format!(r#""{F}" unrecognized field "x""#),
        ),
    ];
    let mut wrong = Vec::new();
    for (json, want) in &cases {
        let got = message(json);
        if &got != want {
            wrong.push(format!("{json}\n  want {want}\n  got  {got}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn well_formed_components_are_accepted() {
    for json in [
        with_filter(
            r#"{"type": "wordDelimiterGraph", "protectedWords": {"words": ["Wi-Fi"], "ignoreCase": false},
                "delimiterOptions": {"concatenateAll": true, "preserveOriginal": true}}"#,
        ),
        with_filter(r#"{"type": "stopword", "tokens": ["The"], "ignoreCase": true}"#),
        with_filter(
            r#"{"type": "regex", "pattern": "a+", "replacement": "b", "matches": "first"}"#,
        ),
        with_filter(r#"{"type": "icuNormalizer", "normalizationForm": "nfkd"}"#),
        with_filter(
            r#"{"type": "edgeGram", "minGram": 1, "maxGram": 3, "termNotInBounds": "include"}"#,
        ),
        with_filter(r#"{"type": "snowballStemming", "stemmerName": "french"}"#),
        with_tokenizer(r#"{"type": "regexCaptureGroup", "pattern": "(a)(b)", "group": 2}"#),
        with_tokenizer(r#"{"type": "uaxUrlEmail", "maxTokenLength": 10}"#),
        with_char_filter(r#"{"type": "htmlStrip", "ignoredTags": ["B"]}"#),
        with_char_filter(r#"{"type": "mapping", "mappings": {"a": "b"}}"#),
    ] {
        assert_eq!(message(&json), "accepted", "{json}");
    }
}
