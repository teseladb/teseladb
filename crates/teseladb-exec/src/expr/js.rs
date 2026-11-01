//! The extension point of server-side JavaScript in the executor (010 FR-019, T012): `$function`
//! and the `$accumulator` of `$group` are read and checked here, as a `refrouter` 9.0.4 reads them
//! (measured in `crates/teseladb-exec/tests/agg/js.out`), and each call of a function goes to the
//! [`JsEngine`] of the scope, which 016 provides. Without one, a call fails.

use std::fmt::Debug;

use teseladb_bson::{RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

use super::{Ctx, Expr, Scope, arguments, error};

/// A JavaScript engine that runs a function's code (a string or `Code`) on arguments.
pub trait JsEngine: Debug {
    /// The value `body` gives for `args`.
    fn call(&self, body: &Value, args: &[Value]) -> Result<Value, MqlError>;
}

/// A location code.
macro_rules! loc {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const FUNCTION_NOT_OBJECT: ErrorCode = loc!(31260);
const BODY_MISSING: ErrorCode = loc!(31261);
const BODY_TYPE: ErrorCode = loc!(31262);
const ARGS_MISSING: ErrorCode = loc!(31263);
const ARGS_TYPE: ErrorCode = loc!(31266);
const LANG_MISSING: ErrorCode = loc!(31418);
const LANG_VALUE: ErrorCode = loc!(31419);
const ACC_NOT_OBJECT: ErrorCode = loc!(4544703);
const ACC_LANG: ErrorCode = loc!(4544705);
const ACC_UNKNOWN: ErrorCode = loc!(4544706);
const ACC_INIT: ErrorCode = loc!(4544707);
const ACC_ACCUMULATE: ErrorCode = loc!(4544708);
const ACC_MERGE: ErrorCode = loc!(4544709);
const ACC_ARGS: ErrorCode = loc!(4544710);
const ACC_INIT_ARGS: ErrorCode = loc!(4544711);
/// The code of a call without an engine.
const NO_ENGINE: ErrorCode = ErrorCode { code: 1, name: "InternalError" };

/// The only language RefDb runs.
const JS: &str = "js";

/// A `$function`.
#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    body: Value,
    args: Vec<Expr>,
}

/// Whether `v` is a function's code: a string or `Code`.
fn is_code(v: RawValue<'_>) -> bool {
    matches!(v, RawValue::String(_) | RawValue::JavaScript(_))
}

/// Calls `body` with `args` in the engine of `scope`.
pub(crate) fn call(
    scope: &Scope,
    what: &str,
    body: &Value,
    args: &[Value],
) -> Result<Value, MqlError> {
    match &scope.js {
        Some(engine) => engine.call(body, args),
        None => Err(error(NO_ENGINE, format!("{what} needs an engine this node does not have"))),
    }
}

impl Function {
    /// Reads `$function`: `body`, `args` and `lang: "js"`; other fields are ignored (measured).
    pub(crate) fn parse(arg: RawValue<'_>, ctx: &Ctx) -> Result<Function, MqlError> {
        let RawValue::Document(d) = arg else {
            return Err(error(
                FUNCTION_NOT_OBJECT,
                format!(
                    "$function requires an object as an argument, found: {}",
                    super::arith::raw_type_name(arg)
                ),
            ));
        };
        let (mut body, mut args, mut lang) = (None, None, None);
        for (k, x) in d.iter() {
            match k {
                "body" if is_code(x) => body = Some(Value::from(x)),
                "body" => {
                    return Err(error(
                        BODY_TYPE,
                        "The body function must evaluate to type string or code",
                    ));
                }
                "args" => match x {
                    RawValue::Array(_) => {
                        args = Some(
                            arguments(x)
                                .into_iter()
                                .map(|a| Expr::parse_in(a, ctx))
                                .collect::<Result<Vec<_>, _>>()?,
                        );
                    }
                    _ => return Err(error(ARGS_TYPE, "The args field must be of type array")),
                },
                "lang" => match x {
                    RawValue::String(JS) => lang = Some(()),
                    _ => {
                        return Err(error(
                            LANG_VALUE,
                            "Currently the only supported language specifier is 'js'.",
                        ));
                    }
                },
                _ => {}
            }
        }
        let body =
            body.ok_or_else(|| error(BODY_MISSING, "The body function must be specified."))?;
        let args = args.ok_or_else(|| error(ARGS_MISSING, "The args field must be specified."))?;
        lang.ok_or_else(|| error(LANG_MISSING, "The lang field must be specified."))?;
        Ok(Function { body, args })
    }

    /// The value of the function for the arguments evaluated on `doc`.
    pub(crate) fn eval(
        &self,
        doc: &teseladb_bson::Document,
        scope: &Scope,
    ) -> Result<super::Eval, MqlError> {
        let mut args = Vec::with_capacity(self.args.len());
        for a in &self.args {
            args.push(a.eval(doc, scope)?.unwrap_or(Value::Undefined));
        }
        call(scope, "$function", &self.body, &args).map(Some)
    }
}

/// An `$accumulator` of `$group`.
#[derive(Clone, Debug)]
pub struct Accumulator {
    /// `init`: the state before the first document.
    pub init: Value,
    /// `accumulate`: the state after one more document.
    pub accumulate: Value,
    /// `merge`: the state of two partial states.
    pub merge: Value,
    /// `finalize`: the group's value from its state.
    pub finalize: Option<Value>,
    /// `initArgs`, evaluated on a group's first document.
    pub init_args: Option<Expr>,
    /// `accumulateArgs`, evaluated on every document.
    pub accumulate_args: Expr,
}

impl Accumulator {
    /// Reads `$accumulator` (measured on a `refrouter` 9.0.4).
    pub(crate) fn parse(arg: RawValue<'_>, ctx: &Ctx) -> Result<Accumulator, MqlError> {
        let RawValue::Document(d) = arg else {
            return Err(error(
                ACC_NOT_OBJECT,
                format!(
                    "$accumulator expects an object as an argument; found: {}",
                    super::arith::raw_type_name(arg)
                ),
            ));
        };
        let (mut init, mut accumulate, mut merge, mut finalize) = (None, None, None, None);
        let (mut init_args, mut accumulate_args) = (None, None);
        for (k, x) in d.iter() {
            match k {
                "init" => init = Some(Value::from(x)),
                "accumulate" => accumulate = Some(Value::from(x)),
                "merge" => merge = Some(Value::from(x)),
                "finalize" => finalize = Some(Value::from(x)),
                "initArgs" => init_args = Some(Expr::parse_in(x, ctx)?),
                "accumulateArgs" => accumulate_args = Some(Expr::parse_in(x, ctx)?),
                "lang" if matches!(x, RawValue::String(JS)) => {}
                "lang" => return Err(error(ACC_LANG, "$accumulator only supports lang: 'js'")),
                other => {
                    return Err(error(
                        ACC_UNKNOWN,
                        format!("$accumulator got an unexpected field: {other}"),
                    ));
                }
            }
        }
        let missing = |code, name: &str| {
            error(code, format!("$accumulator missing required argument '{name}'"))
        };
        let init = init.ok_or_else(|| missing(ACC_INIT, "init"))?;
        let accumulate = accumulate.ok_or_else(|| missing(ACC_ACCUMULATE, "accumulate"))?;
        let merge = merge.ok_or_else(|| missing(ACC_MERGE, "merge"))?;
        let accumulate_args = accumulate_args.ok_or_else(|| missing(ACC_ARGS, "accumulateArgs"))?;
        Ok(Accumulator { init, accumulate, merge, finalize, init_args, accumulate_args })
    }

    /// Reads the specification of `$accumulator` (for the coordinator that merges the partial
    /// states of each range).
    pub fn parse_spec(arg: RawValue<'_>, ctx: &Ctx) -> Result<Accumulator, MqlError> {
        Accumulator::parse(arg, ctx)
    }

    /// The state `init` gives with `initArgs`, which RefDb evaluates on an empty document
    /// (measured: `$$ROOT` is `{}`).
    pub(crate) fn start(&self, scope: &Scope) -> Result<Value, MqlError> {
        let args = match &self.init_args {
            None => Vec::new(),
            Some(e) => {
                let empty = teseladb_bson::Document::new();
                let outer = Scope { root: empty.clone(), ..scope.clone() };
                match e.eval(&empty, &outer)? {
                    Some(Value::Array(items)) => items,
                    other => {
                        let got = other
                            .as_ref()
                            .map_or_else(|| "MISSING".to_owned(), super::values::display);
                        return Err(error(
                            ACC_INIT_ARGS,
                            format!("$accumulator initArgs must evaluate to an array: {got}"),
                        ));
                    }
                }
            }
        };
        call(scope, "$accumulator", &self.init, &args)
    }

    /// The state after document `doc`.
    pub(crate) fn step(
        &self,
        state: Value,
        doc: &teseladb_bson::Document,
        scope: &Scope,
    ) -> Result<Value, MqlError> {
        let mut args = vec![state];
        match self.accumulate_args.eval(doc, scope)? {
            Some(Value::Array(items)) => args.extend(items),
            Some(other) => args.push(other),
            None => {}
        }
        call(scope, "$accumulator", &self.accumulate, &args)
    }

    /// The state of two partial states of one group (from two ranges).
    pub fn merge_states(&self, a: Value, b: Value, scope: &Scope) -> Result<Value, MqlError> {
        call(scope, "$accumulator", &self.merge, &[a, b])
    }

    /// The group's value from its state.
    pub(crate) fn finish(&self, state: Value, scope: &Scope) -> Result<Value, MqlError> {
        match &self.finalize {
            Some(f) => call(scope, "$accumulator", f, &[state]),
            None => Ok(state),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_call_without_an_engine_fails() {
        let err =
            call(&Scope::default(), "$function", &Value::from("f"), &[]).expect_err("no engine");
        assert_eq!(err.code, NO_ENGINE);
    }
}
