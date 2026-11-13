//! Field paths and variables of expressions: `"$a.b"` reads `a.b` of the current document, going
//! into every element of an array on the way (an array of what each element gives, without the
//! elements that give nothing); `"$$name.b"` reads a variable.

use teseladb_mql::MqlError;
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::{Ctx, Eval, Expr, Scope, codes, error};

/// The value of dotted path `path` in `v`, RefDb's way.
pub fn field_path(v: &Value, path: &str) -> Eval {
    let (head, rest) = match path.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (path, None),
    };
    match v {
        Value::Document(d) => {
            let x = d.get(head)?;
            match rest {
                Some(r) => field_path(x, r),
                None => Some(x.clone()),
            }
        }
        Value::Array(items) => Some(Value::Array(
            items
                .iter()
                .filter(|i| matches!(i, Value::Document(_) | Value::Array(_)))
                .filter_map(|i| field_path(i, path))
                .collect(),
        )),
        _ => None,
    }
}

/// The value of dotted path `path` in document `doc`. A document that keeps the bytes it was
/// stored as is read on them (003 FR-020): only the values at the end of the path are made, the
/// rest of the document is not read into values. A `$group` by one field of documents read
/// through an index reads only that field of each.
pub(super) fn field_of(doc: &Document, path: &str) -> Eval {
    match doc.canonical_bytes().and_then(|b| RawDoc::from_stored(b).ok()) {
        Some(raw) => raw_field_path(RawValue::Document(raw), path),
        None => {
            let (head, rest) = match path.split_once('.') {
                Some((h, r)) => (h, Some(r)),
                None => (path, None),
            };
            let x = doc.get(head)?;
            match rest {
                Some(r) => field_path(x, r),
                None => Some(x.clone()),
            }
        }
    }
}

/// [`field_path`] on BSON bytes: the same values, made only at the end of the path.
pub(crate) fn raw_field_path(v: RawValue<'_>, path: &str) -> Eval {
    match v {
        RawValue::Document(d) => {
            let (head, rest) = match path.split_once('.') {
                Some((h, r)) => (h, Some(r)),
                None => (path, None),
            };
            let (_, x) = d.iter().find(|(k, _)| *k == head)?;
            match rest {
                Some(r) => raw_field_path(x, r),
                None => Some(Value::from(x)),
            }
        }
        RawValue::Array(items) => Some(Value::Array(
            items
                .iter()
                .filter(|i| matches!(i, RawValue::Document(_) | RawValue::Array(_)))
                .filter_map(|i| raw_field_path(i, path))
                .collect(),
        )),
        _ => None,
    }
}

/// Checks the dotted path after a `$`, as RefDb parses a field path.
fn check_path(text: &str) -> Result<(), MqlError> {
    if text.is_empty() {
        return Err(error(codes::INVALID_FIELD_PATH, "'$' by itself is not a valid FieldPath"));
    }
    for part in text.split('.') {
        let why = if part.is_empty() {
            "FieldPath field names may not be empty strings."
        } else if part.starts_with('$') {
            "FieldPath field names may not start with '$'."
        } else {
            continue;
        };
        return Err(error(
            codes::BAD_FIELD_PATH,
            format!(
                "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: {why}"
            ),
        ));
    }
    Ok(())
}

/// Parses field path `s` (`"$a.b"`).
pub(super) fn field(s: &str) -> Result<Expr, MqlError> {
    let text = &s[1..];
    check_path(text)?;
    Ok(Expr::Field(text.to_owned()))
}

/// Parses variable `s` (`"$$name.path"`).
pub(super) fn variable(s: &str, ctx: &Ctx) -> Result<Expr, MqlError> {
    let text = &s[2..];
    let (name, rest) = match text.split_once('.') {
        Some((n, r)) => (n, Some(r)),
        None => (text, None),
    };
    if name.is_empty() {
        return Err(error(codes::FAILED_TO_PARSE, "empty variable names are not allowed"));
    }
    if let Some(r) = rest {
        check_path(r)?;
    }
    ctx.check(name)?;
    Ok(Expr::Variable(name.to_owned(), rest.map(str::to_owned)))
}

/// The system variables a command sets (or not, as `IS_MR` and `JS_SCOPE` outside
/// map-reduce and JavaScript): one it did not set is RefDb's 51144.
const BUILTINS: [&str; 5] = ["NOW", "CLUSTER_TIME", "USER_ROLES", "IS_MR", "JS_SCOPE"];

/// `Builtin variable ... is not available`.
const BUILTIN_NOT_AVAILABLE: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 51144, name: "Location51144" };

/// The value of variable `name` (with `path` under it) for current document `doc`.
pub(super) fn eval_variable(
    name: &str,
    path: Option<&str>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let base = match name {
        "ROOT" => Value::Document(scope.root.clone()),
        "CURRENT" => Value::Document(doc.clone()),
        "REMOVE" => return Ok(None),
        other => match scope.vars.get(other) {
            Some(Some(v)) => v.clone(),
            Some(None) => return Ok(None),
            None if BUILTINS.contains(&other) => {
                return Err(error(
                    BUILTIN_NOT_AVAILABLE,
                    format!("Builtin variable '$${other}' is not available"),
                ));
            }
            None => {
                return Err(error(
                    codes::UNDEFINED_VARIABLE,
                    format!("Use of undefined variable: {other}"),
                ));
            }
        },
    };
    Ok(match path {
        Some(p) => field_path(&base, p),
        None => Some(base),
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod raw_path_tests {
    use teseladb_bson::{Document, RawDoc, RawValue, Value};

    use super::{field_of, field_path, raw_field_path};

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    const NAMES: [&str; 3] = ["a", "b", "0"];

    fn value(rng: &mut Rng, depth: u32) -> Value {
        match rng.below(if depth > 3 { 3 } else { 6 }) {
            0 => Value::Int32(i32::try_from(rng.below(9)).expect("small")),
            1 => Value::Null,
            2 => Value::String("s".into()),
            3 | 4 => Value::Document(doc(rng, depth + 1)),
            _ => Value::Array((0..rng.below(4)).map(|_| value(rng, depth + 1)).collect()),
        }
    }

    fn doc(rng: &mut Rng, depth: u32) -> Document {
        let mut d = Document::new();
        for n in NAMES {
            if rng.below(3) != 0 {
                d.insert(n, value(rng, depth));
            }
        }
        d
    }

    /// Every path of up to three segments of a seeded corpus reads the same values from the bytes
    /// as from the document read whole.
    #[test]
    fn the_bytes_give_what_the_values_give() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        let mut paths = Vec::new();
        for a in NAMES {
            paths.push(a.to_owned());
            for b in NAMES {
                paths.push(format!("{a}.{b}"));
                for c in NAMES {
                    paths.push(format!("{a}.{b}.{c}"));
                }
            }
        }
        for _ in 0..500 {
            let d = doc(&mut rng, 0);
            let bytes = d.to_bytes().expect("small");
            let kept = Document::from_stored(bytes.clone()).expect("framed");
            let raw = RawDoc::from_bytes(&bytes).expect("valid");
            let whole = Value::Document(d.clone());
            for p in &paths {
                let want = field_path(&whole, p);
                assert_eq!(raw_field_path(RawValue::Document(raw), p), want, "{p} in {d:?}");
                assert_eq!(field_of(&kept, p), want, "{p} in {d:?}");
                assert_eq!(field_of(&d, p), want, "{p} in {d:?}");
            }
        }
    }
}
