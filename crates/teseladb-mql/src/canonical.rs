//! The order RefDb gives the clauses of a conjunction before it evaluates them: by the kind of
//! each clause, then by its path (`matchExpressionComparator`). Matching does not depend on it;
//! the position of the positional `$` does, since the last clause that goes through an array
//! records it (measured on a `refrouter` 9.0.4: `{a: {$gt: 0, $lt: 3}}` gives the position `$gt`
//! found).

use crate::parse::{BitsOp, CmpOp, Expr, Pred};

/// The rank of a predicate's kind, in the order of RefDb's `MatchType`.
fn pred_rank(p: &Pred) -> u8 {
    match p {
        Pred::And(_) => 0,
        Pred::ElemMatchDoc(_) => 2,
        Pred::ElemMatchValue(_) => 3,
        Pred::Size(_) => 4,
        Pred::Eq(_) => 5,
        Pred::Cmp(CmpOp::Lte, _) => 6,
        Pred::Cmp(CmpOp::Lt, _) => 7,
        Pred::Cmp(CmpOp::Gt, _) => 8,
        Pred::Cmp(CmpOp::Gte, _) => 9,
        Pred::Regex(_) => 10,
        Pred::Mod { .. } => 11,
        Pred::Exists => 12,
        Pred::In(_) => 13,
        Pred::Bits(BitsOp::AllSet, _) => 14,
        Pred::Bits(BitsOp::AllClear, _) => 15,
        Pred::Bits(BitsOp::AnySet, _) => 16,
        Pred::Bits(BitsOp::AnyClear, _) => 17,
        Pred::Not(_) => 18,
        Pred::Type(_) => 20,
        Pred::Geo(g) if g.near => 27,
        Pred::Geo(_) => 21,
        // After every user-facing kind, as RefDb's internal match types.
        Pred::ExprCmp(CmpOp::Gt, _) => 28,
        Pred::ExprCmp(CmpOp::Gte, _) => 29,
        Pred::ExprCmp(CmpOp::Lt, _) => 30,
        Pred::ExprCmp(CmpOp::Lte, _) => 31,
    }
}

fn expr_rank(e: &Expr) -> (u8, &str) {
    match e {
        Expr::And(_) => (0, ""),
        Expr::Or(_) => (1, ""),
        Expr::Field { path, pred } => (pred_rank(pred), path),
        Expr::Nor(_) => (19, ""),
        Expr::Where(_) => (22, ""),
        Expr::Expr(_) | Expr::SampleRate(_) => (23, ""),
        Expr::False => (24, ""),
        Expr::True => (25, ""),
        Expr::Text(_) => (26, ""),
        Expr::JsonSchema(_) => (28, ""),
    }
}

/// Adds `e` to the clauses of a conjunction, opening nested conjunctions and the conjunctions of
/// operators on one path.
fn flatten(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::And(items) => items.into_iter().for_each(|i| flatten(i, out)),
        Expr::Field { path, pred: Pred::And(preds) } if !preds.is_empty() => {
            for p in preds {
                flatten(Expr::Field { path: path.clone(), pred: p }, out);
            }
        }
        other => out.push(canonical(other)),
    }
}

/// `e` with every conjunction in RefDb's order.
pub(crate) fn canonical(e: Expr) -> Expr {
    match e {
        Expr::And(items) => {
            let mut out = Vec::new();
            items.into_iter().for_each(|i| flatten(i, &mut out));
            out.sort_by(|a, b| expr_rank(a).cmp(&expr_rank(b)));
            Expr::And(out)
        }
        Expr::Or(items) => Expr::Or(items.into_iter().map(canonical).collect()),
        Expr::Nor(items) => Expr::Nor(items.into_iter().map(canonical).collect()),
        Expr::Field { path, pred: Pred::And(preds) } if preds.len() > 1 => {
            canonical(Expr::And(vec![Expr::Field { path, pred: Pred::And(preds) }]))
        }
        other => other,
    }
}

/// Whether [`canonical`] can change the order of `e`: a conjunction of several clauses, or of
/// several operators on one path, somewhere in it.
pub(crate) fn reorders(e: &Expr) -> bool {
    match e {
        Expr::And(items) => items.len() > 1 || items.iter().any(reorders),
        Expr::Or(items) | Expr::Nor(items) => items.iter().any(reorders),
        Expr::Field { pred: Pred::And(preds), .. } => preds.len() > 1,
        _ => false,
    }
}
