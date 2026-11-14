//! Whether a data block may hold a document a filter accepts, from the block's zone map (004
//! FR-039): the storage passes over the blocks this rules out without reading them.
//!
//! A zone gives, for a top-level field, the lowest and the highest value of the block in
//! the reference server's order across types, arrays left out. A block is ruled out only by predicates whose
//! documents always hold the field with a value in that order:
//!
//! - `$eq`, `$gt`, `$gte`, `$lt`, `$lte` and `$in` of values on a top-level field, when the value
//!   is not `null`, `undefined`, `MinKey`, `MaxKey`, a regular expression or an array (each of
//!   those also matches documents without the field, or values the bounds do not order);
//! - the conjunctions of a field's predicates and of the filter, when one of them rules it out;
//! - `$or`, when every branch rules it out; `$alwaysFalse`.
//!
//! Under a collation, a string, a symbol and a document or array literal are left out: the zone
//! orders strings by their bytes. Anything else may match.

use teseladb_bson::RawValue;
use teseladb_keys::zone::{Bounds, Zones};
use teseladb_keys::{Direction, TypeBits, encode_value};
use teseladb_mql::{CmpOp, Expr, InItem, Pred};

/// Whether a block whose zones are `zones` may hold a document `filter` accepts; `collated`:
/// the filter compares strings under a collation.
pub fn block_may_match(filter: &Expr, collated: bool, zones: &dyn Zones) -> bool {
    !rules_out(filter, collated, zones)
}

fn rules_out(e: &Expr, collated: bool, zones: &dyn Zones) -> bool {
    match e {
        Expr::And(items) => items.iter().any(|e| rules_out(e, collated, zones)),
        Expr::Or(items) => !items.is_empty() && items.iter().all(|e| rules_out(e, collated, zones)),
        Expr::False => true,
        Expr::Field { path, pred } if !path.contains('.') => {
            pred_rules_out(pred, collated, zones.field(path))
        }
        Expr::Field { .. }
        | Expr::Nor(_)
        | Expr::Expr(_)
        | Expr::Where(_)
        | Expr::Text(_)
        | Expr::JsonSchema(_)
        | Expr::SampleRate(_)
        | Expr::True => false,
    }
}

/// Whether a literal lies outside a block's interval: `(literal, min, max)`.
type Outside<'f> = &'f dyn Fn(&[u8], &[u8], &[u8]) -> bool;

fn pred_rules_out(p: &Pred, collated: bool, b: Bounds<'_>) -> bool {
    let ordered = |v: RawValue<'_>, out: Outside<'_>| {
        literal(v, collated).is_some_and(|x| match b {
            Bounds::Unknown => false,
            Bounds::Absent => true,
            Bounds::Within { min, max } => out(&x, min, max),
        })
    };
    let equal = |v: RawValue<'_>| ordered(v, &|x, min, max| x < min || x > max);
    match p {
        Pred::And(preds) => preds.iter().any(|p| pred_rules_out(p, collated, b)),
        Pred::Eq(v) => equal(v.get()),
        Pred::Cmp(op, v) => ordered(v.get(), &|x, min, max| match op {
            CmpOp::Gt => max <= x,
            CmpOp::Gte => max < x,
            CmpOp::Lt => min >= x,
            CmpOp::Lte => min > x,
        }),
        Pred::In(items) => {
            !items.is_empty()
                && items.iter().all(|i| match i {
                    InItem::Value(v) => equal(v.get()),
                    InItem::Regex(_) => false,
                })
        }
        _ => false,
    }
}

/// The encoding of a literal the zones can compare with, if it is one.
fn literal(v: RawValue<'_>, collated: bool) -> Option<Vec<u8>> {
    let usable = match v {
        RawValue::Null
        | RawValue::Undefined
        | RawValue::MinKey
        | RawValue::MaxKey
        | RawValue::Regex { .. }
        | RawValue::Array(_) => false,
        RawValue::String(_)
        | RawValue::Symbol(_)
        | RawValue::Document(_)
        | RawValue::DbPointer { .. }
        | RawValue::JavaScriptWithScope { .. } => !collated,
        _ => true,
    };
    usable.then(|| {
        let mut out = Vec::new();
        encode_value(&mut out, v, Direction::Ascending, &mut TypeBits::default());
        out
    })
}
