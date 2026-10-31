//! Field operators: `$inc`, `$mul`, `$min`, `$max`, `$rename`, `$currentDate` and `$bit`
//! (`$set`, `$setOnInsert` and `$unset` are applied directly by the walk).

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::positional::{Slot, UpdatePath, is_prefix, not_viable};
use super::tree::{MDoc, MVal};
use super::{Action, Entry, UpdateContext};
use crate::arith::{self, Op};
use crate::error::{MqlError, Result, bad_value, codes, type_mismatch};
use crate::fmt::{shell, shell_doc};
use crate::ops;
use crate::types;
use crate::value::{OwnedDoc, cmp, is_number};

/// An operation of `$bit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BitOp {
    And,
    Or,
    Xor,
}

/// `{name: value}` in RefDb's notation.
fn field_text(name: &str, v: RawValue<'_>) -> String {
    match OwnedDoc::new(&Document::new().with(name, Value::from(v))) {
        Ok(d) => shell_doc(d.get()),
        Err(e) => e.message,
    }
}

pub(super) fn arith(op: &str, path: &str, v: RawValue<'_>) -> Result<Action> {
    if !is_number(&v) {
        let verb = if op == ops::INC { "increment" } else { "multiply" };
        let field = path.rsplit('.').next().unwrap_or(path);
        return Err(type_mismatch(format!(
            "Cannot {verb} with non-numeric argument: {}",
            field_text(field, v)
        )));
    }
    Ok(Action::Arith(if op == ops::INC { Op::Add } else { Op::Mul }))
}

pub(super) fn rename(path: &UpdatePath, v: RawValue<'_>) -> Result<Action> {
    let from = path.text.as_str();
    let RawValue::String(to) = v else {
        return Err(bad_value(format!(
            "The 'to' field for $rename must be a string: {}",
            field_text(from, v).trim_matches(['{', '}'])
        )));
    };
    if path.dynamic() {
        return Err(bad_value(format!("The source field for $rename may not be dynamic: {from}")));
    }
    let target = UpdatePath::parse(to)?;
    if target.dynamic() {
        return Err(bad_value(format!(
            "The destination field for $rename may not be dynamic: {to}"
        )));
    }
    let shown = field_text(from, v);
    let shown = shown.trim_matches(['{', '}']);
    if from == to {
        return Err(bad_value(format!(
            "The source and target field for $rename must differ: {shown}"
        )));
    }
    if is_prefix(from, to) || is_prefix(to, from) {
        return Err(bad_value(format!(
            "The source and target field for $rename must not be on the same path: {shown}"
        )));
    }
    Ok(Action::Rename(target))
}

pub(super) fn current_date(v: RawValue<'_>) -> Result<Action> {
    match v {
        RawValue::Boolean(_) => Ok(Action::CurrentDate { timestamp: false }),
        RawValue::Document(d) => {
            let mut timestamp = None;
            for (k, t) in d.iter() {
                if k != ops::TYPE {
                    return Err(bad_value(format!("Unrecognized $currentDate option: {k}")));
                }
                timestamp = Some(match t {
                    RawValue::String(types::DATE) => false,
                    RawValue::String(types::TIMESTAMP) => true,
                    _ => {
                        return Err(bad_value(
                            "The '$type' string field is required to be 'date' or 'timestamp': {$currentDate: {field : {$type: 'date'}}}",
                        ));
                    }
                });
            }
            match timestamp {
                Some(timestamp) => Ok(Action::CurrentDate { timestamp }),
                None => Err(bad_value(
                    "The '$type' string field is required to be 'date' or 'timestamp': {$currentDate: {field : {$type: 'date'}}}",
                )),
            }
        }
        v => Err(bad_value(format!(
            "{} is not valid type for $currentDate. Please use a boolean ('true') or a $type expression ({{$type: 'timestamp/date'}}).",
            types::name_of(v.element_type())
        ))),
    }
}

/// Names of the `$bit` operations.
const BIT_AND: &str = "and";
const BIT_OR: &str = "or";
const BIT_XOR: &str = "xor";

pub(super) fn bit(v: RawValue<'_>) -> Result<Action> {
    let RawValue::Document(d) = v else {
        return Err(bad_value(format!(
            "The $bit modifier is not compatible with a {}. You must pass in an embedded document: {{$bit: {{field: {{and/or/xor: #}}}}",
            types::name_of(v.element_type())
        )));
    };
    let mut list = Vec::new();
    for (k, operand) in d.iter() {
        let op = match k {
            BIT_AND => BitOp::And,
            BIT_OR => BitOp::Or,
            BIT_XOR => BitOp::Xor,
            _ => {
                return Err(bad_value(format!(
                    "The $bit modifier only supports 'and', 'or', and 'xor', not '{k}' which is an unknown operator: {}",
                    field_text(k, operand)
                )));
            }
        };
        if !matches!(operand, RawValue::Int32(_) | RawValue::Int64(_)) {
            return Err(bad_value(format!(
                "The $bit modifier field must be an Integer(32/64 bit); a '{}' is not supported here: {}",
                types::name_of(operand.element_type()),
                field_text(k, operand)
            )));
        }
        list.push((op, Value::from(operand)));
    }
    if list.is_empty() {
        return Err(bad_value(
            "You must pass in at least one bitwise operation. The format is: {$bit: {field: {and/or/xor: #}}",
        ));
    }
    Ok(Action::Bit(list))
}

/// `{_id: <id>}` of the document being updated, as messages show it.
fn id_text(original: RawDoc<'_>) -> String {
    let d = match original.get(ops::ID) {
        Some(id) => Document::new().with(ops::ID, Value::from(id)),
        None => Document::new(),
    };
    OwnedDoc::new(&d).map(|o| shell_doc(o.get())).unwrap_or_default()
}

/// Applies `$inc`, `$mul`, `$min`, `$max`, `$currentDate` or `$bit` at `slot`.
pub(super) fn apply(
    e: &Entry,
    mut slot: Slot<'_>,
    ctx: &UpdateContext<'_>,
    original: RawDoc<'_>,
) -> Result<()> {
    let arg = e.arg.to_value();
    let current = slot.get().map(MVal::to_value);
    let name = slot.key_text();
    let new = match &e.action {
        Action::Arith(op) => match current {
            None if *op == Op::Mul => arith::zero_like(&arg),
            None => arg,
            Some(cur) => {
                if !matches!(
                    cur,
                    Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_)
                ) {
                    return Err(type_mismatch(format!(
                        "Cannot apply {} to a value of non-numeric type. {} has the field '{name}' of non-numeric type {}",
                        e.op,
                        id_text(original),
                        types::name_of(cur.element_type())
                    )));
                }
                arith::apply(*op, &cur, &arg).ok_or_else(|| {
                    bad_value(format!(
                        "Failed to apply {} operations to current value ({}) for document {}",
                        e.op,
                        typed(&cur),
                        id_text(original)
                    ))
                })?
            }
        },
        Action::MinMax(want) => match current {
            None => arg,
            Some(cur) => {
                let (a, c) = (e.arg.get(), MVal::from_value(&cur).owned()?);
                if cmp(a, c.get(), ctx.matcher.collation) == *want { arg } else { return Ok(()) }
            }
        },
        Action::CurrentDate { timestamp } => {
            if *timestamp {
                Value::Timestamp { time: ctx.now.timestamp.0, increment: ctx.now.timestamp.1 }
            } else {
                Value::DateTime(ctx.now.millis)
            }
        }
        Action::Bit(list) => {
            let mut acc = match current {
                None => Value::Int32(0),
                Some(v @ (Value::Int32(_) | Value::Int64(_))) => v,
                Some(other) => {
                    // Measured on a `refrouter` 9.0.4, words and spacing as they are.
                    return Err(bad_value(format!(
                        "Cannot apply $bit to a value of non-integral type.{} has the field {name} of non-integer type {}",
                        id_text(original).trim_matches(['{', '}']),
                        types::name_of(other.element_type())
                    )));
                }
            };
            for (op, operand) in list {
                acc = bitwise(*op, &acc, operand);
            }
            acc
        }
        _ => return Ok(()),
    };
    slot.set(MVal::from_value(&new))
}

fn bitwise(op: BitOp, a: &Value, b: &Value) -> Value {
    let f = |x: i64, y: i64| match op {
        BitOp::And => x & y,
        BitOp::Or => x | y,
        BitOp::Xor => x ^ y,
    };
    match (a, b) {
        (Value::Int32(x), Value::Int32(y)) => Value::Int32(f(i64::from(*x), i64::from(*y)) as i32),
        _ => Value::Int64(f(as_i64(a), as_i64(b))),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int32(n) => i64::from(*n),
        Value::Int64(n) => *n,
        _ => 0,
    }
}

/// `(NumberLong)9223372036854775807`, as RefDb shows a number with its type.
fn typed(v: &Value) -> String {
    match v {
        Value::Int32(n) => format!("(NumberInt){n}"),
        Value::Int64(n) => format!("(NumberLong){n}"),
        Value::Double(x) => format!("(NumberDouble){x}"),
        Value::Decimal128(d) => format!("(NumberDecimal){d}"),
        other => format!("{other:?}"),
    }
}

/// `$rename`: moves the value of `from` to `to`; neither may go through an array.
pub(super) fn apply_rename(root: &mut MVal, from: &UpdatePath, to: &UpdatePath) -> Result<()> {
    let names = |p: &UpdatePath| -> Vec<String> { p.text.split('.').map(str::to_owned).collect() };
    let (src, dst) = (names(from), names(to));
    let id = id_element(root);
    let Some(value) = take(root, &src, &from.text, &id)? else { return Ok(()) };
    check_no_array(root, &dst, &to.text, &id)?;
    put(root, &dst, value)
}

/// Removes and returns the value at `path`, refusing a path through an array.
fn take(root: &mut MVal, path: &[String], text: &str, id: &str) -> Result<Option<MVal>> {
    let mut cur = root;
    let mut top: Option<(String, MVal)> = None;
    for (i, seg) in path.iter().enumerate() {
        let MVal::Doc(d) = cur else {
            if let (MVal::Arr(_), Some((name, array))) = (&*cur, &top) {
                // A position goes into the array; a field name cannot (measured).
                if crate::matcher::array_index(seg).is_some() {
                    return Err(array_element("source", text, name, id));
                }
                let shown = MDoc { fields: vec![(name.clone(), array.clone())] };
                let element = match crate::value::OwnedDoc::new(&shown.to_document()) {
                    Ok(d) => shell_doc(d.get()),
                    Err(e) => e.message,
                };
                return Err(MqlError::new(
                    codes::PATH_NOT_VIABLE,
                    format!(
                        "cannot use the part ({name} of {text}) to traverse the element ({element})"
                    ),
                ));
            }
            return Ok(None);
        };
        if i + 1 == path.len() {
            return Ok(d.remove(seg));
        }
        let Some(next) = d.get_mut(seg) else { return Ok(None) };
        if top.is_none() || matches!(next, MVal::Arr(_)) {
            top = Some((seg.clone(), next.clone()));
        }
        cur = next;
    }
    Ok(None)
}

/// The `_id` of the document as RefDb names it in `$rename`'s messages (`_id: 1`).
fn id_element(root: &MVal) -> String {
    let MVal::Doc(d) = root else { return NO_ID.to_owned() };
    let Some(id) = d.get(ops::ID) else { return NO_ID.to_owned() };
    OwnedDoc::new(&MDoc { fields: vec![(ops::ID.to_owned(), id.clone())] }.to_document())
        .ok()
        .and_then(|o| o.get().get(ops::ID).map(|v| format!("{}: {}", ops::ID, shell(v))))
        .unwrap_or_else(|| NO_ID.to_owned())
}

/// What `$rename`'s messages say of a document without `_id`.
const NO_ID: &str = "no id";

fn array_element(which: &str, text: &str, name: &str, id: &str) -> MqlError {
    bad_value(format!(
        "The {which} field cannot be an array element, '{text}' in doc with {id} has an array field called '{name}'"
    ))
}

fn check_no_array(root: &MVal, path: &[String], text: &str, id: &str) -> Result<()> {
    let mut cur = root;
    for seg in &path[..path.len().saturating_sub(1)] {
        let MVal::Doc(d) = cur else { return Ok(()) };
        match d.get(seg) {
            Some(MVal::Arr(_)) => return Err(array_element("destination", text, seg, id)),
            Some(v) => cur = v,
            None => return Ok(()),
        }
    }
    Ok(())
}

/// Sets `path` to `value`, creating documents on the way.
fn put(root: &mut MVal, path: &[String], value: MVal) -> Result<()> {
    let mut cur = root;
    let mut parent_name = String::new();
    for (i, seg) in path.iter().enumerate() {
        let MVal::Doc(d) = cur else { return Err(not_viable(seg, &parent_name, cur)) };
        if i + 1 == path.len() {
            d.set(seg, value);
            return Ok(());
        }
        if d.get(seg).is_none() {
            d.set(seg, MVal::Doc(MDoc::default()));
        }
        parent_name = seg.clone();
        let Some(next) = d.get_mut(seg) else { return Ok(()) };
        cur = next;
    }
    Ok(())
}
