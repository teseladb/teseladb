//! The document an upsert inserts (FR-013): the equalities of the filter (`$eq`, a field's value,
//! `$in` with one value, inside `$and` and a one-clause `$or`), then the update applied as an
//! insert, then `_id` first, generated if nothing set it.

use teseladb_bson::{Document, Value};

use super::positional::{ArrayFilters, UpdatePath, Walk, is_prefix};
use super::tree::{MDoc, MVal};
use super::{Update, UpdateContext};
use crate::error::{MqlError, Result, codes};
use crate::matcher::Filter;
use crate::ops;
use crate::parse::{Expr, InItem, Pred};
use crate::value::{OwnedDoc, OwnedValue};

/// The document an upsert inserts when `filter` matches nothing; `new_id` gives the `_id` when
/// neither the filter nor the update sets it.
pub fn upsert(
    filter: &Filter,
    update: &Update,
    ctx: &UpdateContext<'_>,
    new_id: impl FnOnce() -> Value,
) -> Result<Document> {
    let mut eqs = Vec::new();
    collect(filter.expr(), &mut eqs);
    check(&eqs)?;
    if update.is_replacement() {
        eqs.retain(|(p, _)| p == ops::ID);
    }
    let base = build(&eqs, ctx)?;
    let owned = OwnedDoc::new(&base)?;
    let ctx = UpdateContext { insert: true, ..*ctx };
    let doc = update.apply(owned.get(), &ctx)?.doc;
    Ok(id_first(doc, new_id))
}

/// Refuses a filter an upsert cannot take: one with `$expr` anywhere (measured on a `refrouter`
/// 9.0.4, before the update runs).
pub fn check_filter(filter: &Filter) -> Result<()> {
    fn has_expr(e: &Expr) -> bool {
        match e {
            Expr::Expr(_) => true,
            Expr::And(items) | Expr::Or(items) | Expr::Nor(items) => items.iter().any(has_expr),
            _ => false,
        }
    }
    if has_expr(filter.expr()) {
        return Err(MqlError::new(
            codes::QUERY_FEATURE_NOT_ALLOWED,
            "$expr is not allowed in the query predicate for an upsert",
        ));
    }
    Ok(())
}

/// Equalities of the filter, in its order.
fn collect<'f>(expr: &'f Expr, out: &mut Vec<(String, &'f OwnedValue)>) {
    match expr {
        Expr::And(items) => {
            for e in items {
                collect(e, out);
            }
        }
        Expr::Or(items) if items.len() == 1 => collect(&items[0], out),
        Expr::Field { path, pred } => {
            if let Some(v) = equality(pred) {
                out.push((path.clone(), v));
            }
        }
        _ => {}
    }
}

fn equality(pred: &Pred) -> Option<&OwnedValue> {
    match pred {
        Pred::Eq(v) => Some(v),
        Pred::In(items) if items.len() == 1 => match &items[0] {
            InItem::Value(v) => Some(v),
            InItem::Regex(_) => None,
        },
        Pred::And(preds) => preds.iter().find_map(equality),
        _ => None,
    }
}

/// A path set twice, or a path and one inside it, cannot both be set.
fn check(eqs: &[(String, &OwnedValue)]) -> Result<()> {
    for (i, (p, _)) in eqs.iter().enumerate() {
        for (q, _) in &eqs[..i] {
            if p == q {
                return Err(MqlError::new(
                    codes::NOT_SINGLE_VALUE_FIELD,
                    format!("cannot infer query fields to set, path '{p}' is matched twice"),
                ));
            }
            if is_prefix(q, p) || is_prefix(p, q) {
                let (long, short) = if p.len() > q.len() { (p, q) } else { (q, p) };
                return Err(MqlError::new(
                    codes::NOT_SINGLE_VALUE_FIELD,
                    format!(
                        "cannot infer query fields to set, both paths '{long}' and '{short}' are matched"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn build(eqs: &[(String, &OwnedValue)], ctx: &UpdateContext<'_>) -> Result<Document> {
    let filters = ArrayFilters::default();
    let mut root = MVal::Doc(MDoc::default());
    for (path, v) in eqs {
        let parsed = UpdatePath::parse(path)?;
        let walk =
            Walk { filters: &filters, ctx: &ctx.matcher, position: None, create: true, text: path };
        let value = MVal::from_raw(v.get());
        walk.run(&mut root, "", &parsed.segs, &mut |mut slot| slot.set(value.clone()))?;
    }
    Ok(match root {
        MVal::Doc(d) => d.to_document(),
        _ => Document::new(),
    })
}

/// `doc` with `_id` as its first field.
fn id_first(doc: Document, new_id: impl FnOnce() -> Value) -> Document {
    let mut out = Document::new();
    let id = doc.get(ops::ID).cloned().unwrap_or_else(new_id);
    out.push(ops::ID, id);
    for (k, v) in doc.iter() {
        if k != ops::ID {
            out.push(k, v.clone());
        }
    }
    out
}
