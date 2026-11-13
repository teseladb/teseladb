//! From a filter's tree to a [`Program`].

use teseladb_bson::RawValue;

use super::{
    Address, Fast, Insn, Leaf, Literal, NO_EXPAND, NO_PATH, Op, Program, REGISTERS, cmp_code,
};
use crate::parse::{Expr, InItem, Pred};
use crate::value::OwnedValue;

/// The fraction of a range's documents a predicate lets through, which orders the conditions of
/// `$and` and `$or` (010 FR-029, FR-031). The planner gives one from the range's statistics.
pub trait Selectivity {
    /// The fraction, in `[0, 1]`, of documents whose `path` satisfies `pred`.
    fn fraction(&self, path: &str, pred: &Pred) -> f64;
}

/// The estimate without statistics: equality lets few documents through, a range a third, an
/// existence test most.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultSelectivity;

/// Fractions of [`DefaultSelectivity`].
const EQUAL: f64 = 0.05;
const EQUAL_NULL: f64 = 0.3;
const RANGE: f64 = 0.33;
const EXISTS: f64 = 0.9;
const TYPE: f64 = 0.3;
const UNKNOWN: f64 = 0.5;

impl Selectivity for DefaultSelectivity {
    fn fraction(&self, _path: &str, pred: &Pred) -> f64 {
        default_fraction(pred)
    }
}

/// The fraction of [`DefaultSelectivity`].
fn default_fraction(pred: &Pred) -> f64 {
    match pred {
        Pred::Eq(t) if t.get() == RawValue::Null => EQUAL_NULL,
        Pred::Eq(_) | Pred::ExprEq(_) => EQUAL,
        Pred::Cmp(..) | Pred::ExprCmp(..) => RANGE,
        Pred::In(items) => (EQUAL * items.len() as f64).min(1.0),
        Pred::Exists => EXISTS,
        Pred::Type(_) => TYPE,
        Pred::And(preds) => preds.iter().map(default_fraction).product(),
        Pred::Not(p) => 1.0 - default_fraction(p),
        _ => UNKNOWN,
    }
}

/// The fraction of documents `e` lets through.
fn fraction(e: &Expr, sel: &dyn Selectivity) -> f64 {
    match e {
        Expr::Field { path, pred } => sel.fraction(path, pred).clamp(0.0, 1.0),
        Expr::And(items) => items.iter().map(|x| fraction(x, sel)).product(),
        Expr::Or(items) => 1.0 - items.iter().map(|x| 1.0 - fraction(x, sel)).product::<f64>(),
        Expr::Nor(items) => items.iter().map(|x| 1.0 - fraction(x, sel)).product(),
        Expr::True => 1.0,
        Expr::False => 0.0,
        _ => UNKNOWN,
    }
}

/// Whether evaluating `e` can neither fail nor ask anything outside the document (so that the
/// conditions around it may run in any order).
fn infallible(e: &Expr) -> bool {
    match e {
        Expr::And(items) | Expr::Or(items) | Expr::Nor(items) => items.iter().all(infallible),
        Expr::Field { pred, .. } => infallible_pred(pred),
        Expr::True | Expr::False => true,
        Expr::Expr(_)
        | Expr::Where(_)
        | Expr::Text(_)
        | Expr::JsonSchema(_)
        | Expr::SampleRate(_) => false,
    }
}

fn infallible_pred(p: &Pred) -> bool {
    match p {
        Pred::Eq(_)
        | Pred::Cmp(..)
        | Pred::ExprEq(_)
        | Pred::ExprCmp(..)
        | Pred::Exists
        | Pred::Type(_)
        | Pred::Mod { .. }
        | Pred::Size(_)
        | Pred::Bits(..) => true,
        Pred::In(items) => items.iter().all(|i| matches!(i, InItem::Value(_))),
        Pred::And(preds) => preds.iter().all(infallible_pred),
        Pred::Not(p) | Pred::ElemMatchValue(p) => infallible_pred(p),
        Pred::ElemMatchDoc(e) => infallible(e),
        Pred::Regex(_) | Pred::Geo(_) => false,
    }
}

/// A place in the code that jumps go to, bound once the code there is emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Label(usize);

pub(super) struct Compiler<'s> {
    sel: &'s dyn Selectivity,
    code: Vec<Insn>,
    leaf_of: Vec<u32>,
    pool: Vec<Literal>,
    paths: Vec<Vec<String>>,
    path_texts: Vec<String>,
    leaves: Vec<Leaf>,
    trees: Vec<Address>,
    /// Where each label is bound.
    labels: Vec<Option<usize>>,
    /// The jumps to patch: instruction and label.
    jumps: Vec<(usize, Label)>,
    /// The next free register.
    free: usize,
    /// Whether a subtree asks the command's extensions (`$expr`, `$where`, `$text`,
    /// `$sampleRate`, geometry), which a batch must call document after document.
    sequential: bool,
}

/// Registers one comparison uses: the value, the answer, an iterator and an element.
pub(super) const LEAF_REGISTERS: usize = 4;

impl Program {
    /// Compiles `root`, ordering the conditions of its `$and`s and `$or`s by `sel`.
    pub fn compile(root: &Expr, sel: &dyn Selectivity) -> Program {
        let mut c = Compiler {
            sel,
            code: Vec::new(),
            leaf_of: Vec::new(),
            pool: Vec::new(),
            paths: Vec::new(),
            path_texts: Vec::new(),
            leaves: Vec::new(),
            trees: Vec::new(),
            labels: Vec::new(),
            jumps: Vec::new(),
            free: 0,
            sequential: false,
        };
        let (yes, no) = (c.label(), c.label());
        c.expr(root, &mut Vec::new(), yes, no, true);
        c.bind(yes);
        c.emit(Op::Yield, 0, 0, 0, 0);
        c.bind(no);
        c.emit(Op::Reject, 0, 0, 0, 0);
        c.patch();
        Program {
            code: c.code,
            leaf_of: c.leaf_of,
            pool: c.pool,
            paths: c.paths,
            leaves: c.leaves,
            trees: c.trees,
            sequential: c.sequential,
        }
    }
}

/// A count of the program (a table index) as the immediate.
fn imm(n: usize) -> i32 {
    i32::try_from(n).expect("a program has fewer than 2^31 parts")
}

/// A register number as its byte.
fn reg(r: usize) -> u8 {
    u8::try_from(r).expect("32 registers")
}

impl Compiler<'_> {
    pub(super) fn emit(&mut self, op: Op, dst: usize, src1: usize, src2: u8, at: i32) {
        self.code.push(Insn::new(op, reg(dst), reg(src1), src2, at));
        self.leaf_of.push(u32::MAX);
    }

    pub(super) fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    pub(super) fn bind(&mut self, l: Label) {
        self.labels[l.0] = Some(self.code.len());
    }

    /// A jump (or an instruction that may jump) to `l`.
    pub(super) fn jump(&mut self, op: Op, dst: usize, src1: usize, l: Label) {
        self.jumps.push((self.code.len(), l));
        self.emit(op, dst, src1, 0, 0);
    }

    /// Writes the relative offsets of the jumps.
    fn patch(&mut self) {
        for &(at, l) in &self.jumps {
            let target = self.labels[l.0].expect("every label is bound");
            let i = self.code[at];
            let offset = i32::try_from(target as i64 - at as i64).expect("a short program");
            self.code[at] = Insn::new(i.op(), reg(i.dst()), reg(i.src1()), i.src2(), offset);
        }
    }

    /// Goes to `yes` when `cond` holds and to `no` otherwise, falling through to the next
    /// instruction for the answer `fall`.
    pub(super) fn branch(&mut self, cond: usize, yes: Label, no: Label, fall: bool) {
        if fall {
            self.jump(Op::JumpIfFalse, 0, cond, no);
        } else {
            self.jump(Op::JumpIfTrue, 0, cond, yes);
        }
    }

    /// Goes to `yes` or `no` by constant answer `b`.
    pub(super) fn constant(&mut self, b: bool, yes: Label, no: Label, fall: bool) {
        if b != fall {
            self.jump(Op::Jump, 0, 0, if b { yes } else { no });
        }
    }

    fn tree(&mut self, at: Address, yes: Label, no: Label, fall: bool) {
        self.trees.push(at);
        let r = self.free;
        let n = imm(self.trees.len() - 1);
        self.emit(Op::Tree, r, 0, 0, n);
        self.branch(r, yes, no, fall);
    }

    /// The extensions' answer for the node at `at`.
    fn ext(&mut self, at: Address, yes: Label, no: Label, fall: bool) {
        self.trees.push(at);
        let r = self.free;
        let n = imm(self.trees.len() - 1);
        self.emit(Op::Ext, r, 0, 0, n);
        self.branch(r, yes, no, fall);
    }

    /// Adds `leaf` and makes it the last instruction's.
    pub(super) fn attach(&mut self, leaf: Leaf) {
        self.leaves.push(leaf);
        let l = u32::try_from(self.leaves.len() - 1).expect("few leaves");
        if let Some(last) = self.leaf_of.last_mut() {
            *last = l;
        }
    }

    /// The first free register.
    pub(super) fn free(&self) -> usize {
        self.free
    }

    /// Code for `e`: to `yes` when it matches, to `no` otherwise; `fall` is the answer that
    /// falls through.
    fn expr(&mut self, e: &Expr, at: &mut Vec<usize>, yes: Label, no: Label, fall: bool) {
        match e {
            Expr::And(items) => self.junction(items, at, yes, no, fall, true),
            Expr::Or(items) => self.junction(items, at, yes, no, fall, false),
            Expr::Nor(items) => self.junction(items, at, no, yes, !fall, false),
            Expr::Field { path, pred } => self.pred(path, pred, at, &mut Vec::new(), yes, no, fall),
            Expr::True => self.constant(true, yes, no, fall),
            Expr::False => self.constant(false, yes, no, fall),
            Expr::JsonSchema(_) => {
                self.ext(Address { exprs: at.clone(), preds: None }, yes, no, fall);
            }
            Expr::Expr(e) => {
                let enough = self.free + LEAF_REGISTERS <= REGISTERS;
                if !(enough && super::expr::compile_expr(self, e.get(), at, yes, no, fall)) {
                    self.sequential = true;
                    self.ext(Address { exprs: at.clone(), preds: None }, yes, no, fall);
                }
            }
            Expr::Text(_) | Expr::SampleRate(_) => {
                self.sequential = true;
                self.ext(Address { exprs: at.clone(), preds: None }, yes, no, fall);
            }
            Expr::Where(_) => {
                self.sequential = true;
                self.tree(Address { exprs: at.clone(), preds: None }, yes, no, fall);
            }
        }
    }

    /// `$and` (`all`) or `$or` of `items`, each condition jumping out as soon as it decides.
    fn junction(
        &mut self,
        items: &[Expr],
        at: &mut Vec<usize>,
        yes: Label,
        no: Label,
        fall: bool,
        all: bool,
    ) {
        if items.is_empty() {
            self.constant(all, yes, no, fall);
            return;
        }
        let mut order: Vec<usize> = (0..items.len()).collect();
        if items.iter().all(infallible) {
            let f: Vec<f64> = items.iter().map(|e| fraction(e, self.sel)).collect();
            // `$and` first runs what lets the fewest documents through, `$or` what lets the most.
            order.sort_by(|&a, &b| {
                let o = f[a].total_cmp(&f[b]);
                if all { o } else { o.reverse() }
            });
        }
        let last = order.len() - 1;
        for (k, &i) in order.iter().enumerate() {
            at.push(i);
            if k == last {
                self.expr(&items[i], at, yes, no, fall);
            } else {
                let next = self.label();
                if all {
                    self.expr(&items[i], at, next, no, true);
                } else {
                    self.expr(&items[i], at, yes, next, false);
                }
                self.bind(next);
            }
            at.pop();
        }
    }

    /// Predicate `pred` on `path`; `preds` is its place among the field's predicates.
    fn pred(
        &mut self,
        path: &str,
        pred: &Pred,
        at: &mut Vec<usize>,
        preds: &mut Vec<usize>,
        yes: Label,
        no: Label,
        fall: bool,
    ) {
        let enough = self.free + LEAF_REGISTERS <= REGISTERS;
        match pred {
            Pred::And(items) => {
                if items.is_empty() {
                    self.constant(true, yes, no, fall);
                    return;
                }
                for (i, p) in items.iter().enumerate() {
                    preds.push(i);
                    if i + 1 == items.len() {
                        self.pred(path, p, at, preds, yes, no, fall);
                    } else {
                        let next = self.label();
                        self.pred(path, p, at, preds, next, no, true);
                        self.bind(next);
                    }
                    preds.pop();
                }
            }
            Pred::Not(p) => {
                preds.push(0);
                self.pred(path, p, at, preds, no, yes, !fall);
                preds.pop();
            }
            p if enough && leaf_op(p).is_some() => {
                let addr = Address { exprs: at.clone(), preds: Some(preds.clone()) };
                let (v, pi) = self.load(path);
                let r = self.free + 1;
                self.compare(p, addr, r, v, 0, pi, None);
                self.branch(r, yes, no, fall);
            }
            Pred::ElemMatchValue(inner) if enough && on_element(inner) => {
                let addr = Address { exprs: at.clone(), preds: Some(preds.clone()) };
                self.elem_match(path, inner, addr, yes, no, fall);
            }
            p if enough && !geometric(p) => {
                self.sequential |= asks_extensions(p);
                let addr = Address { exprs: at.clone(), preds: Some(preds.clone()) };
                let mut docs = Vec::new();
                elem_match_docs(p, &mut docs);
                let subs = docs.into_iter().map(|e| Program::compile(e, self.sel)).collect();
                let (v, pi) = self.load(path);
                let r = self.free + 1;
                self.emit(Op::Pred, r, v, 0, 0);
                self.attach(Leaf { subs, ..Leaf::new(addr, pi) });
                self.branch(r, yes, no, fall);
            }
            p => {
                self.sequential |= asks_extensions(p);
                self.tree(Address { exprs: at.clone(), preds: Some(preds.clone()) }, yes, no, fall);
            }
        }
    }

    /// Loads `path` into the first free register: the register and the path's index.
    pub(super) fn load(&mut self, path: &str) -> (usize, u32) {
        let p = match self.path_texts.iter().position(|t| t == path) {
            Some(p) => p,
            None => {
                self.path_texts.push(path.to_owned());
                self.paths.push(path.split('.').map(str::to_owned).collect());
                self.path_texts.len() - 1
            }
        };
        let r = self.free;
        self.emit(Op::LoadField, r, 0, 0, imm(p));
        (r, u32::try_from(p).expect("few paths"))
    }

    /// Loads `path` into register `r`.
    pub(super) fn load_into(&mut self, path: &str, r: usize) {
        let saved = self.free;
        self.free = r;
        self.load(path);
        self.free = saved;
    }

    /// Adds `v` to the pool.
    pub(super) fn literal(&mut self, v: &OwnedValue) -> (usize, Fast) {
        let l = literal(v);
        let fast = l.fast.clone();
        self.pool.push(l);
        (self.pool.len() - 1, fast)
    }

    /// The comparison of `pred` on register `v` into `dst`; `flags` may be [`NO_EXPAND`]; `path`
    /// is the path `v` was loaded from and `elem` the comparison's place inside `$elemMatch`.
    #[allow(clippy::too_many_arguments)]
    fn compare(
        &mut self,
        pred: &Pred,
        at: Address,
        dst: usize,
        v: usize,
        flags: u8,
        path: u32,
        elem: Option<Vec<usize>>,
    ) {
        let small = |k: i64| i32::try_from(k).ok();
        let mut leaf = Leaf { elem, ..Leaf::new(at, path) };
        let (op, src2, value) = match pred {
            Pred::Eq(t) => {
                let (i, fast) = self.literal(t);
                leaf.literals.push(i);
                match fast {
                    Fast::I64(k) if small(k).is_some() => {
                        (Op::CmpEqI64Imm, 0, small(k).unwrap_or(0))
                    }
                    Fast::I64(_) => (Op::CmpEqI64, 0, imm(i)),
                    Fast::F64(_) => (Op::CmpEqF64, 0, imm(i)),
                    Fast::Str(_) => (Op::CmpEqStr, 0, imm(i)),
                    Fast::Bool(b) => (Op::CmpEqBoolImm, 0, i32::from(b)),
                    Fast::None => (Op::CmpEq, 0, imm(i)),
                }
            }
            Pred::Cmp(o, t) => {
                let (i, fast) = self.literal(t);
                leaf.literals.push(i);
                let code = cmp_code(*o);
                match fast {
                    Fast::I64(k) if small(k).is_some() => {
                        (Op::CmpI64Imm, code, small(k).unwrap_or(0))
                    }
                    Fast::I64(_) => (Op::CmpI64, code, imm(i)),
                    Fast::F64(_) => (Op::CmpF64, code, imm(i)),
                    Fast::Str(_) => (Op::CmpStr, code, imm(i)),
                    Fast::Bool(_) | Fast::None => (Op::Cmp, code, imm(i)),
                }
            }
            Pred::In(items) => {
                for item in items {
                    if let InItem::Value(t) = item {
                        let (i, _) = self.literal(t);
                        leaf.literals.push(i);
                    }
                }
                (Op::In, 0, 0)
            }
            Pred::Type(types) => {
                leaf.types = types.clone();
                (Op::Type, 0, 0)
            }
            Pred::Size(n) => (Op::Size, NO_EXPAND, i32::try_from(*n).unwrap_or(-1)),
            _ => (Op::Exists, 0, 0),
        };
        self.emit(op, dst, v, src2 | flags, value);
        self.attach(leaf);
    }

    /// `$elemMatch` with operators on `path`: some element of the array satisfies `inner`.
    fn elem_match(
        &mut self,
        path: &str,
        inner: &Pred,
        at: Address,
        yes: Label,
        no: Label,
        fall: bool,
    ) {
        let (v, pi) = self.load(path);
        let (it, elem, answer) = (v + 1, v + 2, v + 3);
        let (through, next, done) = (self.label(), self.label(), self.label());
        self.jump(Op::JumpIfThrough, 0, v, through);
        self.jump(Op::IterInit, it, v, no);
        self.bind(next);
        self.jump(Op::IterNext, elem, it, no);
        self.free += 4;
        let found = self.label();
        self.on_element(inner, &at, elem, answer, found, next, &mut Vec::new());
        self.free -= 4;
        self.bind(found);
        if fall {
            self.jump(Op::Jump, 0, 0, done);
        } else {
            self.jump(Op::Jump, 0, 0, yes);
        }
        // A path through an array: the whole predicate on each value the walk reaches.
        self.bind(through);
        self.emit(Op::Pred, answer, v, 0, 0);
        let mut docs = Vec::new();
        elem_match_docs(inner, &mut docs);
        let subs = docs.into_iter().map(|e| Program::compile(e, self.sel)).collect();
        self.attach(Leaf { subs, ..Leaf::new(at, pi) });
        self.branch(answer, yes, no, fall);
        self.bind(done);
    }

    /// Code that goes to `yes` when element `elem` satisfies `p` (without looking into an
    /// array it holds) and to `no` otherwise, falling through on `yes`; `place` is `p`'s place
    /// inside the `$elemMatch`'s predicate.
    #[allow(clippy::too_many_arguments)]
    fn on_element(
        &mut self,
        p: &Pred,
        at: &Address,
        elem: usize,
        answer: usize,
        yes: Label,
        no: Label,
        place: &mut Vec<usize>,
    ) {
        match p {
            Pred::And(items) => {
                for (i, q) in items.iter().enumerate() {
                    let next = self.label();
                    place.push(i);
                    self.on_element(q, at, elem, answer, next, no, place);
                    place.pop();
                    self.bind(next);
                }
                self.constant(true, yes, no, true);
            }
            Pred::Not(q) => {
                let (inner_yes, inner_no) = (self.label(), self.label());
                place.push(0);
                self.on_element(q, at, elem, answer, inner_yes, inner_no, place);
                place.pop();
                self.bind(inner_yes);
                self.jump(Op::Jump, 0, 0, no);
                self.bind(inner_no);
                self.constant(true, yes, no, true);
            }
            q => {
                let place = Some(place.clone());
                self.compare(q, at.clone(), answer, elem, NO_EXPAND, NO_PATH, place);
                self.branch(answer, yes, no, true);
            }
        }
    }
}

/// Whether evaluating `p` asks the command's extensions.
fn asks_extensions(p: &Pred) -> bool {
    match p {
        Pred::Geo(_) => true,
        Pred::And(items) => items.iter().any(asks_extensions),
        Pred::Not(q) | Pred::ElemMatchValue(q) => asks_extensions(q),
        Pred::ElemMatchDoc(e) => !infallible(e),
        _ => false,
    }
}

/// The filters of the `$elemMatch`es on documents inside `p`, outermost first, in the order of
/// `p`'s tree (not those inside them, which their own programs hold).
pub(super) fn elem_match_docs<'e>(p: &'e Pred, out: &mut Vec<&'e Expr>) {
    match p {
        Pred::ElemMatchDoc(e) => out.push(e),
        Pred::And(items) => items.iter().for_each(|q| elem_match_docs(q, out)),
        Pred::Not(q) | Pred::ElemMatchValue(q) => elem_match_docs(q, out),
        _ => {}
    }
}

/// Whether `p` asks the geometry engine, which the tree evaluator answers.
fn geometric(p: &Pred) -> bool {
    match p {
        Pred::Geo(_) => true,
        Pred::And(items) => items.iter().any(geometric),
        Pred::Not(q) | Pred::ElemMatchValue(q) => geometric(q),
        _ => false,
    }
}

/// Whether `p` is one comparison instruction.
fn leaf_op(p: &Pred) -> Option<()> {
    match p {
        Pred::Eq(_) | Pred::Cmp(..) | Pred::Exists | Pred::Type(_) | Pred::Size(_) => Some(()),
        Pred::In(items) => items.iter().all(|i| matches!(i, InItem::Value(_))).then_some(()),
        _ => None,
    }
}

/// Whether `p` compiles on one element of `$elemMatch`.
fn on_element(p: &Pred) -> bool {
    match p {
        Pred::And(items) => items.iter().all(on_element),
        Pred::Not(q) => on_element(q),
        q => leaf_op(q).is_some(),
    }
}

impl Leaf {
    /// A leaf for the predicate at `at` on path `path`, without literals.
    pub(super) fn new(at: Address, path: u32) -> Leaf {
        Leaf {
            literals: Vec::new(),
            types: Vec::new(),
            at,
            path,
            elem: None,
            subs: Vec::new(),
            expr: None,
        }
    }
}

/// The literal of `v` with its machine value.
pub(super) fn literal(v: &OwnedValue) -> Literal {
    let fast = match v.get() {
        RawValue::Int32(x) => Fast::I64(i64::from(x)),
        RawValue::Int64(x) => Fast::I64(x),
        RawValue::Double(x) if !x.is_nan() => Fast::F64(x),
        RawValue::String(s) => Fast::Str(s.to_owned()),
        RawValue::Boolean(b) => Fast::Bool(b),
        _ => Fast::None,
    };
    Literal { holder: v.holder_bytes().to_vec(), fast }
}
