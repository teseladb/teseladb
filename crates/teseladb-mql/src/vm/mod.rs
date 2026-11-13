//! The register machine that runs compiled filters (010 FR-031).
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
//!   before its end leaves the register "through an array", and the comparison that reads it asks
//!   the tree evaluator about that one predicate, so RefDb's walk of arrays inside a path stays
//!   exactly the evaluator's. An array at the end of the path is compared whole, then element by
//!   element, inside the comparison's own loop.
//! - Comparisons are specialized by the literal's type (`CmpEqStr`, `CmpGtI64Imm`, `CmpF64`…):
//!   when the document's value has the literal's type they compare machine values; any other
//!   type goes to RefDb's generic comparison inside the same instruction (mixed numbers,
//!   collations, `null` against a missing field, `MinKey` and `MaxKey`).
//! - `$and`, `$or`, `$nor` and `$not` compile to jumps straight to `Yield`, `Reject` or the next
//!   condition. When none of the conditions of an `$and` or `$or` can fail, they are ordered by
//!   the estimated fraction of documents they let through ([`Selectivity`]): the most selective
//!   first in `$and`, the least in `$or`.
//! - `$elemMatch` on values loops over the array with `IterInit` and `IterNext`.
//! - Anything else (`$expr`, `$where`, `$regex`, `$elemMatch` on documents…) is one `Tree`
//!   instruction that asks the tree evaluator about its own subtree, in the same order.
//! - [`Program::run_batch`] runs up to 64 documents with a `u64` mask of live documents: each
//!   instruction runs on the lanes that reached it, a jump moves lanes, and the batch ends when
//!   every lane has yielded or been rejected.

mod compile;
mod run;
pub(crate) mod value;

#[cfg(test)]
mod tests;

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
    const ALL: [Op; 25] = [
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
/// `$type`, and the predicate it stands for (for values reached through an array).
#[derive(Clone, Debug)]
struct Leaf {
    literals: Vec<usize>,
    types: Vec<TypeSpec>,
    at: Address,
}

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

    /// The instructions as text, one per line (`LoadField r1 r0 0 #0`).
    pub fn listing(&self) -> Vec<String> {
        self.code.iter().map(|i| format!("{i:?}")).collect()
    }

    /// How many instructions hand a subtree to the tree evaluator.
    pub fn tree_calls(&self) -> usize {
        self.code.iter().filter(|i| i.op() == Op::Tree).count()
    }
}
