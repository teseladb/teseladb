//! Running a [`Program`]: on one document, and on a batch of up to 64 with a mask of live
//! documents ([`VectorVm`]).

use std::cmp::Ordering;

use teseladb_bson::{RawDoc, RawValue};

use super::compile::elem_match_docs;
use super::expr::{EXPR_EQ, EXPR_NE};
use super::value::{Tag, ValueRef, element_type, elements, field, iter, next, root, stored_field};
use super::{Address, BATCH, Fast, Insn, Leaf, Literal, NO_EXPAND, Op, Program, REGISTERS, cmp_of};
use crate::error::Result;
use crate::matcher::{Eval, MatchContext, array_index, compare, eq};
use crate::parse::{CmpOp, Expr, Pred};
use crate::simd::{self, Cmp};
use crate::value::{bson_error, cmp};
use teseladb_keys::stored::{FieldKey, Stored};

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

/// The value at `path`, read through documents: through the stored document's table of offsets
/// where it has one (003 FR-021), else walked on the bytes.
#[inline(always)]
pub(super) fn load<'d>(doc: &Stored<'d>, path: &[String], keys: &[FieldKey]) -> ValueRef<'d> {
    let mut cur = root(doc.bson());
    for (seg, key) in path.iter().zip(keys) {
        cur = match cur.tag {
            Tag::Doc => stored_field(doc, cur, seg.as_bytes(), *key),
            Tag::Array => return ValueRef::new(Tag::ThroughArray, 0, 0),
            _ => return ValueRef::MISSING,
        };
    }
    cur
}

/// Walks `segs` from `cur` with RefDb's rules for arrays inside a path, calling `f` on each
/// value reached with whether an array there is also tried element by element, until `f` says
/// yes: the walk of the tree evaluator, on the bytes.
fn walk<'d>(
    doc: &'d [u8],
    cur: ValueRef<'d>,
    segs: &[String],
    f: &mut dyn FnMut(ValueRef<'d>, bool) -> Result<bool>,
) -> Result<bool> {
    let Some((seg, rest)) = segs.split_first() else { return f(cur, true) };
    match cur.tag {
        Tag::Doc => walk(doc, field(doc, cur, seg.as_bytes()), rest, f),
        Tag::Array => {
            // An array reached by position at the end of the path is not expanded.
            if let Some(e) = array_index(seg).and_then(|i| elements(doc, cur).nth(i)) {
                let hit = if rest.is_empty() { f(e, false)? } else { walk(doc, e, rest, f)? };
                if hit {
                    return Ok(true);
                }
            }
            for e in elements(doc, cur) {
                if e.tag == Tag::Doc && walk(doc, field(doc, e, seg.as_bytes()), rest, f)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => f(ValueRef::MISSING, true),
    }
}

/// The node at `at` of `root`, and its predicate when `at` names one; `None` when `root` has
/// another shape.
pub(super) fn find_node<'r>(
    root: &'r Expr,
    at: &Address,
) -> Option<(&'r Expr, Option<(&'r str, &'r Pred)>)> {
    let mut e = root;
    for &i in &at.exprs {
        e = match e {
            Expr::And(items) | Expr::Or(items) | Expr::Nor(items) => items.get(i)?,
            _ => return None,
        };
    }
    let Some(preds) = &at.preds else { return Some((e, None)) };
    let Expr::Field { path, pred } = e else { return None };
    Some((e, Some((path.as_str(), descend(pred, preds)?))))
}

/// The predicate at positions `place` inside `p`'s `$and`s and `$not`s.
fn descend<'r>(mut p: &'r Pred, place: &[usize]) -> Option<&'r Pred> {
    for &i in place {
        p = match p {
            Pred::And(items) => items.get(i)?,
            Pred::Not(inner) => inner,
            _ => return None,
        };
    }
    Some(p)
}

/// The predicate of a leaf: at `at`, then at `elem` inside its `$elemMatch`.
pub(super) fn find_pred<'r>(
    root: &'r Expr,
    at: &Address,
    elem: Option<&[usize]>,
) -> Option<&'r Pred> {
    let (_, Some((_, p))) = find_node(root, at)? else { return None };
    match elem {
        None => Some(p),
        Some(place) => match p {
            Pred::ElemMatchValue(inner) => descend(inner, place),
            _ => None,
        },
    }
}

/// How many `$elemMatch`es on documents [`elem_match_docs`] finds in `p`.
fn elem_match_count(p: &Pred) -> usize {
    let mut docs = Vec::new();
    elem_match_docs(p, &mut docs);
    docs.len()
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
        find_node(self.root, at).expect("an address follows the tree it was compiled from")
    }

    /// The extensions' answer for node `at` (`$expr`, `$text`, `$sampleRate`, `$jsonSchema`).
    fn ext(&self, at: &Address, doc: RawDoc<'_>) -> Result<bool> {
        let ctx = self.ctx;
        match self.find(at).0 {
            Expr::Expr(e) => ctx.ext.expr(e.get(), doc, ctx.vars),
            Expr::Text(q) => ctx.ext.text(q, doc),
            Expr::SampleRate(rate) => Ok(ctx.ext.random() < *rate),
            Expr::JsonSchema(schema) => schema.matches(doc),
            _ => unreachable!("an extension instruction stands for an extension's node"),
        }
    }

    /// `$expr` comparison `i` of `a` and `b` (or the leaf's literal), in RefDb's order of
    /// values with a missing value below every other; a path through an array leaves the
    /// comparison to the extensions.
    fn expr_cmp(
        &self,
        i: Insn,
        leaf: &Leaf,
        a: ValueRef<'_>,
        b: Option<ValueRef<'_>>,
        doc: RawDoc<'_>,
    ) -> Result<bool> {
        // A path through an array, and strings under a collation (which the extensions' scope
        // decides, not the filter's context), are the extensions' to compare.
        let collated = self.ctx.collation.is_some();
        let theirs = |v: ValueRef<'_>| {
            v.tag == Tag::ThroughArray
                || (collated && matches!(v.tag, Tag::Str | Tag::Symbol | Tag::Doc | Tag::Array))
        };
        let literal_text = b.is_none()
            && collated
            && matches!(self.leaf_literal(leaf).value(), RawValue::String(_) | RawValue::Symbol(_));
        if theirs(a) || b.is_some_and(theirs) || literal_text {
            let whole = &self.program.pool[leaf.expr.expect("an $expr comparison keeps itself")];
            return self.ctx.ext.expr(whole.value(), doc, self.ctx.vars);
        }
        let bytes = doc.as_bytes();
        let x = a.raw(bytes);
        let y = match b {
            Some(b) => b.raw(bytes),
            None => Some(self.leaf_literal(leaf).value()),
        };
        let o = match (x, y) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => cmp(x, y, self.ctx.collation),
        };
        Ok(match i.src2() {
            EXPR_EQ => o == Ordering::Equal,
            EXPR_NE => o != Ordering::Equal,
            code => ordered(cmp_of(code), o),
        })
    }

    /// `Pred` instruction `leaf` on `v`: the leaf's predicate on the value, or on each value the
    /// path reaches through arrays.
    fn pred(&self, leaf: &Leaf, v: ValueRef<'_>, doc: RawDoc<'_>) -> Result<bool> {
        let (_, Some((_, pred))) = self.find(&leaf.at) else {
            unreachable!("a predicate instruction's address names a predicate");
        };
        let bytes = doc.as_bytes();
        if let Pred::ExprEq(_) | Pred::ExprCmp(..) = pred {
            // The first array on the path stands for the whole path, and an array matches.
            if v.tag == Tag::ThroughArray {
                return Ok(true);
            }
            return Eval::new(self.ctx, false).single(pred, v.raw(bytes));
        }
        if v.tag == Tag::ThroughArray {
            let path = &self.program.paths[leaf.path as usize];
            return walk(bytes, root(bytes), path, &mut |w, expand| {
                self.on_value(leaf, pred, 0, w, expand, doc)
            });
        }
        self.on_value(leaf, pred, 0, v, true, doc)
    }

    /// Predicate `p` on value `v` at the end of a path (`expand` also tries an array's
    /// elements); `subs` is the index of `p`'s first `$elemMatch` on documents in the leaf's
    /// programs.
    fn on_value(
        &self,
        leaf: &Leaf,
        p: &Pred,
        subs: usize,
        v: ValueRef<'_>,
        expand: bool,
        doc: RawDoc<'_>,
    ) -> Result<bool> {
        let bytes = doc.as_bytes();
        match p {
            Pred::And(items) => {
                let mut at = subs;
                for q in items {
                    if !self.on_value(leaf, q, at, v, expand, doc)? {
                        return Ok(false);
                    }
                    at += elem_match_count(q);
                }
                Ok(true)
            }
            Pred::Not(q) => Ok(!self.on_value(leaf, q, subs, v, expand, doc)?),
            Pred::Exists => Ok(v.tag != Tag::Missing),
            Pred::Size(n) => {
                Ok(v.tag == Tag::Array
                    && i64::try_from(elements(bytes, v).count()).ok() == Some(*n))
            }
            Pred::ElemMatchDoc(e) => {
                if v.tag != Tag::Array {
                    return Ok(false);
                }
                let program = &leaf.subs[subs];
                for el in elements(bytes, v) {
                    if !matches!(el.tag, Tag::Doc | Tag::Array) {
                        continue;
                    }
                    let Some(b) = el.bytes(bytes) else { continue };
                    let d = RawDoc::from_stored(b).map_err(bson_error)?;
                    if program.run(e, Stored::plain(d), self.ctx)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Pred::ElemMatchValue(q) => {
                if v.tag != Tag::Array {
                    return Ok(false);
                }
                for el in elements(bytes, v) {
                    if self.on_value(leaf, q, subs, el, false, doc)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => {
                let eval = Eval::new(self.ctx, false);
                if eval.single(p, v.raw(bytes))? {
                    return Ok(true);
                }
                if expand && v.tag == Tag::Array {
                    for el in elements(bytes, v) {
                        if eval.single(p, el.raw(bytes))? {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
        }
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
        if i.op() == Op::Pred {
            return self.pred(leaf, v, doc);
        }
        let bytes = doc.as_bytes();
        let mut on = |w: ValueRef<'_>, expand: bool| {
            Ok(self.single(i, leaf, w, bytes)
                || (expand
                    && i.src2() & NO_EXPAND == 0
                    && w.tag == Tag::Array
                    && elements(bytes, w).any(|e| self.single(i, leaf, e, bytes))))
        };
        if v.tag == Tag::ThroughArray {
            let path = &self.program.paths[leaf.path as usize];
            return walk(bytes, root(bytes), path, &mut on);
        }
        on(v, true)
    }

    /// The leaf of the instruction at `pc`.
    fn leaf_at(&self, pc: usize) -> &Leaf {
        &self.program.leaves[self.program.leaf_of[pc] as usize]
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
    pub fn run<'d>(
        &self,
        root: &Expr,
        doc: impl Into<Stored<'d>>,
        ctx: &MatchContext<'_>,
    ) -> Result<bool> {
        self.run_counted(root, doc.into(), ctx).map(|(m, _)| m)
    }

    /// [`Program::run`] and the number of instructions it executed.
    pub fn run_counted<'d>(
        &self,
        root: &Expr,
        stored: impl Into<Stored<'d>>,
        ctx: &MatchContext<'_>,
    ) -> Result<(bool, usize)> {
        let stored = stored.into();
        let env = Env { program: self, root, ctx };
        let doc = stored.raw();
        let bytes = doc.as_bytes();
        let mut regs = [ValueRef::MISSING; REGISTERS];
        let mut pc = 0;
        let mut steps = 0;
        loop {
            let i = self.code[pc];
            steps += 1;
            let mut next_pc = pc + 1;
            match i.op() {
                Op::LoadField => {
                    regs[i.dst()] = load(&stored, &self.paths[i.index()], &self.keys[i.index()])
                }
                Op::Tree => {
                    regs[i.dst()] = ValueRef::boolean(env.tree(&self.trees[i.index()], doc)?);
                }
                Op::Ext => {
                    regs[i.dst()] = ValueRef::boolean(env.ext(&self.trees[i.index()], doc)?);
                }
                Op::ExprCmp | Op::ExprCmpLit => {
                    let b = (i.op() == Op::ExprCmp).then(|| regs[i.index()]);
                    let answer = env.expr_cmp(i, env.leaf_at(pc), regs[i.src1()], b, doc)?;
                    regs[i.dst()] = ValueRef::boolean(answer);
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
        docs: &[Stored<'_>],
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
    docs: &'b [Stored<'d>],
    active: u64,
    regs: [[ValueRef<'d>; BATCH]; REGISTERS],
}

impl<'d, 'b> VectorVm<'d, 'b> {
    fn new(docs: &'b [Stored<'d>]) -> VectorVm<'d, 'b> {
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
                    let (path, keys) = (&p.paths[i.index()], &p.keys[i.index()]);
                    for l in lanes(m) {
                        self.regs[i.dst()][l] = load(&self.docs[l], path, keys);
                    }
                }
                Op::Tree => {
                    let a = &p.trees[i.index()];
                    for l in lanes(m) {
                        self.regs[i.dst()][l] = ValueRef::boolean(env.tree(a, self.docs[l].raw())?);
                    }
                }
                Op::Ext => {
                    let a = &p.trees[i.index()];
                    for l in lanes(m) {
                        self.regs[i.dst()][l] = ValueRef::boolean(env.ext(a, self.docs[l].raw())?);
                    }
                }
                Op::ExprCmp | Op::ExprCmpLit => {
                    let leaf = env.leaf_at(pc);
                    for l in lanes(m) {
                        let b = (i.op() == Op::ExprCmp).then(|| self.regs[i.index()][l]);
                        let a = self.regs[i.src1()][l];
                        let answer = env.expr_cmp(i, leaf, a, b, self.docs[l].raw())?;
                        self.regs[i.dst()][l] = ValueRef::boolean(answer);
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
                        let bytes = self.docs[l].bson();
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
                    // The lanes a vector kernel answers, then the others one by one.
                    let rest = self.vector_cmp(env, i, m);
                    for l in lanes(rest) {
                        let v = self.regs[i.src1()][l];
                        let answer = env.leaf(pc, i, v, self.docs[l].raw())?;
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

impl VectorVm<'_, '_> {
    /// Comparison `i` on the lanes of `m` whose register holds a number of the literal's kind
    /// (an integer against an integer literal, a double that is not NaN against a double), with
    /// a vector kernel (010 FR-030): what [`Env::single`] gives on those values, which are no
    /// arrays to expand. The lanes of `m` it leaves for the comparison one lane at a time.
    fn vector_cmp(&mut self, env: &Env<'_, '_, '_>, i: Insn, m: u64) -> u64 {
        let p = env.program;
        let ordered = |code: u8| match cmp_of(code) {
            CmpOp::Gt => Cmp::Gt,
            CmpOp::Gte => Cmp::Gte,
            CmpOp::Lt => Cmp::Lt,
            CmpOp::Lte => Cmp::Lte,
        };
        // The pool's literal (the immediate of an `Imm` instruction is the literal itself); an
        // instruction whose literal has another kind compares generically, as `single` does.
        let pooled = matches!(i.op(), Op::CmpEqI64 | Op::CmpEqF64 | Op::CmpI64 | Op::CmpF64);
        let literal = if pooled { &p.pool[i.index()].fast } else { &Fast::None };
        let (cmp, kernel) = match (i.op(), literal) {
            (Op::CmpEqI64Imm, _) => (Cmp::Eq, Kernel::I64(i64::from(i.imm()))),
            (Op::CmpI64Imm, _) => (ordered(i.src2()), Kernel::I64(i64::from(i.imm()))),
            (Op::CmpEqI64, Fast::I64(k)) => (Cmp::Eq, Kernel::I64(*k)),
            (Op::CmpEqF64, Fast::F64(k)) => (Cmp::Eq, Kernel::F64(*k)),
            (Op::CmpI64, Fast::I64(k)) => (ordered(i.src2()), Kernel::I64(*k)),
            (Op::CmpF64, Fast::F64(k)) => (ordered(i.src2()), Kernel::F64(*k)),
            _ => return m,
        };
        let regs = &self.regs[i.src1()];
        let (hits, mine) = match kernel {
            Kernel::I64(k) => {
                let mine = lanes(m)
                    .filter(|&l| matches!(regs[l].tag, Tag::I32 | Tag::I64))
                    .fold(0u64, |a, l| a | (1 << l));
                let col: simd::Column<i64> = std::array::from_fn(|l| regs[l].payload as i64);
                (simd::cmp_i64(cmp, &col, k), mine)
            }
            Kernel::F64(k) => {
                let mine = lanes(m)
                    .filter(|&l| {
                        regs[l].tag == Tag::F64 && !f64::from_bits(regs[l].payload).is_nan()
                    })
                    .fold(0u64, |a, l| a | (1 << l));
                let col: simd::Column<f64> =
                    std::array::from_fn(|l| f64::from_bits(regs[l].payload));
                (simd::cmp_f64(cmp, &col, k), mine)
            }
        };
        for l in lanes(mine) {
            self.regs[i.dst()][l] = ValueRef::boolean((hits >> l) & 1 == 1);
        }
        m & !mine
    }
}

/// The literal a vector comparison takes.
#[derive(Clone, Copy)]
enum Kernel {
    I64(i64),
    F64(f64),
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
