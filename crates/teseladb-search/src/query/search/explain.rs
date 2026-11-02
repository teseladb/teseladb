//! `scoreDetails` (011 FR-003, FR-016, T025): the explanation of each result's score as
//! `refsearch` 1.70.6 gives it, RefLib's `Explanation` of the query it builds (measured
//! 02-11-2026, `scratchpad/lane3k/m15.js` and `scratchpad/lane3l/m18.js` to `m22.js`, kept in
//! `tests/fixtures/refsearch/score_details/`). Each operator has the description RefLib writes for
//! its query and its own tree; a score option changes both. What RefLib takes from its memory and
//! not from the data (the document number of a segment after `in`, the identity of an
//! automaton) is written with TeselaDB's document number (DV-083).

use std::collections::BTreeMap;

use super::{Operator, Score, SearchError, eval};
use crate::index::{Explanation, MemIndex, PhraseAt, java_float};

/// Where an explanation is built: the index, the prefix of the field names (the elements of an
/// `embeddedDocuments` path) and whether term queries show as `weight(… in n)`, as they do
/// inside a boosted `compound` of several clauses.
#[derive(Clone)]
pub(super) struct Ctx<'a> {
    pub(super) index: &'a MemIndex,
    pub(super) prefix: String,
    pub(super) weighted: bool,
    /// The levels from the documents down to the elements `index` holds.
    pub(super) levels: Vec<super::embedded::Level<'a>>,
}

impl Ctx<'_> {
    /// The name of the `string` field at `path` in queries.
    pub(super) fn name(&self, path: &str) -> String {
        format!("{}$type:string/{path}", self.prefix)
    }

    /// The description of a term query of `name`, as a weight when `self.weighted`.
    pub(super) fn term_description(&self, name: &str, term: &str, doc: u32) -> String {
        if self.weighted {
            format!("weight({name}:{term} in {doc})")
        } else {
            format!("{name}:{term}")
        }
    }

    /// The explanation of term `term` of the field at `path` for `doc`.
    pub(super) fn term(&self, path: &str, term: &str, boost: f32, doc: u32) -> Option<Explanation> {
        let description = self.term_description(&self.name(path), term, doc);
        self.index.explain_term_as(path, description, term, boost, doc)
    }
}

/// What a query is and how it nests in another one's description.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Desc {
    /// A single term query: under `ConstantScore` it explains as `… [BM25Similarity]`.
    Term(String),
    /// A query of several clauses: in parentheses inside another.
    Bool(String),
    /// Another query.
    Other(String),
}

impl Desc {
    pub(super) fn text(&self) -> &str {
        match self {
            Desc::Term(t) | Desc::Bool(t) | Desc::Other(t) => t,
        }
    }

    /// The text inside another query's description.
    fn nested(&self) -> String {
        match self {
            Desc::Bool(t) => format!("({t})"),
            Desc::Term(t) | Desc::Other(t) => t.clone(),
        }
    }
}

/// A query of constant score: how it explains without a score option.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Leaf {
    /// `ConstantScore(x) [BM25Similarity], result of:`.
    Term(String),
    /// `ConstantScore(x)`, `^v` after it with a boost or a constant.
    Wrapped(String),
    /// `x`, `^v` after it with a boost, `ConstantScore(x)^v` with a constant.
    Bare(String),
}

impl Leaf {
    /// The explanation of value `value` under `score`.
    pub(super) fn explain(&self, score: &Score, value: f32) -> Explanation {
        let factor = match score {
            Score::Boost(b) if *b != 1.0 => Some(*b),
            Score::Constant(c) => Some(*c),
            _ => None,
        };
        let text = match (self, factor) {
            (Leaf::Term(x), _) => format!("ConstantScore({x}) [BM25Similarity], result of:"),
            (Leaf::Wrapped(x), None) => format!("ConstantScore({x})"),
            (Leaf::Wrapped(x), Some(f)) => format!("ConstantScore({x})^{}", java_float(f)),
            (Leaf::Bare(x), None) => x.clone(),
            (Leaf::Bare(x), Some(f)) if matches!(score, Score::Boost(_)) => {
                format!("{x}^{}", java_float(f))
            }
            (Leaf::Bare(x), Some(f)) => format!("ConstantScore({x})^{}", java_float(f)),
        };
        Explanation::leaf(value, text)
    }

    /// The query it is.
    pub(super) fn desc(&self) -> Desc {
        match self {
            Leaf::Term(x) => Desc::Term(x.clone()),
            Leaf::Wrapped(x) => Desc::Bool(x.clone()),
            Leaf::Bare(x) => Desc::Other(x.clone()),
        }
    }

    /// The query inside its `ConstantScore`, as a filter names it.
    fn inner(&self) -> String {
        match self {
            Leaf::Term(x) | Leaf::Wrapped(x) | Leaf::Bare(x) => x.clone(),
        }
    }
}

/// The explanations of `op`'s scores for `docs`.
pub(super) fn explain_all(
    index: &MemIndex,
    op: &Operator,
    docs: &[u32],
) -> Result<BTreeMap<u32, Explanation>, SearchError> {
    let levels = vec![super::embedded::Level { path: "", index }];
    let ctx = Ctx { index, prefix: String::new(), weighted: false, levels };
    let scores = eval(index, op)?;
    let mut out = BTreeMap::new();
    for &doc in docs {
        let Some(&score) = scores.get(&doc) else { continue };
        if let Some(e) = explain(&ctx, op, doc, score)? {
            out.insert(doc, e);
        }
    }
    Ok(out)
}

/// The explanation of `op` for `doc`, which it scores `score`.
pub(super) fn explain(
    ctx: &Ctx<'_>,
    op: &Operator,
    doc: u32,
    score: f32,
) -> Result<Option<Explanation>, SearchError> {
    if let Some(leaf) = leaf_of(ctx, op) {
        let how = score_of(op).unwrap_or(&Score::Plain);
        return Ok(Some(match how {
            Score::Function(_) | Score::BoostPath(_) => modified(ctx, op, how, doc, score)?,
            _ => leaf.explain(how, score),
        }));
    }
    match score_of(op) {
        Some(how @ (Score::Constant(_) | Score::Function(_) | Score::BoostPath(_))) => {
            Ok(Some(modified(ctx, op, how, doc, score)?))
        }
        Some(Score::Boost(b)) => native(ctx, op, *b, doc, score),
        _ => native(ctx, op, 1.0, doc, score),
    }
}

/// The score option of `op`, for the operators that have one.
fn score_of(op: &Operator) -> Option<&Score> {
    Some(match op {
        Operator::Text { score, .. }
        | Operator::Exists { score, .. }
        | Operator::Phrase { score, .. }
        | Operator::TermRange { score, .. }
        | Operator::Span(_, score)
        | Operator::Scored(_, score) => score,
        Operator::Pattern(p) => p.score(),
        Operator::Values(v) => v.score(),
        Operator::Geo(g) => g.score(),
        Operator::Autocomplete(a) => &a.score,
        Operator::Embedded(e) => &e.score,
        Operator::Knn(k) => &k.score,
        Operator::Synonyms(s) => &s.score,
        Operator::AllTerms(inner) => return score_of(inner),
        Operator::Empty
        | Operator::All
        | Operator::Precomputed(_)
        | Operator::Passed(_)
        | Operator::HasRoot(_)
        | Operator::HasAncestor { .. }
        | Operator::Compound { .. } => return None,
    })
}

/// The constant-score form of `op`, for the operators that score a constant.
fn leaf_of(ctx: &Ctx<'_>, op: &Operator) -> Option<Leaf> {
    match op {
        Operator::Exists { path, .. } => Some(if has_embedded(ctx.index) {
            Leaf::Wrapped(format!("#$meta/embeddedRoot:T #$meta/fieldNames:{path}"))
        } else {
            Leaf::Term(format!("$meta/fieldNames:{path}"))
        }),
        Operator::Pattern(p) => Some(Leaf::Bare(p.description(&ctx.prefix))),
        Operator::TermRange { path, lower, upper, .. } => {
            let bound = |b: &Option<(String, bool)>, open: char, closed: char, first: bool| {
                let (text, inclusive) = match b {
                    Some((t, i)) => (t.as_str(), *i),
                    None => ("*", false),
                };
                let mark = if inclusive { closed } else { open };
                if first { format!("{mark}{text}") } else { format!("{text}{mark}") }
            };
            Some(Leaf::Bare(format!(
                "{}:{} TO {}",
                ctx.name(path),
                bound(lower, '{', '[', true),
                bound(upper, '}', ']', false)
            )))
        }
        Operator::Values(v) => super::describe::values_leaf(ctx, v),
        Operator::Geo(g) => g.leaf(),
        _ => None,
    }
}

/// Whether the index's definition has an `embeddedDocuments` field.
fn has_embedded(index: &MemIndex) -> bool {
    fn any(m: &crate::definition::Mapping) -> bool {
        m.fields.iter().flat_map(|(_, ts)| ts).any(|t| match t {
            crate::definition::FieldMapping::EmbeddedDocuments(_) => true,
            crate::definition::FieldMapping::Document(inner) => any(inner),
            _ => false,
        })
    }
    any(&index.definition().mappings)
}

/// The description of `op`'s query.
pub(super) fn describe(ctx: &Ctx<'_>, op: &Operator) -> Desc {
    if let Some(leaf) = leaf_of(ctx, op) {
        return leaf.desc();
    }
    match op {
        Operator::Text { query, paths, fuzzy: None, .. } => {
            let per_path: Vec<Vec<String>> = paths
                .iter()
                .map(|p| {
                    let name = ctx.name(p);
                    query
                        .iter()
                        .flat_map(|q| ctx.index.single_terms(p, q))
                        .map(|t| format!("{name}:{t}"))
                        .collect()
                })
                .collect();
            boolean(per_path.into_iter().map(|terms| terms_desc(&terms)).collect())
        }
        Operator::Text { query, paths, fuzzy: Some(f), .. } => {
            let mut all = Vec::new();
            for p in paths {
                for q in query {
                    for t in ctx.index.query_terms(p, q).unwrap_or_default() {
                        let kept = ctx.index.fuzzy_terms(p, &t, f);
                        let parts: Vec<String> = kept
                            .iter()
                            .map(|(term, w)| {
                                let one = format!("{}:{term}", ctx.name(p));
                                if *w == 1.0 { one } else { format!("({one})^{}", java_float(*w)) }
                            })
                            .collect();
                        all.push(terms_desc(&parts));
                    }
                }
            }
            boolean(all)
        }
        Operator::Phrase { query, paths, slop, .. } => {
            let mut all = Vec::new();
            for p in paths {
                for q in query {
                    let terms = ctx.index.query_terms(p, q).unwrap_or_default();
                    let slop = if *slop > 0 { format!("~{slop}") } else { String::new() };
                    let name = ctx.name(p);
                    all.push(match terms.as_slice() {
                        [one] => Desc::Term(format!("{name}:{one}")),
                        _ => Desc::Other(format!("{name}:\"{}\"{slop}", terms.join(" "))),
                    });
                }
            }
            boolean(all)
        }
        Operator::Synonyms(s) => {
            let text = s.description(ctx.index, &|p: &str| ctx.name(p));
            if text.contains(' ') { Desc::Bool(text) } else { Desc::Term(text) }
        }
        Operator::Span(s, _) => Desc::Other(s.description(ctx)),
        Operator::Compound { must, should, filter, must_not, minimum, .. } => {
            if must.len() + should.len() + filter.len() + must_not.len() == 1
                && let Some(only) = must.first().or(should.first())
            {
                return describe(ctx, only);
            }
            // Measured under a constant score: every clause filters, the filters first.
            let should_mark = if *minimum > 0 { "#" } else { "" };
            let mut parts = Vec::new();
            let mut add = |prefix: &str, ops: &[Operator]| {
                for o in ops {
                    parts.push(format!("{prefix}{}", describe(ctx, o).nested()));
                }
            };
            add("#", filter);
            add("#", must);
            add("-", must_not);
            add(should_mark, should);
            Desc::Bool(parts.join(" "))
        }
        Operator::Scored(inner, _) | Operator::AllTerms(inner) => describe(ctx, inner),
        Operator::Values(v) => super::describe::near_description(ctx, v)
            .unwrap_or_else(|| Desc::Other(operator_name(op).to_owned())),
        other => Desc::Other(operator_name(other).to_owned()),
    }
}

/// The description of term queries `terms` as one clause: the term, or their disjunction.
fn terms_desc(terms: &[String]) -> Desc {
    match terms {
        [one] => Desc::Term(one.clone()),
        many => Desc::Bool(many.join(" ")),
    }
}

/// The disjunction of `clauses`, or the clause alone.
fn boolean(mut clauses: Vec<Desc>) -> Desc {
    if clauses.len() == 1 {
        return clauses.remove(0);
    }
    Desc::Bool(clauses.iter().map(Desc::nested).collect::<Vec<_>>().join(" "))
}

/// A name for an operator whose query has no measured description.
fn operator_name(op: &Operator) -> &'static str {
    match op {
        Operator::Knn(_) => "knnBeta",
        Operator::Empty => "empty",
        Operator::All => "*:*",
        Operator::Embedded(_) => "embeddedDocument",
        Operator::HasRoot(_) => "hasRoot",
        Operator::HasAncestor { .. } => "hasAncestor",
        Operator::Precomputed(_) | Operator::Passed(_) => "precomputed",
        Operator::Autocomplete(_) => "autocomplete",
        _ => "query",
    }
}

/// The explanation of `op` under a constant, a function or a boost by a path.
fn modified(
    ctx: &Ctx<'_>,
    op: &Operator,
    how: &Score,
    doc: u32,
    score: f32,
) -> Result<Explanation, SearchError> {
    let desc = describe(ctx, op);
    let head = |by: &str| match &desc {
        Desc::Term(t) => {
            format!("FunctionScoreQuery({t}, scored by {by}) [BM25Similarity], result of:")
        }
        other => {
            format!("weight(FunctionScoreQuery({}, scored by {by})), result of:", other.text())
        }
    };
    Ok(match how {
        Score::Constant(c) => match &desc {
            Desc::Term(t) => {
                Explanation::leaf(score, format!("ConstantScore({t}) [BM25Similarity], result of:"))
            }
            other => Explanation::leaf(
                score,
                format!("ConstantScore({})^{}", other.text(), java_float(*c)),
            ),
        },
        Score::Function(f) => {
            let text = f.description();
            Explanation::node(score, head(&text), vec![Explanation::leaf(score, text)])
        }
        Score::BoostPath(p) => {
            let weighted = Ctx { weighted: true, ..ctx.clone() };
            let plain = super::eval(ctx.index, &without_score(op))?;
            let relevance = plain.get(&doc).copied().unwrap_or(0.0);
            let inner = native(&weighted, &without_score(op), 1.0, doc, relevance)?;
            let factor = super::number_at(ctx.index.document(doc), p);
            let product = Explanation::node(
                score,
                "product of:",
                inner.into_iter().chain([Explanation::leaf(factor, p.clone())]).collect(),
            );
            Explanation::node(score, head(&format!("boost({p})")), vec![product])
        }
        Score::Plain | Score::Boost(_) => Explanation::leaf(score, desc.text().to_owned()),
    })
}

/// `op` with its score option plain.
fn without_score(op: &Operator) -> Operator {
    let mut out = op.clone();
    match &mut out {
        Operator::Text { score, .. }
        | Operator::Exists { score, .. }
        | Operator::Phrase { score, .. }
        | Operator::TermRange { score, .. }
        | Operator::Span(_, score) => *score = Score::Plain,
        Operator::Synonyms(s) => s.score = Score::Plain,
        Operator::Scored(inner, _) => return (**inner).clone(),
        _ => {}
    }
    out
}

/// A node adding `parts` (a sum in doubles, as RefLib adds the clauses of a query).
pub(super) fn sum(parts: Vec<Explanation>) -> Explanation {
    let total: f64 = parts.iter().map(|p| f64::from(p.value)).sum();
    Explanation::node(total as f32, "sum of:", parts)
}

/// The explanation of `op` with weight `boost`, scored `score`, without a score option.
fn native(
    ctx: &Ctx<'_>,
    op: &Operator,
    boost: f32,
    doc: u32,
    score: f32,
) -> Result<Option<Explanation>, SearchError> {
    Ok(match op {
        Operator::Text { query, paths, fuzzy: None, .. } => {
            let per_path: Vec<(usize, Vec<Explanation>)> = paths
                .iter()
                .map(|p| {
                    let terms: Vec<String> =
                        query.iter().flat_map(|q| ctx.index.single_terms(p, q)).collect();
                    let found = terms.iter().filter_map(|t| ctx.term(p, t, boost, doc)).collect();
                    (terms.len(), found)
                })
                .collect();
            let units: Vec<Explanation> = per_path
                .into_iter()
                .filter(|(_, found)| !found.is_empty())
                .map(|(n, mut found)| if n == 1 { found.remove(0) } else { sum(found) })
                .collect();
            group(paths.len(), units)
        }
        Operator::Text { query, paths, fuzzy: Some(f), .. } => {
            let mut units = Vec::new();
            let mut count = 0;
            for p in paths {
                for q in query {
                    for t in ctx.index.query_terms(p, q)? {
                        count += 1;
                        let kept = ctx.index.fuzzy_terms(p, &t, f);
                        let weighted =
                            Ctx { weighted: ctx.weighted || kept.len() > 1, ..ctx.clone() };
                        let found: Vec<Explanation> = kept
                            .iter()
                            .filter_map(|(term, w)| {
                                let description =
                                    weighted.term_description(&ctx.name(p), term, doc);
                                ctx.index.explain_fuzzy(p, description, term, &kept, boost * w, doc)
                            })
                            .collect();
                        match (kept.len(), found.len()) {
                            (_, 0) => {}
                            (1, _) => units.extend(found),
                            _ => units.push(sum(found)),
                        }
                    }
                }
            }
            group(count, units)
        }
        Operator::Phrase { query, paths, slop, .. } => {
            let mut units = Vec::new();
            for p in paths {
                for q in query {
                    let name = ctx.name(p);
                    let at = PhraseAt {
                        key: p,
                        name: &name,
                        text: q,
                        slop: *slop,
                        weighted: ctx.weighted,
                    };
                    if let Some(e) = ctx.index.explain_phrase(&at, boost, doc)? {
                        units.push(e);
                    }
                }
            }
            group(paths.len() * query.len(), units)
        }
        Operator::Synonyms(s) => s.explain(ctx.index, boost, doc, &|p: &str| ctx.name(p))?,
        Operator::Span(s, _) => s.explain(ctx, boost, doc)?,
        Operator::Knn(_) => {
            let k = eval(ctx.index, op)?.len();
            Some(Explanation::leaf(score, format!("within top {k} docs")))
        }
        Operator::Values(v) => super::describe::near(ctx, v, boost, doc, score),
        Operator::Geo(g) => g.explain_near(ctx.index, boost, doc, score),
        Operator::Autocomplete(a) => a.explain(ctx, boost, doc)?,
        Operator::Embedded(e) => e.explain(ctx, boost, doc, score)?,
        Operator::Compound { .. } => compound(ctx, op, doc, score)?,
        Operator::Scored(inner, _) | Operator::AllTerms(inner) => {
            native(ctx, inner, boost, doc, score)?
        }
        Operator::Passed(p) => p.explain(ctx, doc, score)?,
        Operator::Precomputed(_) => Some(Explanation::leaf(score, "Score based on child docs")),
        _ => Some(Explanation::leaf(score, describe(ctx, op).text().to_owned())),
    })
}

/// The explanations of a query's `count` clauses that matched: the clause's alone when there is
/// one, their sum otherwise.
fn group(count: usize, mut units: Vec<Explanation>) -> Option<Explanation> {
    match (count, units.len()) {
        (_, 0) => None,
        (1, _) => Some(units.remove(0)),
        _ => Some(sum(units)),
    }
}

/// The explanation of `compound` `op`: a clause alone, a filter alone as a constant 0, or the
/// sum of the filters' matches and the scoring clauses.
fn compound(
    ctx: &Ctx<'_>,
    op: &Operator,
    doc: u32,
    score: f32,
) -> Result<Option<Explanation>, SearchError> {
    let Operator::Compound { must, should, filter, must_not, weighted, .. } = op else {
        return Ok(None);
    };
    let total = must.len() + should.len() + filter.len() + must_not.len();
    let inner = Ctx { weighted: ctx.weighted || (*weighted && total > 1), ..ctx.clone() };
    let scored = |o: &Operator| -> Result<Option<Explanation>, SearchError> {
        let s = eval(ctx.index, o)?;
        match s.get(&doc) {
            Some(&v) => explain(&inner, o, doc, v),
            None => Ok(None),
        }
    };
    if total == 1 {
        if let Some(o) = must.first().or(should.first()) {
            return scored(o);
        }
        if let Some(o) = filter.first() {
            let text = format!("ConstantScore(ScoreDetailsWrapped ({}))^0.0", wrapped(ctx, o));
            return Ok(Some(Explanation::leaf(0.0, text)));
        }
    }
    let mut parts = Vec::new();
    for f in filter {
        let name = match leaf_of(ctx, f) {
            Some(leaf) => leaf.inner(),
            None => describe(ctx, f).text().to_owned(),
        };
        parts.push(Explanation::node(
            0.0,
            "match on required clause, product of:",
            vec![Explanation::leaf(0.0, "# clause"), Explanation::leaf(1.0, name)],
        ));
    }
    for o in must.iter().chain(should) {
        parts.extend(scored(o)?);
    }
    let mut e = sum(parts);
    e.value = score;
    Ok(Some(e))
}

/// How a filter clause names `op` inside `ScoreDetailsWrapped`.
fn wrapped(ctx: &Ctx<'_>, op: &Operator) -> String {
    match op {
        Operator::Compound { must, should, filter, must_not, .. } => {
            let mut parts = Vec::new();
            let mut add = |prefix: &str, ops: &[Operator]| {
                for o in ops {
                    parts.push(format!("{prefix}ScoreDetailsWrapped ({})", wrapped(ctx, o)));
                }
            };
            add("+", must);
            add("", should);
            add("#", filter);
            add("-", must_not);
            parts.join(" ")
        }
        other => match leaf_of(ctx, other) {
            Some(Leaf::Term(x)) => format!("ConstantScore({x})"),
            Some(leaf) => leaf.desc().text().to_owned(),
            None => describe(ctx, other).text().to_owned(),
        },
    }
}
