//! `$jsonSchema`: the reference server's keywords and `bsonType`, the parse errors and the
//! explanation of a failed validation (006 T017, FR-009, FR-022).

pub mod common;

use common::{bytes, canon, json, raw};
use teseladb_mql::codes;
use teseladb_mql::jsonschema::Schema;

fn schema(text: &str) -> Schema {
    let b = bytes(text);
    Schema::parse(raw(&b)).unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// `(schema, document, valid)`.
fn check(cases: &[(&str, &str, bool)]) {
    for &(s, d, want) in cases {
        let b = bytes(d);
        let parsed = schema(s);
        assert_eq!(parsed.matches(raw(&b)).unwrap(), want, "{s} on {d}");
        assert_eq!(parsed.explain(raw(&b)).unwrap().is_none(), want, "{s} on {d}");
    }
}

#[test]
fn types() {
    check(&[
        (r#"{"bsonType": "object"}"#, "{}", true),
        (r#"{"properties": {"a": {"bsonType": "int"}}}"#, r#"{"a": 1}"#, true),
        (r#"{"properties": {"a": {"bsonType": "int"}}}"#, r#"{"a": 1.5}"#, false),
        (r#"{"properties": {"a": {"bsonType": ["int", "string"]}}}"#, r#"{"a": "x"}"#, true),
        (r#"{"properties": {"a": {"bsonType": "number"}}}"#, r#"{"a": {"$numberInt": "2"}}"#, true),
        (r#"{"properties": {"a": {"type": "number"}}}"#, r#"{"a": 2.5}"#, true),
        (r#"{"properties": {"a": {"type": ["string", "null"]}}}"#, r#"{"a": null}"#, true),
        (r#"{"properties": {"a": {"type": "boolean"}}}"#, r#"{"a": 1}"#, false),
        (r#"{"properties": {"a": {"type": "object"}}}"#, r#"{"a": {}}"#, true),
        (r#"{"properties": {"a": {"type": "array"}}}"#, r#"{"a": []}"#, true),
        (r#"{"properties": {"a": {"enum": [1, "x", [1]]}}}"#, r#"{"a": [1]}"#, true),
        (r#"{"properties": {"a": {"enum": [1, "x"]}}}"#, r#"{"a": 2}"#, false),
    ]);
}

#[test]
fn objects() {
    check(&[
        (r#"{"required": ["a", "b"]}"#, r#"{"a": 1, "b": null}"#, true),
        (r#"{"required": ["a", "b"]}"#, r#"{"a": 1}"#, false),
        (r#"{"properties": {"a": {"required": ["x"]}}}"#, r#"{"a": 5}"#, true),
        (r#"{"properties": {"a": {"minimum": 0}}}"#, r#"{"b": -1}"#, true),
        (r#"{"additionalProperties": false, "properties": {"a": {}}}"#, r#"{"a": 1}"#, true),
        (
            r#"{"additionalProperties": false, "properties": {"a": {}}}"#,
            r#"{"a": 1, "b": 2}"#,
            false,
        ),
        (
            r#"{"additionalProperties": false, "patternProperties": {"^x": {}}}"#,
            r#"{"xy": 1}"#,
            true,
        ),
        (r#"{"additionalProperties": true}"#, r#"{"b": 2}"#, true),
        (r#"{"additionalProperties": {"bsonType": "int"}}"#, r#"{"b": 2}"#, true),
        (r#"{"additionalProperties": {"bsonType": "int"}}"#, r#"{"b": "2"}"#, false),
        (r#"{"patternProperties": {"^n": {"bsonType": "int"}}}"#, r#"{"n1": 1, "m": "x"}"#, true),
        (r#"{"patternProperties": {"^n": {"bsonType": "int"}}}"#, r#"{"n1": "x"}"#, false),
        (r#"{"minProperties": 2}"#, r#"{"a": 1}"#, false),
        (r#"{"maxProperties": 1}"#, r#"{"a": 1}"#, true),
        (r#"{"maxProperties": 1}"#, r#"{"a": 1, "b": 2}"#, false),
        (r#"{"dependencies": {"a": ["b"]}}"#, r#"{"a": 1}"#, false),
        (r#"{"dependencies": {"a": ["b"]}}"#, r#"{"a": 1, "b": 1}"#, true),
        (r#"{"dependencies": {"a": ["b"]}}"#, r#"{"c": 1}"#, true),
        (r#"{"dependencies": {"a": {"required": ["c"]}}}"#, r#"{"a": 1}"#, false),
        (r#"{"dependencies": {"a": {"required": ["c"]}}}"#, r#"{"a": 1, "c": 2}"#, true),
    ]);
}

#[test]
fn arrays_numbers_and_strings() {
    check(&[
        (r#"{"properties": {"a": {"items": {"bsonType": "int"}}}}"#, r#"{"a": [1, 2]}"#, true),
        (r#"{"properties": {"a": {"items": {"bsonType": "int"}}}}"#, r#"{"a": [1, "x"]}"#, false),
        (
            r#"{"properties": {"a": {"items": [{"bsonType": "int"}, {"bsonType": "string"}]}}}"#,
            r#"{"a": [1, "x", true]}"#,
            true,
        ),
        (
            r#"{"properties": {"a": {"items": [{"bsonType": "int"}], "additionalItems": false}}}"#,
            r#"{"a": [1, 2]}"#,
            false,
        ),
        (
            r#"{"properties": {"a": {"items": [{"bsonType": "int"}], "additionalItems": {"bsonType": "string"}}}}"#,
            r#"{"a": [1, "x"]}"#,
            true,
        ),
        (
            r#"{"properties": {"a": {"items": [{"bsonType": "int"}], "additionalItems": {"bsonType": "string"}}}}"#,
            r#"{"a": [1, 2]}"#,
            false,
        ),
        (
            r#"{"properties": {"a": {"items": [{"bsonType": "int"}], "additionalItems": true}}}"#,
            r#"{"a": [1, 2]}"#,
            true,
        ),
        (r#"{"properties": {"a": {"minItems": 2}}}"#, r#"{"a": [1]}"#, false),
        (r#"{"properties": {"a": {"maxItems": 1}}}"#, r#"{"a": [1]}"#, true),
        (r#"{"properties": {"a": {"maxItems": 1}}}"#, r#"{"a": [1, 2]}"#, false),
        (r#"{"properties": {"a": {"uniqueItems": true}}}"#, r#"{"a": [1, 1.0]}"#, false),
        (r#"{"properties": {"a": {"uniqueItems": true}}}"#, r#"{"a": [1, 2]}"#, true),
        (r#"{"properties": {"a": {"uniqueItems": false}}}"#, r#"{"a": [1, 1]}"#, true),
        (r#"{"properties": {"a": {"minimum": 5}}}"#, r#"{"a": 5}"#, true),
        (
            r#"{"properties": {"a": {"minimum": 5, "exclusiveMinimum": true}}}"#,
            r#"{"a": 5}"#,
            false,
        ),
        (r#"{"properties": {"a": {"maximum": 5}}}"#, r#"{"a": 6}"#, false),
        (
            r#"{"properties": {"a": {"maximum": 5, "exclusiveMaximum": true}}}"#,
            r#"{"a": 4.9}"#,
            true,
        ),
        (
            r#"{"properties": {"a": {"maximum": 5, "exclusiveMaximum": false}}}"#,
            r#"{"a": 5}"#,
            true,
        ),
        (r#"{"properties": {"a": {"minimum": 5}}}"#, r#"{"a": "x"}"#, true),
        (r#"{"properties": {"a": {"minimum": 5}}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, false),
        (r#"{"properties": {"a": {"multipleOf": 3}}}"#, r#"{"a": 9}"#, true),
        (r#"{"properties": {"a": {"multipleOf": 3}}}"#, r#"{"a": 10}"#, false),
        (r#"{"properties": {"a": {"multipleOf": 0.5}}}"#, r#"{"a": 2.5}"#, true),
        (
            r#"{"properties": {"a": {"multipleOf": 0.5}}}"#,
            r#"{"a": {"$numberDouble": "Infinity"}}"#,
            false,
        ),
        (r#"{"properties": {"a": {"multipleOf": 2}}}"#, r#"{"a": "x"}"#, true),
        (r#"{"properties": {"a": {"minLength": 2}}}"#, r#"{"a": "ñ"}"#, false),
        (r#"{"properties": {"a": {"maxLength": 2}}}"#, r#"{"a": "ññ"}"#, true),
        (r#"{"properties": {"a": {"maxLength": 2}}}"#, r#"{"a": 123}"#, true),
        (r#"{"properties": {"a": {"pattern": "^x"}}}"#, r#"{"a": "xy"}"#, true),
        (r#"{"properties": {"a": {"pattern": "^x"}}}"#, r#"{"a": "y"}"#, false),
        (r#"{"properties": {"a": {"pattern": "^x"}}}"#, r#"{"a": 1}"#, true),
    ]);
}

#[test]
fn combinations_and_encryption() {
    check(&[
        (r#"{"allOf": [{"required": ["a"]}, {"required": ["b"]}]}"#, r#"{"a": 1}"#, false),
        (r#"{"allOf": [{"required": ["a"]}, {"required": ["b"]}]}"#, r#"{"a": 1, "b": 1}"#, true),
        (r#"{"anyOf": [{"required": ["a"]}, {"required": ["b"]}]}"#, r#"{"b": 1}"#, true),
        (r#"{"anyOf": [{"required": ["a"]}, {"required": ["b"]}]}"#, r#"{"c": 1}"#, false),
        (r#"{"oneOf": [{"required": ["a"]}, {"required": ["b"]}]}"#, r#"{"a": 1, "b": 1}"#, false),
        (r#"{"oneOf": [{"required": ["a"]}, {"required": ["b"]}]}"#, r#"{"a": 1}"#, true),
        (r#"{"oneOf": [{"required": ["a"]}, {"required": ["b"]}]}"#, r#"{"c": 1}"#, false),
        (r#"{"not": {"required": ["a"]}}"#, r#"{"a": 1}"#, false),
        (r#"{"not": {"required": ["a"]}}"#, r#"{"b": 1}"#, true),
        (
            r#"{"properties": {"s": {"encrypt": {}}}}"#,
            r#"{"s": {"$binary": {"base64": "AA==", "subType": "06"}}}"#,
            true,
        ),
        (
            r#"{"properties": {"s": {"encrypt": {"bsonType": ["string", "int"]}}}}"#,
            r#"{"s": {"$binary": {"base64": "AWtra2tra2tra2tra2tra2sCeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
            true,
        ),
        (
            r#"{"properties": {"s": {"encrypt": {"bsonType": ["string", "int"]}}}}"#,
            r#"{"s": {"$binary": {"base64": "Amtra2tra2tra2tra2tra2sCeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
            true,
        ),
        (
            r#"{"properties": {"s": {"encrypt": {"bsonType": ["string", "int"]}}}}"#,
            r#"{"s": {"$binary": {"base64": "EGtra2tra2tra2tra2tra2sCeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
            false,
        ),
        (
            r#"{"properties": {"s": {"encrypt": {"bsonType": ["string", "int"]}}}}"#,
            r#"{"s": {"$binary": {"base64": "AWtra2tra2tra2tra2tra2tjeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
            false,
        ),
        (
            r#"{"properties": {"s": {"encrypt": {}}}}"#,
            r#"{"s": {"$binary": {"base64": "EGtra2tra2tra2tra2tra2tjeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
            true,
        ),
        (
            r#"{"properties": {"s": {"encrypt": {"bsonType": "string"}}}, "encryptMetadata": {}}"#,
            r#"{"s": "plain"}"#,
            false,
        ),
        (
            r#"{"title": "t", "description": "d", "properties": {"a": {"title": "x", "description": "y"}}}"#,
            r#"{"a": 1}"#,
            true,
        ),
    ]);
}

#[test]
fn explanations_follow_errinfo() {
    let s = schema(
        r#"{"title": "Contacts", "required": ["phone"], "properties": {
            "email": {"bsonType": "string", "pattern": "@example\\.com$", "description": "must end in example.com"},
            "age": {"bsonType": "int", "minimum": 0}}}"#,
    );
    let d = bytes(r#"{"email": "amanda@xyz.com", "age": "x"}"#);
    let details = s.explain(raw(&d)).unwrap().unwrap();
    assert_eq!(
        json(&details),
        canon(
            // `properties` before `required`, whatever the schema's order (measured).
            r#"{"operatorName": "$jsonSchema", "title": "Contacts", "schemaRulesNotSatisfied": [
                {"operatorName": "properties", "propertiesNotSatisfied": [
                    {"propertyName": "email", "description": "must end in example.com", "details": [
                        {"operatorName": "pattern", "specifiedAs": {"pattern": "@example\\.com$"},
                         "reason": "regular expression did not match", "consideredValue": "amanda@xyz.com"}]},
                    {"propertyName": "age", "details": [
                        {"operatorName": "bsonType", "specifiedAs": {"bsonType": "int"},
                         "reason": "type did not match", "consideredValue": "x", "consideredType": "string"}]}]},
                {"operatorName": "required", "specifiedAs": {"required": ["phone"]}, "missingProperties": ["phone"]}]}"#
        )
    );
}

#[test]
fn explanations_of_every_rule() {
    let explain = |s: &str, d: &str| {
        let b = bytes(d);
        json(&schema(s).explain(raw(&b)).unwrap().unwrap())
    };
    let has = |s: &str, d: &str, part: &str| {
        let text = explain(s, d);
        assert!(text.contains(part), "{s} on {d}: {text} lacks {part}");
    };
    has(r#"{"additionalProperties": false}"#, r#"{"b": 1}"#, r#""additionalProperties":["b"]"#);
    has(
        r#"{"additionalProperties": {"bsonType": "int"}}"#,
        r#"{"b": "x"}"#,
        r#""failingProperty":"b""#,
    );
    has(
        r#"{"patternProperties": {"^n": {"bsonType": "int"}}}"#,
        r#"{"n": "x"}"#,
        r#""regexMatched":"^n""#,
    );
    has(r#"{"minProperties": 2}"#, "{}", r#""numberOfProperties":{"$numberInt":"0"}"#);
    has(
        r#"{"dependencies": {"a": ["b"]}}"#,
        r#"{"a": 1}"#,
        r#""conditionalProperty":"a","missingProperties":["b"]"#,
    );
    has(
        r#"{"dependencies": {"a": {"title": "T", "required": ["c"]}}}"#,
        r#"{"a": 1}"#,
        r#""title":"T""#,
    );
    has(
        r#"{"properties": {"a": {"items": {"bsonType": "int"}}}}"#,
        r#"{"a": ["x"]}"#,
        r#""itemIndex":{"$numberInt":"0"}"#,
    );
    has(
        r#"{"properties": {"a": {"items": [{}], "additionalItems": false}}}"#,
        r#"{"a": [1, 2]}"#,
        "found additional items",
    );
    has(
        r#"{"properties": {"a": {"uniqueItems": true}}}"#,
        r#"{"a": [1, 1]}"#,
        r#""duplicatedValue":{"$numberInt":"1"}"#,
    );
    has(
        r#"{"properties": {"a": {"minItems": 3}}}"#,
        r#"{"a": [1]}"#,
        "array did not match specified length",
    );
    has(r#"{"properties": {"a": {"minimum": 3}}}"#, r#"{"a": 1}"#, "comparison failed");
    has(r#"{"properties": {"a": {"multipleOf": 3}}}"#, r#"{"a": 1}"#, "not a multiple");
    has(
        r#"{"properties": {"a": {"maxLength": 0}}}"#,
        r#"{"a": "x"}"#,
        "specified string length was not satisfied",
    );
    has(r#"{"properties": {"a": {"enum": [2]}}}"#, r#"{"a": 1}"#, "value was not found in enum");
    has(
        r#"{"allOf": [{"required": ["a"]}]}"#,
        "{}",
        r#""schemasNotSatisfied":[{"index":{"$numberInt":"0"}"#,
    );
    has(r#"{"anyOf": [{"required": ["a"]}]}"#, "{}", "schemasNotSatisfied");
    has(r#"{"oneOf": [{"required": ["a"]}]}"#, "{}", "schemasNotSatisfied");
    has(
        r#"{"oneOf": [{}, {}]}"#,
        "{}",
        r#""matchingSchemaIndexes":[{"$numberInt":"0"},{"$numberInt":"1"}]"#,
    );
    has(r#"{"not": {}}"#, "{}", "child expression matched");
    has(r#"{"properties": {"s": {"encrypt": {}}}}"#, r#"{"s": 1}"#, "value was not encrypted");
    // With `bsonType`, a CSFLE value (subtype 1 or 2) whose original type is one of them (measured
    // on a reference replica set member 9.0.4).
    let wrong = "encrypted value has wrong type";
    let typed = r#"{"properties": {"s": {"encrypt": {"bsonType": "string"}}}}"#;
    has(
        typed,
        r#"{"s": {"$binary": {"base64": "AGtra2tra2tra2tra2tra2sCeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
        wrong,
    );
    has(
        typed,
        r#"{"s": {"$binary": {"base64": "EGtra2tra2tra2tra2tra2sCeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
        wrong,
    );
    has(
        typed,
        r#"{"s": {"$binary": {"base64": "AWtra2tra2tra2tra2tra2sQeHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHg=", "subType": "06"}}}"#,
        wrong,
    );
    has(
        typed,
        r#"{"s": {"$binary": {"base64": "AWtra2tra2tra2tra2tra2s=", "subType": "06"}}}"#,
        wrong,
    );

    has(r#"{"type": "array"}"#, "{}", r#""consideredType":"object""#);
}

#[test]
fn malformed_schemas() {
    let cases: &[(&str, i32, &str)] = &[
        (r#"{"$ref": "x"}"#, 9, "$jsonSchema keyword '$ref' is not currently supported"),
        (r#"{"foo": 1}"#, 9, "Unknown $jsonSchema keyword: foo"),
        (
            r#"{"type": "object", "bsonType": "object"}"#,
            9,
            "Cannot specify both $jsonSchema keywords 'type' and 'bsonType'",
        ),
        (r#"{"type": "integer"}"#, 9, "$jsonSchema type 'integer' is not currently supported."),
        (r#"{"type": "nope"}"#, 2, "Unknown type name alias: nope"),
        (
            r#"{"type": 1}"#,
            14,
            "$jsonSchema keyword 'type' must be a string or an array of strings",
        ),
        (r#"{"type": []}"#, 9, "$jsonSchema keyword 'type' must match at least one type"),
        (r#"{"bsonType": "nope"}"#, 2, "Unknown type name alias: nope"),
        (
            r#"{"bsonType": 1}"#,
            14,
            "$jsonSchema keyword 'bsonType' must be a string or an array of strings",
        ),
        (r#"{"bsonType": []}"#, 9, "$jsonSchema keyword 'bsonType' must match at least one type"),
        (r#"{"title": 1}"#, 14, "$jsonSchema keyword 'title' must be a string"),
        (r#"{"enum": 1}"#, 14, "$jsonSchema keyword 'enum' must be an array"),
        (r#"{"enum": []}"#, 9, "$jsonSchema keyword 'enum' cannot be an empty array"),
        (r#"{"required": 1}"#, 14, "$jsonSchema keyword 'required' must be an array"),
        (r#"{"required": [1]}"#, 14, "$jsonSchema keyword 'required' must contain only strings"),
        (
            r#"{"required": ["a", "a"]}"#,
            9,
            "$jsonSchema keyword 'required' array cannot contain duplicate values",
        ),
        (r#"{"required": []}"#, 2, "$jsonSchema keyword 'required' cannot be an empty array"),
        (r#"{"properties": 1}"#, 14, "$jsonSchema keyword 'properties' must be an object"),
        (r#"{"properties": {"a": 1}}"#, 14, "$jsonSchema keyword 'properties' must be an object"),
        (
            r#"{"additionalProperties": 1}"#,
            14,
            "$jsonSchema keyword 'additionalProperties' must be either an object or a boolean",
        ),
        (
            r#"{"additionalItems": 1}"#,
            14,
            "$jsonSchema keyword 'additionalItems' must be either an object or a boolean",
        ),
        (
            r#"{"patternProperties": {"(": {}}}"#,
            51091,
            "Regular expression is invalid: missing closing parenthesis",
        ),
        (r#"{"minProperties": "1"}"#, 14, "$jsonSchema keyword 'minProperties' must be a number"),
        (
            r#"{"minItems": -1}"#,
            9,
            "$jsonSchema keyword 'minItems' must be a representable non-negative integer",
        ),
        (
            r#"{"dependencies": {"a": 1}}"#,
            14,
            "property dependencies must be an object or an array",
        ),
        (r#"{"items": 1}"#, 14, "$jsonSchema keyword 'items' must be an array or an object"),
        (r#"{"uniqueItems": 1}"#, 14, "$jsonSchema keyword 'uniqueItems' must be a boolean"),
        (r#"{"minimum": "x"}"#, 14, "$jsonSchema keyword 'minimum' must be a number"),
        (
            r#"{"exclusiveMinimum": true}"#,
            9,
            "$jsonSchema keyword 'minimum' must be a present if exclusiveMinimum is present",
        ),
        (
            r#"{"maximum": 1, "exclusiveMaximum": 1}"#,
            14,
            "$jsonSchema keyword 'exclusiveMaximum' must be a boolean",
        ),
        (r#"{"multipleOf": 0}"#, 9, "$jsonSchema keyword 'multipleOf' must have a positive value"),
        (r#"{"pattern": 1}"#, 14, "$jsonSchema keyword 'pattern' must be a string"),
        (r#"{"allOf": 1}"#, 14, "$jsonSchema keyword 'allOf' must be an array"),
        (r#"{"anyOf": []}"#, 2, "$jsonSchema keyword 'anyOf' must be a non-empty array"),
        (r#"{"not": 1}"#, 14, "$jsonSchema keyword 'not' must be an object"),
        (r#"{"encrypt": 1}"#, 14, "$jsonSchema keyword 'encrypt' must be an object"),
        (
            r#"{"encryptMetadata": 1}"#,
            14,
            "$jsonSchema keyword 'encryptMetadata' must be an object",
        ),
    ];
    for &(s, code, message) in cases {
        let b = bytes(s);
        let e = Schema::parse(raw(&b)).expect_err(s);
        assert_eq!((e.code.code, e.message.as_str()), (code, message), "{s}");
    }
    assert_eq!(codes::FAILED_TO_PARSE.code, 9);
}

#[test]
fn items_explain_as_the_reference_9_0_does() {
    let explain = |s: &str, d: &str| {
        let b = bytes(d);
        json(&schema(s).explain(raw(&b)).unwrap().unwrap())
    };
    // One schema for every item: the first item that fails (measured on a reference router 9.0.4).
    let text = explain(
        r#"{"properties": {"t": {"items": {"bsonType": "string"}}}}"#,
        r#"{"t": ["x", 1, 2]}"#,
    );
    let want = r#"{"operatorName":"items","reason":"At least one item did not match the sub-schema","itemIndex":{"$numberInt":"1"},"details":[{"operatorName":"bsonType""#;
    assert!(text.contains(want), "{text}");
    assert!(!text.contains(r#""itemIndex":{"$numberInt":"2"}"#), "{text}");
    // A schema per position: every position that fails, under `details`.
    let text = explain(
        r#"{"properties": {"t": {"items": [{"bsonType": "string"}, {"bsonType": "int"}]}}}"#,
        r#"{"t": [1, "x"]}"#,
    );
    let want = r#"{"operatorName":"items","details":[{"itemIndex":{"$numberInt":"0"},"details""#;
    assert!(text.contains(want), "{text}");
    assert!(text.contains(r#""itemIndex":{"$numberInt":"1"}"#), "{text}");
}

/// What a reference router 9.0.4 writes for the counts of items, `additionalItems: false`, an
/// exclusive bound, and a missing property under `oneOf` once the document fails for another
/// reason.
#[test]
fn explanation_details_measured_on_the_reference() {
    let explain = |s: &str, d: &str| {
        let b = bytes(d);
        json(&schema(s).explain(raw(&b)).unwrap().unwrap())
    };
    assert!(
        explain(r#"{"properties": {"c": {"minItems": 1}}}"#, r#"{"c": []}"#)
            .contains(r#""numberOfItems":{"$numberInt":"0"}"#)
    );
    let t = explain(
        r#"{"properties": {"c": {"items": [{"bsonType": "int"}], "additionalItems": false}}}"#,
        r#"{"c": [1, "x"]}"#,
    );
    assert!(t.contains(r#""operatorName":"additionalItems","specifiedAs":{"additionalItems":false},"reason":"found additional items","additionalItems":["x"]"#), "{t}");
    let t = explain(
        r#"{"properties": {"a": {"maximum": 4, "exclusiveMaximum": true}}}"#,
        r#"{"a": 5}"#,
    );
    assert!(
        t.contains(r#""specifiedAs":{"maximum":{"$numberInt":"4"},"exclusiveMaximum":true}"#),
        "{t}"
    );
    let t = explain(
        r#"{"minProperties": 2, "properties": {"x": {"oneOf": [{"type": "number"}, {"bsonType": "int"}]}}}"#,
        r#"{"y": 1}"#,
    );
    assert!(
        t.contains(
            r#""propertyName":"x","details":[{"operatorName":"oneOf","schemasNotSatisfied""#
        ),
        "{t}"
    );
    // The missing property alone does not make the document fail.
    let s =
        schema(r#"{"properties": {"x": {"oneOf": [{"type": "number"}, {"bsonType": "int"}]}}}"#);
    assert!(s.matches(raw(&bytes(r#"{"y": 1}"#))).unwrap());
    let t = explain(r#"{"properties": {"a": {"not": {"bsonType": "int"}}}}"#, r#"{"a": 1}"#);
    assert!(t.contains(r#"{"operatorName":"not","reason":"child expression matched"}"#), "{t}");
}
