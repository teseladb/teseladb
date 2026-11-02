//! Definitions as `refsearch` 1.70.6 checks and shows them (measured on a 9.0.4 replica set,
//! 01-11-2026).

use teseladb_bson::{Document, Value};

use super::*;

fn mappings(fields: Document) -> Document {
    Document::new().with("mappings", Document::new().with("fields", fields))
}

fn one(name: &str, field: Document) -> Document {
    mappings(Document::new().with(name, field))
}

fn ty(t: &str) -> Document {
    Document::new().with("type", t)
}

fn string_defaults() -> Document {
    Document::new()
        .with("type", "string")
        .with("indexOptions", "offsets")
        .with("store", true)
        .with("norms", "include")
}

fn normalized(def: &Document) -> Document {
    Definition::parse(def).expect("a valid definition").normalized()
}

fn error(def: &Document) -> String {
    Definition::parse(def).expect_err("an invalid definition").message("userCommand.indexes[0].")
}

fn normalized_fields(fields: Document) -> Document {
    Document::new().with("mappings", Document::new().with("dynamic", false).with("fields", fields))
}

#[test]
fn mappings_get_their_defaults() {
    let dynamic = Document::new().with("mappings", Document::new().with("dynamic", true));
    let want = Document::new()
        .with("mappings", Document::new().with("dynamic", true).with("fields", Document::new()));
    assert_eq!(normalized(&dynamic), want);
    let t = one("t", ty("string"));
    assert_eq!(normalized(&t), normalized_fields(Document::new().with("t", string_defaults())));
    let english = Document::new()
        .with("type", "string")
        .with("analyzer", "reflib.english")
        .with("indexOptions", "offsets")
        .with("store", true)
        .with("norms", "include");
    let multi = one(
        "t",
        ty("string").with(
            "multi",
            Document::new().with("en", ty("string").with("analyzer", "reflib.english")),
        ),
    );
    let want = string_defaults().with("multi", Document::new().with("en", english));
    assert_eq!(normalized(&multi), normalized_fields(Document::new().with("t", want)));
    let both = one("t", Document::new());
    let both = mappings(
        Document::new().with("t", Value::Array(vec![ty("string").into(), ty("token").into()])),
    )
    .with("unused", both);
    let want = Value::Array(vec![string_defaults().into(), ty("token").into()]);
    assert_eq!(normalized(&both), normalized_fields(Document::new().with("t", want)));
}

#[test]
fn each_type_shows_its_defaults() {
    let cases = vec![
        (
            ty("number"),
            ty("number")
                .with("representation", "double")
                .with("indexDoubles", true)
                .with("indexIntegers", true),
        ),
        (
            ty("number")
                .with("representation", "int64")
                .with("indexIntegers", true)
                .with("indexDoubles", false),
            ty("number")
                .with("representation", "int64")
                .with("indexDoubles", false)
                .with("indexIntegers", true),
        ),
        (ty("date"), ty("date")),
        (ty("boolean"), ty("boolean")),
        (ty("objectId"), ty("objectId")),
        (ty("uuid"), ty("uuid")),
        (ty("token"), ty("token")),
        (ty("token").with("normalizer", "lowercase"), ty("token").with("normalizer", "lowercase")),
        (
            ty("autocomplete"),
            ty("autocomplete")
                .with("minGrams", 2)
                .with("maxGrams", 15)
                .with("foldDiacritics", true)
                .with("tokenization", "edgeGram"),
        ),
        (
            ty("autocomplete").with("analyzer", "reflib.english"),
            ty("autocomplete")
                .with("minGrams", 2)
                .with("maxGrams", 15)
                .with("foldDiacritics", true)
                .with("tokenization", "edgeGram")
                .with("analyzer", "reflib.english"),
        ),
        (ty("geo").with("indexShapes", true), ty("geo").with("indexShapes", true)),
        (
            ty("document").with("dynamic", true),
            ty("document").with("dynamic", true).with("fields", Document::new()),
        ),
        (
            ty("embeddedDocuments").with("storedSource", true),
            ty("embeddedDocuments")
                .with("dynamic", false)
                .with("fields", Document::new())
                .with("storedSource", true),
        ),
        (
            ty("knnVector").with("dimensions", 3).with("similarity", "cosine"),
            ty("knnVector").with("dimensions", 3).with("similarity", "cosine"),
        ),
        (
            ty("string")
                .with("searchAnalyzer", "reflib.simple")
                .with("indexOptions", "docs")
                .with("store", false)
                .with("norms", "omit")
                .with("ignoreAbove", 10),
            ty("string")
                .with("searchAnalyzer", "reflib.simple")
                .with("ignoreAbove", 10)
                .with("indexOptions", "docs")
                .with("store", false)
                .with("norms", "omit"),
        ),
    ];
    for (given, want) in cases {
        assert_eq!(
            normalized(&one("f", given.clone())),
            normalized_fields(Document::new().with("f", want)),
            "{given:?}"
        );
    }
}

#[test]
fn the_top_options_are_kept_and_unknown_ones_dropped() {
    let def = Document::new()
        .with("analyzer", "reflib.french")
        .with("searchAnalyzer", "reflib.french")
        .with("mappings", Document::new().with("dynamic", true))
        .with("storedSource", Document::new().with("include", Value::Array(vec!["t".into()])))
        .with("numPartitions", 1)
        .with("foo", 1);
    let want = Document::new()
        .with("analyzer", "reflib.french")
        .with("searchAnalyzer", "reflib.french")
        .with("mappings", Document::new().with("dynamic", true).with("fields", Document::new()))
        .with("storedSource", Document::new().with("include", Value::Array(vec!["t".into()])));
    assert_eq!(normalized(&def), want);
    let custom = Document::new()
        .with("name", "my")
        .with("tokenizer", Document::new().with("type", "whitespace"));
    let def = one("t", ty("string").with("analyzer", "my"))
        .with("analyzers", Value::Array(vec![custom.clone().into()]));
    assert_eq!(normalized(&def).get("analyzers"), Some(&Value::Array(vec![custom.into()])));
    let synonyms = Document::new()
        .with("name", "syn")
        .with("analyzer", "reflib.standard")
        .with("source", Document::new().with("collection", "syncoll"));
    let def = Document::new()
        .with("mappings", Document::new().with("dynamic", true))
        .with("synonyms", Value::Array(vec![synonyms.into()]));
    let want = Document::new()
        .with("name", "syn")
        .with("source", Document::new().with("collection", "syncoll"))
        .with("analyzer", "reflib.standard");
    assert_eq!(normalized(&def).get("synonyms"), Some(&Value::Array(vec![want.into()])));
    let def = Document::new()
        .with("mappings", Document::new().with("dynamic", Document::new().with("typeSet", "ts")))
        .with(
            "typeSets",
            Value::Array(vec![
                Document::new()
                    .with("name", "ts")
                    .with("types", Value::Array(vec![ty("string").into()]))
                    .into(),
            ]),
        );
    let want = Value::Array(vec![
        Document::new()
            .with("name", "ts")
            .with("types", Value::Array(vec![string_defaults().into()]))
            .into(),
    ]);
    assert_eq!(normalized(&def).get("typeSets"), Some(&want));
}

#[test]
fn errors_name_the_path_as_refsearch_does() {
    let p = "\"userCommand.indexes[0].";
    let cases = vec![
        (
            one("t", ty("nope")),
            format!(
                "{p}mappings.fields.t.type\" must be one of [autoEmbedVector, autocomplete, boolean, date, document, embeddedDocuments, geo, knnVector, number, objectId, string, token, uuid, vector]"
            ),
        ),
        (
            one("t", ty("string").with("foo", 1)),
            format!("{p}mappings.fields.t\" unrecognized field \"foo\""),
        ),
        (
            one("v", ty("knnVector").with("dimensions", 0).with("similarity", "cosine")),
            format!("{p}mappings.fields.v.dimensions\" must be within bounds [1..8192]"),
        ),
        (
            Document::new().with("mappings", Document::new().with("fields", 5)),
            format!("{p}mappings.fields\" must be a document"),
        ),
        (
            one("t", Document::new().with("analyzer", "reflib.standard")),
            format!("{p}mappings.fields.t.type\" is required"),
        ),
        (
            one("t", ty("string").with("analyzer", 5)),
            format!("{p}mappings.fields.t.analyzer\" must be a string"),
        ),
        (
            one("t", ty("string").with("indexOptions", "nope")),
            format!(
                "{p}mappings.fields.t.indexOptions\" must be one of [docs, freqs, offsets, positions]"
            ),
        ),
        (
            one("t", ty("string").with("norms", "nope")),
            format!("{p}mappings.fields.t.norms\" must be one of [include, omit]"),
        ),
        (
            one("t", ty("string").with("store", 5)),
            format!("{p}mappings.fields.t.store\" must be a boolean"),
        ),
        (
            one("k", ty("token").with("normalizer", "x")),
            format!("{p}mappings.fields.k.normalizer\" must be one of [lowercase, none]"),
        ),
        (
            one("n", ty("number").with("representation", "x")),
            format!("{p}mappings.fields.n.representation\" must be one of [double, int64]"),
        ),
        (
            one("a", ty("autocomplete").with("minGrams", 0)),
            format!("{p}mappings.fields.a.minGrams\" must be positive"),
        ),
        (
            one("v", ty("knnVector").with("dimensions", 3).with("similarity", "x")),
            format!(
                "{p}mappings.fields.v.similarity\" must be one of [cosine, dotProduct, euclidean]"
            ),
        ),
        (
            Document::new()
                .with("mappings", Document::new().with("dynamic", Document::new().with("foo", 1))),
            format!("{p}mappings.dynamic.typeSet\" is required"),
        ),
        (
            Document::new().with("mappings", Document::new().with("dynamic", "x")),
            format!("{p}mappings.dynamic\" must be a boolean or document"),
        ),
        (
            Document::new().with("fields", Value::Array(vec![])),
            format!("{p}mappings\" is required"),
        ),
        (
            one("t", ty("string").with("analyzer", "nope1").with("searchAnalyzer", "nope2")),
            "references non-existent analyzers: nope1, nope2".to_owned(),
        ),
        (
            Document::new()
                .with("analyzer", "nopeTop")
                .with("mappings", Document::new().with("dynamic", true)),
            "references non-existent analyzers: nopeTop".to_owned(),
        ),
    ];
    for (def, want) in cases {
        assert_eq!(error(&def), want, "{def:?}");
    }
    let shape = DefinitionError::shape("mappings.dynamic", "must be a boolean or document");
    assert_eq!(shape.message(""), "\"mappings.dynamic\" must be a boolean or document");
}

fn vector(path: &str, dims: i32, similarity: &str) -> Document {
    Document::new()
        .with("type", "vector")
        .with("path", path)
        .with("numDimensions", dims)
        .with("similarity", similarity)
}

fn vector_def(fields: Vec<Document>) -> Document {
    Document::new().with("fields", Value::Array(fields.into_iter().map(Value::Document).collect()))
}

#[test]
fn vector_definitions_are_checked_as_refsearch_does() {
    let basic = vector_def(vec![vector("v", 3, "cosine")]);
    let parsed = VectorDefinition::parse(&basic).expect("valid");
    let want = vector_def(vec![
        Document::new()
            .with("type", "vector")
            .with("path", "v")
            .with("numDimensions", 3)
            .with("similarity", "cosine"),
    ]);
    assert_eq!(parsed.normalized(), want);
    let quant = vector_def(vec![
        vector("v", 3, "dotProduct").with("quantization", "scalar"),
        Document::new().with("type", "filter").with("path", "f"),
    ]);
    assert_eq!(VectorDefinition::parse(&quant).expect("valid").fields.len(), 2);
    assert!(VectorDefinition::parse(&vector_def(vec![])).is_ok());
    let p = "\"userCommand.indexes[0].";
    let err = |d: &Document| {
        VectorDefinition::parse(d).expect_err("invalid").message("userCommand.indexes[0].")
    };
    let cases = vec![
        (
            vector_def(vec![vector("v", 3, "nope")]),
            format!("{p}fields[0].similarity\" must be one of [cosine, dotProduct, euclidean]"),
        ),
        (
            vector_def(vec![vector("v", 0, "cosine")]),
            format!("{p}fields[0].numDimensions\" must be within bounds [1..8192]"),
        ),
        (
            vector_def(vec![
                Document::new()
                    .with("type", "vector")
                    .with("numDimensions", 3)
                    .with("similarity", "cosine"),
            ]),
            format!("{p}fields[0].path\" is required"),
        ),
        (
            vector_def(vec![Document::new().with("type", "string").with("path", "v")]),
            format!(
                "{p}fields[0].type\" must be one of [autoEmbed, embeddedDocuments, filter, text, vector]"
            ),
        ),
        (
            vector_def(vec![
                Document::new().with("type", "filter").with("path", "f").with("foo", 1),
            ]),
            format!("{p}fields[0]\" unrecognized field \"foo\""),
        ),
        (Document::new().with("mappings", Document::new()), format!("{p}fields\" is required")),
    ];
    for (def, want) in cases {
        assert_eq!(err(&def), want, "{def:?}");
    }
}

/// `returnStoredSource` (measured on `refsearch` 1.70.6 with `include: ["t"]`, scenario
/// `search-type-objectId`): `_id` and the fields kept.
#[test]
fn stored_source_keeps_the_id_and_the_fields_it_names() {
    let doc = Document::new()
        .with("_id", 1)
        .with("o", 2)
        .with("t", "red fox")
        .with("a", Document::new().with("b", 1).with("c", 2))
        .with("s", "x");
    let names = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let include = StoredSource::Include(names(&["t", "a.c", "s.z"]));
    assert_eq!(
        include.project(&doc),
        Document::new().with("_id", 1).with("t", "red fox").with("a", Document::new().with("c", 2))
    );
    let exclude = StoredSource::Exclude(names(&["o", "a.b", "t.z"]));
    assert_eq!(
        exclude.project(&doc),
        Document::new()
            .with("_id", 1)
            .with("t", "red fox")
            .with("a", Document::new().with("c", 2))
            .with("s", "x")
    );
    assert_eq!(StoredSource::All.project(&doc), doc);
    assert_eq!(
        StoredSource::Include(names(&["a.z"])).project(&doc),
        Document::new().with("_id", 1)
    );
}

/// Synonyms, type sets, `storedSource` and whole-number options as `refsearch` 1.70.6 checks them
/// (`scratchpad/lane3p/m63.js`, 02-11-2026).
#[test]
fn synonyms_type_sets_and_stored_source_errors_as_refsearch_gives_them() {
    let p = "\"userCommand.indexes[0].";
    let dynamic = || Document::new().with("dynamic", true);
    let def = |k: &str, v: Value| Document::new().with("mappings", dynamic()).with(k, v);
    let syn = |s: Document| def("synonyms", Value::Array(vec![s.into()]));
    let source = Document::new().with("collection", "s");
    let named = |n: bool, a: bool| {
        let mut d = Document::new();
        if n {
            d.insert("name", "x");
        }
        if a {
            d.insert("analyzer", "reflib.standard");
        }
        d
    };
    let cases: Vec<(Document, String)> = vec![
        (
            syn(named(false, true).with("source", source.clone())),
            format!("{p}synonyms[0].name\" is required"),
        ),
        (
            syn(named(true, false).with("source", source.clone())),
            format!("{p}synonyms[0].analyzer\" is required"),
        ),
        (
            syn(named(true, true).with("source", Document::new())),
            format!("{p}synonyms[0].source.collection\" is required"),
        ),
        (
            syn(named(true, true).with("source", 1)),
            format!("{p}synonyms[0].source\" must be a document"),
        ),
        (syn(named(true, true)), format!("{p}synonyms[0].source\" is required")),
        (
            def(
                "typeSets",
                Value::Array(vec![
                    Document::new().with("types", Value::Array(vec![ty("string").into()])).into(),
                ]),
            ),
            format!("{p}typeSets[0].name\" is required"),
        ),
        (
            def(
                "storedSource",
                Document::new().with("include", Value::Array(vec![1.into()])).into(),
            ),
            format!("{p}storedSource.include[0]\" must be a string"),
        ),
        (
            def("storedSource", Document::new().with("include", "a").into()),
            format!("{p}storedSource.include\" must be a array"),
        ),
        (
            def("storedSource", Document::new().into()),
            format!("{p}storedSource\" one of [include, exclude] must be present"),
        ),
        (def("storedSource", 1.into()), format!("{p}storedSource\" must be a boolean or document")),
        (
            def(
                "storedSource",
                Document::new()
                    .with("include", Value::Array(vec!["a".into()]))
                    .with("exclude", Value::Array(vec!["b".into()]))
                    .into(),
            ),
            format!("{p}storedSource\" only one of [include, exclude] may be present"),
        ),
        (
            one("t", ty("string").with("ignoreAbove", 1.5)),
            format!("{p}mappings.fields.t.ignoreAbove\" must be an integer"),
        ),
    ];
    for (d, want) in cases {
        assert_eq!(error(&d), want);
    }
    assert!(Definition::parse(&one("t", ty("string").with("ignoreAbove", 2.0))).is_ok());
}

/// The automatic embedding types as `refsearch` 1.70.6 checks them without an embedding service
/// (`scratchpad/lane3p/m70.js` to `m72.js`): the options it asks for, then the models refused.
#[test]
fn automatic_embedding_needs_a_service_this_engine_lacks() {
    let p = "\"userCommand.indexes[0].";
    let auto = |extra: &[(&str, &str)]| {
        let mut f = ty("autoEmbedVector");
        for (k, v) in extra {
            f.insert(*k, *v);
        }
        one("t", f)
    };
    assert_eq!(error(&auto(&[])), format!("{p}mappings.fields.t.model\" is required"));
    assert_eq!(
        error(&auto(&[("model", "voyage-3")])),
        format!("{p}mappings.fields.t.modality\" is required")
    );
    assert_eq!(
        error(&auto(&[("model", "voyage-3"), ("modality", "text")])),
        format!("{p}mappings.fields.t.sourceField\" is required")
    );
    let refused = "The following embedding model(s) are not supported: voyage-3. Supported \
                   models: none (embedding service not configured).";
    assert_eq!(
        error(&auto(&[("model", "voyage-3"), ("modality", "text"), ("sourceField", "s")])),
        refused
    );
    let vector = |fields: Vec<Document>| {
        let def = Document::new()
            .with("fields", Value::Array(fields.into_iter().map(Value::Document).collect()));
        VectorDefinition::parse(&def).expect_err("refused").message("userCommand.indexes[0].")
    };
    let field = |kind: &str| Document::new().with("type", kind).with("path", "t");
    assert_eq!(vector(vec![field("autoEmbed")]), format!("{p}fields[0].model\" is required"));
    assert_eq!(
        vector(vec![field("autoEmbed").with("model", "m")]),
        format!("{p}fields[0].modality\" is required")
    );
    assert_eq!(
        vector(vec![field("autoEmbed").with("model", "voyage-3").with("modality", "text")]),
        "CanonicalModel: voyage-3 not registered yet, supported models are: []"
    );
    assert_eq!(vector(vec![field("text").with("model", "voyage-3")]), refused);
    assert_eq!(
        vector(vec![
            field("text"),
            Document::new().with("type", "text").with("path", "u").with("model", "m2")
        ]),
        "The following embedding model(s) are not supported: voyage-3-large, m2. Supported \
         models: none (embedding service not configured)."
    );
}
