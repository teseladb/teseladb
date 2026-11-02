//! How deep the calls of a program go before `too much recursion`, as a `refrouter` 9.0.4 counts
//! them (016 A016-3-18, measured on 02-11).
//!
//! Its engine counts two things. The frames of functions: the program's function and every
//! function it calls, the builtins it writes in JavaScript (`map`, `forEach`, `replace`…)
//! included, at most [`crate::MAX_FRAMES`]. And the native stack of its own code: each time that
//! code calls back into JavaScript (a getter, a conversion, `f.call`, a comparator of `sort`) it
//! takes a slice of a budget that ends after about 350 of them. A call back costs [`REENTRY`] and
//! every native function between two JavaScript frames [`NATIVE_FRAME`] more, which gives the
//! measured counts: 349 nested getters or conversions, 175 nested `f.call` or `f.apply`, 117
//! nested `Function.prototype.call.call` and 88 with one more `call`.

use crate::interp::{Interp, R};
use crate::value::{Func, Kind, Val};

/// The native stack of the engine, in the units of [`REENTRY`] and [`NATIVE_FRAME`], less what
/// its call of the program's function takes.
pub const NATIVE_BUDGET: usize = 996_000;
/// What a call back into JavaScript from the engine's own code takes of [`NATIVE_BUDGET`].
pub const REENTRY: usize = 2_862;
/// What a direct `eval` takes of [`NATIVE_BUDGET`]: 343 nested ones.
pub const EVAL: usize = 2_910;
/// What the handler of a proxy takes of [`NATIVE_BUDGET`] when it calls a trap, beyond
/// [`REENTRY`]: 174 nested `get` traps, 117 nested `ownKeys` traps under `Object.keys`.
pub const PROXY_TRAP: usize = 2_850;
/// The frames of the helpers of an iterator helper (`map`, `filter`, `flatMap`) beyond the
/// function's own and that of `next`: 10 200 nested calls.
pub const ITERATOR_HELPER_FRAMES: usize = 2;
/// The frames of the helpers of `replace` with a regular expression and a function, beyond the
/// function's own and that of `replace`: 10 200 nested calls.
pub const REGEXP_REPLACE_FRAMES: usize = 3;
/// What a native function takes of [`NATIVE_BUDGET`] while it runs.
pub const NATIVE_FRAME: usize = 2_830;

// What the constructs of a parse take of the engine's stack, in the units of [`NATIVE_BUDGET`]
// (measured on a `refrouter` 9.0.4 on 02-11 with nesting of each kind, in a program's function and
// in an `eval`): 330 and 341 unary operators, 66 and 69 parentheses or template substitutions, 168
// and 173 blocks, 83 and 86 argument lists, 33 and 34 object literals in parentheses, 27 and 28
// function expressions in parentheses, 331 and 342 conditional operators, 335 and 345 `if`.

/// A statement other than those below.
pub const STATEMENT: usize = 2_955;
/// An `if`, a loop, a `switch`, a `with` or a `try`.
pub const COMPOUND_STATEMENT: usize = 2_875;
/// A block statement.
pub const BLOCK: usize = 5_750;
/// The operand of a unary operator, the value of an assignment.
pub const UNARY: usize = 2_915;
/// A branch of a conditional operator.
pub const CONDITIONAL: usize = 2_905;
/// A parenthesized expression, an array literal, a template's substitutions.
pub const PAREN: usize = 14_400;
/// An argument list.
pub const ARGUMENTS: usize = 11_520;
/// An object literal.
pub const OBJECT: usize = 14_430;
/// A function's body.
pub const FUNCTION: usize = 17_645;
/// What a program's parse may take, measured inside its function.
pub const TOP_PARSE: usize = 966_750;
/// What the parse of an `eval` has beyond the call's room: the engine checks its depth against
/// a limit of its own.
pub const EVAL_PARSE: usize = 4_250;
/// What a value nested in another takes of [`NATIVE_BUDGET`] while `JSON.stringify` writes it
/// (346 nested arrays), and a group nested in another while a regular expression compiles on its
/// first match (346 nested groups, then `regexp too big`).
pub const NESTING: usize = 2_865;
/// What a conversion that calls a native method of the engine takes of [`NATIVE_BUDGET`] beyond
/// that native's frame (172 nested arrays converted to a string).
pub const NATIVE_CONVERSION: usize = 80;
/// The arrow functions nested in one another after which the engine's stack runs out: in a
/// program and in an `eval` (measured as a trap of the `refrouter`).
pub const TOP_ARROWS: usize = 313;
/// See [`TOP_ARROWS`].
pub const EVAL_ARROWS: usize = 315;

/// Whether the engine writes the builtin `name` in JavaScript: its frame counts like that of a
/// program's function, and the functions it calls are not calls back.
pub fn self_hosted(name: &str) -> bool {
    matches!(
        name,
        "map"
            | "forEach"
            | "filter"
            | "find"
            | "findIndex"
            | "findLast"
            | "findLastIndex"
            | "some"
            | "every"
            | "reduce"
            | "reduceRight"
            | "flatMap"
            | "from"
            | "groupBy"
            | "replace"
            | "replaceAll"
            | "split"
            | "isNaN"
            | "next"
            | "return"
            | "throw"
            | "toArray"
    )
}

/// The frames beyond its own that a self-hosted builtin takes while it calls back: the helper
/// of `flatMap`, measured as 17 000 nested calls.
fn helper_frames(name: &str) -> usize {
    match name {
        "flatMap" => 1,
        _ => 0,
    }
}

impl Interp<'_> {
    /// What a call of `func` takes: frames, native stack, and whether the code it runs is the
    /// engine's own (a native that is not self-hosted, or the dispatch of a bound function).
    pub(crate) fn call_cost(&self, func: &Func) -> (usize, usize, bool) {
        let back = if self.in_native { REENTRY } else { 0 };
        match func {
            Func::Native(_, name) if self_hosted(name) => (1 + helper_frames(name), back, false),
            Func::Native(..) | Func::Bound(..) => (1, NATIVE_FRAME, true),
            Func::Closure { .. } => (1, back, false),
        }
    }

    /// Whether a call that takes `frames` and `units` goes past a limit.
    pub(crate) fn too_deep(&mut self, frames: usize, units: usize) -> bool {
        self.depth + frames > self.limits.depth
            || self.units + units > NATIVE_BUDGET
            || self.stack_exhausted()
    }

    /// Runs `run` as a call that takes `frames` and `units`, or ends it as `too much recursion`.
    pub(crate) fn nested<T>(
        &mut self,
        frames: usize,
        units: usize,
        native: bool,
        run: impl FnOnce(&mut Self) -> R<T>,
    ) -> R<T> {
        if self.too_deep(frames, units) {
            return Err(self.error("InternalError", "too much recursion"));
        }
        self.depth += frames;
        self.units += units;
        let outer = std::mem::replace(&mut self.in_native, native);
        let r = run(self);
        self.in_native = outer;
        self.units -= units;
        self.depth -= frames;
        r
    }

    /// The room the parse of code in an `eval` or `Function` of this call has.
    pub(crate) fn parse_room(&mut self) -> crate::parser::Room {
        let used = self.stack_base.saturating_sub(crate::interp::stack_position());
        crate::parser::Room {
            units: (NATIVE_BUDGET + EVAL_PARSE).saturating_sub(self.units),
            arrows: EVAL_ARROWS,
            stack: self.limits.stack.saturating_sub(used) / 2,
        }
    }

    /// Calls `f` back from the engine's own code (a getter, a setter, a conversion, the
    /// arguments of a spread call): [`REENTRY`], and `extra` more for the native frames between.
    /// A native of the engine's own code takes its frame, not a call back.
    pub(crate) fn call_back(&mut self, extra: usize, f: &Val, this: &Val, args: &[Val]) -> R {
        let own = matches!(f, Val::Obj(o) if matches!(&o.borrow().kind,
            Kind::Func(Func::Native(_, name)) if !self_hosted(name)));
        let units = if own { NATIVE_CONVERSION + extra } else { REENTRY + extra };
        self.nested(0, units, false, |it| it.call(f, this, args))
    }
}
