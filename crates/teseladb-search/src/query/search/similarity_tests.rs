//! The `similarity` of a `string` field (`bm25`, `boolean` and `stableTfl`) as `refsearch` 1.70.6
//! scores it (measured 02-11-2026 on a 9.0.4 replica set, `scratchpad/lane3o/sim2.js` and
//! `sim3.js`, whose documents and indexes these are). `stableTfl` gives each term a rarity from its
//! length and the document's, `1 - (1 - m·2^(-c·tl))^dl`, instead of the index's statistics, so
//! the scores do not depend on the other documents; `boolean` scores each matching term with its
//! boost.

use teseladb_bson::Document;

use super::*;
use crate::definition::Definition;

/// The words of `sim2.js`, each in documents of 1, 2, 3, 5, 8, 13, 40 and 200 tokens.
const WORDS: [&str; 10] = [
    "a",
    "ab",
    "fox",
    "wolf",
    "tiger",
    "jaguars",
    "elephant",
    "crocodiles",
    "hippopotamus",
    "ñandúes",
];
const LENGTHS: [usize; 8] = [1, 2, 3, 5, 8, 13, 40, 200];

fn index(similarity: &str, docs: &[(u8, String)]) -> MemIndex {
    let field = Document::new()
        .with("type", "string")
        .with("similarity", Document::new().with("type", similarity));
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", false).with("fields", Document::new().with("t", field)),
    );
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    for (id, t) in docs {
        idx.put(&[*id], &Document::new().with("t", t.as_str())).expect("indexed");
    }
    idx
}

/// The documents of `sim2.js`.
fn sim2() -> Vec<(u8, String)> {
    let mut docs = Vec::new();
    for w in WORDS {
        for len in LENGTHS {
            let mut t = vec![w.to_owned()];
            t.extend((1..len).map(|i| format!("zz{}", i % 7)));
            docs.push((docs.len() as u8 + 1, t.join(" ")));
        }
    }
    docs.push((81, "fox fox wolf".to_owned()));
    docs
}

fn found(idx: &MemIndex, spec: &Document) -> Vec<(u8, f64)> {
    let mut v: Vec<(u8, f64)> =
        run(idx, spec).expect("runs").into_iter().map(|h| (h.key[0], f64::from(h.score))).collect();
    v.sort_by_key(|h| h.0);
    v
}

fn text(query: &str) -> Document {
    Document::new().with("text", Document::new().with("path", "t").with("query", query))
}

/// `sim2.out`: similarity, query and the scores `refsearch` gives, by `_id`.
type Case = (&'static str, &'static str, &'static [(u8, f64)]);
const SIM2: &[Case] = &[
    (
        "stableTfl",
        "a",
        &[
            (1, 1.3688914775848389),
            (2, 1.335558533668518),
            (3, 1.3046280145645142),
            (4, 1.2487720251083374),
            (5, 1.176761507987976),
            (6, 1.0789201259613037),
            (7, 0.7686142921447754),
            (8, 0.2654029428958893),
        ],
    ),
    (
        "stableTfl",
        "ab",
        &[
            (9, 1.3855286836624146),
            (10, 1.3668797016143799),
            (11, 1.3490042686462402),
            (12, 1.315340280532837),
            (13, 1.2693427801132202),
            (14, 1.2022696733474731),
            (15, 0.9569798707962036),
            (16, 0.4541035294532776),
        ],
    ),
    (
        "stableTfl",
        "fox",
        &[
            (17, 1.3945927619934082),
            (18, 1.3844119310379028),
            (19, 1.3744654655456543),
            (20, 1.3552354574203491),
            (21, 1.3279119729995728),
            (22, 1.2859200239181519),
            (23, 1.1114921569824219),
            (24, 0.6605755686759949),
            (81, 1.8898900747299194),
        ],
    ),
    (
        "stableTfl",
        "wolf",
        &[
            (25, 1.3994675874710083),
            (26, 1.3939862251281738),
            (27, 1.388573408126831),
            (28, 1.377947211265564),
            (29, 1.3624831438064575),
            (30, 1.3378832340240479),
            (31, 1.225070834159851),
            (32, 0.8604878187179565),
            (81, 1.388573408126831),
        ],
    ),
    (
        "stableTfl",
        "tiger",
        &[
            (33, 1.4020705223083496),
            (34, 1.399141788482666),
            (35, 1.3962327241897583),
            (36, 1.3904727697372437),
            (37, 1.381974220275879),
            (38, 1.3681715726852417),
            (39, 1.3004443645477295),
            (40, 1.0343226194381714),
        ],
    ),
    (
        "stableTfl",
        "jaguars",
        &[
            (41, 1.404190182685852),
            (42, 1.40336275100708),
            (43, 1.4025369882583618),
            (44, 1.4008901119232178),
            (45, 1.3984313011169434),
            (46, 1.3943642377853394),
            (47, 1.3730400800704956),
            (48, 1.264654278755188),
        ],
    ),
    (
        "stableTfl",
        "elephant",
        &[
            (49, 1.404579997062683),
            (50, 1.4041411876678467),
            (51, 1.4037028551101685),
            (52, 1.402827501296997),
            (53, 1.4015177488327026),
            (54, 1.399343729019165),
            (55, 1.3877888917922974),
            (56, 1.3250230550765991),
        ],
    ),
    (
        "stableTfl",
        "crocodiles",
        &[
            (57, 1.4048960208892822),
            (58, 1.4047727584838867),
            (59, 1.4046496152877808),
            (60, 1.4044033288955688),
            (61, 1.4040342569351196),
            (62, 1.4034197330474854),
            (63, 1.4001166820526123),
            (64, 1.3810445070266724),
        ],
    ),
    (
        "stableTfl",
        "hippopotamus",
        &[
            (65, 1.404984712600708),
            (66, 1.4049501419067383),
            (67, 1.4049155712127686),
            (68, 1.404846429824829),
            (69, 1.40474271774292),
            (70, 1.4045701026916504),
            (71, 1.4036386013031006),
            (72, 1.3981595039367676),
        ],
    ),
    (
        "stableTfl",
        "ñandúes",
        &[
            (73, 1.404190182685852),
            (74, 1.40336275100708),
            (75, 1.4025369882583618),
            (76, 1.4008901119232178),
            (77, 1.3984313011169434),
            (78, 1.3943642377853394),
            (79, 1.3730400800704956),
            (80, 1.264654278755188),
        ],
    ),
    (
        "stableTfl",
        "fox wolf",
        &[
            (17, 1.3945927619934082),
            (18, 1.3844119310379028),
            (19, 1.3744654655456543),
            (20, 1.3552354574203491),
            (21, 1.3279119729995728),
            (22, 1.2859200239181519),
            (23, 1.1114921569824219),
            (24, 0.6605755686759949),
            (25, 1.3994675874710083),
            (26, 1.3939862251281738),
            (27, 1.388573408126831),
            (28, 1.377947211265564),
            (29, 1.3624831438064575),
            (30, 1.3378832340240479),
            (31, 1.225070834159851),
            (32, 0.8604878187179565),
            (81, 3.278463363647461),
        ],
    ),
    (
        "boolean",
        "a",
        &[(1, 1.0), (2, 1.0), (3, 1.0), (4, 1.0), (5, 1.0), (6, 1.0), (7, 1.0), (8, 1.0)],
    ),
    (
        "boolean",
        "ab",
        &[(9, 1.0), (10, 1.0), (11, 1.0), (12, 1.0), (13, 1.0), (14, 1.0), (15, 1.0), (16, 1.0)],
    ),
    (
        "boolean",
        "fox",
        &[
            (17, 1.0),
            (18, 1.0),
            (19, 1.0),
            (20, 1.0),
            (21, 1.0),
            (22, 1.0),
            (23, 1.0),
            (24, 1.0),
            (81, 1.0),
        ],
    ),
    (
        "boolean",
        "wolf",
        &[
            (25, 1.0),
            (26, 1.0),
            (27, 1.0),
            (28, 1.0),
            (29, 1.0),
            (30, 1.0),
            (31, 1.0),
            (32, 1.0),
            (81, 1.0),
        ],
    ),
    (
        "boolean",
        "tiger",
        &[(33, 1.0), (34, 1.0), (35, 1.0), (36, 1.0), (37, 1.0), (38, 1.0), (39, 1.0), (40, 1.0)],
    ),
    (
        "boolean",
        "jaguars",
        &[(41, 1.0), (42, 1.0), (43, 1.0), (44, 1.0), (45, 1.0), (46, 1.0), (47, 1.0), (48, 1.0)],
    ),
    (
        "boolean",
        "elephant",
        &[(49, 1.0), (50, 1.0), (51, 1.0), (52, 1.0), (53, 1.0), (54, 1.0), (55, 1.0), (56, 1.0)],
    ),
    (
        "boolean",
        "crocodiles",
        &[(57, 1.0), (58, 1.0), (59, 1.0), (60, 1.0), (61, 1.0), (62, 1.0), (63, 1.0), (64, 1.0)],
    ),
    (
        "boolean",
        "hippopotamus",
        &[(65, 1.0), (66, 1.0), (67, 1.0), (68, 1.0), (69, 1.0), (70, 1.0), (71, 1.0), (72, 1.0)],
    ),
    (
        "boolean",
        "ñandúes",
        &[(73, 1.0), (74, 1.0), (75, 1.0), (76, 1.0), (77, 1.0), (78, 1.0), (79, 1.0), (80, 1.0)],
    ),
    (
        "boolean",
        "fox wolf",
        &[
            (17, 1.0),
            (18, 1.0),
            (19, 1.0),
            (20, 1.0),
            (21, 1.0),
            (22, 1.0),
            (23, 1.0),
            (24, 1.0),
            (25, 1.0),
            (26, 1.0),
            (27, 1.0),
            (28, 1.0),
            (29, 1.0),
            (30, 1.0),
            (31, 1.0),
            (32, 1.0),
            (81, 2.0),
        ],
    ),
];

#[test]
fn stable_tfl_and_boolean_term_scores_match_refsearch() {
    let docs = sim2();
    let indexes = [("stableTfl", index("stableTfl", &docs)), ("boolean", index("boolean", &docs))];
    for &(similarity, query, want) in SIM2 {
        let idx = &indexes.iter().find(|(s, _)| *s == similarity).expect("an index").1;
        assert_eq!(found(idx, &text(query)), want.to_vec(), "{similarity} {query:?}");
    }
}

#[test]
fn stable_tfl_and_boolean_phrases_fuzzy_terms_and_boosts_match_refsearch() {
    let docs: Vec<(u8, String)> =
        ["quick brown fox", "the quick brown fox quick brown dog", "foxes and a fax", "quick"]
            .iter()
            .enumerate()
            .map(|(i, t)| (i as u8 + 1, (*t).to_owned()))
            .collect();
    let phrase = Document::new()
        .with("phrase", Document::new().with("path", "t").with("query", "quick brown"));
    let fuzzy = Document::new().with(
        "text",
        Document::new()
            .with("path", "t")
            .with("query", "fox")
            .with("fuzzy", Document::new().with("maxEdits", 1)),
    );
    let boosted = Document::new().with(
        "text",
        Document::new()
            .with("path", "t")
            .with("query", "quick")
            .with("score", Document::new().with("boost", Document::new().with("value", 2))),
    );
    let stable = index("stableTfl", &docs);
    assert_eq!(found(&stable, &phrase), vec![(1, 2.7924654483795166), (2, 3.808168649673462)]);
    assert_eq!(
        found(&stable, &fuzzy),
        vec![(1, 1.3744654655456543), (2, 1.3368281126022339), (3, 0.909828782081604)]
    );
    assert_eq!(
        found(&stable, &boosted),
        vec![(1, 2.7924654483795166), (2, 3.808168649673462), (4, 2.804141044616699)]
    );
    let boolean = index("boolean", &docs);
    assert_eq!(found(&boolean, &phrase), vec![(1, 1.0), (2, 1.0)]);
    assert_eq!(found(&boolean, &fuzzy), vec![(1, 1.0), (2, 1.0), (3, 0.6666666269302368)]);
    assert_eq!(found(&boolean, &boosted), vec![(1, 2.0), (2, 2.0), (4, 2.0)]);
}

#[test]
fn a_similarity_is_one_of_three_and_shows_in_the_definition() {
    let def = |s: Document| {
        let field = Document::new().with("type", "string").with("similarity", s);
        Document::new().with(
            "mappings",
            Document::new().with("dynamic", false).with("fields", Document::new().with("t", field)),
        )
    };
    let shown = Definition::parse(&def(Document::new().with("type", "stableTfl")))
        .expect("valid")
        .normalized();
    let text = format!("{shown:?}");
    assert!(text.contains("stableTfl"), "{text}");
    assert!(Definition::parse(&def(Document::new().with("type", "tfidf"))).is_err());
    assert!(Definition::parse(&def(Document::new().with("kind", "bm25"))).is_err());
}

/// The leaves of an explanation, depth first, as `(value, description)`.
fn leaves(e: &crate::index::Explanation, out: &mut Vec<(f64, String)>) {
    out.push((f64::from(e.value), e.description.clone()));
    for d in &e.details {
        leaves(d, out);
    }
}

#[test]
fn stable_tfl_and_boolean_explanations_match_refsearch() {
    let docs: Vec<(u8, String)> =
        vec![(1, "quick brown fox".into()), (2, "the quick brown fox quick brown dog".into())];
    let explained = |idx: &MemIndex, spec: &Document, key: u8| {
        let got = explain(idx, spec, &[vec![key]]).expect("explained");
        let mut out = Vec::new();
        leaves(&got.into_iter().next().expect("one").1, &mut out);
        out
    };
    let phrase = Document::new()
        .with("phrase", Document::new().with("path", "t").with("query", "quick brown"));
    let stable = explained(&index("stableTfl", &docs), &phrase, 2);
    let p = "p, probability that the term appears in the doc, computed as 1 - (1 - m * 2 ^ (-c * \
             tl)) ^ dl from:";
    let rarity = "tr, term rarity, computed as log(1 + (1 - p + 0.05) / (p + 0.05)) from:";
    let term = |out: &mut Vec<(f64, String)>| {
        out.push((3.046535015106201, rarity.into()));
        out.push((0.002275634789839387, p.into()));
        out.push((
            0.007809999864548445,
            "m, multiplicative constant to term mismatch probability".into(),
        ));
        out.push((0.9169999957084656, "c, decaying constant for term length".into()));
        out.push((5.0, "tl, term length".into()));
        out.push((7.0, "dl, document length".into()));
    };
    let mut want: Vec<(f64, String)> = vec![
        (
            3.808168649673462,
            "$type:string/t:\"quick brown\" [SimilarityWrapper], result of:".into(),
        ),
        (3.808168649673462, "score(freq=2.0), computed as boost * tr * tf from:".into()),
        (1.0, "boost".into()),
        (0.625, "tf, computed as freq / (freq + k1)) from:".into()),
        (2.0, "phraseFreq=2.0".into()),
        (1.2000000476837158, "k1, term saturation parameter".into()),
        (6.093070030212402, "tr, term rarity, computed as the sum of:".into()),
    ];
    term(&mut want);
    term(&mut want);
    assert_eq!(stable, want);
    let boolean = explained(&index("boolean", &docs), &text("quick"), 1);
    assert_eq!(
        boolean,
        vec![
            (1.0, "$type:string/t:quick [BooleanSimilarity], result of:".into()),
            (1.0, "score(BooleanWeight), computed from:".into()),
            (1.0, "boost, query boost".into()),
        ]
    );
}
