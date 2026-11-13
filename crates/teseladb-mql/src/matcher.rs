//! Evaluation of a filter on a document (FR-009) with RefDb's semantics of dotted paths and
//! arrays: a path crosses arrays of documents, a numeric segment also picks an element, and at
//! the end of the path most predicates match the value or any element of an array.

use std::cell::Cell;
use std::cmp::Ordering;

use teseladb_bson::types::BINARY_OLD;
use teseladb_bson::{Collator, RawDoc, RawValue, type_class};

use crate::error::Result;
use crate::ext::Extensions;
use crate::parse::{self, BitsOp, CmpOp, Expr, FilterUse, InItem, Pred, TextQuery};
use crate::regex::Regex;
use crate::value::{cmp, equal, exact_i64, is_nan, is_number, truncate_i64};

/// What evaluation needs from the command: its collation and the extensions.
#[derive(Clone, Copy)]
pub struct MatchContext<'a> {
    /// Collation of the command, for strings.
    pub collation: Option<&'a dyn Collator>,
    /// Integration points of 009, 010, 011 and 016.
    pub ext: &'a dyn Extensions,
    /// The command's `let` variables, resolved ([`crate::vars::resolve_let`]).
    pub vars: Option<&'a teseladb_bson::Document>,
}

/// A parsed query filter.
#[derive(Debug)]
pub struct Filter {
    expr: Expr,
    /// The same filter with its conjunctions in RefDb's order, when that order differs, for the
    /// position of `$` ([`crate::canonical`]); the upsert and the validators follow `expr`.
    ordered: Option<Expr>,
    /// The tree a query evaluates, in RefDb's order, when it leaves out `$_internalExpr…`
    /// comparisons ([`crate::implied`]).
    run: Option<Expr>,
    /// The tree a query evaluates, compiled for the register machine (010 FR-031).
    program: Box<crate::vm::Program>,
}

impl Filter {
    /// Reads `doc` as a filter used in `usage`.
    pub fn parse(doc: RawDoc<'_>, usage: FilterUse) -> Result<Filter> {
        let expr = parse::parse(doc, usage)?;
        let query = usage == FilterUse::Query;
        let reorders = query && crate::canonical::reorders(&expr);
        let ordered = if reorders {
            Some(crate::canonical::canonical(parse::parse(doc, usage)?))
        } else {
            None
        };
        let run = if query && crate::implied::has_implied(&expr) {
            let stripped = crate::implied::strip(parse::parse(doc, usage)?);
            Some(if reorders { crate::canonical::canonical(stripped) } else { stripped })
        } else {
            None
        };
        let program = Box::new(crate::vm::Program::compile_cached(
            run.as_ref().unwrap_or(&expr),
            &crate::vm::DefaultSelectivity,
            0,
        ));
        Ok(Filter { expr, ordered, run, program })
    }

    /// The filter with the conditions of its `$and`s and `$or`s ordered by `sel`, the range's
    /// estimate of what each lets through (010 FR-029, FR-031). `epoch` names the statistics
    /// behind `sel` (never 0, which is the default estimate's) and changes with them: the
    /// program of the filter's shape is kept on the core for that epoch
    /// ([`crate::vm::Program::compile_cached`]).
    pub fn with_selectivity(mut self, sel: &dyn crate::vm::Selectivity, epoch: u64) -> Filter {
        self.program =
            Box::new(crate::vm::Program::compile_cached(self.run_tree(), sel, epoch.max(1)));
        self
    }

    /// The compiled program of the filter.
    pub fn program(&self) -> &crate::vm::Program {
        &self.program
    }

    /// Whether `doc` matches, run by the register machine (010 FR-031).
    pub fn matches(&self, doc: RawDoc<'_>, ctx: &MatchContext<'_>) -> Result<bool> {
        self.program.run(self.run_tree(), doc, ctx)
    }

    /// The documents of `docs` (at most 64) that match, as the bits of a mask: bit `i` for
    /// `docs[i]` (010 FR-028, FR-031). An error is the first one the documents give in order.
    pub fn matches_batch(&self, docs: &[RawDoc<'_>], ctx: &MatchContext<'_>) -> Result<u64> {
        self.program.run_batch(self.run_tree(), docs, ctx)
    }

    /// Whether `doc` matches, by the tree evaluator: what the register machine is checked
    /// against.
    pub fn matches_by_tree(&self, doc: RawDoc<'_>, ctx: &MatchContext<'_>) -> Result<bool> {
        Eval::new(ctx, false).expr(self.run_tree(), doc)
    }

    /// Whether `doc` matches and, if it does, the position in the first array the match went
    /// through (what the positional operators `$` use).
    pub fn match_position(
        &self,
        doc: RawDoc<'_>,
        ctx: &MatchContext<'_>,
    ) -> Result<Option<Option<usize>>> {
        let eval = Eval::new(ctx, true);
        let expr = self.run.as_ref().or(self.ordered.as_ref()).unwrap_or(&self.expr);
        Ok(eval.expr(expr, doc)?.then(|| eval.pos.get()))
    }

    /// The tree a query evaluates ([`Filter::matches`]).
    pub fn run_tree(&self) -> &Expr {
        self.run.as_ref().unwrap_or(&self.expr)
    }

    /// The parsed tree.
    pub fn expr(&self) -> &Expr {
        &self.expr
    }

    /// The tree with its conjunctions in RefDb's order ([`crate::canonical`]).
    pub(crate) fn ordered_expr(&self) -> &Expr {
        self.ordered.as_ref().unwrap_or(&self.expr)
    }

    /// Whether the filter has `$text`.
    pub fn uses_text(&self) -> bool {
        fn walk(e: &Expr) -> bool {
            match e {
                Expr::Text(_) => true,
                Expr::And(items) | Expr::Or(items) | Expr::Nor(items) => items.iter().any(walk),
                _ => false,
            }
        }
        walk(&self.expr)
    }

    /// The `$text` query of the filter, at its top level or in its `$and` and `$or`.
    pub fn text_query(&self) -> Option<&TextQuery> {
        fn walk(e: &Expr) -> Option<&TextQuery> {
            match e {
                Expr::Text(q) => Some(q),
                Expr::And(items) | Expr::Or(items) => items.iter().find_map(walk),
                _ => None,
            }
        }
        walk(&self.expr)
    }

    /// Whether the filter has `$near` or `$nearSphere`.
    pub fn uses_near(&self) -> bool {
        fn pred(p: &Pred) -> bool {
            match p {
                Pred::Geo(g) => g.near,
                Pred::And(preds) => preds.iter().any(pred),
                _ => false,
            }
        }
        fn walk(e: &Expr) -> bool {
            match e {
                Expr::Field { pred: p, .. } => pred(p),
                Expr::And(items) | Expr::Or(items) => items.iter().any(walk),
                _ => false,
            }
        }
        walk(&self.expr)
    }

    /// Whether the filter decides a document from the document alone: it has no `$expr`,
    /// `$where`, `$text`, `$sampleRate` nor geographic operator, which ask the reader's
    /// [`crate::Extensions`] and its variables. Under one collation such a filter matches the
    /// same documents whoever evaluates it, so a reader may run it for another (010 FR-027).
    pub fn reads_document_alone(&self) -> bool {
        fn pred(p: &Pred) -> bool {
            match p {
                Pred::Geo(_) => false,
                Pred::And(preds) => preds.iter().all(pred),
                Pred::Not(p) | Pred::ElemMatchValue(p) => pred(p),
                Pred::ElemMatchDoc(e) => walk(e),
                Pred::Eq(_)
                | Pred::Cmp(..)
                | Pred::ExprCmp(..)
                | Pred::ExprEq(_)
                | Pred::In(_)
                | Pred::Regex(_)
                | Pred::Exists
                | Pred::Type(_)
                | Pred::Mod { .. }
                | Pred::Size(_)
                | Pred::Bits(..) => true,
            }
        }
        fn walk(e: &Expr) -> bool {
            match e {
                Expr::And(items) | Expr::Or(items) | Expr::Nor(items) => items.iter().all(walk),
                Expr::Field { pred: p, .. } => pred(p),
                Expr::Expr(_) | Expr::Where(_) | Expr::Text(_) | Expr::SampleRate(_) => false,
                Expr::JsonSchema(_) | Expr::True | Expr::False => true,
            }
        }
        walk(&self.expr)
    }
}

/// The value of the first field named `name`.
pub(crate) fn field<'a>(doc: RawDoc<'a>, name: &str) -> Option<RawValue<'a>> {
    doc.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// The value a `$_internalExpr…` comparison reads at `path`: the first array on the way stands
/// for the whole path (it matches), and a path that leaves the documents is missing.
fn internal_value<'d>(doc: RawDoc<'d>, path: &str) -> Option<RawValue<'d>> {
    let mut cur = RawValue::Document(doc);
    for seg in path.split('.') {
        cur = match cur {
            RawValue::Document(d) => field(d, seg)?,
            RawValue::Array(_) => return Some(cur),
            _ => return None,
        };
    }
    Some(cur)
}

/// A path segment that is an array position: digits without a leading zero.
pub(crate) fn array_index(segment: &str) -> Option<usize> {
    let ok = !segment.is_empty()
        && segment.bytes().all(|b| b.is_ascii_digit())
        && (segment == "0" || !segment.starts_with('0'));
    if ok { segment.parse().ok() } else { None }
}

pub(crate) struct Eval<'c, 'a> {
    ctx: &'c MatchContext<'a>,
    record: bool,
    pos: Cell<Option<usize>>,
}

impl<'c, 'a> Eval<'c, 'a> {
    pub(crate) fn new(ctx: &'c MatchContext<'a>, record: bool) -> Eval<'c, 'a> {
        Eval { ctx, record, pos: Cell::new(None) }
    }

    /// Evaluates `f`, forgetting a position recorded inside it if it fails.
    fn scoped(&self, f: impl FnOnce() -> Result<bool>) -> Result<bool> {
        let before = self.pos.get();
        let r = f()?;
        if !r {
            self.pos.set(before);
        }
        Ok(r)
    }

    /// Records the position of an array a predicate matched through; the last predicate that
    /// records one wins, as in RefDb (`{$all: [2, 3]}` gives the position of 3).
    fn hit(&self, pos: Option<usize>) {
        if self.record && pos.is_some() {
            self.pos.set(pos);
        }
    }

    /// A copy that records nothing (negations and nested documents).
    fn quiet(&self) -> Eval<'c, 'a> {
        Eval::new(self.ctx, false)
    }

    pub(crate) fn expr(&self, expr: &Expr, doc: RawDoc<'_>) -> Result<bool> {
        self.scoped(|| {
            Ok(match expr {
                Expr::And(items) => {
                    for e in items {
                        if !self.expr(e, doc)? {
                            return Ok(false);
                        }
                    }
                    true
                }
                Expr::Or(items) => {
                    for e in items {
                        if self.expr(e, doc)? {
                            return Ok(true);
                        }
                    }
                    false
                }
                Expr::Nor(items) => {
                    let quiet = self.quiet();
                    for e in items {
                        if quiet.expr(e, doc)? {
                            return Ok(false);
                        }
                    }
                    true
                }
                Expr::Field { path, pred } => self.path(pred, doc, path)?,
                Expr::Expr(e) => self.ctx.ext.expr(e.get(), doc, self.ctx.vars)?,
                Expr::Where(code) => self.ctx.ext.where_js(code.get(), doc)?,
                Expr::Text(q) => self.ctx.ext.text(q, doc)?,
                Expr::JsonSchema(schema) => schema.matches(doc)?,
                Expr::SampleRate(rate) => self.ctx.ext.random() < *rate,
                Expr::True => true,
                Expr::False => false,
            })
        })
    }

    /// `pred` on the values of `path` in `doc`.
    pub(crate) fn path(&self, pred: &Pred, doc: RawDoc<'_>, path: &str) -> Result<bool> {
        self.scoped(|| match pred {
            Pred::And(preds) => {
                for p in preds {
                    if !self.path(p, doc, path)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Pred::Not(p) => Ok(!self.quiet().path(p, doc, path)?),
            Pred::ExprEq(_) | Pred::ExprCmp(..) => self.single(pred, internal_value(doc, path)),
            _ => {
                let segments: Vec<&str> = path.split('.').collect();
                self.walk(pred, Some(RawValue::Document(doc)), &segments, None)
            }
        })
    }

    fn walk(
        &self,
        pred: &Pred,
        cur: Option<RawValue<'_>>,
        segs: &[&str],
        pos: Option<usize>,
    ) -> Result<bool> {
        let Some((seg, rest)) = segs.split_first() else { return self.leaf(pred, cur, pos, true) };
        match cur {
            Some(RawValue::Document(d)) => self.walk(pred, field(d, seg), rest, pos),
            Some(RawValue::Array(arr)) => {
                // An array reached by position at the end of the path is not expanded.
                if let Some(e) = array_index(seg).and_then(|i| arr.iter().nth(i)) {
                    let hit = if rest.is_empty() {
                        self.leaf(pred, Some(e), pos, false)?
                    } else {
                        self.walk(pred, Some(e), rest, pos)?
                    };
                    if hit {
                        return Ok(true);
                    }
                }
                for (i, e) in arr.iter().enumerate() {
                    if let RawValue::Document(d) = e
                        && self.walk(pred, field(d, seg), rest, pos.or(Some(i)))?
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => self.leaf(pred, None, pos, true),
        }
    }

    /// `pred` on the value at the end of a path; `expand` also tries an array's elements.
    pub(crate) fn leaf(
        &self,
        pred: &Pred,
        v: Option<RawValue<'_>>,
        pos: Option<usize>,
        expand: bool,
    ) -> Result<bool> {
        match pred {
            Pred::And(preds) => {
                for p in preds {
                    if !self.leaf(p, v, pos, expand)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Pred::Not(p) => Ok(!self.quiet().leaf(p, v, pos, expand)?),
            Pred::Exists => Ok(v.is_some()),
            Pred::Size(n) => Ok(match v {
                Some(RawValue::Array(arr)) => i64::try_from(arr.iter().count()).ok() == Some(*n),
                _ => false,
            }),
            Pred::ElemMatchDoc(expr) => {
                let Some(RawValue::Array(arr)) = v else { return Ok(false) };
                let quiet = self.quiet();
                for (i, e) in arr.iter().enumerate() {
                    let doc = match e {
                        RawValue::Document(d) => d,
                        RawValue::Array(a) => a.as_doc(),
                        _ => continue,
                    };
                    if quiet.expr(expr, doc)? {
                        self.hit(pos.or(Some(i)));
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Pred::ElemMatchValue(p) => {
                let Some(RawValue::Array(arr)) = v else { return Ok(false) };
                let quiet = self.quiet();
                for (i, e) in arr.iter().enumerate() {
                    if quiet.leaf(p, Some(e), None, false)? {
                        self.hit(pos.or(Some(i)));
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => {
                if self.single(pred, v)? {
                    self.hit(pos);
                    return Ok(true);
                }
                if let (true, Some(RawValue::Array(arr))) = (expand, v) {
                    for (i, e) in arr.iter().enumerate() {
                        if self.single(pred, Some(e))? {
                            self.hit(pos.or(Some(i)));
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
        }
    }

    /// A predicate that tests one value.
    pub(crate) fn single(&self, pred: &Pred, v: Option<RawValue<'_>>) -> Result<bool> {
        let coll = self.ctx.collation;
        Ok(match pred {
            Pred::Eq(t) => eq(v, t.get(), coll),
            Pred::Cmp(op, t) => compare(*op, v, t.get(), coll),
            Pred::ExprEq(_) | Pred::ExprCmp(..) if matches!(v, Some(RawValue::Array(_))) => true,
            Pred::ExprEq(t) => v.is_some_and(|v| cmp(v, t.get(), coll) == Ordering::Equal),
            Pred::ExprCmp(op, t) => {
                let o = v.map_or(Ordering::Less, |v| cmp(v, t.get(), coll));
                match op {
                    CmpOp::Gt => o == Ordering::Greater,
                    CmpOp::Gte => o != Ordering::Less,
                    CmpOp::Lt => o == Ordering::Less,
                    CmpOp::Lte => o != Ordering::Greater,
                }
            }
            Pred::In(items) => {
                for item in items {
                    let hit = match item {
                        InItem::Value(t) => eq(v, t.get(), coll),
                        InItem::Regex(re) => regex_matches(re, v)?,
                    };
                    if hit {
                        return Ok(true);
                    }
                }
                false
            }
            Pred::Regex(re) => regex_matches(re, v)?,
            Pred::Type(specs) => {
                v.is_some_and(|v| specs.iter().any(|s| s.matches(v.element_type())))
            }
            Pred::Mod { divisor, remainder } => v
                .filter(|v| is_number(v) && !is_nan(v))
                .and_then(|v| truncate_i64(&v))
                .is_some_and(|n| n.wrapping_rem(*divisor) == *remainder),
            Pred::Bits(op, positions) => v.is_some_and(|v| bits(*op, positions, v)),
            Pred::Geo(q) => match v {
                Some(v) => self.ctx.ext.geo(q, v)?,
                None => false,
            },
            Pred::And(_)
            | Pred::Not(_)
            | Pred::Exists
            | Pred::Size(_)
            | Pred::ElemMatchDoc(_)
            | Pred::ElemMatchValue(_) => self.leaf(pred, v, None, false)?,
        })
    }
}

/// Equality as a query sees it: `null` also matches a missing value.
pub(crate) fn eq(v: Option<RawValue<'_>>, t: RawValue<'_>, coll: Option<&dyn Collator>) -> bool {
    match v {
        None => t == RawValue::Null,
        Some(v) => equal(v, t, coll),
    }
}

/// An ordering within the target's type class (MinKey and MaxKey compare with every type); NaN
/// is only equal to NaN.
pub(crate) fn compare(
    op: CmpOp,
    v: Option<RawValue<'_>>,
    t: RawValue<'_>,
    coll: Option<&dyn Collator>,
) -> bool {
    let inclusive = matches!(op, CmpOp::Gte | CmpOp::Lte);
    let keys = matches!(t, RawValue::MinKey | RawValue::MaxKey);
    let o = if keys {
        // A missing value compares as `null`, and NaN as any number.
        cmp(v.unwrap_or(RawValue::Null), t, coll)
    } else {
        let Some(v) = v else { return t == RawValue::Null && inclusive };
        if is_nan(&v) || is_nan(&t) {
            return inclusive && is_nan(&v) && is_nan(&t);
        }
        if type_class(&v) != type_class(&t) {
            return false;
        }
        cmp(v, t, coll)
    };
    match op {
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Gte => o != Ordering::Less,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Lte => o != Ordering::Greater,
    }
}

/// A string or symbol that the expression matches, or a regular expression equal to it.
fn regex_matches(re: &Regex, v: Option<RawValue<'_>>) -> Result<bool> {
    Ok(match v {
        Some(RawValue::String(s) | RawValue::Symbol(s)) => re.is_match(s)?,
        Some(RawValue::Regex { pattern, options }) => {
            pattern == re.pattern() && options == re.options()
        }
        _ => false,
    })
}

/// The length of binary data as BSON stores it: subtype 2 keeps its 4-byte inner length in front
/// of the bytes, and the bit operators test those bytes too (measured on a `refrouter` 9.0.4).
pub(crate) fn stored_len(subtype: u8, bytes: &[u8]) -> usize {
    bytes.len() + if subtype == BINARY_OLD { 4 } else { 0 }
}

/// Byte `i` of binary data as BSON stores it (see [`stored_len`]).
pub(crate) fn stored_byte(subtype: u8, bytes: &[u8], i: usize) -> Option<u8> {
    if subtype != BINARY_OLD {
        return bytes.get(i).copied();
    }
    match i.checked_sub(4) {
        Some(j) => bytes.get(j).copied(),
        None => u32::try_from(bytes.len()).ok().map(|n| n.to_le_bytes()[i]),
    }
}

/// The bit operators on an integral number (two's complement, sign-extended) or binary data.
fn bits(op: BitsOp, positions: &[u32], v: RawValue<'_>) -> bool {
    let bit: Box<dyn Fn(u32) -> bool> = match v {
        RawValue::Binary { subtype, bytes } => Box::new(move |p| {
            let byte = usize::try_from(p / 8).ok().and_then(|i| stored_byte(subtype, bytes, i));
            byte.is_some_and(|b| b >> (p % 8) & 1 == 1)
        }),
        v if is_number(&v) && !is_nan(&v) => {
            let Some(n) = exact_i64(&v) else { return false };
            Box::new(move |p| if p >= 64 { n < 0 } else { n >> p & 1 == 1 })
        }
        _ => return false,
    };
    match op {
        BitsOp::AllSet => positions.iter().all(|p| bit(*p)),
        BitsOp::AllClear => positions.iter().all(|p| !bit(*p)),
        BitsOp::AnySet => positions.iter().any(|p| bit(*p)),
        BitsOp::AnyClear => positions.iter().any(|p| !bit(*p)),
    }
}
