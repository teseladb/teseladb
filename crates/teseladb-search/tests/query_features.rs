//! What `$search` and `$searchMeta` give for queries with synonyms, highlights, score details,
//! facets, counts, spans, phrases of several terms at one position and embedded documents, over
//! two segments (011 FR-002 to FR-005, FR-015): `data/query_fingerprints.txt` holds a fingerprint
//! of each answer, recorded with the code of 14-11-2026 before FR-032 changed the segment in
//! memory. Each fingerprint is printed before it is compared, so a run with `--nocapture` on
//! another version of the code writes the file anew.

use std::collections::BTreeMap;

use teseladb_bson::{Document, extjson};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;

const FINGERPRINTS: &str = include_str!("data/query_fingerprints.txt");

const DEFINITION: &str = r#"{
  "mappings": {"dynamic": true, "fields": {
    "g": {"type": "token"},
    "n": {"type": "number"},
    "items": {"type": "embeddedDocuments", "dynamic": true},
    "syn": {"type": "string", "analyzer": "${name:ANALYZER_ENGLISH}"},
    "a": {"type": "autocomplete", "tokenization": "nGram", "minGrams": 2, "maxGrams": 6, "foldDiacritics": true},
    "loc": {"type": "geo", "indexShapes": true},
    "flag": {"type": "boolean"}
  }},
  "synonyms": [
    {"name": "s", "analyzer": "${name:ANALYZER_STANDARD}", "source": {"collection": "syn"}},
    {"name": "bad", "analyzer": "${name:ANALYZER_STANDARD}", "source": {"collection": "bad"}}
  ]
}"#;

const SYNONYMS: &[&str] = &[
    r#"{"mappingType": "equivalent", "synonyms": ["car", "automobile", "motor car"]}"#,
    r#"{"mappingType": "explicit", "input": ["fast"], "synonyms": ["quick", "rapid"]}"#,
    r#"{"mappingType": "explicit", "input": ["big cat"], "synonyms": ["tiger", "lion"]}"#,
];

const TEXTS: &[&str] = &[
    "the quick brown fox jumps over the lazy dog",
    "a fast car and a big cat",
    "the automobile is a motor car with a quick engine",
    "lion and tiger and big cat in the zoo",
    "rapid rapid rapid fox",
    "dogs and cats and cars",
    "nothing to see here",
    "quick brown dogs jump over lazy foxes in the quick brown night",
];

fn index() -> Indexer {
    let names = teseladb_names::expand(DEFINITION).expect("marks");
    let def = Definition::parse(&extjson::parse(&names).expect("JSON")).expect("definition");
    let mut ix = Indexer::new(def);
    ix.reshape(&[(Vec::new(), vec![4]), (vec![4], Vec::new())]).expect("segments");
    let synonyms: Vec<Document> =
        SYNONYMS.iter().map(|s| extjson::parse(s).expect("JSON")).collect();
    ix.set_synonyms("s", &synonyms).expect("synonyms");
    let invalid =
        extjson::parse(r#"{"mappingType": "explicit", "synonyms": ["x"]}"#).expect("JSON");
    ix.set_synonyms("bad", &[invalid]).expect("synonyms");
    let docs = TEXTS.iter().enumerate().map(|(i, t)| {
        let item =
            |k: usize| Document::new().with("t", TEXTS[(i + k) % TEXTS.len()]).with("k", k as i32);
        let doc = Document::new()
            .with("_id", i as i32)
            .with("t", *t)
            .with("syn", *t)
            .with("g", ["red", "green", "blue"][i % 3])
            .with("n", (i * 7 % 30) as i32)
            .with("items", vec![item(1).into(), item(2).into()])
            .with("a", *t)
            .with("flag", i % 2 == 0)
            .with(
                "loc",
                Document::new().with("type", "Point").with(
                    "coordinates",
                    vec![
                        teseladb_bson::Value::Double(i as f64 * 0.5 - 2.0),
                        teseladb_bson::Value::Double(1.0 - i as f64 * 0.25),
                    ],
                ),
            );
        (vec![i as u8], Some(doc))
    });
    ix.apply(docs).expect("indexed");
    ix
}

/// The `$search` specifications, by name.
const SEARCHES: &[(&str, &str)] = &[
    ("synonyms-equivalent", r#"{"text": {"query": "car", "path": "t", "synonyms": "s"}}"#),
    ("synonyms-explicit", r#"{"text": {"query": "fast fox", "path": "t", "synonyms": "s"}}"#),
    ("synonyms-phrase", r#"{"phrase": {"query": "big cat", "path": "t", "synonyms": "s"}}"#),
    ("synonyms-english", r#"{"text": {"query": "cars", "path": "syn", "synonyms": "s"}}"#),
    ("highlight", r#"{"text": {"query": "quick fox", "path": "t"}, "highlight": {"path": "t"}}"#),
    (
        "highlight-limits",
        r#"{"text": {"query": "quick", "path": ["t", "syn"]},
            "highlight": {"path": {"wildcard": "*"}, "maxNumPassages": 1, "maxCharsToExamine": 20}}"#,
    ),
    ("details", r#"{"text": {"query": "quick brown", "path": "t"}, "scoreDetails": true}"#),
    (
        "details-compound",
        r#"{"compound": {"must": [{"text": {"query": "fox", "path": "t"}}],
            "should": [{"phrase": {"query": "brown fox", "path": "t", "slop": 1}}],
            "filter": [{"range": {"path": "n", "gte": 0, "lte": 20}}]},
            "scoreDetails": true}"#,
    ),
    (
        "details-synonyms",
        r#"{"text": {"query": "fast", "path": "t", "synonyms": "s"}, "scoreDetails": true}"#,
    ),
    (
        "phrase-several",
        r#"{"phrase": {"query": "motor car", "path": "t", "synonyms": "s", "slop": 2}}"#,
    ),
    (
        "span-near",
        r#"{"span": {"near": {"clauses": [{"term": {"query": "quick", "path": "t"}},
            {"term": {"query": "dogs", "path": "t"}}], "slop": 3, "inOrder": false}}}"#,
    ),
    (
        "span-first",
        r#"{"span": {"first": {"operator": {"term": {"query": "brown", "path": "t"}}, "endPositionLte": 3}}}"#,
    ),
    (
        "embedded",
        r#"{"embeddedDocument": {"path": "items", "operator": {"text": {"query": "zoo", "path": "items.t"}},
            "score": {"embedded": {"aggregate": "maximum"}}}}"#,
    ),
    ("equals-token", r#"{"equals": {"path": "g", "value": "red"}}"#),
    ("equals-bool", r#"{"equals": {"path": "flag", "value": true}}"#),
    ("in-numbers", r#"{"in": {"path": "n", "value": [0, 7, 14]}}"#),
    ("near-number", r#"{"near": {"path": "n", "origin": 10, "pivot": 2}}"#),
    ("range-number", r#"{"range": {"path": "n", "gt": 3, "lt": 25}}"#),
    (
        "criteria-any",
        r#"{"text": {"query": "fast car zoo", "path": "t", "synonyms": "s", "matchCriteria": "any"}}"#,
    ),
    (
        "criteria-all",
        r#"{"text": {"query": "big cat zoo", "path": "t", "synonyms": "s", "matchCriteria": "all"}}"#,
    ),
    (
        "criteria-words",
        r#"{"text": {"query": "motor car", "path": ["t", "syn"], "synonyms": "s", "matchCriteria": "any"}}"#,
    ),
    ("synonyms-one-phrase", r#"{"phrase": {"query": "fast", "path": "t", "synonyms": "s"}}"#),
    (
        "synonyms-plain-phrase",
        r#"{"phrase": {"query": "lazy dog", "path": "t", "synonyms": "s", "slop": 1}}"#,
    ),
    ("synonyms-invalid", r#"{"text": {"query": "car", "path": "t", "synonyms": "bad"}}"#),
    ("synonyms-words", r#"{"text": {"query": "motor car", "path": "t", "synonyms": "s"}}"#),
    ("autocomplete", r#"{"autocomplete": {"query": "qui bro", "path": "a"}}"#),
    (
        "autocomplete-sequential",
        r#"{"autocomplete": {"query": "quick bro", "path": "a", "tokenOrder": "sequential", "fuzzy": {"maxEdits": 1}}}"#,
    ),
    (
        "fuzzy",
        r#"{"text": {"query": "quikc fxo", "path": "t", "fuzzy": {"maxEdits": 2, "prefixLength": 1}}}"#,
    ),
    ("wildcard", r#"{"wildcard": {"query": "qu*k", "path": "t", "allowAnalyzedField": true}}"#),
    ("regex", r#"{"regex": {"query": "(fox|dog)s?", "path": "t", "allowAnalyzedField": true}}"#),
    (
        "query-string",
        r#"{"queryString": {"defaultPath": "t", "query": "quick AND (fox OR dogs)"}}"#,
    ),
    ("more-like-this", r#"{"moreLikeThis": {"like": {"t": "quick brown fox jumps"}}}"#),
    ("exists", r#"{"exists": {"path": "items.t"}}"#),
    (
        "minimum-should",
        r#"{"compound": {"should": [{"text": {"query": "fox", "path": "t"}}, {"text": {"query": "dog", "path": "t"}},
            {"text": {"query": "cat", "path": "t"}}], "minimumShouldMatch": 2}}"#,
    ),
    (
        "geo-circle",
        r#"{"geoWithin": {"path": "loc", "circle": {"center": {"type": "Point", "coordinates": [0, 0]}, "radius": 200000}}}"#,
    ),
    (
        "geo-box",
        r#"{"geoWithin": {"path": "loc", "box": {"bottomLeft": {"type": "Point", "coordinates": [-1, -1]},
            "topRight": {"type": "Point", "coordinates": [1, 1]}}}}"#,
    ),
    (
        "geo-shape",
        r#"{"geoShape": {"path": "loc", "relation": "within", "geometry": {"type": "Polygon",
            "coordinates": [[[-3, -3], [3, -3], [3, 3], [-3, 3], [-3, -3]]]}}}"#,
    ),
    (
        "geo-near",
        r#"{"near": {"path": "loc", "origin": {"type": "Point", "coordinates": [0, 0]}, "pivot": 50000}}"#,
    ),
    (
        "embedded-sum",
        r#"{"embeddedDocument": {"path": "items", "operator": {"text": {"query": "quick fox", "path": "items.t"}},
            "score": {"embedded": {"aggregate": "sum"}}}}"#,
    ),
    (
        "embedded-mean",
        r#"{"embeddedDocument": {"path": "items", "operator": {"range": {"path": "items.k", "gte": 2}},
            "score": {"embedded": {"aggregate": "mean"}}}}"#,
    ),
    (
        "highlight-phrase",
        r#"{"phrase": {"query": "brown fox", "path": "t"}, "highlight": {"path": "t", "maxCharsToExamine": 30}}"#,
    ),
    (
        "highlight-synonyms",
        r#"{"text": {"query": "fast", "path": "t", "synonyms": "s"}, "highlight": {"path": ["t", "syn"]}}"#,
    ),
    (
        "highlight-compound",
        r#"{"compound": {"must": [{"wildcard": {"query": "dog*", "path": "t", "allowAnalyzedField": true}}],
            "should": [{"autocomplete": {"query": "laz", "path": "a"}}]}, "highlight": {"path": {"wildcard": "*"}}}"#,
    ),
    (
        "score-boost",
        r#"{"text": {"query": "quick", "path": "t", "score": {"boost": {"path": "n", "undefined": 1}}}}"#,
    ),
    (
        "score-function",
        r#"{"text": {"query": "quick", "path": "t", "score": {"function": {"add": [{"score": "relevance"}, {"path": {"value": "n", "undefined": 0}}]}}}}"#,
    ),
    (
        "facet-operator",
        r#"{"facet": {"operator": {"exists": {"path": "t"}},
            "facets": {"g": {"type": "string", "path": "g"}}}}"#,
    ),
    ("range-negative", r#"{"range": {"path": "n", "gte": -5.5, "lt": 3}}"#),
    ("range-no-integer", r#"{"range": {"path": "n", "gt": 3.25, "lt": 3.75}}"#),
    ("range-two-paths", r#"{"range": {"path": ["n", "items.k"], "gte": 1, "lte": 14}}"#),
    ("range-tokens", r#"{"range": {"path": "g", "gte": "blue", "lt": "red"}}"#),
    ("equals-fraction", r#"{"equals": {"path": "n", "value": 3.5}}"#),
    ("equals-huge", r#"{"equals": {"path": "n", "value": 1e19}}"#),
    ("in-tokens", r#"{"in": {"path": "g", "value": ["red", "blue"]}}"#),
    ("in-dates", r#"{"in": {"path": "n", "value": [{"$date": "2026-01-01T00:00:00Z"}, 7]}}"#),
    ("near-two-paths", r#"{"near": {"path": ["n", "items.k"], "origin": 7, "pivot": 3}}"#),
    (
        "filter-criteria-all",
        r#"{"compound": {"filter": [{"text": {"query": "big cat zoo", "path": "t", "synonyms": "s", "matchCriteria": "all"}}]}}"#,
    ),
    (
        "filter-criteria-any",
        r#"{"compound": {"filter": [{"text": {"query": "fast car", "path": ["t", "syn"], "synonyms": "s", "matchCriteria": "any"}}]}}"#,
    ),
    (
        "filter-synonyms-paths",
        r#"{"compound": {"filter": [{"text": {"query": "motor car", "path": ["t", "syn"], "synonyms": "s"}}]}}"#,
    ),
    (
        "filter-synonyms-phrase",
        r#"{"compound": {"filter": [{"phrase": {"query": "big cat", "path": "t", "synonyms": "s"}}]}}"#,
    ),
    (
        "boost-value",
        r#"{"text": {"query": "quick", "path": "t", "score": {"boost": {"value": 2}}}}"#,
    ),
    ("range-token-one-bound", r#"{"range": {"path": "g", "gt": "green"}}"#),
    ("should-one", r#"{"compound": {"should": [{"text": {"query": "fox", "path": "t"}}]}}"#),
    (
        "should-minimum-one",
        r#"{"compound": {"should": [{"text": {"query": "fox", "path": "t"}}, {"text": {"query": "zoo", "path": "t"}}], "minimumShouldMatch": 1}}"#,
    ),
    ("filter-only", r#"{"compound": {"filter": [{"text": {"query": "dog", "path": "t"}}]}}"#),
    (
        "compound-boost",
        r#"{"compound": {"must": [{"text": {"query": "quick", "path": "t"}}], "should": [{"text": {"query": "fox", "path": "t"}}], "score": {"boost": {"value": 3}}}}"#,
    ),
    (
        "criteria-all-plain",
        r#"{"text": {"query": "quick brown", "path": "t", "matchCriteria": "all"}}"#,
    ),
    (
        "embedded-minimum",
        r#"{"embeddedDocument": {"path": "items", "operator": {"text": {"query": "quick fox", "path": "items.t"}},
            "score": {"embedded": {"aggregate": "minimum"}}}}"#,
    ),
    (
        "embedded-boost",
        r#"{"embeddedDocument": {"path": "items", "operator": {"text": {"query": "zoo", "path": "items.t"}},
            "score": {"boost": {"value": 2}}}}"#,
    ),
    (
        "autocomplete-boost",
        r#"{"autocomplete": {"query": "qui", "path": "a", "score": {"boost": {"value": 2}}}}"#,
    ),
    ("autocomplete-list", r#"{"autocomplete": {"query": ["qui", "laz"], "path": "a"}}"#),
    (
        "geo-polygon-hole",
        r#"{"geoShape": {"path": "loc", "relation": "within", "geometry": {"type": "Polygon",
            "coordinates": [[[-3, -3], [3, -3], [3, 3], [-3, 3], [-3, -3]], [[-0.5, -0.5], [0.5, -0.5], [0.5, 0.5], [-0.5, 0.5], [-0.5, -0.5]]]}}}"#,
    ),
    (
        "geo-contains-points",
        r#"{"geoShape": {"path": "loc", "relation": "contains", "geometry": {"type": "MultiPoint",
            "coordinates": [[0, 0], [1, 1]]}}}"#,
    ),
    (
        "geo-intersects-line",
        r#"{"geoShape": {"path": "loc", "relation": "intersects", "geometry": {"type": "LineString",
            "coordinates": [[-3, 1], [3, -1]]}}}"#,
    ),
    (
        "geo-near-two-paths",
        r#"{"near": {"path": ["loc", "other"], "origin": {"type": "Point", "coordinates": [0, 0]}, "pivot": 50000}}"#,
    ),
    ("phrase-one-word", r#"{"phrase": {"query": "fox", "path": "t"}}"#),
    ("phrase-missing-path", r#"{"phrase": {"query": "brown fox", "path": "nope"}}"#),
    (
        "must-criteria-all-paths",
        r#"{"compound": {"must": [{"text": {"query": "fast fox", "path": ["t", "syn"], "synonyms": "s", "matchCriteria": "all"}}]}}"#,
    ),
];

/// The `$searchMeta` specifications, by name.
const METAS: &[(&str, &str)] = &[
    ("count-total", r#"{"exists": {"path": "t"}, "count": {"type": "total"}}"#),
    (
        "count-bound",
        r#"{"text": {"query": "quick", "path": "t"}, "count": {"type": "lowerBound", "threshold": 2}}"#,
    ),
    (
        "facets",
        r#"{"facet": {"operator": {"text": {"query": "the", "path": "t"}},
            "facets": {"colors": {"type": "string", "path": "g", "numBuckets": 2},
                       "numbers": {"type": "number", "path": "n", "boundaries": [0, 10, 20, 30], "default": "other"}}}}"#,
    ),
];

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }
}

#[test]
fn every_feature_answers_as_it_did() {
    let ix = index();
    let mut found: Vec<(String, u64)> = Vec::new();
    // Each query also with `scoreDetails`, whose descriptions name every part of it.
    let detailed: Vec<(String, String)> = SEARCHES
        .iter()
        .filter(|(_, spec)| !spec.contains("scoreDetails"))
        .flat_map(|(name, spec)| {
            let open = spec.trim_end().strip_suffix('}').expect("an object");
            let with = format!("{open}, \"scoreDetails\": true}}");
            [(name.to_string(), spec.to_string()), (format!("{name}+details"), with)]
        })
        .chain(
            SEARCHES
                .iter()
                .filter(|(_, spec)| spec.contains("scoreDetails"))
                .map(|(name, spec)| (name.to_string(), spec.to_string())),
        )
        .collect();
    for (name, spec) in &detailed {
        let spec = extjson::parse(spec).expect("JSON");
        let mut h = 0xcbf2_9ce4_8422_2325;
        let answer = match ix.search(&spec) {
            Ok(hits) => {
                for f in &hits {
                    fnv(&mut h, &f.key);
                    fnv(&mut h, &f.score.to_bits().to_le_bytes());
                    fnv(
                        &mut h,
                        format!("{:?}", f.details.as_ref().map(|d| d.to_document())).as_bytes(),
                    );
                    fnv(&mut h, format!("{:?}", f.highlights).as_bytes());
                }
                format!("{name} ({} hits)", hits.len())
            }
            Err(e) => {
                fnv(&mut h, e.0.as_bytes());
                format!("{name} (error)")
            }
        };
        found.push((answer, h));
    }
    for (name, spec) in METAS {
        let spec = extjson::parse(spec).expect("JSON");
        let mut h = 0xcbf2_9ce4_8422_2325;
        let answer = match ix.meta(&spec) {
            Ok(doc) => {
                fnv(&mut h, format!("{doc:?}").as_bytes());
                name.to_string()
            }
            Err(e) => {
                fnv(&mut h, e.0.as_bytes());
                format!("{name} (error)")
            }
        };
        found.push((answer, h));
    }
    for (name, hash) in &found {
        println!("{name}\t{hash:016x}");
    }
    let expected: BTreeMap<&str, &str> =
        FINGERPRINTS.lines().filter_map(|l| l.split_once('\t')).collect();
    assert_eq!(expected.len(), found.len());
    for (name, hash) in &found {
        assert_eq!(expected.get(name.as_str()), Some(&format!("{hash:016x}").as_str()), "{name}");
    }
}
