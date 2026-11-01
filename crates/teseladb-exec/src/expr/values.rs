//! Helpers the operator families share: values written as RefDb writes them in messages,
//! integral arguments, and comparing and sorting values in RefDb's order.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, RawValue, Value, compare, shell};
use teseladb_mql::{MqlError, decimal_to_f64};

/// `v` as RefDb writes a value of an expression in a message (its `Value` printer): strings
/// in double quotes, `{a: 1}`, `[1, 2]`, doubles as C++ streams write them (`1.5`, `9.22337e+18`).
pub(crate) fn display(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{s}\""),
        Value::Double(x) => teseladb_mql::stream_double(*x),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Decimal128(d) => d.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Null => "null".to_owned(),
        Value::DateTime(ms) => super::string::date_text(*ms),
        Value::Undefined => "undefined".to_owned(),
        Value::Array(items) => {
            format!("[{}]", items.iter().map(display).collect::<Vec<_>>().join(", "))
        }
        Value::Document(d) => {
            let fields: Vec<String> =
                d.iter().map(|(k, x)| format!("{k}: {}", display(x))).collect();
            format!("{{{}}}", fields.join(", "))
        }
        other => display_bson(other),
    }
}

/// `v` as RefDb writes a BSON value of a command in a message (`[ 0.5, "$i" ]`).
pub(crate) fn display_bson(v: &Value) -> String {
    let holder = Document::new().with("v", v.clone());
    match holder.to_bytes().ok().as_deref().map(RawDoc::from_bytes) {
        Some(Ok(raw)) => raw.get("v").map(shell::shell_value).unwrap_or_default(),
        _ => String::new(),
    }
}

/// A result as [`display`] writes it, `MISSING` for nothing.
pub(crate) fn display_eval(v: &Option<Value>) -> String {
    match v {
        Some(x) => display(x),
        None => "MISSING".to_owned(),
    }
}

/// A number as a 32-bit integer when it is one exactly (`2`, `2.0`, `NumberLong(2)`,
/// `NumberDecimal("2")`).
pub(super) fn int32_of(v: &Value) -> Option<i32> {
    let x = match v {
        Value::Int32(n) => return Some(*n),
        Value::Int64(n) => return i32::try_from(*n).ok(),
        Value::Double(x) => *x,
        Value::Decimal128(d) => decimal_to_f64(*d),
        _ => return None,
    };
    let fits = x.fract() == 0.0 && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&x);
    fits.then_some(x as i32)
}

/// A number as a 64-bit integer when it is one exactly.
pub(crate) fn int64_of(v: &Value) -> Option<i64> {
    let x = match v {
        Value::Int32(n) => return Some(i64::from(*n)),
        Value::Int64(n) => return Some(*n),
        Value::Double(x) => *x,
        Value::Decimal128(d) => decimal_to_f64(*d),
        _ => return None,
    };
    let fits =
        x.fract() == 0.0 && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&x);
    fits.then_some(x as i64)
}

/// A number as a double, `None` for anything else.
pub(crate) fn f64_of(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(x) => Some(*x),
        Value::Decimal128(d) => Some(decimal_to_f64(*d)),
        _ => None,
    }
}

/// Whether `v` is a number.
pub(crate) fn is_number(v: &Value) -> bool {
    matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_))
}

/// Runs `f` over the raw forms of the values of each list.
pub(super) fn with_raw<R>(
    lists: &[&[Value]],
    f: impl FnOnce(&[Vec<RawValue<'_>>]) -> R,
) -> Result<R, MqlError> {
    let mut holder = Document::new();
    for (i, l) in lists.iter().enumerate() {
        holder.push(i.to_string().as_str(), Value::Array(l.to_vec()));
    }
    let bytes = crate::raw::bytes_of(&holder)?;
    let raw = RawDoc::from_bytes(&bytes).map_err(crate::raw::bad)?;
    let raws: Vec<Vec<RawValue<'_>>> = raw
        .iter()
        .map(|(_, v)| match v {
            RawValue::Array(a) => a.iter().collect(),
            _ => Vec::new(),
        })
        .collect();
    Ok(f(&raws))
}

/// The positions of `keys` in RefDb's order (a stable sort): each element's keys compare in
/// turn, each in its direction (`true` for descending).
pub(super) fn sorted_positions(
    keys: &[Vec<Value>],
    descending: &[bool],
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Vec<usize>, MqlError> {
    let columns: Vec<Vec<Value>> = (0..descending.len())
        .map(|c| keys.iter().map(|k| k.get(c).cloned().unwrap_or(Value::Null)).collect())
        .collect();
    let lists: Vec<&[Value]> = columns.iter().map(Vec::as_slice).collect();
    with_raw(&lists, |raws| {
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by(|&i, &j| {
            for (c, desc) in descending.iter().enumerate() {
                let o = compare(raws[c][i], raws[c][j], col);
                let o = if *desc { o.reverse() } else { o };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
        order
    })
}

/// `values` in RefDb's order, ascending, strings in collation `col`.
pub(super) fn sorted(
    values: Vec<Value>,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Vec<Value>, MqlError> {
    let keys: Vec<Vec<Value>> = values.iter().map(|v| vec![v.clone()]).collect();
    let order = sorted_positions(&keys, &[false], col)?;
    let mut slots: Vec<Option<Value>> = values.into_iter().map(Some).collect();
    Ok(order.into_iter().filter_map(|i| slots[i].take()).collect())
}
