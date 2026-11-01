//! A filter's clauses as RefDb rewrites them before it plans (measured on a `refrouter` 9.0.4 by
//! `explain`'s `parsedQuery`): nested `$and` and `$or` opened into their parents, the clauses that
//! match every document or none settling their parents, one clause left of a conjunction or
//! disjunction standing alone, equalities on one path in a disjunction joined in one `$in`, and
//! the clauses of each in the order of RefDb's match types, then by path, the others as made.

use teseladb_bson::{Document, Value};

use crate::ops;

/// The rank of each kind of clause, in the order of RefDb's `MatchType`.
pub(crate) mod rank {
    pub const AND: u8 = 0;
    pub const OR: u8 = 1;
    pub const EQ: u8 = 5;
    pub const LTE: u8 = 6;
    pub const LT: u8 = 7;
    pub const GT: u8 = 8;
    pub const GTE: u8 = 9;
    pub const REGEX: u8 = 10;
    pub const EXISTS: u8 = 12;
    pub const NOT: u8 = 18;
    pub const NOR: u8 = 19;
    pub const ALWAYS_FALSE: u8 = 24;
    pub const ALWAYS_TRUE: u8 = 25;
    pub const SCHEMA_ALLOWED_PROPERTIES: u8 = 40;
    pub const SCHEMA_ALL_ELEM_MATCH_FROM_INDEX: u8 = 41;
    pub const SCHEMA_COND: u8 = 45;
    pub const SCHEMA_EQ: u8 = 46;
    pub const SCHEMA_FMOD: u8 = 47;
    pub const SCHEMA_MATCH_ARRAY_INDEX: u8 = 48;
    pub const SCHEMA_MAX_ITEMS: u8 = 49;
    pub const SCHEMA_MAX_LENGTH: u8 = 50;
    pub const SCHEMA_MAX_PROPERTIES: u8 = 51;
    pub const SCHEMA_MIN_ITEMS: u8 = 52;
    pub const SCHEMA_MIN_LENGTH: u8 = 53;
    pub const SCHEMA_MIN_PROPERTIES: u8 = 54;
    pub const SCHEMA_OBJECT_MATCH: u8 = 55;
    pub const SCHEMA_ROOT_DOC_EQ: u8 = 56;
    pub const SCHEMA_TYPE: u8 = 57;
    pub const SCHEMA_UNIQUE_ITEMS: u8 = 58;
    pub const SCHEMA_XOR: u8 = 59;
}

/// A clause of a filter.
#[derive(Clone, Debug)]
pub(crate) enum Clause {
    And(Vec<Clause>),
    Or(Vec<Clause>),
    Nor(Vec<Clause>),
    /// `{path: pred}`: its rank and, for an equality, its value (which a `$in` may take).
    Field {
        rank: u8,
        path: String,
        pred: Document,
        eq: Option<Value>,
    },
    /// A clause of its own (`$expr`, `$where`, the schema's): its rank and how it is written.
    Other {
        rank: u8,
        doc: Document,
    },
    True,
    False,
}

impl Clause {
    /// `{path: {op: v}}`.
    pub(crate) fn field(rank: u8, path: &str, op: &str, v: impl Into<Value>) -> Clause {
        Clause::Field { rank, path: path.to_owned(), pred: Document::new().with(op, v), eq: None }
    }

    /// `{path: {$not: pred}}` of a clause on `path`; a `$_internalExpr…` comparison, which has
    /// no `$not` form, under `$nor` (measured on 9.0.4).
    pub(crate) fn not(c: Clause) -> Clause {
        match c {
            Clause::Field { path, pred, .. }
                if !pred.iter().any(|(op, _)| op.starts_with(ops::EXPR_PREFIX)) =>
            {
                Clause::Field {
                    rank: rank::NOT,
                    path,
                    pred: Document::new().with("$not", pred),
                    eq: None,
                }
            }
            other => Clause::Nor(vec![other]),
        }
    }

    /// Where the clause goes in a sorted conjunction or disjunction: by kind, a `$not` by the
    /// kind of what it negates (RefDb compares a `$not`'s child before any path), then by path.
    fn rank(&self) -> (u8, u8, &str) {
        match self {
            Clause::And(_) => (rank::AND, 0, ""),
            Clause::Or(_) => (rank::OR, 0, ""),
            Clause::Nor(_) => (rank::NOR, 0, ""),
            Clause::Field { rank, path, pred, .. } => {
                let sub = if *rank == rank::NOT { negated_rank(pred) } else { 0 };
                (*rank, sub, path)
            }
            Clause::Other { rank, .. } => (*rank, 0, ""),
            Clause::False => (rank::ALWAYS_FALSE, 0, ""),
            Clause::True => (rank::ALWAYS_TRUE, 0, ""),
        }
    }

    /// The clause rewritten as RefDb does.
    pub(crate) fn simplified(self) -> Clause {
        match self {
            Clause::And(items) => {
                let mut out = Vec::new();
                for i in items {
                    match i.simplified() {
                        Clause::False => return Clause::False,
                        Clause::True => {}
                        Clause::And(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                out.sort_by(compare);
                match out.len() {
                    0 => Clause::True,
                    1 => out.remove(0),
                    _ => Clause::And(out),
                }
            }
            Clause::Or(items) => {
                let mut out = Vec::new();
                for i in items {
                    match i.simplified() {
                        Clause::True => return Clause::True,
                        Clause::False => {}
                        Clause::Or(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                let mut out = joined(out);
                out.sort_by(compare);
                match out.len() {
                    0 => Clause::False,
                    1 => out.remove(0),
                    _ => Clause::Or(out),
                }
            }
            Clause::Nor(items) => {
                let mut out = Vec::new();
                for i in items {
                    match i.simplified() {
                        Clause::True => return Clause::False,
                        Clause::False => {}
                        other => out.push(other),
                    }
                }
                match out.len() {
                    0 => Clause::True,
                    1 if matches!(out[0], Clause::Field { .. }) => Clause::not(out.remove(0)),
                    _ => Clause::Nor(out),
                }
            }
            other => other,
        }
    }

    /// The clause as `explain` writes it; `top`: the whole filter, where a clause that matches
    /// every document is `{}`.
    pub(crate) fn written(&self) -> Document {
        match self {
            Clause::And(items) => list("$and", items),
            Clause::Or(items) => list("$or", items),
            Clause::Nor(items) => list("$nor", items),
            Clause::Field { path, pred, .. } => Document::new().with(path.as_str(), pred.clone()),
            Clause::Other { doc, .. } => doc.clone(),
            Clause::True => Document::new().with(ops::ALWAYS_TRUE, 1),
            Clause::False => Document::new().with(ops::ALWAYS_FALSE, 1),
        }
    }
}

/// The rank of the operator a `$not` predicate negates (0 for any other predicate), as
/// [`crate::canonical::pred_rank`] ranks it.
fn negated_rank(pred: &Document) -> u8 {
    let op = match pred.get("$not") {
        Some(Value::Document(inner)) => inner.iter().next().map_or("", |(op, _)| op),
        _ => "",
    };
    match op {
        "$elemMatch" => 2,
        "$size" => 4,
        "$eq" => 5,
        "$lte" => 6,
        "$lt" => 7,
        "$gt" => 8,
        "$gte" => 9,
        "$regex" => rank::REGEX,
        "$mod" => 11,
        "$exists" => rank::EXISTS,
        "$in" => crate::canonical::IN_RANK,
        "$bitsAllSet" => 14,
        "$bitsAllClear" => 15,
        "$bitsAnySet" => 16,
        "$bitsAnyClear" => 17,
        "$type" => 20,
        // The schema's internal operators, after every user-facing one.
        _ => u8::MAX,
    }
}

/// The order of two clauses in a sorted conjunction or disjunction: by rank, then, between two
/// `$and`, `$or` or `$nor`, by their clauses in turn (RefDb compares a node's children).
fn compare(a: &Clause, b: &Clause) -> std::cmp::Ordering {
    let children = |c: &'_ Clause| match c {
        Clause::And(items) | Clause::Or(items) | Clause::Nor(items) => items.clone(),
        _ => Vec::new(),
    };
    a.rank().cmp(&b.rank()).then_with(|| {
        let (x, y) = (children(a), children(b));
        x.iter()
            .zip(&y)
            .map(|(p, q)| compare(p, q))
            .find(|o| o.is_ne())
            .unwrap_or(x.len().cmp(&y.len()))
    })
}

fn list(op: &str, items: &[Clause]) -> Document {
    Document::new()
        .with(op, Value::Array(items.iter().map(|i| Value::Document(i.written())).collect()))
}

/// The clauses of a disjunction with its equalities on one path joined in one `$in`, in the
/// place of the first.
fn joined(items: Vec<Clause>) -> Vec<Clause> {
    let count = |path: &str| {
        items
            .iter()
            .filter(|i| matches!(i, Clause::Field { path: p, eq: Some(_), .. } if p == path))
            .count()
    };
    let mut out: Vec<Clause> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    for i in &items {
        match i {
            Clause::Field { path, eq: Some(_), .. } if count(path) > 1 => {
                if done.contains(path) {
                    continue;
                }
                done.push(path.clone());
                let values: Vec<Value> = items
                    .iter()
                    .filter_map(|j| match j {
                        Clause::Field { path: p, eq: Some(v), .. } if p == path => Some(v.clone()),
                        _ => None,
                    })
                    .collect();
                out.push(in_clause(path, values, Vec::new()));
            }
            other => out.push(other.clone()),
        }
    }
    out
}

/// `{path: {$in: …}}` as RefDb writes it: the values in BSON order without repeats, then the
/// expressions; one value alone is `$eq`, one expression alone `$regex`.
pub(crate) fn in_clause(path: &str, values: Vec<Value>, regexes: Vec<Value>) -> Clause {
    let mut owned: Vec<crate::value::OwnedValue> =
        values.into_iter().filter_map(|v| crate::value::OwnedValue::new(v).ok()).collect();
    owned.sort_by(|a, b| teseladb_bson::compare(a.get(), b.get(), None));
    owned.dedup_by(|a, b| teseladb_bson::compare(a.get(), b.get(), None).is_eq());
    match (owned.as_slice(), regexes.as_slice()) {
        ([one], []) => {
            let v = one.to_value();
            Clause::Field {
                rank: rank::EQ,
                path: path.to_owned(),
                pred: Document::new().with("$eq", v.clone()),
                eq: mergeable(&v).then_some(v),
            }
        }
        ([], [Value::Regex { pattern, options }]) => {
            let d = Document::new().with("$regex", pattern.as_str());
            let pred = if options.is_empty() { d } else { d.with("$options", options.as_str()) };
            Clause::Field { rank: rank::REGEX, path: path.to_owned(), pred, eq: None }
        }
        _ => Clause::Field {
            rank: crate::canonical::IN_RANK,
            path: path.to_owned(),
            pred: Document::new().with(
                "$in",
                Value::Array(
                    owned.iter().map(crate::value::OwnedValue::to_value).chain(regexes).collect(),
                ),
            ),
            eq: None,
        },
    }
}

/// Whether an equality may join others on its path in a `$in`.
pub(crate) fn mergeable(v: &Value) -> bool {
    !matches!(v, Value::Regex { .. } | Value::Array(_))
}
