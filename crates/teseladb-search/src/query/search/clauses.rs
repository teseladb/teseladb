//! RefLib's limit of 1 024 clauses once `refsearch` has rewritten a query (FR-018, audit A011-3-5),
//! measured on `refsearch` 1.70.6 on 02-11-2026 (`scratchpad/lane3p/m60.js` to `m66.js`):
//!
//! - a disjunction (a `text` on one path, a `text` on several paths over its paths, the
//!   expansions of a fuzzy word, a `compound` with only `should`, a numeric `range`) first joins
//!   the clauses of its own that are equal, then takes in the disjunctions among them: more than
//!   1 024 clauses between them give «maxClauseCount is set to 1024», and the clauses taken in
//!   count once each from then on;
//! - with `must`, `filter` or `mustNot` nothing is taken in, a `filter` equal to a `must` does
//!   not count, and more than 1 024 leaf queries in the whole tree give «Query contains too many
//!   nested clauses; maxClauseCount is set to 1024».
//!
//! A `range` on numbers is two clauses (of longs and of doubles), and any other operator one leaf.
//! Fuzzy expansions are those of the index's own terms, as [`MemIndex::fuzzy_scores`] picks them.

use std::collections::BTreeMap;

use super::values::{Key, ValueOp};
use super::{MAX_CLAUSES, Operator, SearchError};
use crate::index::MemIndex;

/// The leaf queries an operator counts, by the clause each one is under.
enum Shape {
    /// A disjunction a parent disjunction takes in: its clauses, each with its leaves.
    Any(BTreeMap<String, usize>),
    /// Anything else: its leaves.
    Other(usize),
}

impl Shape {
    fn leaves(&self) -> usize {
        match self {
            Shape::Any(clauses) => clauses.values().sum(),
            Shape::Other(n) => *n,
        }
    }
}

/// Checks the clauses of `op` once rewritten.
pub(super) fn check(index: &MemIndex, op: &Operator) -> Result<(), SearchError> {
    if shape(index, op)?.leaves() > MAX_CLAUSES {
        return Err(SearchError(format!(
            "Query contains too many nested clauses; maxClauseCount is set to {MAX_CLAUSES}"
        )));
    }
    Ok(())
}

/// The disjunction of `children`, each with the key that tells equal ones apart.
fn any(children: Vec<(String, Shape)>) -> Result<Shape, SearchError> {
    let distinct: BTreeMap<String, Shape> = children.into_iter().collect();
    let taken: usize = distinct
        .values()
        .map(|s| match s {
            Shape::Any(clauses) => clauses.len(),
            Shape::Other(_) => 1,
        })
        .sum();
    if taken > MAX_CLAUSES {
        return Err(SearchError(format!("maxClauseCount is set to {MAX_CLAUSES}")));
    }
    let mut clauses = BTreeMap::new();
    for (key, s) in distinct {
        match s {
            Shape::Any(inner) => clauses.extend(inner),
            Shape::Other(n) => {
                clauses.insert(key, n);
            }
        }
    }
    Ok(Shape::Any(clauses))
}

/// A term of `path`, as a clause.
fn term(path: &str, term: &str) -> (String, Shape) {
    (format!("{path}\u{0}{term}"), Shape::Other(1))
}

fn shape(index: &MemIndex, op: &Operator) -> Result<Shape, SearchError> {
    match op {
        Operator::Text { query, paths, fuzzy, .. } => {
            let mut per_path = Vec::new();
            for p in paths {
                let mut words = Vec::new();
                for q in query {
                    for t in index.analyze(index.query_analyzer(p), q)? {
                        words.push(match fuzzy {
                            None => term(p, &t),
                            Some(f) => {
                                let terms = index.fuzzy_expansions(p, &t, f);
                                let key = format!("{p}\u{0}{t}\u{0}~");
                                (key, any(terms.iter().map(|e| term(p, e)).collect())?)
                            }
                        });
                    }
                }
                per_path.push((p.clone(), any(words)?));
            }
            any(per_path)
        }
        Operator::Compound { must, should, filter, must_not, minimum, .. } => {
            if must.is_empty() && filter.is_empty() && must_not.is_empty() && *minimum == 0 {
                let children = should
                    .iter()
                    .map(|c| Ok((format!("{c:?}"), shape(index, c)?)))
                    .collect::<Result<Vec<_>, SearchError>>()?;
                return any(children);
            }
            let kept = filter.iter().filter(|f| !must.contains(f));
            let mut leaves = 0;
            for c in must.iter().chain(should).chain(must_not).chain(kept) {
                leaves += shape(index, c)?.leaves();
            }
            Ok(Shape::Other(leaves))
        }
        Operator::Values(ValueOp::Range { paths, lower, upper, .. })
            if [lower, upper].iter().any(|b| matches!(b, Some((Key::Number(_), _)))) =>
        {
            let both = |p: &String| ["long", "double"].map(|kind| term(p, kind));
            any(paths.iter().flat_map(both).collect())
        }
        Operator::AllTerms(inner) | Operator::Scored(inner, _) => {
            Ok(Shape::Other(shape(index, inner)?.leaves()))
        }
        _ => Ok(Shape::Other(1)),
    }
}
