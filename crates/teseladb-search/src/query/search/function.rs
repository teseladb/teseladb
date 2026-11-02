//! `score.function` (011 FR-003): an expression over the operator's score (`relevance`),
//! constants and the numbers of the document, as `refsearch` 1.70.6 computes it (measured
//! 02-11-2026, `scratchpad/lane3j/m2.js`): `log` is base 10, `log1p` is `log10(1 + x)`, `gauss`
//! is `decay ^ (max(0, |x − origin| − offset)² / scale²)`, and a negative result scores 0.

use teseladb_bson::{Document, Value};

use super::{SearchError, shape, value_at};
use crate::definition::Definition;

/// The kinds of expression, in the order `refsearch` names them.
const KINDS: [&str; 8] = ["add", "constant", "gauss", "log", "log1p", "multiply", "path", "score"];

/// Default `decay` of `gauss`.
const DEFAULT_DECAY: f64 = 0.5;

/// A number of the document: its path and the value without one.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PathValue {
    path: String,
    undefined: f64,
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Expr {
    Constant(f64),
    Path(PathValue),
    Relevance,
    Add(Vec<Expr>),
    Multiply(Vec<Expr>),
    Log(Box<Expr>),
    Log1p(Box<Expr>),
    Gauss { at: PathValue, origin: f64, scale: f64, decay: f64, offset: f64 },
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(f) => Some(*f),
        _ => None,
    }
}

fn number_of(d: &Document, at: &str, field: &str) -> Result<Option<f64>, SearchError> {
    match d.get(field) {
        None => Ok(None),
        Some(v) => {
            number(v).map(Some).ok_or_else(|| shape(&format!("{at}.{field}"), "must be a number"))
        }
    }
}

fn path_value(v: &Value, at: &str) -> Result<PathValue, SearchError> {
    match v {
        Value::String(p) => Ok(PathValue { path: p.clone(), undefined: 0.0 }),
        Value::Document(d) => {
            let path = match d.get("value") {
                Some(Value::String(p)) => p.clone(),
                Some(_) => return Err(shape(&format!("{at}.value"), "must be a string")),
                None => return Err(shape(&format!("{at}.value"), "is required")),
            };
            Ok(PathValue { path, undefined: number_of(d, at, "undefined")?.unwrap_or(0.0) })
        }
        _ => Err(shape(at, "must be a string or document")),
    }
}

/// The expression `v` at `at` of the query.
pub(super) fn parse(v: &Value, at: &str) -> Result<Expr, SearchError> {
    let Value::Document(d) = v else { return Err(shape(at, "must be a document")) };
    let kinds: Vec<&str> = d.iter().map(|(k, _)| k).filter(|k| KINDS.contains(k)).collect();
    let list = KINDS.join(", ");
    let kind = match kinds.as_slice() {
        [] => return Err(shape(at, &format!("one of [{list}] must be present"))),
        [one] => *one,
        _ => return Err(shape(at, &format!("only one of [{list}] may be present"))),
    };
    let inner = format!("{at}.{kind}");
    let arg = d.get(kind).expect("one of the keys");
    match kind {
        "constant" => {
            Ok(Expr::Constant(number(arg).ok_or_else(|| shape(&inner, "must be a number"))?))
        }
        "path" => Ok(Expr::Path(path_value(arg, &inner)?)),
        "score" => match arg {
            Value::String(s) if s == "relevance" => Ok(Expr::Relevance),
            _ => Err(shape(&inner, "score field must have value \"relevance\"")),
        },
        "add" | "multiply" => {
            let Value::Array(items) = arg else { return Err(shape(&inner, "must be an array")) };
            if items.len() < 2 {
                return Err(shape(
                    &inner,
                    &format!("{kind} expression must have at least two arguments"),
                ));
            }
            let args = items
                .iter()
                .enumerate()
                .map(|(i, e)| parse(e, &format!("{inner}[{i}]")))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(if kind == "add" { Expr::Add(args) } else { Expr::Multiply(args) })
        }
        "log" => Ok(Expr::Log(Box::new(parse(arg, &inner)?))),
        "log1p" => Ok(Expr::Log1p(Box::new(parse(arg, &inner)?))),
        _ => parse_gauss(arg, &inner),
    }
}

fn parse_gauss(arg: &Value, at: &str) -> Result<Expr, SearchError> {
    let Value::Document(g) = arg else { return Err(shape(at, "must be a document")) };
    let required =
        |f: &str| number_of(g, at, f)?.ok_or_else(|| shape(&format!("{at}.{f}"), "is required"));
    let path = g.get("path").ok_or_else(|| shape(&format!("{at}.path"), "is required"))?;
    let at_path = path_value(path, &format!("{at}.path"))?;
    let origin = required("origin")?;
    let scale = required("scale")?;
    let decay = number_of(g, at, "decay")?.unwrap_or(DEFAULT_DECAY);
    if decay <= 0.0 || decay >= 1.0 {
        return Err(shape(&format!("{at}.decay"), "must be between 0 and 1 (exclusive)"));
    }
    let offset = number_of(g, at, "offset")?.unwrap_or(0.0);
    Ok(Expr::Gauss { at: at_path, origin, scale, decay, offset })
}

impl Expr {
    /// Refuses a path the index does not keep as numbers (measured message).
    pub(super) fn check(&self, definition: &Definition) -> Result<(), SearchError> {
        let numeric = |p: &PathValue| {
            if super::values::numeric(definition, &p.path) {
                Ok(())
            } else {
                Err(SearchError(format!(
                    "path expression for function score requires path \"{}\" to be indexed as \
                     numeric",
                    p.path
                )))
            }
        };
        match self {
            Expr::Constant(_) | Expr::Relevance => Ok(()),
            Expr::Path(p) | Expr::Gauss { at: p, .. } => numeric(p),
            Expr::Add(args) | Expr::Multiply(args) => {
                args.iter().try_for_each(|a| a.check(definition))
            }
            Expr::Log(a) | Expr::Log1p(a) => a.check(definition),
        }
    }

    fn value(&self, relevance: f64, doc: Option<&Document>) -> f64 {
        let at = |p: &PathValue| {
            doc.and_then(|d| value_at(d, &p.path)).and_then(number).unwrap_or(p.undefined)
        };
        match self {
            Expr::Constant(c) => *c,
            Expr::Path(p) => at(p),
            Expr::Relevance => relevance,
            Expr::Add(args) => args.iter().map(|a| a.value(relevance, doc)).sum(),
            Expr::Multiply(args) => args.iter().map(|a| a.value(relevance, doc)).product(),
            Expr::Log(a) => a.value(relevance, doc).log10(),
            Expr::Log1p(a) => (1.0 + a.value(relevance, doc)).log10(),
            Expr::Gauss { at: p, origin, scale, decay, offset } => {
                let d = ((at(p) - origin).abs() - offset).max(0.0);
                decay.powf(d * d / (scale * scale))
            }
        }
    }

    /// The score of a document whose operator scored `relevance`.
    pub(super) fn score(&self, relevance: f32, doc: Option<&Document>) -> f32 {
        let v = self.value(f64::from(relevance), doc);
        if v.is_nan() || v < 0.0 { 0.0 } else { v as f32 }
    }
}
