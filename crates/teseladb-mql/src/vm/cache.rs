//! The programs of a core by query shape (010 FR-031): a filter is compiled once per shape, and a
//! query of a known shape takes a copy of the program with the literals of its equalities,
//! comparisons and `$in`s rebound in the pool. The shape keeps the operators, the paths and the
//! kind of each such literal (which picks the specialized comparison); any other predicate is kept
//! whole, values included.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;

use super::compile::literal;
use super::{Fast, Op, Program, Selectivity};
use crate::parse::{Expr, InItem, Pred};
use crate::value::OwnedValue;

/// The programs a core keeps; past this many, the cache starts over.
pub const CACHED_PROGRAMS: usize = 1024;

thread_local! {
    /// The programs of this core by shape and by the epoch of the estimate that ordered them.
    static PROGRAMS: RefCell<HashMap<(String, u64), Program>> = RefCell::new(HashMap::new());
}

/// The kind of a literal: which comparison its value picks.
fn kind(v: &OwnedValue, out: &mut String) {
    let k = match literal(v).fast {
        Fast::I64(x) if i32::try_from(x).is_ok() => 'i',
        Fast::I64(_) => 'l',
        Fast::F64(_) => 'f',
        Fast::Str(_) => 's',
        Fast::Bool(_) => 'b',
        // `null` is told apart: the default estimate gives its equality another fraction.
        Fast::None if v.get() == teseladb_bson::RawValue::Null => 'z',
        Fast::None => 'v',
    };
    out.push(k);
}

/// The shape of filter `e`: equal for two filters that compile to the same program up to the
/// literals of their equalities, comparisons and `$in`s.
pub fn shape_key(e: &Expr) -> String {
    let mut out = String::new();
    expr_shape(e, &mut out);
    out
}

fn expr_shape(e: &Expr, out: &mut String) {
    match e {
        Expr::And(items) | Expr::Or(items) | Expr::Nor(items) => {
            out.push(match e {
                Expr::And(_) => '&',
                Expr::Or(_) => '|',
                _ => '!',
            });
            out.push('(');
            for x in items {
                expr_shape(x, out);
                out.push(',');
            }
            out.push(')');
        }
        Expr::Field { path, pred } => {
            let _ = write!(out, "{path:?}:");
            pred_shape(pred, out);
        }
        other => {
            let _ = write!(out, "{other:?}");
        }
    }
}

fn pred_shape(p: &Pred, out: &mut String) {
    match p {
        Pred::Eq(t) => {
            out.push_str("eq");
            kind(t, out);
        }
        Pred::Cmp(o, t) => {
            let _ = write!(out, "{o:?}");
            kind(t, out);
        }
        Pred::In(items) if items.iter().all(|i| matches!(i, InItem::Value(_))) => {
            out.push_str("in[");
            for i in items {
                if let InItem::Value(t) = i {
                    kind(t, out);
                }
            }
            out.push(']');
        }
        Pred::And(items) => {
            out.push_str("and(");
            for q in items {
                pred_shape(q, out);
                out.push(',');
            }
            out.push(')');
        }
        Pred::Not(q) => {
            out.push_str("not(");
            pred_shape(q, out);
            out.push(')');
        }
        Pred::ElemMatchValue(q) => {
            out.push_str("em(");
            pred_shape(q, out);
            out.push(')');
        }
        other => {
            let _ = write!(out, "{other:?}");
        }
    }
}

impl Program {
    /// The program of `root` from this core's cache, compiled and kept on a miss. `epoch` names
    /// the estimate `sel` gives: 0 for [`super::DefaultSelectivity`], and a number that changes
    /// whenever the statistics behind `sel` do.
    pub fn compile_cached(root: &Expr, sel: &dyn Selectivity, epoch: u64) -> Program {
        let key = (shape_key(root), epoch);
        let found = PROGRAMS.with(|c| c.borrow().get(&key).and_then(|p| p.rebound(root)));
        if let Some(p) = found {
            return p;
        }
        let p = Program::compile(root, sel);
        PROGRAMS.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= CACHED_PROGRAMS {
                c.clear();
            }
            c.insert(key, p.clone());
        });
        p
    }

    /// How many programs this core keeps.
    pub fn cached_programs() -> usize {
        PROGRAMS.with(|c| c.borrow().len())
    }

    /// A copy of this program with the literals of `root`, a filter of the same shape; `None`
    /// when a literal does not fit the instruction compiled for it.
    pub(crate) fn rebound(&self, root: &Expr) -> Option<Program> {
        let mut p = self.clone();
        for (k, leaf) in self.leaves.iter().enumerate() {
            if leaf.literals.is_empty() || leaf.expr.is_some() {
                continue;
            }
            let pred = super::run::find_pred(root, &leaf.at, leaf.elem.as_deref())?;
            let values: Vec<&OwnedValue> = match pred {
                Pred::Eq(t) | Pred::Cmp(_, t) => vec![t],
                Pred::In(items) => items
                    .iter()
                    .filter_map(|i| match i {
                        InItem::Value(t) => Some(t),
                        InItem::Regex(_) => None,
                    })
                    .collect(),
                _ => return None,
            };
            if values.len() != leaf.literals.len() {
                return None;
            }
            for (&slot, v) in leaf.literals.iter().zip(values) {
                let fresh = literal(v);
                if std::mem::discriminant(&fresh.fast)
                    != std::mem::discriminant(&self.pool[slot].fast)
                {
                    return None;
                }
                p.pool[slot] = fresh;
            }
            // The instructions that carry the literal inline.
            for (pc, &l) in self.leaf_of.iter().enumerate() {
                if l as usize != k {
                    continue;
                }
                let i = self.code[pc];
                let inline = match (i.op(), &p.pool[leaf.literals[0]].fast) {
                    (Op::CmpEqI64Imm | Op::CmpI64Imm, Fast::I64(x)) => i32::try_from(*x).ok()?,
                    (Op::CmpEqBoolImm, Fast::Bool(b)) => i32::from(*b),
                    _ => continue,
                };
                p.code[pc] = i.with_imm(inline);
            }
        }
        Some(p)
    }
}
