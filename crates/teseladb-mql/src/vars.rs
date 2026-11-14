//! The `let` of a command (006 FR-032): its variable names are checked with the reference server's
//! rules for user variables and its values are resolved before any document is read. A literal
//! stays as it is; an operator or a variable reference goes to the expression engine of 010 through
//! [`Extensions::eval`], with the variables defined before it. Messages and codes measured on a
//! reference router 9.0.4 (30-10-2026).

use teseladb_bson::{Document, RawDoc, Value};

use crate::error::{ErrorCode, MqlError, Result, codes};
use crate::ext::Extensions;

/// `Attempt to set internal constant`.
pub const INTERNAL_CONSTANT: ErrorCode = ErrorCode { code: 4_738_901, name: "Location4738901" };
/// A command variable that reads a field of a document.
pub const LET_READS_FIELD: ErrorCode = ErrorCode { code: 4_890_500, name: "Location4890500" };

/// The system variables that a command may not set ("internal constants").
const INTERNAL_CONSTANTS: [&str; 5] = ["NOW", "CLUSTER_TIME", "USER_ROLES", "IS_MR", "JS_SCOPE"];
/// The one system variable a `let` may name.
const CURRENT: &str = "CURRENT";
/// The operator whose argument is a value, never an expression.
const LITERAL: &str = "$literal";
/// The variables whose path reads the document.
const DOCUMENT_VARIABLES: [&str; 2] = ["CURRENT", "ROOT"];

fn invalid(name: &str, why: &str) -> MqlError {
    MqlError::new(
        codes::FAILED_TO_PARSE,
        format!("Invalid 'let' parameter :: caused by :: '{name}' {why}"),
    )
}

/// Checks `name` as the reference server checks the name of a user variable it may write.
fn check_name(name: &str) -> Result<()> {
    if INTERNAL_CONSTANTS.contains(&name) {
        return Err(MqlError::new(
            INTERNAL_CONSTANT,
            format!("Attempt to set internal constant: {name}"),
        ));
    }
    if name == CURRENT {
        return Ok(());
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(MqlError::new(
            codes::FAILED_TO_PARSE,
            "Invalid 'let' parameter :: caused by :: empty variable names are not allowed",
        ));
    };
    if !(first.is_ascii_lowercase() || !first.is_ascii()) {
        return Err(invalid(name, "starts with an invalid character for a user variable name"));
    }
    match chars.find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || !c.is_ascii())) {
        Some(c) => {
            Err(invalid(name, &format!("contains an invalid character for a variable name: '{c}'")))
        }
        None => Ok(()),
    }
}

/// What a value of a `let` needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Needs {
    /// Nothing: it is a literal.
    Nothing,
    /// The expression engine: an operator or a variable.
    Engine,
}

/// Whether string `s` of a `let` value reads a document: a field path (`$a`) or a path through
/// `$$CURRENT` or `$$ROOT`.
fn reads_document(s: &str) -> bool {
    match s.strip_prefix("$$") {
        Some(var) => {
            let name = var.split('.').next().unwrap_or(var);
            DOCUMENT_VARIABLES.contains(&name)
        }
        None => s.starts_with('$'),
    }
}

fn needs(v: &Value) -> Result<Needs> {
    let read = || {
        MqlError::new(
            LET_READS_FIELD,
            "Command let Expression tried to access a field, but this is not allowed \
             becauseCommand let Expressions run before the query examines any documents.",
        )
    };
    match v {
        Value::String(s) if reads_document(s) => Err(read()),
        Value::String(s) if s.starts_with("$$") => Ok(Needs::Engine),
        // `$literal` keeps its argument as it is: nothing in it reads a document.
        Value::Document(d) if d.iter().next().is_some_and(|(k, _)| k == LITERAL) => {
            Ok(Needs::Engine)
        }
        Value::Document(d) => {
            let mut n = if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) {
                Needs::Engine
            } else {
                Needs::Nothing
            };
            for (_, x) in d.iter() {
                if needs(x)? == Needs::Engine {
                    n = Needs::Engine;
                }
            }
            Ok(n)
        }
        Value::Array(items) => {
            let mut n = Needs::Nothing;
            for x in items {
                if needs(x)? == Needs::Engine {
                    n = Needs::Engine;
                }
            }
            Ok(n)
        }
        _ => Ok(Needs::Nothing),
    }
}

/// The variables of `let_doc`, checked and resolved in order: a literal as it is, anything else
/// evaluated by `ext` with the variables before it and no document.
pub fn resolve_let(let_doc: &Document, ext: &dyn Extensions) -> Result<Document> {
    let mut out = Document::new();
    let empty = Document::new().to_bytes().map_err(|e| {
        MqlError::new(codes::INTERNAL_ERROR, format!("an empty document did not encode: {e:?}"))
    })?;
    for (name, v) in let_doc.iter() {
        check_name(name)?;
        let value = match needs(v)? {
            Needs::Nothing => v.clone(),
            Needs::Engine => {
                let holder = Document::new().with("v", v.clone()).to_bytes().map_err(|e| {
                    MqlError::new(
                        codes::INTERNAL_ERROR,
                        format!("a variable did not encode: {e:?}"),
                    )
                })?;
                let raw = RawDoc::from_bytes(&holder)
                    .map_err(|e| MqlError::new(codes::INTERNAL_ERROR, format!("{e:?}")))?;
                let expr = raw.get("v").ok_or_else(|| {
                    MqlError::new(codes::INTERNAL_ERROR, "a variable lost its value")
                })?;
                let doc = RawDoc::from_bytes(&empty)
                    .map_err(|e| MqlError::new(codes::INTERNAL_ERROR, format!("{e:?}")))?;
                // A variable whose expression gives nothing holds `null`.
                ext.eval(expr, doc, Some(&out))?.unwrap_or(Value::Null)
            }
        };
        out.push(name, value);
    }
    Ok(out)
}
