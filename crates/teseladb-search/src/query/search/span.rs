//! The deprecated `span` operator (011 FR-002) as `refsearch` 1.70.6 runs it (measured 02-11-2026,
//! `scratchpad/lane3j/m3.js`): RefLib's span queries over the positions of one field. A document's
//! frequency adds `1 / (1 + width)` for each span it holds, the width being the positions a
//! `near` match leaves between its clauses, and the weight is the sum of the idf of the terms of
//! the query (those of `include` alone for `subtract`).

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::{Operator, SearchError, parse_score, shape};
use crate::index::MemIndex;

/// The kinds of span, in the order `refsearch` names them.
const KINDS: [&str; 6] = ["contains", "first", "near", "or", "subtract", "term"];

/// Default `endPositionLte` of `first`.
const DEFAULT_END: u32 = 3;

/// A span query.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Span {
    Term { path: String, query: String },
    First { op: Box<Span>, end: u32 },
    Near { clauses: Vec<Span>, slop: u32, in_order: bool },
    Or(Vec<Span>),
    Subtract { include: Box<Span>, exclude: Box<Span> },
    Contains { little: Box<Span>, big: Box<Span>, outer: bool },
}

/// One match: its first position, the position after it, and its width.
type Match = (u32, u32, u32);

/// The `span` operator with its options `d` at `path` of the query.
pub(super) fn parse(d: &Document, path: &str) -> Result<Operator, SearchError> {
    let score = parse_score(d.get("score"), &format!("{path}.score"))?;
    let mut without_score = Document::new();
    for (k, v) in d.iter().filter(|(k, _)| *k != "score") {
        without_score.push(k, v.clone());
    }
    Ok(Operator::Span(parse_span(&without_score, path)?, score))
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(f) => Some(*f),
        _ => None,
    }
}

fn sub(d: &Document, at: &str, field: &str) -> Result<Span, SearchError> {
    let at = format!("{at}.{field}");
    match d.get(field) {
        None => Err(shape(&at, "is required")),
        Some(Value::Document(s)) => parse_span(s, &at),
        Some(_) => Err(shape(&at, "must be a document")),
    }
}

fn clauses(d: &Document, at: &str) -> Result<Vec<Span>, SearchError> {
    let at = format!("{at}.clauses");
    match d.get("clauses") {
        None => Err(shape(&at, "is required")),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Document(s) => parse_span(s, &at),
                _ => Err(shape(&at, "must be a document")),
            })
            .collect(),
        Some(_) => Err(shape(&at, "must be an array")),
    }
}

fn parse_span(d: &Document, at: &str) -> Result<Span, SearchError> {
    let kinds: Vec<&str> = d.iter().map(|(k, _)| k).filter(|k| KINDS.contains(k)).collect();
    let [kind] = kinds.as_slice() else {
        let how = if kinds.is_empty() { "one of" } else { "only one of" };
        let end = if kinds.is_empty() { "must be present" } else { "may be present" };
        return Err(shape(at, &format!("{how} [{}] {end}", KINDS.join(", "))));
    };
    let inner_at = format!("{at}.{kind}");
    let Some(Value::Document(o)) = d.get(kind) else {
        return Err(shape(&inner_at, "must be a document"));
    };
    let count = |f: &str, default: u32| match o.get(f) {
        None => Ok(default),
        Some(v) => number(v)
            .map(|n| n.max(0.0) as u32)
            .ok_or_else(|| shape(&format!("{inner_at}.{f}"), "must be a number")),
    };
    match *kind {
        "term" => {
            let string = |f: &str| match o.get(f) {
                None => Err(shape(&format!("{inner_at}.{f}"), "is required")),
                Some(Value::String(s)) => Ok(s.clone()),
                Some(_) => Err(shape(&format!("{inner_at}.{f}"), "must be a string")),
            };
            Ok(Span::Term { path: string("path")?, query: string("query")? })
        }
        "first" => Ok(Span::First {
            op: Box::new(sub(o, &inner_at, "operator")?),
            end: count("endPositionLte", DEFAULT_END)?,
        }),
        "near" => Ok(Span::Near {
            clauses: clauses(o, &inner_at)?,
            slop: count("slop", 0)?,
            in_order: matches!(o.get("inOrder"), Some(Value::Boolean(true))),
        }),
        "or" => Ok(Span::Or(clauses(o, &inner_at)?)),
        "subtract" => Ok(Span::Subtract {
            include: Box::new(sub(o, &inner_at, "include")?),
            exclude: Box::new(sub(o, &inner_at, "exclude")?),
        }),
        _ => Ok(Span::Contains {
            little: Box::new(sub(o, &inner_at, "little")?),
            big: Box::new(sub(o, &inner_at, "big")?),
            outer: !matches!(o.get("spanToReturn"), Some(Value::String(s)) if s == "inner"),
        }),
    }
}

impl Span {
    /// The field of the query, which every term shares.
    fn path(&self) -> &str {
        match self {
            Span::Term { path, .. } => path,
            Span::First { op, .. } => op.path(),
            Span::Near { clauses, .. } | Span::Or(clauses) => {
                clauses.first().map_or("", |c| c.path())
            }
            Span::Subtract { include, .. } => include.path(),
            Span::Contains { big, .. } => big.path(),
        }
    }

    /// Whether every term of the query is on `path`.
    fn on(&self, path: &str) -> bool {
        match self {
            Span::Term { path: p, .. } => p == path,
            Span::First { op, .. } => op.on(path),
            Span::Near { clauses, .. } | Span::Or(clauses) => clauses.iter().all(|c| c.on(path)),
            Span::Subtract { include, exclude } => include.on(path) && exclude.on(path),
            Span::Contains { little, big, .. } => little.on(path) && big.on(path),
        }
    }

    /// The terms whose idf weighs the query, added to `out`.
    fn weight_terms(&self, index: &MemIndex, out: &mut Vec<String>) -> Result<(), SearchError> {
        match self {
            Span::Term { path, query } => {
                for t in index.query_terms(path, query)? {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
            Span::First { op, .. } => op.weight_terms(index, out)?,
            Span::Near { clauses, .. } | Span::Or(clauses) => {
                for c in clauses {
                    c.weight_terms(index, out)?;
                }
            }
            Span::Subtract { include, .. } => include.weight_terms(index, out)?,
            Span::Contains { little, big, .. } => {
                big.weight_terms(index, out)?;
                little.weight_terms(index, out)?;
            }
        }
        Ok(())
    }

    /// The matches of the query in each document.
    fn matches(&self, index: &MemIndex) -> Result<BTreeMap<u32, Vec<Match>>, SearchError> {
        Ok(match self {
            Span::Term { path, query } => match index.query_terms(path, query)?.as_slice() {
                [term] => index
                    .positions(path, term)
                    .map(|docs| {
                        docs.iter()
                            .map(|(&doc, ps)| (doc, ps.iter().map(|&p| (p, p + 1, 0)).collect()))
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => BTreeMap::new(),
            },
            Span::First { op, end } => keep(op.matches(index)?, |m| m.1 <= *end),
            Span::Or(clauses) => {
                let mut all: BTreeMap<u32, Vec<Match>> = BTreeMap::new();
                for c in clauses {
                    for (doc, ms) in c.matches(index)? {
                        all.entry(doc).or_default().extend(ms);
                    }
                }
                all.values_mut().for_each(|ms| ms.sort_unstable());
                all
            }
            Span::Subtract { include, exclude } => {
                let excluded = exclude.matches(index)?;
                let mut kept = BTreeMap::new();
                for (doc, ms) in include.matches(index)? {
                    let ex = excluded.get(&doc).map_or(&[][..], Vec::as_slice);
                    let left: Vec<Match> = ms
                        .into_iter()
                        .filter(|m| !ex.iter().any(|e| e.0 < m.1 && m.0 < e.1))
                        .collect();
                    if !left.is_empty() {
                        kept.insert(doc, left);
                    }
                }
                kept
            }
            Span::Contains { little, big, outer } => {
                let littles = little.matches(index)?;
                let mut kept = BTreeMap::new();
                for (doc, bigs) in big.matches(index)? {
                    let Some(ls) = littles.get(&doc) else { continue };
                    let inside = |b: &Match, l: &Match| b.0 <= l.0 && l.1 <= b.1;
                    let found: Vec<Match> = if *outer {
                        // Measured: a containing term keeps its width (0); a containing phrase
                        // counts its whole length.
                        let whole = !matches!(**big, Span::Term { .. });
                        bigs.iter()
                            .filter(|b| ls.iter().any(|l| inside(b, l)))
                            .map(|b| if whole { (b.0, b.1, b.1 - b.0) } else { *b })
                            .collect()
                    } else {
                        ls.iter().filter(|l| bigs.iter().any(|b| inside(b, l))).copied().collect()
                    };
                    if !found.is_empty() {
                        kept.insert(doc, found);
                    }
                }
                kept
            }
            Span::Near { clauses, slop, in_order } => near(index, clauses, *slop, *in_order)?,
        })
    }

    /// How RefLib writes the span query (measured with `scoreDetails`).
    pub(super) fn description(&self, ctx: &super::explain::Ctx<'_>) -> String {
        let list = |all: &[Span]| -> String {
            all.iter().map(|c| c.description(ctx)).collect::<Vec<_>>().join(", ")
        };
        match self {
            Span::Term { path, query } => {
                let terms = ctx.index.query_terms(path, query).unwrap_or_default();
                let t = terms.first().cloned().unwrap_or_else(|| query.clone());
                format!("{}:{t}", ctx.name(path))
            }
            Span::First { op, end } => format!("spanFirst({}, {end})", op.description(ctx)),
            Span::Near { clauses, slop, in_order } => {
                format!("spanNear([{}], {slop}, {in_order})", list(clauses))
            }
            Span::Or(clauses) => format!("spanOr([{}])", list(clauses)),
            Span::Subtract { include, exclude } => {
                format!("spanNot({}, {}, 0, 0)", include.description(ctx), exclude.description(ctx))
            }
            Span::Contains { little, big, outer } => format!(
                "{}({}, {})",
                if *outer { "SpanContaining" } else { "SpanWithin" },
                big.description(ctx),
                little.description(ctx)
            ),
        }
    }

    /// The explanation of the query's score for `doc`, with weight `boost` (measured: always
    /// `weight(… in n)`, its frequency a `phraseFreq`).
    pub(super) fn explain(
        &self,
        ctx: &super::explain::Ctx<'_>,
        boost: f32,
        doc: u32,
    ) -> Result<Option<crate::index::Explanation>, SearchError> {
        let path = self.path().to_owned();
        let mut terms = Vec::new();
        self.weight_terms(ctx.index, &mut terms)?;
        let Some(ms) = self.matches(ctx.index)?.remove(&doc) else { return Ok(None) };
        let freq: f32 = ms.iter().map(|m| 1.0 / (1.0 + m.2 as f32)).sum();
        let dfs: Vec<u64> = terms.iter().map(|t| ctx.index.doc_freq(&path, t)).collect();
        let weight = crate::index::Weight {
            description: format!("weight({} in {doc})", self.description(ctx)),
            dfs: &dfs,
            boost,
            kind: crate::index::FreqKind::Phrase,
            terms: terms.clone(),
        };
        Ok(ctx.index.explain_weight(&path, &weight, doc, freq))
    }

    /// The scores of the query in `index`, with weight `boost`.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        let path = self.path().to_owned();
        if !self.on(&path) {
            return Err(SearchError("Clauses must have same field.".to_owned()));
        }
        let mut terms = Vec::new();
        self.weight_terms(index, &mut terms)?;
        let freqs = self
            .matches(index)?
            .into_iter()
            .map(|(doc, ms)| (doc, ms.iter().map(|m| 1.0 / (1.0 + m.2 as f32)).sum()))
            .collect();
        Ok(index.span_scores(&path, &terms, freqs, boost))
    }
}

/// The matches of `all` that `pred` keeps, without the documents left with none.
fn keep(
    all: BTreeMap<u32, Vec<Match>>,
    pred: impl Fn(&Match) -> bool,
) -> BTreeMap<u32, Vec<Match>> {
    all.into_iter()
        .filter_map(|(doc, ms)| {
            let left: Vec<Match> = ms.into_iter().filter(&pred).collect();
            (!left.is_empty()).then_some((doc, left))
        })
        .collect()
}

/// The matches of `near`: one match of each clause (in their order with `in_order`) whose gaps
/// add up to at most `slop`, starting from each match of the first clause.
fn near(
    index: &MemIndex,
    clauses: &[Span],
    slop: u32,
    in_order: bool,
) -> Result<BTreeMap<u32, Vec<Match>>, SearchError> {
    let lists: Vec<BTreeMap<u32, Vec<Match>>> =
        clauses.iter().map(|c| c.matches(index)).collect::<Result<_, _>>()?;
    let Some((first, rest)) = lists.split_first() else { return Ok(BTreeMap::new()) };
    let mut out = BTreeMap::new();
    for (doc, starts) in first {
        let Some(others): Option<Vec<&Vec<Match>>> = rest.iter().map(|l| l.get(doc)).collect()
        else {
            continue;
        };
        let mut found = Vec::new();
        for s in starts {
            let mut chosen = vec![*s];
            let mut ok = true;
            for list in &others {
                let prev = *chosen.last().expect("one chosen");
                let next = if in_order {
                    list.iter().filter(|m| m.0 >= prev.1).min_by_key(|m| m.0)
                } else {
                    list.iter().min_by_key(|m| m.0.abs_diff(s.0))
                };
                match next {
                    Some(m) => chosen.push(*m),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let start = chosen.iter().map(|m| m.0).min().unwrap_or(s.0);
            let end = chosen.iter().map(|m| m.1).max().unwrap_or(s.1);
            let covered: u32 = chosen.iter().map(|m| m.1 - m.0).sum();
            let width = (end - start).saturating_sub(covered);
            if width <= slop {
                found.push((start, end, width));
            }
        }
        if !found.is_empty() {
            out.insert(*doc, found);
        }
    }
    Ok(out)
}
