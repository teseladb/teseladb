//! The expressions of a pipeline as RefDb optimises them when it parses the command (measured
//! on a `refrouter` 9.0.4, 01-11-2026): a constant that fails, in any stage and at any position,
//! fails the command with «Failed to optimize pipeline» before any document is read; in a
//! `$match`, after «Failed to optimize expression» as in a filter.

use teseladb_mql::MqlError;
use teseladb_bson::{Document, RawValue, Value};

use crate::expr::optimize::{optimize, optimize_filter};
use crate::expr::{Ctx, Scope};

/// What RefDb adds before the error of a constant of a pipeline that fails.
const FAILED_TO_OPTIMIZE_PIPELINE: &str = "Failed to optimize pipeline :: caused by :: ";

/// The values of a `$project` field that keep or drop it.
fn is_flag(v: &Value) -> bool {
    matches!(
        v,
        Value::Boolean(_)
            | Value::Int32(_)
            | Value::Int64(_)
            | Value::Double(_)
            | Value::Decimal128(_)
    )
}

/// Optimises the expressions of `pipeline` with the command's variables `vars`, as RefDb does
/// once the stages parsed.
pub fn optimize_stages(pipeline: &[RawValue<'_>], vars: Option<&Document>) -> Result<(), MqlError> {
    let mut names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
    // The variables `$redact` gives its expression.
    names.extend(["KEEP", "PRUNE", "DESCEND"]);
    let walk = Walk { ctx: Ctx::checked(names), scope: Scope::new(Document::new(), vars), vars };
    for st in pipeline {
        if let RawValue::Document(d) = st {
            walk.stage(&Document::from(*d)).map_err(|e| MqlError {
                message: format!("{FAILED_TO_OPTIMIZE_PIPELINE}{}", e.message),
                ..e
            })?;
        }
    }
    Ok(())
}

struct Walk<'a> {
    ctx: Ctx,
    scope: Scope,
    vars: Option<&'a Document>,
}

impl Walk<'_> {
    fn expr(&self, v: &Value) -> Result<(), MqlError> {
        optimize(v, &self.ctx, &self.scope).map(|_| ())
    }

    fn stage(&self, st: &Document) -> Result<(), MqlError> {
        let Some((name, arg)) = st.iter().next() else { return Ok(()) };
        match (name, arg) {
            ("$match", Value::Document(f)) => optimize_filter(f, self.vars).map(|_| ()),
            ("$project", Value::Document(p)) => self.projection(p),
            ("$addFields" | "$set", Value::Document(p)) => self.fields(p),
            ("$group", Value::Document(g)) => {
                for (k, v) in g.iter() {
                    match k {
                        "_id" => self.expr(v)?,
                        _ => self.accumulator(v)?,
                    }
                }
                Ok(())
            }
            ("$bucket" | "$bucketAuto", Value::Document(b)) => {
                if let Some(v) = b.get("groupBy") {
                    self.expr(v)?;
                }
                if let Some(Value::Document(out)) = b.get("output") {
                    for (_, acc) in out.iter() {
                        self.accumulator(acc)?;
                    }
                }
                Ok(())
            }
            ("$replaceRoot", Value::Document(r)) => {
                r.get("newRoot").map_or(Ok(()), |v| self.expr(v))
            }
            ("$replaceWith" | "$sortByCount" | "$redact", v) => self.expr(v),
            ("$facet", Value::Document(f)) => {
                for (_, p) in f.iter() {
                    if let Value::Array(stages) = p {
                        for s in stages {
                            if let Value::Document(d) = s {
                                self.stage(d)?;
                            }
                        }
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// An accumulator `{$op: argument}`: its argument, or the expressions of its named ones.
    fn accumulator(&self, v: &Value) -> Result<(), MqlError> {
        let Value::Document(d) = v else { return Ok(()) };
        let Some((_, arg)) = d.iter().next() else { return Ok(()) };
        match arg {
            Value::Document(named)
                if named.iter().next().is_some_and(|(k, _)| !k.starts_with('$')) =>
            {
                for k in ["n", "input", "output"] {
                    if let Some(x) = named.get(k) {
                        self.expr(x)?;
                    }
                }
                Ok(())
            }
            other => self.expr(other),
        }
    }

    /// The fields of a `$project`: flags keep or drop, documents without operators nest.
    fn projection(&self, p: &Document) -> Result<(), MqlError> {
        for (_, v) in p.iter() {
            match v {
                v if is_flag(v) => {}
                Value::Document(d) if d.iter().next().is_some_and(|(k, _)| !k.starts_with('$')) => {
                    self.projection(d)?;
                }
                other => self.expr(other)?,
            }
        }
        Ok(())
    }

    /// The fields of `$addFields` and `$set`: documents without operators nest.
    fn fields(&self, p: &Document) -> Result<(), MqlError> {
        for (_, v) in p.iter() {
            match v {
                Value::Document(d) if d.iter().next().is_some_and(|(k, _)| !k.starts_with('$')) => {
                    self.fields(d)?;
                }
                other => self.expr(other)?,
            }
        }
        Ok(())
    }
}
