//! Running a [`Program`]: on one document, and on a batch of up to 64 with a mask of live
//! documents ([`VectorVm`]).

use std::cmp::Ordering;

use teseladb_bson::{RawDoc, RawValue};

use super::value::{Tag, ValueRef, element_type, elements, field, iter, next, root};
use super::{Address, BATCH, Fast, Insn, Leaf, Literal, NO_EXPAND, Op, Program, REGISTERS, cmp_of};
use crate::error::Result;
use crate::matcher::{Eval, MatchContext, compare, eq};
use crate::parse::{CmpOp, Expr, Pred};

/// What the instructions read besides their registers.
struct Env<'p, 'c, 'a> {
    program: &'p Program,
    root: &'p Expr,
    ctx: &'c MatchContext<'a>,
}

impl Literal {
    /// The literal as BSON.
    #[inline(always)]
    fn value(&self) -> RawValue<'_> {
        // The holder is `{"": value}` as `Document::to_bytes` wrote it.
        RawDoc::from_stored(&self.holder)
            .ok()
            .and_then(|d| d.iter().next())
            .map_or(RawValue::Null, |(_, v)| v)
    }
}

/// The value at `path`, walked on the bytes through documents.
#[inline(always)]
fn load<'d>(doc: &'d [u8], path: &[String]) -> ValueRef<'d> {
    let mut cur = root(doc);
    for seg in path {
        cur = match cur.tag {
            Tag::Doc => field(doc, cur, seg.as_bytes()),
            Tag::Array => return ValueRef::new(Tag::ThroughArray, 0, 0),
            _ => return ValueRef::MISSING,
        };
    }
    cur
}

/// Whether ordering `o` satisfies `op`.
#[inline(always)]
fn ordered(op: CmpOp, o: Ordering) -> bool {
    match op {
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Gte => o != Ordering::Less,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Lte => o != Ordering::Greater,
    }
}

impl Env<'_, '_, '_> {
    /// The subtree at `at` of the filter's tree, and its predicate when `at` names one.
    fn find(&self, at: &Address) -> (&Expr, Option<(&str, &Pred)>) {
        let mut e = self.root;
        for &i in &at.exprs {
            e = match e {
                Expr::And(items) | Expr::Or(items) | Expr::Nor(items) => &items[i],
                _ => unreachable!("an address follows the tree it was compiled from"),
            };
        }
        let Some(preds) = &at.preds else { return (e, None) };
        let Expr::Field { path, pred } = e else {
            unreachable!("a predicate's address ends at its field");
        };
        let mut p = pred;
        for &i in preds {
            p = match p {
                Pred::And(items) => &items[i],
                Pred::Not(inner) => inner,
                _ => unreachable!("an address follows the tree it was compiled from"),
            };
        }
        (e, Some((path.as_str(), p)))
    }

    /// The tree evaluator's answer for the subtree at `at`.
    fn tree(&self, at: &Address, doc: RawDoc<'_>) -> Result<bool> {
        let eval = Eval::new(self.ctx, false);
        match self.find(at) {
            (_, Some((path, pred))) => eval.path(pred, doc, path),
            (e, None) => eval.expr(e, doc),
        }
    }

    fn literal(&self, i: Insn) -> &Literal {
        &self.program.pool[i.index()]
    }

    /// Comparison instruction `i` (at `pc`) on `v`: the value itself, then, unless `NO_EXPAND`,
    /// each element of an array at the end of the path.
    #[inline(always)]
    fn leaf(&self, pc: usize, i: Insn, v: ValueRef<'_>, doc: RawDoc<'_>) -> Result<bool> {
        let leaf: &Leaf = &self.program.leaves[self.program.leaf_of[pc] as usize];
        if v.tag == Tag::ThroughArray {
            return self.tree(&leaf.at, doc);
        }
        let bytes = doc.as_bytes();
        if self.single(i, leaf, v, bytes) {
            return Ok(true);
        }
        if i.src2() & NO_EXPAND == 0 && v.tag == Tag::Array {
            return Ok(elements(bytes, v).any(|e| self.single(i, leaf, e, bytes)));
        }
        Ok(false)
    }

    /// Comparison instruction `i` on one value.
    #[inline(always)]
    fn single<'d>(&self, i: Insn, leaf: &Leaf, v: ValueRef<'d>, doc: &'d [u8]) -> bool {
        let coll = self.ctx.collation;
        let int = matches!(v.tag, Tag::I32 | Tag::I64);
        let generic_eq = |l: &Literal| eq(v.raw(doc), l.value(), coll);
        let generic_cmp = |l: &Literal| compare(cmp_of(i.src2()), v.raw(doc), l.value(), coll);
        match i.op() {
            Op::Exists => v.tag != Tag::Missing,
            Op::Size => {
                v.tag == Tag::Array
                    && i64::try_from(elements(doc, v).count()).ok() == Some(i64::from(i.imm()))
            }
            Op::Type => {
                v.tag != Tag::Missing && leaf.types.iter().any(|s| s.matches(element_type(v.tag)))
            }
            Op::CmpEqI64Imm if int => v.payload as i64 == i64::from(i.imm()),
            Op::CmpEqI64 => match (int, &self.literal(i).fast) {
                (true, Fast::I64(k)) => v.payload as i64 == *k,
                _ => generic_eq(self.literal(i)),
            },
            Op::CmpEqF64 => match (v.tag, &self.literal(i).fast) {
                (Tag::F64, Fast::F64(k)) if !f64::from_bits(v.payload).is_nan() => {
                    f64::from_bits(v.payload) == *k
                }
                _ => generic_eq(self.literal(i)),
            },
            Op::CmpEqStr => match (v.tag, &self.literal(i).fast, coll) {
                (Tag::Str, Fast::Str(k), None) => v.bytes(doc) == Some(k.as_bytes()),
                _ => generic_eq(self.literal(i)),
            },
            Op::CmpEqBoolImm if v.tag == Tag::Bool => (v.payload != 0) == (i.imm() != 0),
            Op::CmpEqI64Imm | Op::CmpEqBoolImm | Op::CmpEq => generic_eq(self.leaf_literal(leaf)),
            Op::CmpI64Imm if int => {
                ordered(cmp_of(i.src2()), (v.payload as i64).cmp(&i64::from(i.imm())))
            }
            Op::CmpI64 => match (int, &self.literal(i).fast) {
                (true, Fast::I64(k)) => ordered(cmp_of(i.src2()), (v.payload as i64).cmp(k)),
                _ => generic_cmp(self.literal(i)),
            },
            Op::CmpF64 => match (v.tag, &self.literal(i).fast) {
                (Tag::F64, Fast::F64(k)) if !f64::from_bits(v.payload).is_nan() => {
                    // Neither is NaN, and -0.0 equals 0.0 as numbers do in RefDb.
                    let o = f64::from_bits(v.payload).partial_cmp(k).unwrap_or(Ordering::Equal);
                    ordered(cmp_of(i.src2()), o)
                }
                _ => generic_cmp(self.literal(i)),
            },
            Op::CmpStr => match (v.tag, &self.literal(i).fast, coll, v.bytes(doc)) {
                (Tag::Str, Fast::Str(k), None, Some(b)) => {
                    ordered(cmp_of(i.src2()), b.cmp(k.as_bytes()))
                }
                _ => generic_cmp(self.literal(i)),
            },
            Op::CmpI64Imm | Op::Cmp => {
                compare(cmp_of(i.src2()), v.raw(doc), self.leaf_literal(leaf).value(), coll)
            }
            Op::In => leaf.literals.iter().any(|&l| generic_eq(&self.program.pool[l])),
            _ => unreachable!("a comparison instruction"),
        }
    }

    /// The literal of a leaf whose instruction carries it inline.
    fn leaf_literal(&self, leaf: &Leaf) -> &Literal {
        &self.program.pool[leaf.literals[0]]
    }
}

impl Program {
    /// Whether `doc` matches the filter `root` this program was compiled from.
    pub fn run(&self, root: &Expr, doc: RawDoc<'_>, ctx: &MatchContext<'_>) -> Result<bool> {
        self.run_counted(root, doc, ctx).map(|(m, _)| m)
    }

    /// [`Program::run`] and the number of instructions it executed.
    pub fn run_counted(
        &self,
        root: &Expr,
        doc: RawDoc<'_>,
        ctx: &MatchContext<'_>,
    ) -> Result<(bool, usize)> {
        let env = Env { program: self, root, ctx };
        let bytes = doc.as_bytes();
        let mut regs = [ValueRef::MISSING; REGISTERS];
        let mut pc = 0;
        let mut steps = 0;
        loop {
            let i = self.code[pc];
            steps += 1;
            let mut next_pc = pc + 1;
            match i.op() {
                Op::LoadField => regs[i.dst()] = load(bytes, &self.paths[i.index()]),
                Op::Tree => {
                    regs[i.dst()] = ValueRef::boolean(env.tree(&self.trees[i.index()], doc)?);
                }
                Op::IterInit => {
                    let a = regs[i.src1()];
                    if a.tag == Tag::Array {
                        regs[i.dst()] = iter(a);
                    } else {
                        next_pc = i.target(pc);
                    }
                }
                Op::IterNext => match next(bytes, &mut regs[i.src1()]) {
                    Some(e) => regs[i.dst()] = e,
                    None => next_pc = i.target(pc),
                },
                Op::JumpIfThrough if regs[i.src1()].tag == Tag::ThroughArray => {
                    next_pc = i.target(pc);
                }
                Op::JumpIfFalse if !regs[i.src1()].truth() => next_pc = i.target(pc),
                Op::JumpIfTrue if regs[i.src1()].truth() => next_pc = i.target(pc),
                Op::Jump => next_pc = i.target(pc),
                Op::JumpIfThrough | Op::JumpIfFalse | Op::JumpIfTrue => {}
                Op::Yield => return Ok((true, steps)),
                Op::Reject => return Ok((false, steps)),
                _ => {
                    let answer = env.leaf(pc, i, regs[i.src1()], doc)?;
                    regs[i.dst()] = ValueRef::boolean(answer);
                }
            }
            pc = next_pc;
        }
    }

    /// The documents of `docs` (at most 64) that match `root`, as the bits of a mask: bit `i`
    /// for `docs[i]`. An error is the first one the documents give evaluated in order.
    pub fn run_batch(
        &self,
        root: &Expr,
        docs: &[RawDoc<'_>],
        ctx: &MatchContext<'_>,
    ) -> Result<u64> {
        assert!(docs.len() <= BATCH, "a batch holds at most {BATCH} documents");
        let env = Env { program: self, root, ctx };
        let lanes =
            if self.sequential { Err(()) } else { VectorVm::new(docs).run(&env).map_err(drop) };
        match lanes {
            Ok(mask) => Ok(mask),
            Err(()) => {
                // The error the documents give one after the other.
                let mut mask = 0u64;
                for (l, d) in docs.iter().enumerate() {
                    if self.run(root, *d, ctx)? {
                        mask |= 1 << l;
                    }
                }
                Ok(mask)
            }
        }
    }
}

/// The machine over a batch: a document per lane, the lanes still running, and a bank of
/// registers per lane.
struct VectorVm<'d, 'b> {
    docs: &'b [RawDoc<'d>],
    active: u64,
    regs: [[ValueRef<'d>; BATCH]; REGISTERS],
}

impl<'d, 'b> VectorVm<'d, 'b> {
    fn new(docs: &'b [RawDoc<'d>]) -> VectorVm<'d, 'b> {
        let active = if docs.len() == BATCH { u64::MAX } else { (1u64 << docs.len()) - 1 };
        VectorVm { docs, active, regs: [[ValueRef::MISSING; BATCH]; REGISTERS] }
    }

    /// The lanes whose register `r` holds true, among `m`.
    #[inline(always)]
    fn truths(&self, r: usize, m: u64) -> u64 {
        let mut bits = 0u64;
        for (l, v) in self.regs[r].iter().enumerate() {
            bits |= u64::from(v.truth()) << l;
        }
        bits & m
    }

    /// Runs the program; the mask of the lanes that yielded.
    fn run(&mut self, env: &Env<'_, '_, '_>) -> Result<u64> {
        let p = env.program;
        // The lanes waiting at each instruction.
        let mut at = vec![0u64; p.code.len()];
        at[0] = self.active;
        let mut matched = 0u64;
        let mut pc = 0;
        while pc < p.code.len() && self.active != 0 {
            let m = std::mem::take(&mut at[pc]);
            if m == 0 {
                pc += 1;
                continue;
            }
            let i = p.code[pc];
            // The lanes that jump, and where to.
            let mut jump = 0u64;
            match i.op() {
                Op::LoadField => {
                    let path = &p.paths[i.index()];
                    for l in lanes(m) {
                        self.regs[i.dst()][l] = load(self.docs[l].as_bytes(), path);
                    }
                }
                Op::Tree => {
                    let a = &p.trees[i.index()];
                    for l in lanes(m) {
                        self.regs[i.dst()][l] = ValueRef::boolean(env.tree(a, self.docs[l])?);
                    }
                }
                Op::IterInit => {
                    for l in lanes(m) {
                        let a = self.regs[i.src1()][l];
                        if a.tag == Tag::Array {
                            self.regs[i.dst()][l] = iter(a);
                        } else {
                            jump |= 1 << l;
                        }
                    }
                }
                Op::IterNext => {
                    for l in lanes(m) {
                        let bytes = self.docs[l].as_bytes();
                        match next(bytes, &mut self.regs[i.src1()][l]) {
                            Some(e) => self.regs[i.dst()][l] = e,
                            None => jump |= 1 << l,
                        }
                    }
                }
                Op::JumpIfThrough => {
                    for l in lanes(m) {
                        jump |= u64::from(self.regs[i.src1()][l].tag == Tag::ThroughArray) << l;
                    }
                }
                Op::JumpIfFalse => jump = m & !self.truths(i.src1(), m),
                Op::JumpIfTrue => jump = self.truths(i.src1(), m),
                Op::Jump => jump = m,
                Op::Yield => {
                    matched |= m;
                    self.active &= !m;
                    pc += 1;
                    continue;
                }
                Op::Reject => {
                    self.active &= !m;
                    pc += 1;
                    continue;
                }
                _ => {
                    for l in lanes(m) {
                        let v = self.regs[i.src1()][l];
                        let answer = env.leaf(pc, i, v, self.docs[l])?;
                        self.regs[i.dst()][l] = ValueRef::boolean(answer);
                    }
                }
            }
            let stay = m & !jump;
            if stay != 0 {
                at[pc + 1] |= stay;
            }
            if jump != 0 {
                let target = i.target(pc);
                at[target] |= jump;
                // A jump back (the loop of `$elemMatch`) runs again from its target.
                if target <= pc {
                    pc = target;
                    continue;
                }
            }
            pc += 1;
        }
        Ok(matched)
    }
}

/// The lanes of mask `m`, in order.
#[inline(always)]
fn lanes(mut m: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if m == 0 {
            return None;
        }
        let l = m.trailing_zeros() as usize;
        m &= m - 1;
        Some(l)
    })
}
