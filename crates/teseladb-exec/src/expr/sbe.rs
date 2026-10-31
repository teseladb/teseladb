//! The errors of expressions that RefDb evaluates with its slot-based engine (SBE): the keys
//! and accumulators of `$group`, `$bucket` and `$bucketAuto`. A `refrouter` 9.0.4 gives other codes
//! and messages there than in `$project` for some operators (measured 31-10-2026); an error of
//! one of them is rewritten where the operator raises it.

use teseladb_mql::{ErrorCode, MqlError};

use super::Op;
use super::arith::Arith;
use super::dates::DateExpr;
use super::string::Str;

/// A location code.
macro_rules! loc {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

/// The error `op` gives under SBE for its error `e`.
pub(super) fn remap(op: &Op, e: MqlError) -> MqlError {
    let code = e.code.code;
    let (sbe, msg): (ErrorCode, &str) = match (op, code) {
        (Op::Arith(Arith::Add, _), 14) => {
            (loc!(7157723), "only numbers and dates are allowed in an $add expression")
        }
        (Op::Arith(Arith::Subtract, _), 14) => (
            loc!(7157720),
            "Only numbers and dates are allowed in an $subtract expression. To subtract a number from a date, the date must be the first argument.",
        ),
        (Op::Arith(Arith::Multiply, _), 14) => {
            (loc!(7157721), "only numbers are allowed in an $multiply expression")
        }
        (Op::Arith(Arith::Divide, _), 2) if e.message == "can't $divide by zero" => {
            (loc!(4848401), "can't $divide by zero")
        }
        (Op::Arith(Arith::Mod, _), 16610) => (loc!(4848403), "can't $mod by zero"),
        (Op::Arith(Arith::Sqrt, _), 28765) => (loc!(7157709), "$sqrt only supports numeric types"),
        (Op::Str(Str::Concat, _), 16702) => (loc!(7158201), "$concat supports only strings"),
        (Op::Str(Str::SubstrBytes, _), 16007) => {
            (loc!(5155608), "$substrBytes: string expression could not be resolved to a string")
        }
        (Op::Str(Str::ToUpper, _), 16007) => {
            (loc!(7158200), "$toUpper input type is not supported")
        }
        (Op::Date(d), 16006) if matches!(**d, DateExpr::ToString { .. }) => {
            (loc!(4997901), "$dateToString parameter 'date' must be coercible to date")
        }
        _ => return e,
    };
    MqlError::new(sbe, msg)
}
