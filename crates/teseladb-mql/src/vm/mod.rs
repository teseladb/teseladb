//! The register machine that runs compiled filters (010 FR-031); the projections of `find` and
//! `$project` run on their own steps ([`project`]).
//!
//! Two phases. A filter's tree ([`crate::Expr`]) is compiled once per query shape into a
//! [`Program`]: a contiguous array of 64-bit instructions ([`Insn`]: an 8-bit code, a
//! destination, two 8-bit sources and a signed 32-bit immediate) and a pool of literals. Then the
//! program runs on each document's bytes from `pc = 0` until `Yield` (the document matches) or
//! `Reject`.
//!
//! - Registers are 32 [`value::ValueRef`]s of 16 bytes: a type tag and a scalar, or the offset and
//!   length of a string, document or array inside the document's bytes. Nothing is copied onto
//!   the heap while a program runs.
//! - `LoadField` walks a dotted path on the bytes, crossing documents. A path that meets an array
//!   before its end leaves the register "through an array", and the comparison that reads it
//!   walks the path again on the bytes with the reference server's rules for arrays inside a path
//!   (each document of the array, and the element at a numeric segment), testing every value it
//!   reaches. An array at the end of the path is compared whole, then element by element, inside
//!   the comparison's own loop.
//! - Comparisons are specialized by the literal's type (`CmpEqStr`, `CmpGtI64Imm`, `CmpF64`…):
//!   when the document's value has the literal's type they compare machine values; any other
//!   type goes to the reference server's generic comparison inside the same instruction (mixed
//!   numbers, collations, `null` against a missing field, `MinKey` and `MaxKey`).
//! - `$and`, `$or`, `$nor` and `$not` compile to jumps straight to `Yield`, `Reject` or the next
//!   condition. When none of the conditions of an `$and` or `$or` can fail, they are ordered by
//!   the estimated fraction of documents they let through ([`Selectivity`]): the most selective
//!   first in `$and`, the least in `$or`.
//! - `$elemMatch` on values loops over the array with `IterInit` and `IterNext`.
//! - `$regex`, `$mod`, the bit operators, `$in` with expressions and `$elemMatch` on documents
//!   (a program of its own run on each element) are one `Pred` instruction on the path's value.
//! - `$expr` comparisons of field paths and constants compile to `ExprCmp` instructions; any other
//!   `$expr`, `$text`, `$sampleRate` and `$jsonSchema` are one `Ext` instruction that calls the
//!   command's extensions directly. Only `$where` and the geometric operators remain `Tree`
//!   instructions that ask the tree evaluator about their own subtree.
//! - [`Program::compile_cached`] keeps the program of each query shape (the tree without the
//!   values of its equalities, comparisons and `$in`s) on its core, and a query of a known shape
//!   takes a copy whose literal pool is rebound to its own values.
//! - [`Program::run_batch`] runs up to 64 documents with a `u64` mask of live documents: each
//!   instruction runs on the lanes that reached it, a jump moves lanes, and the batch ends when
//!   every lane has yielded or been rejected.

mod cache;
mod column;
mod compile;
mod expr;
pub(crate) mod project;
mod run;
pub(crate) mod value;

#[cfg(test)]
mod project_tests;
#[cfg(test)]
mod tests;

pub use cache::shape_key;
pub use column::{Cell, load_column};
pub use compile::{DefaultSelectivity, Selectivity};

use crate::parse::CmpOp;
use crate::types::TypeSpec;

/// The registers of one run.
pub const REGISTERS: usize = 32;

/// The documents of one batch: the bits of a `u64`.
pub const BATCH: usize = 64;

/// One instruction: code, destination, two sources and a signed immediate, in 64 bits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Insn(u64);

impl Insn {
    const fn new(op: Op, dst: u8, src1: u8, src2: u8, imm: i32) -> Insn {
        Insn(
            (op as u64)
                | ((dst as u64) << 8)
                | ((src1 as u64) << 16)
                | ((src2 as u64) << 24)
                | ((imm as u32 as u64) << 32),
        )
    }

    /// The operation.
    #[inline(always)]
    fn op(self) -> Op {
        Op::ALL[usize::from(self.0 as u8)]
    }

    #[inline(always)]
    fn dst(self) -> usize {
        usize::from((self.0 >> 8) as u8)
    }

    #[inline(always)]
    fn src1(self) -> usize {
        usize::from((self.0 >> 16) as u8)
    }

    #[inline(always)]
    fn src2(self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// The immediate: a relative jump, an index into the program's tables or a literal.
    #[inline(always)]
    fn imm(self) -> i32 {
        (self.0 >> 32) as u32 as i32
    }

    /// The immediate as an index.
    #[inline(always)]
    fn index(self) -> usize {
        self.imm() as u32 as usize
    }

    /// The target of a jump at `pc`.
    #[inline(always)]
    fn target(self, pc: usize) -> usize {
        pc.wrapping_add_signed(self.imm() as isize)
    }

    /// The same instruction with immediate `imm`.
    fn with_imm(self, imm: i32) -> Insn {
        Insn((self.0 & 0xFFFF_FFFF) | ((imm as u32 as u64) << 32))
    }

    /// Its 64 bits.
    pub fn bits(self) -> u64 {
        self.0
    }
}

impl std::fmt::Debug for Insn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} r{} r{} {} #{}",
            self.op().name(),
            self.dst(),
            self.src1(),
            self.src2(),
            self.imm()
        )
    }
}

/// The operation codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Op {
    /// `dst` = the value at path `imm`.
    LoadField,
    /// `dst` = `src1` equal to the integer `imm`.
    CmpEqI64Imm,
    /// `dst` = `src1` equal to the integer literal `imm`.
    CmpEqI64,
    /// `dst` = `src1` equal to the double literal `imm`.
    CmpEqF64,
    /// `dst` = `src1` equal to the string literal `imm`.
    CmpEqStr,
    /// `dst` = `src1` equal to the boolean `imm`.
    CmpEqBoolImm,
    /// `dst` = `src1` equal to literal `imm` of any type.
    CmpEq,
    /// `dst` = `src1` ordered (`src2`) against the integer `imm`.
    CmpI64Imm,
    /// The same against the integer literal `imm`.
    CmpI64,
    /// The same against the double literal `imm`.
    CmpF64,
    /// The same against the string literal `imm`.
    CmpStr,
    /// The same against literal `imm` of any type.
    Cmp,
    /// `dst` = `src1` equal to one of the literals of the instruction's leaf.
    In,
    /// `dst` = `src1` exists.
    Exists,
    /// `dst` = `src1` of one of the types of the instruction's leaf.
    Type,
    /// `dst` = `src1` an array of `imm` elements.
    Size,
    /// `dst` = the tree evaluator's answer for subtree `imm`.
    Tree,
    /// `dst` = the leaf's predicate (any predicate on values) on `src1`.
    Pred,
    /// `dst` = the extension's answer for node `imm` (`$expr`, `$text`, `$sampleRate`,
    /// `$jsonSchema`).
    Ext,
    /// `dst` = `$expr` comparison (`src2`) of `src1` and register `imm`.
    ExprCmp,
    /// `dst` = `$expr` comparison (`src2`) of `src1` and the leaf's literal.
    ExprCmpLit,
    /// `dst` = an iterator over array `src1`; jumps by `imm` when `src1` is no array.
    IterInit,
    /// `dst` = the next element of iterator `src1`; jumps by `imm` at the end.
    IterNext,
    /// Jumps by `imm` when `src1` is a value reached through an array.
    JumpIfThrough,
    /// Jumps by `imm` when `src1` is false.
    JumpIfFalse,
    /// Jumps by `imm` when `src1` is true.
    JumpIfTrue,
    /// Jumps by `imm`.
    Jump,
    /// The document matches.
    Yield,
    /// The document does not match.
    Reject,
}

impl Op {
    const ALL: [Op; 29] = [
        Op::LoadField,
        Op::CmpEqI64Imm,
        Op::CmpEqI64,
        Op::CmpEqF64,
        Op::CmpEqStr,
        Op::CmpEqBoolImm,
        Op::CmpEq,
        Op::CmpI64Imm,
        Op::CmpI64,
        Op::CmpF64,
        Op::CmpStr,
        Op::Cmp,
        Op::In,
        Op::Exists,
        Op::Type,
        Op::Size,
        Op::Tree,
        Op::Pred,
        Op::Ext,
        Op::ExprCmp,
        Op::ExprCmpLit,
        Op::IterInit,
        Op::IterNext,
        Op::JumpIfThrough,
        Op::JumpIfFalse,
        Op::JumpIfTrue,
        Op::Jump,
        Op::Yield,
        Op::Reject,
    ];

    fn name(self) -> &'static str {
        match self {
            Op::LoadField => "LoadField",
            Op::CmpEqI64Imm => "CmpEqI64Imm",
            Op::CmpEqI64 => "CmpEqI64",
            Op::CmpEqF64 => "CmpEqF64",
            Op::CmpEqStr => "CmpEqStr",
            Op::CmpEqBoolImm => "CmpEqBoolImm",
            Op::CmpEq => "CmpEq",
            Op::CmpI64Imm => "CmpI64Imm",
            Op::CmpI64 => "CmpI64",
            Op::CmpF64 => "CmpF64",
            Op::CmpStr => "CmpStr",
            Op::Cmp => "Cmp",
            Op::In => "In",
            Op::Exists => "Exists",
            Op::Type => "Type",
            Op::Size => "Size",
            Op::Tree => "Tree",
            Op::Pred => "Pred",
            Op::Ext => "Ext",
            Op::ExprCmp => "ExprCmp",
            Op::ExprCmpLit => "ExprCmpLit",
            Op::IterInit => "IterInit",
            Op::IterNext => "IterNext",
            Op::JumpIfThrough => "JumpIfThrough",
            Op::JumpIfFalse => "JumpIfFalse",
            Op::JumpIfTrue => "JumpIfTrue",
            Op::Jump => "Jump",
            Op::Yield => "YieldDoc",
            Op::Reject => "RejectDoc",
        }
    }
}

/// The `src2` bit of a comparison that does not look into an array at the end of the path (the
/// elements of `$elemMatch`, and `$size`).
const NO_EXPAND: u8 = 0x80;

/// The code of an ordering in the `src2` byte.
fn cmp_code(op: CmpOp) -> u8 {
    match op {
        CmpOp::Gt => 0,
        CmpOp::Gte => 1,
        CmpOp::Lt => 2,
        CmpOp::Lte => 3,
    }
}

fn cmp_of(code: u8) -> CmpOp {
    match code & !NO_EXPAND {
        0 => CmpOp::Gt,
        1 => CmpOp::Gte,
        2 => CmpOp::Lt,
        _ => CmpOp::Lte,
    }
}

/// Where a subtree sits in the filter's tree, for the instructions that hand it to the tree
/// evaluator: the positions of the children of `$and`, `$or` and `$nor` down to it, then, for a
/// predicate, the positions inside the field's `$and` of predicates and `$not`s.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Address {
    exprs: Vec<usize>,
    /// For a predicate: its path through the field's predicates; `None` for a whole node.
    preds: Option<Vec<usize>>,
}

/// A literal of the pool, kept as the BSON of `{"": value}`.
#[derive(Clone, Debug)]
struct Literal {
    holder: Vec<u8>,
    fast: Fast,
}

/// A literal's machine value, for the specialized comparisons.
#[derive(Clone, Debug, PartialEq)]
enum Fast {
    I64(i64),
    F64(f64),
    Str(String),
    Bool(bool),
    None,
}

/// What a comparison instruction needs besides its operands: its literals, the types of
/// `$type`, the predicate it stands for and the path it reads (for values reached through an
/// array).
#[derive(Clone, Debug)]
struct Leaf {
    literals: Vec<usize>,
    types: Vec<TypeSpec>,
    at: Address,
    /// The path it reads (an index of [`Program::paths`]), or [`NO_PATH`] for an element of
    /// `$elemMatch`.
    path: u32,
    /// For a comparison on the elements of `$elemMatch`: its place inside the `$elemMatch`'s
    /// predicate (the positions inside `$and`s and `$not`s).
    elem: Option<Vec<usize>>,
    /// For `Pred`: the programs of the filters of the `$elemMatch`es on documents inside its
    /// predicate, in the order of [`compile::elem_match_docs`], run on each element.
    subs: Vec<Program>,
    /// For an `$expr` comparison: the comparison as an expression, which the extensions evaluate
    /// when a path crosses an array.
    expr: Option<usize>,
}

/// The path of a leaf that reads no path.
const NO_PATH: u32 = u32::MAX;

/// A compiled filter.
#[derive(Clone, Debug)]
pub struct Program {
    code: Vec<Insn>,
    /// For each instruction, its leaf (comparisons) or `u32::MAX`.
    leaf_of: Vec<u32>,
    /// The literal pool.
    pool: Vec<Literal>,
    /// The dotted paths of the loads, split.
    paths: Vec<Vec<String>>,
    /// The key of each segment of [`Program::paths`] for the tables of stored documents: its
    /// number in the collection's dictionary once [`Program::bind_fields`] gave it (003 FR-022).
    keys: Vec<Vec<teseladb_keys::stored::FieldKey>>,
    leaves: Vec<Leaf>,
    /// The subtrees of the `Tree` instructions.
    trees: Vec<Address>,
    /// Whether a batch runs its documents one after the other: some subtree asks the command's
    /// extensions, whose calls (a random draw, a script) must come in the documents' order.
    sequential: bool,
}

impl Program {
    /// Its instructions.
    pub fn code(&self) -> &[Insn] {
        &self.code
    }

    /// Gives the segments of its paths their numbers in the dictionary `numbers` of the
    /// collection it runs on (003 FR-022); the segments without one compare names.
    pub fn bind_fields(&mut self, numbers: &teseladb_keys::fields::FieldNumbers) {
        use teseladb_keys::stored::FieldKey;
        self.keys = self
            .paths
            .iter()
            .map(|p| p.iter().map(|s| FieldKey::in_dictionary(s.as_bytes(), numbers)).collect())
            .collect();
    }

    /// The segments of the paths it reads.
    pub fn path_segments(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().flatten().map(String::as_str)
    }

    /// The instructions as text, one per line (`LoadField r1 r0 0 #0`).
    pub fn listing(&self) -> Vec<String> {
        self.code.iter().map(|i| format!("{i:?}")).collect()
    }

    /// How many instructions hand a subtree to the tree evaluator, here and in the programs of
    /// its `$elemMatch`es.
    pub fn tree_calls(&self) -> usize {
        let own = self.code.iter().filter(|i| i.op() == Op::Tree).count();
        own + self.leaves.iter().flat_map(|l| &l.subs).map(Program::tree_calls).sum::<usize>()
    }
}
