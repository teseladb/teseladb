//! The stages that reshape each document (measured on a `refrouter` 9.0.4): `$project`,
//! `$addFields` and `$set`, `$unset`, and `$replaceRoot` and `$replaceWith`.
//!
//! A `$project` is an inclusion (fields to keep and computed fields) or an exclusion (fields to
//! drop); `_id` stays unless excluded. Kept fields come in the document's order and computed ones
//! after them, in the specification's order; a path through an array applies to each element
//! that is a document (an inclusion drops the others). `$addFields` evaluates every expression on
//! the document as it came, then sets each path, turning what is in the way into documents
//! (each element of an array).

use teseladb_bson::{Decimal, Document, RawValue, Value};
use teseladb_mql::MqlError;

use super::errors::{
    NEW_ROOT, PROJECT_COLLISION, PROJECT_EMPTY, PROJECT_EMPTY_SUB, PROJECT_EXCLUSION_IN_INCLUSION,
    PROJECT_INCLUSION_IN_EXCLUSION, PROJECT_NOT_OBJECT, UNKNOWN_EXPRESSION, UNSET_EMPTY,
    UNSET_TYPE, error,
};
use super::sort::check_path;
use crate::expr::{Ctx, Eval, Expr, Scope};

/// What a projection does with one field.
#[derive(Clone, Debug)]
enum Item {
    Include,
    Exclude,
    Compute(Expr),
    Sub(Tree),
}

/// The fields of a projection at one level, in the specification's order.
#[derive(Clone, Debug, Default)]
struct Tree {
    fields: Vec<(String, Item)>,
}

impl Tree {
    fn get(&self, name: &str) -> Option<&Item> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, i)| i)
    }

    /// Puts `item` at the dotted `path` (`full` is the whole path, for messages).
    fn put(&mut self, path: &str, full: &str, item: Item) -> Result<(), MqlError> {
        let (head, rest) = match path.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (path, None),
        };
        let slot = self.fields.iter().position(|(n, _)| n == head);
        match (rest, slot) {
            (None, None) => self.fields.push((head.to_owned(), item)),
            (None, Some(_)) => {
                return Err(error(PROJECT_COLLISION, format!("Path collision at {full}")));
            }
            (Some(r), None) => {
                let mut sub = Tree::default();
                sub.put(r, full, item)?;
                self.fields.push((head.to_owned(), Item::Sub(sub)));
            }
            (Some(r), Some(i)) => match &mut self.fields[i].1 {
                Item::Sub(sub) => sub.put(r, full, item)?,
                _ => {
                    return Err(error(
                        PROJECT_COLLISION,
                        format!("Path collision at {full} remaining portion {r}"),
                    ));
                }
            },
        }
        Ok(())
    }
}

/// A `$project` (or `$unset`).
#[derive(Clone, Debug)]
pub struct Projection {
    inclusion: bool,
    tree: Tree,
    /// Whether `_id` stays (an inclusion keeps it unless `_id: 0`).
    keep_id: bool,
}

/// Whether a projection value keeps the field (`true`, a non-zero number).
fn flag(v: RawValue<'_>) -> Option<bool> {
    match v {
        RawValue::Boolean(b) => Some(b),
        RawValue::Int32(n) => Some(n != 0),
        RawValue::Int64(n) => Some(n != 0),
        RawValue::Double(x) => Some(x != 0.0),
        RawValue::Decimal128(d) => {
            Some(!matches!(d.classify(), Decimal::Finite { coefficient: 0, .. }))
        }
        _ => None,
    }
}

impl Projection {
    /// Reads the specification of a `$project`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Projection, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(PROJECT_NOT_OBJECT, "$project specification must be an object"));
        };
        let mut p = Projection { inclusion: true, tree: Tree::default(), keep_id: true };
        let mut mode: Option<bool> = None;
        let mut empty = true;
        p.read(d, "", ctx, &mut mode, &mut empty)?;
        if empty {
            return Err(error(
                PROJECT_EMPTY,
                "projection specification must have at least one field",
            ));
        }
        // `_id` alone decides: `{_id: 0}` is an exclusion that drops only `_id`, `{_id: 1}` an
        // inclusion that keeps only `_id`.
        p.inclusion = mode.unwrap_or(p.keep_id);
        Ok(p)
    }

    /// Reads the fields of `d` under `prefix`.
    fn read(
        &mut self,
        d: teseladb_bson::RawDoc<'_>,
        prefix: &str,
        ctx: &Ctx,
        mode: &mut Option<bool>,
        empty: &mut bool,
    ) -> Result<(), MqlError> {
        for (k, v) in d.iter() {
            *empty = false;
            if k.ends_with(".$") {
                return Err(error(
                    super::errors::PROJECT_POSITIONAL,
                    "Cannot use positional projection in aggregation projection",
                ));
            }
            check_path(k)?;
            let full = if prefix.is_empty() { k.to_owned() } else { format!("{prefix}.{k}") };
            let item = match (flag(v), v) {
                (Some(keep), _) => {
                    if full == "_id" {
                        self.keep_id = keep;
                        if !keep {
                            continue;
                        }
                        Item::Include
                    } else {
                        set_mode(mode, keep, k)?;
                        if keep { Item::Include } else { Item::Exclude }
                    }
                }
                (None, RawValue::Document(sub)) => {
                    let first = sub.iter().next().map(|(n, _)| n);
                    match first {
                        None => {
                            return Err(error(
                                PROJECT_EMPTY_SUB,
                                format!("Invalid empty sub-projection: {full}"),
                            ));
                        }
                        Some(n) if n.starts_with('$') => {
                            set_mode(mode, true, k)?;
                            Item::Compute(projected_expr(v, ctx)?)
                        }
                        Some(_) => {
                            self.read(sub, &full, ctx, mode, empty)?;
                            continue;
                        }
                    }
                }
                (None, other) => {
                    set_mode(mode, true, k)?;
                    Item::Compute(projected_expr(other, ctx)?)
                }
            };
            self.tree.put(&full, &full, item)?;
        }
        Ok(())
    }

    /// An exclusion of `paths` (`$unset`).
    pub fn unset(v: RawValue<'_>) -> Result<Projection, MqlError> {
        let paths: Vec<&str> = match v {
            RawValue::String(s) => vec![s],
            RawValue::Array(a) => {
                let mut out = Vec::new();
                for x in a.iter() {
                    match x {
                        RawValue::String(s) => out.push(s),
                        _ => return Err(unset_type()),
                    }
                }
                if out.is_empty() {
                    return Err(error(
                        UNSET_EMPTY,
                        "$unset specification must be a string or an array with at least one field",
                    ));
                }
                out
            }
            _ => return Err(unset_type()),
        };
        let mut p = Projection { inclusion: false, tree: Tree::default(), keep_id: true };
        for path in paths {
            check_path(path)?;
            if path == "_id" {
                p.keep_id = false;
                continue;
            }
            p.tree.put(path, path, Item::Exclude)?;
        }
        Ok(p)
    }

    /// `doc` projected.
    pub fn apply(&self, doc: &Document, scope: &Scope) -> Result<Document, MqlError> {
        if self.inclusion {
            let mut out = Document::new();
            if self.keep_id
                && self.tree.get("_id").is_none()
                && let Some(id) = doc.get("_id")
            {
                out.push("_id", id.clone());
            }
            include_into(&self.tree, Some(&Value::Document(doc.clone())), doc, scope, &mut out)?;
            return Ok(out);
        }
        let mut out = exclude(&self.tree, doc);
        if !self.keep_id {
            out.remove("_id");
        }
        Ok(out)
    }
}

/// An expression of a projection: an unknown operator is 31325 there (168 elsewhere).
pub fn projected_expr(v: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
    Expr::parse_in(v, ctx).map_err(|e| match e.message.strip_prefix("Unrecognized expression '") {
        Some(rest) if e.code.code == 168 => {
            error(UNKNOWN_EXPRESSION, format!("Unknown expression {}", rest.trim_end_matches('\'')))
        }
        _ => e,
    })
}

fn unset_type() -> MqlError {
    error(UNSET_TYPE, "$unset specification must be a string or an array")
}

/// Records whether the projection includes (`keep`), refusing a change of mind; the error names
/// the field as written in its document, without the path of a nested one (measured).
fn set_mode(mode: &mut Option<bool>, keep: bool, field: &str) -> Result<(), MqlError> {
    match *mode {
        None => *mode = Some(keep),
        Some(true) if !keep => {
            return Err(error(
                PROJECT_EXCLUSION_IN_INCLUSION,
                format!("Cannot do exclusion on field {field} in inclusion projection"),
            ));
        }
        Some(false) if keep => {
            return Err(error(
                PROJECT_INCLUSION_IN_EXCLUSION,
                format!("Cannot do inclusion on field {field} in exclusion projection"),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Adds to `out` the fields `tree` keeps of `value` (a document) and its computed fields.
fn include_into(
    tree: &Tree,
    value: Option<&Value>,
    root: &Document,
    scope: &Scope,
    out: &mut Document,
) -> Result<(), MqlError> {
    if let Some(Value::Document(d)) = value {
        for (k, v) in d.iter() {
            match tree.get(k) {
                Some(Item::Include) => out.push(k, v.clone()),
                Some(Item::Sub(sub)) => {
                    if let Some(x) = include_value(sub, v, root, scope)? {
                        out.push(k, x);
                    }
                }
                _ => {}
            }
        }
    }
    for (k, item) in &tree.fields {
        match item {
            Item::Compute(e) => {
                if let Some(v) = e.eval(root, scope)? {
                    out.insert(k.clone(), v);
                }
            }
            Item::Sub(sub) if has_compute(sub) => {
                let present = matches!(value, Some(Value::Document(d)) if d.get(k).is_some_and(|x| matches!(x, Value::Document(_) | Value::Array(_))));
                if !present {
                    let mut inner = Document::new();
                    include_into(sub, None, root, scope, &mut inner)?;
                    out.insert(k.clone(), Value::Document(inner));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// What a sub-projection keeps of value `v`: a document's fields, each document of an array.
fn include_value(tree: &Tree, v: &Value, root: &Document, scope: &Scope) -> Result<Eval, MqlError> {
    match v {
        Value::Document(_) => {
            let mut inner = Document::new();
            include_into(tree, Some(v), root, scope, &mut inner)?;
            Ok(Some(Value::Document(inner)))
        }
        Value::Array(items) => {
            let mut out = Vec::new();
            for x in items {
                if let Some(y) = include_value(tree, x, root, scope)? {
                    out.push(y);
                }
            }
            Ok(Some(Value::Array(out)))
        }
        _ if has_compute(tree) => {
            let mut inner = Document::new();
            include_into(tree, None, root, scope, &mut inner)?;
            Ok(Some(Value::Document(inner)))
        }
        _ => Ok(None),
    }
}

fn has_compute(tree: &Tree) -> bool {
    tree.fields.iter().any(|(_, i)| match i {
        Item::Compute(_) => true,
        Item::Sub(s) => has_compute(s),
        _ => false,
    })
}

/// `doc` without the fields `tree` excludes.
fn exclude(tree: &Tree, doc: &Document) -> Document {
    let mut out = Document::new();
    for (k, v) in doc.iter() {
        match tree.get(k) {
            Some(Item::Exclude) => {}
            Some(Item::Sub(sub)) => out.push(k, exclude_value(sub, v)),
            _ => out.push(k, v.clone()),
        }
    }
    out
}

fn exclude_value(tree: &Tree, v: &Value) -> Value {
    match v {
        Value::Document(d) => Value::Document(exclude(tree, d)),
        Value::Array(items) => Value::Array(items.iter().map(|x| exclude_value(tree, x)).collect()),
        other => other.clone(),
    }
}

/// `$addFields` and `$set`: paths and their expressions.
#[derive(Clone, Debug)]
pub struct AddFields {
    fields: Vec<(String, Expr)>,
}

impl AddFields {
    /// Reads the specification of `$addFields` or `$set` (`name`).
    pub fn parse(name: &str, v: RawValue<'_>, ctx: &Ctx) -> Result<AddFields, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                super::errors::ADD_FIELDS_NOT_OBJECT,
                format!(
                    "{name} specification stage must be an object, got {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let mut fields = Vec::new();
        let mut tree = Tree::default();
        for (k, x) in d.iter() {
            check_path(k)?;
            add_field(k, x, (d, ctx), &mut tree, &mut fields)?;
        }
        Ok(AddFields { fields })
    }

    /// `doc` with the fields set.
    pub fn apply(&self, doc: &Document, scope: &Scope) -> Result<Document, MqlError> {
        let values: Vec<Eval> =
            self.fields.iter().map(|(_, e)| e.eval(doc, scope)).collect::<Result<_, _>>()?;
        let mut out = doc.clone();
        for ((path, _), v) in self.fields.iter().zip(values) {
            let segs: Vec<&str> = path.split('.').collect();
            set_path(&mut out, &segs, v);
        }
        Ok(out)
    }
}

/// One field of `$addFields` at `path`: a document that is not an expression (its first field
/// does not start with `$`) adds each of its fields inside the field's document, or inside each
/// element of its array, as a dotted path does; any other value is an expression. An empty
/// document is the value `{}` (measured on a `refnode` 9.0.4 on 04-11-2026).
fn add_field(
    path: &str,
    x: RawValue<'_>,
    (spec, ctx): (teseladb_bson::RawDoc<'_>, &Ctx),
    tree: &mut Tree,
    fields: &mut Vec<(String, Expr)>,
) -> Result<(), MqlError> {
    let nested = match x {
        RawValue::Document(sub) => match sub.iter().next() {
            Some((first, _)) if !first.starts_with('$') => Some(sub),
            _ => None,
        },
        _ => None,
    };
    let Some(sub) = nested else {
        tree.put(path, path, Item::Include)?;
        fields.push((path.to_owned(), Expr::parse_in(x, ctx)?));
        return Ok(());
    };
    for (k, y) in sub.iter() {
        if k.contains('.') {
            let msg = format!(
                "cannot use dotted field name '{k}' in a sub object: {}",
                teseladb_bson::shell::shell_doc(spec)
            );
            return Err(error(super::errors::ADD_FIELDS_DOTTED_SUB_OBJECT, msg));
        }
        check_path(k)?;
        add_field(&format!("{path}.{k}"), y, (spec, ctx), tree, fields)?;
    }
    Ok(())
}

/// Sets `v` at `segs` of `doc`.
pub(super) fn set_path_value(doc: &mut Document, segs: &[&str], v: Value) {
    set_path(doc, segs, Some(v));
}

/// Sets (or, with `None`, removes) the field at `segs` of `doc`.
fn set_path(doc: &mut Document, segs: &[&str], v: Eval) {
    let Some((first, rest)) = segs.split_first() else { return };
    if rest.is_empty() {
        match v {
            Some(v) => {
                doc.insert(*first, v);
            }
            None => {
                doc.remove(first);
            }
        }
        return;
    }
    let child = doc.get(first).cloned();
    let new = match child {
        Some(Value::Document(mut d)) => {
            set_path(&mut d, rest, v);
            Value::Document(d)
        }
        Some(Value::Array(items)) => {
            Value::Array(items.into_iter().map(|x| set_in(x, rest, &v)).collect())
        }
        _ => {
            let mut d = Document::new();
            set_path(&mut d, rest, v);
            Value::Document(d)
        }
    };
    doc.insert(*first, new);
}

/// An element of an array with the path set in it.
fn set_in(x: Value, segs: &[&str], v: &Eval) -> Value {
    match x {
        Value::Document(mut d) => {
            set_path(&mut d, segs, v.clone());
            Value::Document(d)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|y| set_in(y, segs, v)).collect())
        }
        _ => {
            let mut d = Document::new();
            set_path(&mut d, segs, v.clone());
            Value::Document(d)
        }
    }
}

/// `$replaceRoot` and `$replaceWith`: the expression of the new document.
#[derive(Clone, Debug)]
pub struct ReplaceRoot {
    root: Expr,
    /// The top-level fields the expression reads (all of them when `None`), for the message.
    reads: Option<Vec<String>>,
}

impl ReplaceRoot {
    /// Reads `$replaceRoot` (`with` false: `{newRoot: e}`) or `$replaceWith` (`e`).
    pub fn parse(v: RawValue<'_>, with: bool, ctx: &Ctx) -> Result<ReplaceRoot, MqlError> {
        let e = if with {
            v
        } else {
            let RawValue::Document(d) = v else {
                return Err(error(
                    super::errors::REPLACE_ROOT_NOT_OBJECT,
                    format!(
                        "expected an object as specification for $replaceRoot stage, got {}",
                        crate::expr::arith::raw_type_name(v)
                    ),
                ));
            };
            let mut root = None;
            for (k, x) in d.iter() {
                if k == "newRoot" {
                    root = Some(x);
                } else {
                    return Err(error(
                        super::errors::UNKNOWN_FIELD,
                        format!("BSON field '$replaceRoot.{k}' is an unknown field."),
                    ));
                }
            }
            root.ok_or_else(|| {
                error(
                    super::errors::MISSING_FIELD,
                    "BSON field '$replaceRoot.newRoot' is missing but a required field",
                )
            })?
        };
        let mut reads = Some(Vec::new());
        reads_of(e, &mut reads);
        Ok(ReplaceRoot { root: Expr::parse_in(e, ctx)?, reads })
    }

    /// The new document of `doc`.
    pub fn apply(&self, doc: &Document, scope: &Scope) -> Result<Document, MqlError> {
        match self.root.eval(doc, scope)? {
            Some(Value::Document(d)) => Ok(d),
            other => {
                let (shown, kind) = match &other {
                    None => ("MISSING".to_owned(), "missing"),
                    Some(v) => {
                        (crate::expr::values::display(v), crate::expr::arith::type_name_of(v))
                    }
                };
                let input = match &self.reads {
                    None => doc.clone(),
                    Some(names) => {
                        let mut d = Document::new();
                        for (k, v) in doc.iter() {
                            if names.iter().any(|n| n == k) {
                                d.push(k, v.clone());
                            }
                        }
                        d
                    }
                };
                Err(error(
                    NEW_ROOT,
                    format!(
                        "'newRoot' expression  must evaluate to an object, but resulting value was: {shown}. Type of resulting value: '{kind}'. Input document: {}",
                        crate::expr::values::display(&Value::Document(input))
                    ),
                ))
            }
        }
    }
}

/// The top-level fields an expression reads, `None` once it reads the whole document.
fn reads_of(v: RawValue<'_>, out: &mut Option<Vec<String>>) {
    match v {
        RawValue::String(s) => {
            if let Some(var) = s.strip_prefix("$$") {
                let name = var.split('.').next().unwrap_or("");
                if name == "ROOT" || name == "CURRENT" {
                    *out = None;
                }
            } else if let Some(path) = s.strip_prefix('$') {
                let head = path.split('.').next().unwrap_or("").to_owned();
                if let Some(list) = out
                    && !list.contains(&head)
                {
                    list.push(head);
                }
            }
        }
        RawValue::Document(d) => d.iter().for_each(|(_, x)| reads_of(x, out)),
        RawValue::Array(a) => a.iter().for_each(|x| reads_of(x, out)),
        _ => {}
    }
}
