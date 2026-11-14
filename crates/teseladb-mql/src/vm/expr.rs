//! `$expr` on the register machine (010 FR-031): comparisons (`$eq`, `$ne`, `$gt`, `$gte`, `$lt`,
//! `$lte`) of field paths and constants, inside `$and` and `$or`, compile to `ExprCmp`
//! instructions that compare the registers in the reference server's order of values, a missing
//! value below every other. A path that crosses an array gives an array of what each element gives,
//! which the instruction leaves to the extensions for that comparison on that document. Any other
//! expression is one `Ext` instruction.

use teseladb_bson::RawValue;

use super::compile::{Compiler, LEAF_REGISTERS, Label};
use super::{Address, Leaf, NO_PATH, Op, REGISTERS};
use crate::value::OwnedValue;

/// The `src2` codes of `$eq` and `$ne` (the orderings use [`super::cmp_code`]).
pub(super) const EXPR_EQ: u8 = 4;
pub(super) const EXPR_NE: u8 = 5;

/// One side of a comparison.
enum Operand {
    Path(String),
    Const(OwnedValue),
}

/// An expression the machine runs.
enum Node {
    Cmp { code: u8, left: Operand, right: Operand, whole: OwnedValue },
    And(Vec<Node>),
    Or(Vec<Node>),
}

/// The code of comparison operator `name`.
fn code_of(name: &str) -> Option<u8> {
    Some(match name {
        "$gt" => 0,
        "$gte" => 1,
        "$lt" => 2,
        "$lte" => 3,
        "$eq" => EXPR_EQ,
        "$ne" => EXPR_NE,
        _ => return None,
    })
}

/// The code of the comparison with its sides swapped.
fn swapped(code: u8) -> u8 {
    match code {
        0 => 2,
        1 => 3,
        2 => 0,
        3 => 1,
        c => c,
    }
}

/// A field path of the current document (`"$a.b"`), or a constant that is neither a string
/// starting with `$`, a document nor an array (which are expressions of their own).
fn operand(v: RawValue<'_>) -> Option<Operand> {
    match v {
        RawValue::String(s) if s.starts_with('$') => {
            let path = &s[1..];
            let valid = !path.is_empty()
                && path.split('.').all(|seg| !seg.is_empty() && !seg.starts_with('$'));
            valid.then(|| Operand::Path(path.to_owned()))
        }
        RawValue::Document(_) | RawValue::Array(_) => None,
        v => OwnedValue::from_raw(v).ok().map(Operand::Const),
    }
}

/// The node of expression `e`, when the machine runs it.
fn node(e: RawValue<'_>) -> Option<Node> {
    let RawValue::Document(d) = e else { return None };
    let mut fields = d.iter();
    let (name, arg) = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    let RawValue::Array(args) = arg else { return None };
    let args: Vec<RawValue<'_>> = args.iter().collect();
    match name {
        "$and" | "$or" => {
            let items = args.into_iter().map(node).collect::<Option<Vec<Node>>>()?;
            Some(if name == "$and" { Node::And(items) } else { Node::Or(items) })
        }
        _ => {
            let code = code_of(name)?;
            let [a, b] = args.as_slice() else { return None };
            let (left, right) = (operand(*a)?, operand(*b)?);
            let whole = OwnedValue::from_raw(e).ok()?;
            match (left, right) {
                (Operand::Const(_), Operand::Const(_)) => None,
                (Operand::Const(c), Operand::Path(p)) => Some(Node::Cmp {
                    code: swapped(code),
                    left: Operand::Path(p),
                    right: Operand::Const(c),
                    whole,
                }),
                (left, right) => Some(Node::Cmp { code, left, right, whole }),
            }
        }
    }
}

/// Compiles `$expr` `e` at node `at`; `false` (with nothing emitted) when the machine does not
/// run it.
pub(super) fn compile_expr(
    c: &mut Compiler<'_>,
    e: RawValue<'_>,
    at: &[usize],
    yes: Label,
    no: Label,
    fall: bool,
) -> bool {
    let Some(n) = node(e) else { return false };
    if c.free() + LEAF_REGISTERS > REGISTERS {
        return false;
    }
    emit(c, &n, at, yes, no, fall);
    true
}

fn emit(c: &mut Compiler<'_>, n: &Node, at: &[usize], yes: Label, no: Label, fall: bool) {
    match n {
        Node::And(items) | Node::Or(items) => {
            let all = matches!(n, Node::And(_));
            if items.is_empty() {
                c.constant(all, yes, no, fall);
                return;
            }
            let last = items.len() - 1;
            for (k, item) in items.iter().enumerate() {
                if k == last {
                    emit(c, item, at, yes, no, fall);
                } else {
                    let next = c.label();
                    if all {
                        emit(c, item, at, next, no, true);
                    } else {
                        emit(c, item, at, yes, next, false);
                    }
                    c.bind(next);
                }
            }
        }
        Node::Cmp { code, left, right, whole } => {
            let (a, b, dst) = (c.free(), c.free() + 1, c.free() + 2);
            let Operand::Path(lp) = left else { unreachable!("a comparison reads a path first") };
            c.load_into(lp, a);
            let (whole_at, _) = c.literal(whole);
            let addr = Address { exprs: at.to_vec(), preds: None };
            let mut leaf = Leaf { expr: Some(whole_at), ..Leaf::new(addr, NO_PATH) };
            match right {
                Operand::Path(rp) => {
                    c.load_into(rp, b);
                    let r = i32::try_from(b).expect("32 registers");
                    c.emit(Op::ExprCmp, dst, a, *code, r);
                }
                Operand::Const(v) => {
                    let (i, _) = c.literal(v);
                    leaf.literals.push(i);
                    c.emit(Op::ExprCmpLit, dst, a, *code, 0);
                }
            }
            c.attach(leaf);
            c.branch(dst, yes, no, fall);
        }
    }
}
