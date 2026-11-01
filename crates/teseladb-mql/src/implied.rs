//! The `$_internalExpr…` comparisons of a query, which RefDb only uses to bound an index:
//! measured on a `refrouter` 9.0.4, a query leaves them out of what it evaluates where they sit in
//! a conjunction (the top of the filter, `$and`, the operators of a path), drops a whole `$or`
//! that holds one, and keeps them under `$nor`, `$not` and `$elemMatch`. The partial filters of
//! indexes evaluate them ([`crate::FilterUse::Index`]).

use crate::parse::{Expr, Pred};

/// Whether a conjunction position of `e` holds a comparison a query leaves out.
pub(crate) fn has_implied(e: &Expr) -> bool {
    match e {
        Expr::And(items) | Expr::Or(items) => items.iter().any(has_implied),
        Expr::Field { pred, .. } => implied_pred(pred) || internal_in_and(pred),
        _ => false,
    }
}

/// `e` with each comparison in a conjunction position replaced by a match of everything; an
/// `$or` with a clause that is all such comparisons matches everything.
pub(crate) fn strip(e: Expr) -> Expr {
    match e {
        Expr::And(items) => Expr::And(items.into_iter().map(strip).collect()),
        Expr::Or(items) if items.iter().any(all_implied) => Expr::True,
        Expr::Or(items) => Expr::Or(items.into_iter().map(strip).collect()),
        Expr::Field { pred, .. } if implied_pred(&pred) => Expr::True,
        Expr::Field { path, pred: Pred::And(preds) } => {
            let kept: Vec<Pred> = preds.into_iter().filter(|p| !implied_pred(p)).collect();
            if kept.is_empty() { Expr::True } else { Expr::Field { path, pred: Pred::And(kept) } }
        }
        other => other,
    }
}

/// Whether `p` is one of the comparisons.
fn implied_pred(p: &Pred) -> bool {
    matches!(p, Pred::ExprEq(_) | Pred::ExprCmp(..))
}

/// Whether the operators of a path, together, hold one of the comparisons.
fn internal_in_and(p: &Pred) -> bool {
    matches!(p, Pred::And(preds) if preds.iter().any(implied_pred))
}

/// Whether clause `e` is only comparisons a query leaves out, so that it matches everything.
fn all_implied(e: &Expr) -> bool {
    match e {
        Expr::And(items) => !items.is_empty() && items.iter().all(all_implied),
        Expr::Or(items) => items.iter().any(all_implied),
        Expr::Field { pred: Pred::And(preds), .. } => preds.iter().all(implied_pred),
        Expr::Field { pred, .. } => implied_pred(pred),
        _ => false,
    }
}
