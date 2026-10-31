//! Aggregation expressions (010 FR-005, T007): an expression is parsed once ([`Expr::parse`])
//! and evaluated against each document ([`Expr::eval`]). A value is a literal, a field path
//! (`"$a.b"`), a variable (`"$$ROOT"`, `"$$name.b"`), a document or array of expressions, or an
//! operator `{"$op": args}`. Evaluation gives a value or nothing: a missing field is not `null`,
//! and a document drops a field whose expression gives nothing, as RefDb's do.
//!
//! The operators are in the submodules by family; what each one returns, with its types, was
//! measured on a `refrouter` 9.0.4 (`crates/teseladb-exec/tests/expr_*.rs`).

pub(crate) mod arith;
mod array;
mod calendar;
mod convert;
mod convert_bin;
mod date_format;
mod date_free;
mod date_parse;
mod date_pattern;
mod date_relative;
mod date_rules;
mod date_words;
mod dates;
pub(crate) mod dates_arith;
mod ejson;
pub(crate) mod forms;
mod json;
mod logic;
mod misc;
mod object;
mod path;
mod sbe;
mod sets;
pub(crate) mod stats;
mod string;
mod text;
pub(crate) mod values;
mod vars;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

pub use logic::{order, truthy};
pub use path::field_path;

/// The result of an expression: a value, or nothing (a missing field).
pub type Eval = Option<Value>;

/// The error codes of expressions, with RefDb's names.
pub mod codes {
    use teseladb_mql::ErrorCode;

    /// A location code: its number and `LocationNNNNN`.
    macro_rules! location {
        ($name:ident, $n:literal) => {
            #[doc = concat!("RefDb's location code ", stringify!($n), ".")]
            pub const $name: ErrorCode = ErrorCode { code: $n, name: concat!("Location", $n) };
        };
    }

    /// An operand of the wrong type.
    pub const TYPE_MISMATCH: ErrorCode = ErrorCode { code: 14, name: "TypeMismatch" };
    /// A malformed argument.
    pub const FAILED_TO_PARSE: ErrorCode = ErrorCode { code: 9, name: "FailedToParse" };
    /// An invalid value.
    pub const BAD_VALUE: ErrorCode = ErrorCode { code: 2, name: "BadValue" };
    /// `$divide` or `$mod` by zero.
    pub const BAD_DIVISION: ErrorCode = ErrorCode { code: 2, name: "BadValue" };
    /// An operator that does not exist (`$project` words it as its own 31325).
    pub const UNKNOWN_EXPRESSION: ErrorCode =
        ErrorCode { code: 168, name: "InvalidPipelineOperator" };
    location!(RANGE_ARGUMENTS, 28667);
    location!(WRONG_ARGUMENT_COUNT, 16020);
    location!(ADD_TWO_DATES, 16612);
    location!(MOD_TYPES, 16611);
    location!(MOD_BY_ZERO, 16610);
    location!(UNARY_NOT_NUMBER, 28765);
    location!(OBJECT_OPERATOR, 15983);
    location!(INVALID_FIELD_PATH, 16872);
    location!(UNDEFINED_VARIABLE, 17276);
    location!(COND_MISSING_IF, 17080);
    location!(COND_MISSING_THEN, 17081);
    location!(COND_MISSING_ELSE, 17082);
    location!(COND_UNKNOWN, 17083);
    location!(SWITCH_NOT_OBJECT, 40060);
    location!(SWITCH_BRANCHES_ARRAY, 40061);
    location!(SWITCH_BRANCH_OBJECT, 40062);
    location!(SWITCH_CASE_MISSING, 40064);
    location!(SWITCH_THEN_MISSING, 40065);
    location!(SWITCH_BRANCH_UNKNOWN, 40063);
    location!(SWITCH_NO_MATCH, 40069);
    location!(SWITCH_UNKNOWN, 40067);
    location!(SWITCH_NO_BRANCHES, 40068);
    location!(IF_NULL_ARGUMENTS, 1257300);
    location!(SQRT_NEGATIVE, 28714);
    location!(LOG_NOT_POSITIVE, 28758);
    location!(LOG_BASE, 28759);
    location!(LOG_ARGUMENT_NUMBER, 28756);
    location!(LOG_BASE_NUMBER, 28757);
    location!(CANNOT_CONVERT_TO_LONG, 16004);
    location!(LN_NOT_POSITIVE, 28766);
    location!(LOG10_NOT_POSITIVE, 28761);
    location!(POW_ZERO_NEGATIVE, 28764);
    location!(POW_BASE, 28762);
    location!(POW_EXPONENT, 28763);
    location!(ABS_LONG_MIN, 28680);
    location!(ROUND_TYPES, 51081);
    location!(ROUND_PLACES_INTEGRAL, 51082);
    location!(ROUND_PLACES, 51083);
    location!(BAD_FIELD_PATH, 15998);
    location!(CANNOT_CONVERT_TO_STRING, 16007);
    location!(CONCAT_TYPES, 16702);
    location!(STR_LEN_BYTES, 34473);
    location!(STR_LEN_CP, 34471);
    location!(SUBSTR_START, 50752);
    location!(SUBSTR_START_UTF8, 28656);
    location!(SUBSTR_END_UTF8, 28657);
    location!(SUBSTR_CP_START, 34455);
    location!(SUBSTR_CP_LENGTH, 34454);
    location!(INDEX_OF_FIRST, 40091);
    location!(INDEX_OF_SECOND, 40092);
    location!(INDEX_OF_BOUND, 40096);
    location!(SPLIT_FIRST, 40085);
    location!(SPLIT_SECOND, 40086);
    location!(SPLIT_EMPTY, 40087);
}

fn error(code: ErrorCode, msg: impl Into<String>) -> MqlError {
    MqlError::new(code, msg)
}

/// A parsed expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A constant.
    Literal(Value),
    /// A field of the current document: its dotted path.
    Field(String),
    /// A variable (`ROOT`, `CURRENT`, `NOW`, `REMOVE` or a user's), and a path under it.
    Variable(String, Option<String>),
    /// A document whose fields are expressions.
    Object(Vec<(String, Expr)>),
    /// An array of expressions.
    Array(Vec<Expr>),
    /// An operator and its parsed arguments.
    Op(Op),
}

/// What parsing knows of the variables in scope: with `checked`, a variable that is neither a
/// system variable nor one of `names` is RefDb's 17276 when the expression is parsed, as in
/// `$let`, `$map`, `$filter` and `$reduce`; without it (an expression parsed alone, whose
/// variables the caller sets) the check waits for evaluation.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    checked: bool,
    names: Vec<String>,
}

/// The binary subtype of an MD5 digest and its length, which a literal must have.
const MD5_SUBTYPE: u8 = 5;
const MD5_LEN: usize = 16;

/// The variables RefDb defines itself.
const SYSTEM_VARIABLES: [&str; 8] =
    ["ROOT", "CURRENT", "REMOVE", "NOW", "CLUSTER_TIME", "USER_ROLES", "SEARCH_META", "IS_MR"];

impl Ctx {
    /// A context that checks variables, with the command's `let` variables `names`.
    pub fn checked<'a>(names: impl IntoIterator<Item = &'a str>) -> Ctx {
        Ctx { checked: true, names: names.into_iter().map(str::to_owned).collect() }
    }

    /// This context with variables `names` defined too.
    pub fn with<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Ctx {
        let mut out = self.clone();
        out.names.extend(names.into_iter().map(str::to_owned));
        out
    }

    /// Checks that variable `name` is defined.
    fn check(&self, name: &str) -> Result<(), MqlError> {
        if !self.checked || SYSTEM_VARIABLES.contains(&name) || self.names.iter().any(|n| n == name)
        {
            return Ok(());
        }
        Err(error(codes::UNDEFINED_VARIABLE, format!("Use of undefined variable: {name}")))
    }
}

/// An operator with its arguments.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Arithmetic: `$add`, `$subtract`, `$multiply`, `$divide`, `$mod`, `$abs`, `$ceil`,
    /// `$floor`, `$sqrt`, `$exp`, `$ln`, `$log10`, `$log`, `$pow`, `$round` and `$trunc`.
    Arith(arith::Arith, Vec<Expr>),
    /// Comparison, boolean and conditional operators.
    Logic(logic::Logic, Vec<Expr>),
    /// String operators.
    Str(string::Str, Vec<Expr>),
    /// `$switch`: its branches and default.
    Switch(Vec<(Expr, Expr)>, Option<Box<Expr>>),
    /// Array operators with positional arguments.
    Array(array::ArrayOp, Vec<Expr>),
    /// Set operators.
    Set(sets::SetOp, Vec<Expr>),
    /// `$sum`, `$avg`, `$max`, `$min`, `$stdDevPop` and `$stdDevSamp` over their arguments.
    Stat(stats::Stat, Vec<Expr>),
    /// `$let`, `$map`, `$filter` and `$reduce`.
    Vars(Box<vars::Binding>),
    /// `$mergeObjects`, `$getField`, `$setField` and `$unsetField`.
    Object(Box<object::ObjectExpr>),
    /// Array operators that take a document of named arguments.
    Form(Box<forms::Form>),
    /// `$type`, `$isNumber`, `$convert` and its shorthands.
    Type(Box<convert::TypeExpr>),
    /// `$trim`, `$ltrim`, `$rtrim`, `$regexMatch`, `$regexFind`, `$regexFindAll`, `$replaceOne`
    /// and `$replaceAll`.
    Text(Box<text::Text>),
    /// Bitwise, trigonometric, size, timestamp, hashing, vector and context operators.
    Misc(Box<misc::Misc>),
    /// `$serializeEJSON` and `$deserializeEJSON`.
    Ejson(Box<ejson::Ejson>),
    /// The date field operators, `$dateToString`, `$dateToParts` and `$dateFromParts`.
    Date(Box<dates::DateExpr>),
    /// `$dateAdd`, `$dateSubtract`, `$dateDiff` and `$dateTrunc`.
    DateArith(Box<dates_arith::DateArith>),
    /// `$dateFromString`.
    FromString(Box<date_parse::FromString>),
}

/// The variables an evaluation sees.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    /// `$$ROOT`.
    pub root: Document,
    /// The user's variables (`let`, `$let`) and `NOW` and the others the command sets; a
    /// variable bound to a missing value holds `None`.
    pub vars: BTreeMap<String, Option<Value>>,
    /// The current document's metadata that `$meta` reads (`textScore`, `searchScore`…).
    pub meta: BTreeMap<String, Value>,
    /// Whether RefDb evaluates this expression with its slot-based engine (the keys and
    /// accumulators of `$group`), whose errors differ for some operators ([`sbe`]).
    pub sbe: bool,
    /// The state of `$rand` and `$createObjectId`, shared by the scopes made from this one; the
    /// caller seeds it with [`Scope::seeded`] from its entropy (this crate reads no system
    /// source).
    random: Rc<Cell<u64>>,
    /// The collation strings compare with (010 FR-014).
    pub collation: crate::collation::Collation,
}

impl Scope {
    /// A scope over document `root` with the command's `let` variables `vars`.
    pub fn new(root: Document, vars: Option<&Document>) -> Scope {
        let vars = vars.map(|v| v.iter().map(|(k, x)| (k.to_owned(), Some(x.clone()))).collect());
        Scope { root, vars: vars.unwrap_or_default(), ..Scope::default() }
    }

    /// The collator strings compare with, `None` for the binary order.
    pub fn collator(&self) -> Option<&dyn teseladb_bson::Collator> {
        self.collation.get()
    }

    /// This scope comparing strings with `collation`.
    pub fn with_collation(self, collation: crate::collation::Collation) -> Scope {
        Scope { collation, ..self }
    }

    /// This scope with `$rand` and `$createObjectId` drawing from `seed`.
    pub fn seeded(self, seed: u64) -> Scope {
        Scope { random: Rc::new(Cell::new(seed)), ..self }
    }

    /// The next 64 random bits (SplitMix64).
    fn random_u64(&self) -> u64 {
        let state = self.random.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.random.set(state);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A random double in [0, 1).
    fn random_unit(&self) -> f64 {
        (self.random_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The arguments of an operator as a list: an array's elements, or the one value.
pub(crate) fn arguments(v: RawValue<'_>) -> Vec<RawValue<'_>> {
    match v {
        RawValue::Array(a) => a.iter().collect(),
        other => vec![other],
    }
}

/// Checks the number of arguments of `name`: exactly one count (16020) or a range (28667).
fn count(name: &str, args: &[Expr], want: std::ops::RangeInclusive<usize>) -> Result<(), MqlError> {
    if want.contains(&args.len()) {
        return Ok(());
    }
    if want.start() == want.end() {
        return Err(error(
            codes::WRONG_ARGUMENT_COUNT,
            format!(
                "Expression {name} takes exactly {} arguments. {} were passed in.",
                want.start(),
                args.len()
            ),
        ));
    }
    Err(error(
        codes::RANGE_ARGUMENTS,
        format!(
            "Expression {name} takes at least {} arguments, and at most {}, but {} were passed in.",
            want.start(),
            want.end(),
            args.len()
        ),
    ))
}

impl Expr {
    /// Parses expression `v`; its variables are checked when it is evaluated.
    pub fn parse(v: RawValue<'_>) -> Result<Expr, MqlError> {
        Expr::parse_in(v, &Ctx::default())
    }

    /// Parses expression `v` with the variables of `ctx`.
    pub fn parse_in(v: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
        match v {
            RawValue::String(s) if s.starts_with("$$") => path::variable(s, ctx),
            RawValue::String(s) if s.starts_with('$') => path::field(s),
            RawValue::Document(d) => {
                let mut fields = d.iter();
                match fields.next() {
                    Some((name, arg)) if name.starts_with('$') => {
                        if d.iter().count() > 1 {
                            return Err(error(
                                codes::OBJECT_OPERATOR,
                                "an expression specification must contain exactly one field, the name of the expression. Found 2 fields",
                            ));
                        }
                        Expr::operator(name, arg, ctx)
                    }
                    _ => {
                        let mut out = Vec::new();
                        for (k, x) in d.iter() {
                            if k.starts_with('$') {
                                return Err(error(
                                    codes::OBJECT_OPERATOR,
                                    format!(
                                        "an expression operator specification must contain exactly one field, found 2 fields '{}' and '{k}'.",
                                        d.iter().next().map(|(f, _)| f).unwrap_or_default()
                                    ),
                                ));
                            }
                            out.push((k.to_owned(), Expr::parse_in(x, ctx)?));
                        }
                        Ok(Expr::Object(out))
                    }
                }
            }
            RawValue::Array(a) => {
                let items = a.iter().map(|x| Expr::parse_in(x, ctx));
                Ok(Expr::Array(items.collect::<Result<_, _>>()?))
            }
            RawValue::Binary { subtype: MD5_SUBTYPE, bytes } if bytes.len() != MD5_LEN => {
                Err(error(
                    codes::FAILED_TO_PARSE,
                    "BinData subtype 5 requires exactly 16 bytes as an expression literal",
                ))
            }
            other => Ok(Expr::Literal(Value::from(other))),
        }
    }

    /// Parses operator `name` with argument `arg`.
    fn operator(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
        if name == "$literal" {
            return Ok(Expr::Literal(Value::from(arg)));
        }
        if name == "$switch" {
            return logic::parse_switch(arg, ctx);
        }
        if name == "$cond" {
            return logic::parse_cond(arg, ctx);
        }
        let args = || {
            arguments(arg)
                .into_iter()
                .map(|x| Expr::parse_in(x, ctx))
                .collect::<Result<Vec<_>, _>>()
        };
        if let Some(b) = vars::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Vars(Box::new(b))));
        }
        if let Some(o) = object::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Object(Box::new(o))));
        }
        if let Some(d) = dates::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Date(Box::new(d))));
        }
        if let Some(d) = dates_arith::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::DateArith(Box::new(d))));
        }
        if let Some(d) = date_parse::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::FromString(Box::new(d))));
        }
        if let Some(e) = ejson::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Ejson(Box::new(e))));
        }
        if let Some(m) = misc::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Misc(Box::new(m))));
        }
        if let Some(t) = text::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Text(Box::new(t))));
        }
        if let Some(t) = convert::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Type(Box::new(t))));
        }
        if let Some(f) = forms::Form::parse(name, arg, ctx)? {
            return Ok(Expr::Op(Op::Form(Box::new(f))));
        }
        if let Some(a) = array::ArrayOp::of(name) {
            let args = args()?;
            a.check(name, &args)?;
            return Ok(Expr::Op(Op::Array(a, args)));
        }
        if let Some(a) = sets::SetOp::of(name) {
            let args = args()?;
            a.check(name, &args)?;
            return Ok(Expr::Op(Op::Set(a, args)));
        }
        if let Some(a) = stats::Stat::of(name) {
            return Ok(Expr::Op(Op::Stat(a, args()?)));
        }
        if let Some(a) = arith::Arith::of(name) {
            let args = args()?;
            a.check(name, &args)?;
            return Ok(Expr::Op(Op::Arith(a, args)));
        }
        if let Some(st) = string::Str::of(name) {
            let args = args()?;
            st.check(name, &args)?;
            return Ok(Expr::Op(Op::Str(st, args)));
        }
        if let Some(l) = logic::Logic::of(name) {
            let args = args()?;
            l.check(name, &args)?;
            return Ok(Expr::Op(Op::Logic(l, args)));
        }
        Err(error(codes::UNKNOWN_EXPRESSION, format!("Unrecognized expression '{name}'")))
    }

    /// Evaluates the expression in `scope` over the current document `doc`.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            Expr::Op(op) if scope.sbe => self.eval_plain(doc, scope).map_err(|e| sbe::remap(op, e)),
            _ => self.eval_plain(doc, scope),
        }
    }

    /// Evaluates the expression with RefDb's classic engine's errors.
    fn eval_plain(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            Expr::Literal(v) => Ok(Some(v.clone())),
            Expr::Field(p) => Ok(path::field_of(doc, p)),
            Expr::Variable(name, p) => path::eval_variable(name, p.as_deref(), doc, scope),
            Expr::Object(fields) => {
                let mut out = Document::new();
                for (k, e) in fields {
                    if let Some(v) = e.eval(doc, scope)? {
                        out.push(k.as_str(), v);
                    }
                }
                Ok(Some(Value::Document(out)))
            }
            // An element that gives nothing is `null` in an array.
            Expr::Array(items) => Ok(Some(Value::Array(
                items
                    .iter()
                    .map(|e| e.eval(doc, scope).map(|v| v.unwrap_or(Value::Null)))
                    .collect::<Result<_, _>>()?,
            ))),
            Expr::Op(Op::Arith(a, args)) => a.eval(args, doc, scope),
            Expr::Op(Op::Logic(l, args)) => l.eval(args, doc, scope),
            Expr::Op(Op::Str(st, args)) => st.eval(args, doc, scope),
            Expr::Op(Op::Switch(branches, default)) => {
                logic::eval_switch(branches, default.as_deref(), doc, scope)
            }
            Expr::Op(Op::Array(a, args)) => a.eval(args, doc, scope),
            Expr::Op(Op::Set(a, args)) => a.eval(args, doc, scope),
            Expr::Op(Op::Stat(a, args)) => a.eval(args, doc, scope),
            Expr::Op(Op::Vars(b)) => b.eval(doc, scope),
            Expr::Op(Op::Object(o)) => o.eval(doc, scope),
            Expr::Op(Op::Form(f)) => f.eval(doc, scope),
            Expr::Op(Op::Type(t)) => t.eval(doc, scope),
            Expr::Op(Op::Text(t)) => t.eval(doc, scope),
            Expr::Op(Op::Misc(m)) => m.eval(doc, scope),
            Expr::Op(Op::Ejson(e)) => e.eval(doc, scope),
            Expr::Op(Op::Date(d)) => d.eval(doc, scope),
            Expr::Op(Op::DateArith(d)) => d.eval(doc, scope),
            Expr::Op(Op::FromString(d)) => d.eval(doc, scope),
        }
    }
}
