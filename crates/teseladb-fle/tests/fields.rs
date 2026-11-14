//! The `encryptedFields` of `create` (017 FR-002): every rule of the IDL and of the checks after it,
//! one case each with the code and message it refuses with, and the option as the catalog keeps
//! and shows it.

use teseladb_bson::types as t;
use teseladb_bson::{Decimal128, Document, RawDoc, Value};
use teseladb_fle::FleError;
use teseladb_fle::fields::{self, EncryptedFieldConfig, QueryType};

fn bin(sub: u8, b: &[u8]) -> Value {
    Value::Binary { subtype: sub, bytes: b.to_vec() }
}

fn create(ef: &Document) -> Result<EncryptedFieldConfig, FleError> {
    let b = ef.to_bytes().expect("bytes");
    fields::for_create("c", RawDoc::from_bytes(&b).expect("raw"))
}

fn field_with(key: u8, path: &str, ty: Option<&str>, queries: Option<Value>) -> Value {
    let mut f = Document::new().with("keyId", bin(t::BINARY_UUID, &[key; 16])).with("path", path);
    if let Some(ty) = ty {
        f.push("bsonType", ty);
    }
    if let Some(q) = queries {
        f.push("queries", q);
    }
    Value::Document(f)
}

fn field(path: &str, ty: &str, q: Option<Document>) -> Value {
    field_with(1, path, Some(ty), q.map(Value::Document))
}

fn listed(path: &str, ty: &str, queries: Vec<Document>) -> Value {
    let list = queries.into_iter().map(Value::Document).collect();
    field_with(1, path, Some(ty), Some(Value::Array(list)))
}

fn one(f: Value) -> Document {
    Document::new().with("fields", vec![f])
}

fn q(kind: &str) -> Document {
    Document::new().with("queryType", kind)
}

fn text(kind: &str) -> Document {
    q(kind)
        .with("strMinQueryLength", 2)
        .with("strMaxQueryLength", 4)
        .with("caseSensitive", true)
        .with("diacriticSensitive", true)
}

fn substring() -> Document {
    text("substring").with("strMaxLength", 10)
}

fn refuse(ef: &Document, code: i32, message: &str) {
    let e = create(ef).expect_err("must be refused");
    assert_eq!((e.code, e.message.as_str()), (code, message), "{ef:?}");
}

fn first_contention(ef: &Document) -> Option<i64> {
    create(ef).expect("valid").fields[0].query().map(|q| q.contention)
}

#[test]
fn the_idl_refuses_unknown_missing_and_mistyped_top_fields() {
    let empty = || Document::new().with("fields", Vec::<Value>::new());
    refuse(
        &empty().with("x", 1),
        40_415,
        "BSON field 'create.encryptedFields.x' is an unknown field.",
    );
    refuse(
        &Document::new(),
        40_414,
        "BSON field 'create.encryptedFields.fields' is missing but a required field",
    );
    refuse(
        &Document::new().with("fields", 1),
        14,
        "BSON field 'create.encryptedFields.fields' is the wrong type 'int', expected type 'array'",
    );
    refuse(
        &Document::new().with("fields", vec![Value::Int32(1)]),
        14,
        "BSON field 'create.encryptedFields.fields.0' is the wrong type 'int', expected type \
         'object'",
    );
    refuse(
        &empty().with("escCollection", 1),
        14,
        "BSON field 'create.encryptedFields.escCollection' is the wrong type 'int', expected type \
         'string'",
    );
}

#[test]
fn the_idl_refuses_mistyped_fields_of_a_field() {
    let no_path = Document::new().with("keyId", bin(t::BINARY_UUID, &[1; 16]));
    refuse(
        &one(Value::Document(no_path)),
        40_414,
        "BSON field 'create.encryptedFields.fields.path' is missing but a required field",
    );
    let general_key =
        Document::new().with("keyId", bin(t::BINARY_GENERIC, &[1; 16])).with("path", "a");
    refuse(
        &one(Value::Document(general_key)),
        14,
        "BSON field 'create.encryptedFields.fields.keyId' is the wrong binData type 'general', \
         expected type 'UUID'",
    );
    let int_key = Document::new().with("keyId", 1).with("path", "a");
    refuse(
        &one(Value::Document(int_key)),
        14,
        "BSON field 'create.encryptedFields.fields.keyId' is the wrong type 'int', expected type \
         'binData'",
    );
    refuse(&one(field("a", "notatype", None)), 2, "Unknown type name: notatype");
    refuse(
        &one(field_with(1, "a", Some("string"), Some(Value::Int32(1)))),
        14,
        "BSON field 'create.encryptedFields.fields.queries' is the wrong type 'int', expected \
         types '[]'",
    );
    refuse(
        &one(field_with(1, "a", Some("string"), Some(Value::Array(vec![Value::Int32(1)])))),
        14,
        "BSON field 'create.encryptedFields.fields.queries' is the wrong type 'int', expected type \
         'object'",
    );
}

#[test]
fn str_encode_version_is_a_positive_int() {
    let base = || Document::new().with("fields", Vec::<Value>::new());
    let ok = create(&base().with("strEncodeVersion", 1)).expect("ok");
    assert_eq!(ok.str_encode_version, Some(1));
    refuse(
        &base().with("strEncodeVersion", 0),
        2,
        "BSON field 'strEncodeVersion' value must be > 0, actual value '0'",
    );
    refuse(
        &base().with("strEncodeVersion", "1"),
        14,
        "BSON field 'create.encryptedFields.strEncodeVersion' is the wrong type 'string', expected \
         type 'int'",
    );
}

#[test]
fn query_types_are_one_of_the_five_and_previews_are_refused() {
    refuse(
        &one(field("a", "string", Some(Document::new()))),
        40_414,
        "BSON field 'create.encryptedFields.fields.queries.queryType' is missing but a required \
         field",
    );
    refuse(
        &one(field("a", "string", Some(q("nope")))),
        2,
        "Enumeration value 'nope' for field 'create.encryptedFields.fields.queries.queryType' is \
         not a valid value.",
    );
    refuse(
        &one(field("a", "int", Some(q("rangePreview")))),
        8_575_605,
        "Cannot create a collection with an encrypted field with query type rangePreview, as it is \
         deprecated",
    );
    refuse(
        &one(field("a", "string", Some(q("prefixPreview")))),
        12_341_600,
        "Cannot create a collection with an encrypted field with query type prefixPreview, as it \
         is deprecated",
    );
    assert_eq!(QueryType::Substring.name(), "substring");
    assert!(QueryType::Suffix.is_text() && !QueryType::Range.is_text());
}

#[test]
fn the_numbers_of_a_query_are_integers_within_their_bounds() {
    refuse(
        &one(field("a", "string", Some(q("equality").with("contention", -1)))),
        2,
        "BSON field 'contention' value must be >= 0, actual value '-1'",
    );
    let over = q("equality").with("contention", Value::Int64(200_001));
    refuse(
        &one(field("a", "string", Some(over))),
        2,
        "contention factor (200001) exceeds the maximum allowed value (200000)",
    );
    refuse(
        &one(field("a", "string", Some(q("equality").with("contention", "x")))),
        14,
        "BSON field 'create.encryptedFields.fields.queries.contention' is the wrong type 'string', \
         expected types '[long, int, double, decimal]'",
    );
    refuse(
        &one(field("a", "int", Some(q("range").with("sparsity", 9)))),
        2,
        "BSON field 'sparsity' value must be <= 8, actual value '9'",
    );
    refuse(
        &one(field("a", "string", Some(text("prefix").with("strMaxQueryLength", 0)))),
        2,
        "BSON field 'strMaxQueryLength' value must be > 0, actual value '0'",
    );
    // Integral doubles and decimals are integers.
    let double = q("equality").with("contention", 2.0);
    assert_eq!(first_contention(&one(field("a", "string", Some(double)))), Some(2));
    let three = Value::Decimal128(Decimal128::from_parts(false, 30, -1).expect("ok"));
    let decimal = q("equality").with("contention", three);
    assert_eq!(first_contention(&one(field("a", "string", Some(decimal)))), Some(3));
}

#[test]
fn types_kinds_and_query_lists_are_checked() {
    refuse(
        &one(field_with(1, "a", Some("null"), None)),
        6_338_406,
        "Type 'null' is not a supported unindexed type",
    );
    refuse(
        &one(field_with(1, "a", None, Some(Value::Document(q("equality"))))),
        6_412_601,
        "BSON type needs to be specified for an indexed field",
    );
    refuse(
        &one(listed("a", "string", vec![])),
        9_783_412,
        "At least one query type should be specified per field",
    );
    let multiple = "Multiple query types may only include the suffix and prefix query types";
    refuse(&one(listed("a", "string", vec![q("equality"), text("prefix")])), 9_783_414, multiple);
    refuse(&one(listed("a", "string", vec![text("prefix"), text("prefix")])), 9_783_414, multiple);
}

fn not_allowed(name: &str, index: &str) -> String {
    format!("The field '{name}' is not allowed for {index} index but is present")
}

#[test]
fn an_equality_field_takes_neither_bounds_nor_range_or_text_options() {
    refuse(
        &one(field("a", "double", Some(q("equality")))),
        6_338_405,
        "Type 'double' is not a supported equality indexed type",
    );
    for (name, code) in [
        ("min", 6_775_206),
        ("max", 6_775_206),
        ("sparsity", 6_775_205),
        ("precision", 6_775_205),
        ("trimFactor", 6_775_205),
    ] {
        let query = q("equality").with(name, 1);
        refuse(&one(field("a", "int", Some(query))), code, &not_allowed(name, "equality"));
    }
    let case = q("equality").with("caseSensitive", true);
    refuse(
        &one(field("a", "string", Some(case))),
        10_774_903,
        &not_allowed("caseSensitive", "equality"),
    );
}

#[test]
fn a_range_field_checks_its_type_bounds_precision_and_trim_factor() {
    let range = |extra: Document| {
        let mut d = q("range");
        for (k, v) in extra.iter() {
            d.push(k, v.clone());
        }
        d
    };
    refuse(
        &one(field("a", "int", Some(range(Document::new().with("strMaxLength", 3))))),
        10_774_908,
        &not_allowed("strMaxLength", "range"),
    );
    refuse(
        &one(field("a", "string", Some(q("range")))),
        6_775_201,
        "Type 'string' is not a supported range indexed type",
    );
    refuse(
        &one(field("a", "int", Some(range(Document::new().with("min", 1.5).with("max", 3))))),
        7_018_200,
        "Range field type 'int' does not match the min value type 'double'",
    );
    let trio = "Precision, min, and max must all be specified together for floating point fields";
    refuse(
        &one(field("a", "double", Some(range(Document::new().with("min", 0.0).with("max", 1.0))))),
        6_967_100,
        trio,
    );
    refuse(
        &one(field("a", "double", Some(range(Document::new().with("precision", 1))))),
        6_967_100,
        trio,
    );
    refuse(
        &one(field("a", "int", Some(range(Document::new().with("min", 5).with("max", 5))))),
        6_720_005,
        "Min must be less than max.",
    );
    let trimmed = Document::new().with("min", 0).with("max", 15).with("trimFactor", 4);
    refuse(
        &one(field("a", "int", Some(range(trimmed)))),
        8_574_000,
        "The field 'trimFactor' must be >= 0 and less than the total number of bits needed to \
         represent elements in the domain (4)",
    );
    refuse(
        &one(field(
            "a",
            "long",
            Some(range(Document::new().with("trimFactor", 19).with("sparsity", 1))),
        )),
        9_203_504,
        "Sparsity and trimFactor together are too large and could create queries that exceed the \
         BSON size limit",
    );
    refuse(
        &one(field(
            "a",
            "long",
            Some(range(Document::new().with("trimFactor", 17).with("sparsity", 2))),
        )),
        9_203_508,
        "Sparsity, trimFactor, min, and max together are too large and could create queries that \
         exceed the BSON size limit",
    );
}

#[test]
fn a_text_field_needs_its_parameters_and_takes_no_range_options() {
    refuse(
        &one(field("a", "string", Some(text("prefix").with("min", 1)))),
        10_774_911,
        &not_allowed("min", "text-based"),
    );
    refuse(
        &one(field("a", "string", Some(text("prefix").with("trimFactor", 1)))),
        10_774_911,
        &not_allowed("trimFactor", "text-based"),
    );
    refuse(
        &one(field("a", "int", Some(text("prefix")))),
        9_783_400,
        "Type 'int' is not a supported type for text search indexed encrypted field a",
    );
    let required = |name: &str, kind: &str| {
        format!("{name} parameter is required for {kind} query type of field a")
    };
    refuse(
        &one(field("a", "string", Some(q("prefix")))),
        9_783_402,
        &required("strMinQueryLength", "prefix"),
    );
    let min_only = q("prefix").with("strMinQueryLength", 2);
    refuse(
        &one(field("a", "string", Some(min_only))),
        9_783_403,
        &required("strMaxQueryLength", "prefix"),
    );
    let lengths = q("suffix").with("strMinQueryLength", 2).with("strMaxQueryLength", 3);
    refuse(
        &one(field("a", "string", Some(lengths.clone()))),
        9_783_404,
        &required("caseSensitive", "suffix"),
    );
    let case = lengths.with("caseSensitive", true);
    refuse(
        &one(field("a", "string", Some(case))),
        9_783_405,
        &required("diacriticSensitive", "suffix"),
    );
    refuse(
        &one(field("a", "string", Some(text("prefix").with("strMinQueryLength", 5)))),
        9_783_406,
        "strMinQueryLength cannot be greater than strMaxQueryLength",
    );
    refuse(
        &one(field("a", "string", Some(text("substring")))),
        9_783_407,
        &required("strMaxLength", "substring"),
    );
    refuse(
        &one(field("a", "string", Some(text("substring").with("strMaxLength", 3)))),
        9_783_408,
        "strMaxQueryLength cannot be greater than strMaxLength",
    );
}

#[test]
fn a_substring_field_keeps_its_lengths_within_the_limits() {
    let long_query = substring().with("strMaxQueryLength", 8).with("strMaxLength", 20);
    refuse(
        &one(field("a", "string", Some(long_query))),
        12_860_002,
        "strMaxQueryLength (8) must be <= 6 for substring query type of field a.",
    );
    refuse(
        &one(field("a", "string", Some(substring().with("strMaxLength", 60)))),
        12_860_003,
        "strMaxLength (60) must be <= 50 for substring query type of field a.",
    );
    assert!(create(&one(field("a", "string", Some(substring())))).is_ok());
}

#[test]
fn a_text_family_shares_its_sensitivity_and_contention_and_bounds_its_tags() {
    let pair = |a: Document, b: Document| one(listed("a", "string", vec![a, b]));
    refuse(
        &pair(text("prefix"), text("suffix").with("caseSensitive", false)),
        9_783_409,
        "caseSensitive parameter must be the same for all query types of field a",
    );
    refuse(
        &pair(text("prefix"), text("suffix").with("diacriticSensitive", false)),
        9_783_410,
        "diacriticSensitive parameter must be the same for all query types of field a",
    );
    refuse(
        &pair(text("prefix").with("contention", 1), text("suffix").with("contention", 2)),
        9_783_411,
        "contention parameter must be the same for all query types of field a",
    );
    let ok = create(&pair(text("prefix"), text("suffix"))).expect("prefix and suffix together");
    assert!(ok.fields[0].has(QueryType::Suffix) && ok.fields[0].has(QueryType::Prefix));
    let huge =
        text("prefix").with("strMinQueryLength", 1).with("strMaxQueryLength", Value::Int64(90_000));
    refuse(
        &one(field("a", "string", Some(huge))),
        10_384_602,
        "Queryable Encryption tag limit exceeded for field 'a'. Worst case tag count is 90001",
    );
}

#[test]
fn keys_and_paths_meet_the_rules_after_every_field_is_read() {
    let two = |a: Value, b: Value| Document::new().with("fields", vec![a, b]);
    let plain = |key: u8, path: &str| field_with(key, path, Some("string"), None);
    refuse(&two(plain(1, "a"), plain(1, "b")), 6_338_401, "Duplicate key ids are not allowed");
    refuse(&one(plain(1, "")), 6_316_402, "Encrypted field must have a non-empty path");
    refuse(&one(plain(1, "_id")), 6_316_403, "Cannot encrypt _id or its subfields");
    refuse(&two(plain(1, "a"), plain(2, "a")), 6_338_402, "Duplicate paths are not allowed");
    refuse(
        &two(plain(1, "a.b"), plain(2, "a")),
        6_338_403,
        "Conflicting index paths found as one is a prefix of another 'a' and 'a.b'",
    );
    refuse(
        &Document::new().with("escCollection", "enxcol_.c.x").with("fields", Vec::<Value>::new()),
        7_406_900,
        "Encrypted State Collection name should follow enxcol_.<collection>.esc naming pattern",
    );
    refuse(
        &Document::new().with("ecocCollection", "x.ecoc").with("fields", Vec::<Value>::new()),
        7_406_902,
        "Encrypted Compaction Collection name should follow enxcol_.<collection>.ecoc naming \
         pattern",
    );
}

#[test]
fn the_catalog_keeps_the_option_as_given_with_defaults_filled_in() {
    let range = q("range")
        .with("contention", 0)
        .with("min", 0.0)
        .with("max", 10.0)
        .with("precision", 1)
        .with("sparsity", 2)
        .with("trimFactor", 1);
    let ef =
        Document::new().with("eccCollection", "enxcol_.c.ecc").with("strEncodeVersion", 1).with(
            "fields",
            vec![
                field("ssn", "string", Some(q("equality"))),
                field_with(
                    2,
                    "score",
                    Some("double"),
                    Some(Value::Array(vec![Value::Document(range)])),
                ),
                field_with(
                    3,
                    "name",
                    Some("string"),
                    Some(Value::Array(vec![Value::Document(substring())])),
                ),
                field_with(4, "note", None, None),
            ],
        );
    let cfg = create(&ef).expect("valid");
    assert_eq!((cfg.esc.as_str(), cfg.ecoc.as_str()), ("enxcol_.c.esc", "enxcol_.c.ecoc"));
    let doc = cfg.to_document();
    assert_eq!(doc.get("eccCollection"), Some(&Value::String("enxcol_.c.ecc".into())));
    assert_eq!(doc.get("strEncodeVersion"), Some(&Value::Int32(1)));
    // Read again, the kept option gives the same configuration.
    assert_eq!(create(&doc).expect("kept option is valid"), cfg);
    let Some(Value::Array(kept)) = doc.get("fields") else { panic!("fields") };
    // A query given as a document stays a document, with its default contention.
    let ssn_query =
        Document::new().with("queryType", "equality").with("contention", Value::Int64(8));
    assert_eq!(
        kept[0].as_document().expect("doc").get("queries"),
        Some(&Value::Document(ssn_query))
    );
    let score = kept[1].as_document().expect("doc");
    assert!(matches!(score.get("queries"), Some(Value::Array(a)) if a.len() == 1));
    assert_eq!(kept[3].as_document().expect("doc").get("bsonType"), None);
    assert_eq!(cfg.field("note").map(|f| f.query()), Some(None));
    assert!(cfg.field("missing").is_none());
    assert_eq!(fields::esc_name("x"), "enxcol_.x.esc");
    assert_eq!(fields::ecoc_name("x"), "enxcol_.x.ecoc");
}
