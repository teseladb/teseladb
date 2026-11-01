//! Projections of `find` and `findAndModify` (FR-011): inclusion and exclusion with `_id`, dotted
//! paths that cross arrays of documents, the positional `$`, `$elemMatch`, `$slice`, `$meta` and
//! computed fields, whose expressions go to [`crate::Extensions::eval`] (FR-032).

use teseladb_bson::{Document, RawArray, RawDoc, RawValue, Value};

use crate::error::{ErrorCode, MqlError, Result, bad_value, codes};
use crate::matcher::{Eval, MatchContext};
use crate::ops;
use crate::parse::{self, Expr, FilterUse, Pred};
use crate::value::{OwnedValue, is_number, truncate_i64, truthy};

/// `$meta` names RefDb accepts.
pub const META_NAMES: [&str; 14] = [
    "textScore",
    "indexKey",
    "recordId",
    "randVal",
    "sortKey",
    "searchScore",
    "searchHighlights",
    "searchScoreDetails",
    "searchSequenceToken",
    "vectorSearchScore",
    "score",
    "scoreDetails",
    "geoNearDistance",
    "geoNearPoint",
];

/// The `$meta` of the values the document was sorted by.
const SORT_KEY: &str = "sortKey";

/// Codes of the projection errors.
mod code {
    use crate::error::ErrorCode;

    pub const PATH_COLLISION_REMAINING: ErrorCode =
        ErrorCode { code: 31249, name: "Location31249" };
    pub const MORE_THAN_ONE_POSITIONAL: ErrorCode =
        ErrorCode { code: 31276, name: "Location31276" };
    pub const POSITIONAL_IN_THE_MIDDLE: ErrorCode =
        ErrorCode { code: 31394, name: "Location31394" };
    pub const POSITIONAL_EXCLUSION: ErrorCode = ErrorCode { code: 31395, name: "Location31395" };
    pub const NESTED_ELEM_MATCH: ErrorCode = ErrorCode { code: 31275, name: "Location31275" };
    pub const EMPTY_SUB_PROJECTION: ErrorCode = ErrorCode { code: 51270, name: "Location51270" };
    pub const DOLLAR_FIELD_PATH: ErrorCode = ErrorCode { code: 16410, name: "Location16410" };
    pub const EMPTY_FIELD_PATH: ErrorCode = ErrorCode { code: 15998, name: "Location15998" };
    pub const EMPTY_PATH: ErrorCode = ErrorCode { code: 40352, name: "Location40352" };
    pub const PATH_ENDS_WITH_DOT: ErrorCode = ErrorCode { code: 40353, name: "Location40353" };
    pub const ELEM_MATCH_NOT_OBJECT: ErrorCode = ErrorCode { code: 31274, name: "Location31274" };
    pub const POSITIONAL_THEN_ELEM_MATCH: ErrorCode =
        ErrorCode { code: 31255, name: "Location31255" };
    pub const ELEM_MATCH_THEN_POSITIONAL: ErrorCode =
        ErrorCode { code: 31256, name: "Location31256" };
    pub const POSITIONAL_SUB_OBJECT: ErrorCode = ErrorCode { code: 31271, name: "Location31271" };
    pub const POSITIONAL_LITERAL: ErrorCode = ErrorCode { code: 31308, name: "Location31308" };
    pub const SLICE_FORM: ErrorCode = ErrorCode { code: 31272, name: "Location31272" };
    pub const SLICE_TYPE: ErrorCode = ErrorCode { code: 31273, name: "Location31273" };
    pub const SLICE_SYNTAX: ErrorCode = ErrorCode { code: 28667, name: "Location28667" };
    pub const META_NOT_STRING: ErrorCode = ErrorCode { code: 17307, name: "Location17307" };
    pub const META_UNKNOWN: ErrorCode = ErrorCode { code: 17308, name: "Location17308" };
    pub const META_UNAVAILABLE: ErrorCode = ErrorCode { code: 40218, name: "Location40218" };
    /// `$` without a position of the filter.
    pub const POSITIONAL_NO_MATCH: ErrorCode = ErrorCode { code: 51246, name: "Location51246" };
    /// `$` with a position past the end of the array.
    pub const POSITIONAL_MISMATCH: ErrorCode = ErrorCode { code: 51247, name: "Location51247" };
}

pub use code::{POSITIONAL_MISMATCH, POSITIONAL_NO_MATCH};

/// What evaluation of a projection needs.
#[derive(Clone, Copy)]
pub struct ProjectionContext<'a> {
    /// Collation and extensions.
    pub matcher: MatchContext<'a>,
    /// Position in the array the filter matched, for `$` ([`crate::Filter::match_position`]).
    pub position: Option<usize>,
    /// The values the document was sorted by, for `{$meta: "sortKey"}`.
    pub sort_key: Option<&'a [Value]>,
}

/// A parsed projection.
#[derive(Debug)]
pub struct Projection {
    tree: Tree,
    inclusion: bool,
    positional: bool,
    metas: Vec<String>,
}

#[derive(Debug, Default)]
struct Tree {
    fields: Vec<(String, Node)>,
}

#[derive(Debug)]
enum Node {
    Leaf(Action),
    Sub(Tree),
}

#[derive(Debug)]
enum Action {
    Include,
    Exclude,
    Slice { skip: Option<i64>, limit: i64 },
    ElemMatch(Pred),
    Meta(String),
    Computed(OwnedValue),
    Positional,
}

impl Tree {
    fn get(&self, name: &str) -> Option<&Node> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, n)| n)
    }

    /// Adds `action` at `path`, refusing collisions.
    fn insert(&mut self, path: &[&str], action: Action, full: &str) -> Result<()> {
        let (first, rest) = match path.split_first() {
            Some(x) => x,
            None => return Ok(()),
        };
        let pos = self.fields.iter().position(|(k, _)| k == first);
        if rest.is_empty() {
            if pos.is_some() {
                return Err(MqlError::new(
                    codes::PROJECTION_PATH_COLLISION,
                    format!("Path collision at {full}"),
                ));
            }
            self.fields.push(((*first).to_owned(), Node::Leaf(action)));
            return Ok(());
        }
        let i = match pos {
            Some(i) => i,
            None => {
                self.fields.push(((*first).to_owned(), Node::Sub(Tree::default())));
                self.fields.len() - 1
            }
        };
        match &mut self.fields[i].1 {
            Node::Sub(sub) => sub.insert(rest, action, full),
            Node::Leaf(_) => Err(MqlError::new(
                code::PATH_COLLISION_REMAINING,
                format!("Path collision at {full} remaining portion {}", rest.join(".")),
            )),
        }
    }
}

fn err(code: ErrorCode, message: impl Into<String>) -> MqlError {
    MqlError::new(code, message)
}

/// The prefix of the messages about a field path a projection names.
const FIELD_PATH_HINT: &str =
    "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: ";

/// The segments of a projection path, checked as RefDb's `FieldPath` checks them.
fn segments(path: &str) -> Result<Vec<&str>> {
    if path.is_empty() {
        return Err(err(code::EMPTY_PATH, "FieldPath cannot be constructed with empty string"));
    }
    if path.ends_with('.') {
        return Err(err(code::PATH_ENDS_WITH_DOT, "FieldPath must not end with a '.'."));
    }
    let segs: Vec<&str> = path.split('.').collect();
    for s in &segs {
        if s.is_empty() {
            let msg = format!("{FIELD_PATH_HINT}FieldPath field names may not be empty strings.");
            return Err(err(code::EMPTY_FIELD_PATH, msg));
        }
        if s.starts_with('$') {
            let msg = format!(
                "{FIELD_PATH_HINT}FieldPath field names may not start with '$', given '{s}'."
            );
            return Err(err(code::DOLLAR_FIELD_PATH, msg));
        }
    }
    Ok(segs)
}

struct Builder {
    tree: Tree,
    /// `Some(true)` inclusion, `Some(false)` exclusion.
    mode: Option<bool>,
    positional: bool,
    elem_match: bool,
    slice: bool,
    metas: Vec<String>,
}

impl Builder {
    fn mode(&mut self, inclusion: bool, path: &str) -> Result<()> {
        match self.mode {
            Some(m) if m != inclusion => Err(if m {
                err(
                    codes::PROJECTION_EXCLUSION_IN_INCLUSION,
                    format!("Cannot do exclusion on field {path} in inclusion projection"),
                )
            } else {
                err(
                    codes::PROJECTION_INCLUSION_IN_EXCLUSION,
                    format!("Cannot do inclusion on field {path} in exclusion projection"),
                )
            }),
            _ => {
                self.mode = Some(inclusion);
                Ok(())
            }
        }
    }

    /// `path: value`; `name` is the field as it is written at its level, for the messages.
    fn field(&mut self, path: &str, name: &str, value: RawValue<'_>) -> Result<()> {
        if let Some(base) = path.strip_suffix(".$").filter(|b| !b.is_empty()) {
            return self.positional(path, base, value);
        }
        let positional_inside = path.split('.').any(|s| s == "$") && path != "$";
        if positional_inside {
            return Err(err(
                code::POSITIONAL_IN_THE_MIDDLE,
                "As of 4.4, it's illegal to specify positional operator in the middle of a path.Positional projection may only be used at the end, for example: a.b.$. If the query previously used a form like a.b.$.d, remove the parts following the '$' and the results will be equivalent.",
            ));
        }
        let segs = segments(path)?;
        let action = match value {
            RawValue::Document(d) => match (d.iter().next(), d.iter().count()) {
                (None, _) => {
                    let msg = format!("Invalid empty sub-projection: {name}");
                    return Err(err(code::EMPTY_SUB_PROJECTION, msg));
                }
                (Some((ops::SLICE, arg)), 1) => return self.slice(path, &segs, arg),
                (Some((ops::ELEM_MATCH, arg)), 1) => {
                    if segs.len() > 1 {
                        return Err(err(
                            code::NESTED_ELEM_MATCH,
                            "Cannot use $elemMatch projection on a nested field.",
                        ));
                    }
                    if self.positional {
                        return Err(err(
                            code::POSITIONAL_THEN_ELEM_MATCH,
                            "Cannot specify positional operator and $elemMatch.",
                        ));
                    }
                    self.elem_match = true;
                    Action::ElemMatch(elem_match(path, arg)?)
                }
                (Some((ops::META, arg)), 1) => {
                    let RawValue::String(meta) = arg else {
                        return Err(err(
                            code::META_NOT_STRING,
                            "$meta only supports string arguments",
                        ));
                    };
                    if !META_NAMES.contains(&meta) {
                        let msg = format!("Unsupported $meta field: {meta}");
                        return Err(err(code::META_UNKNOWN, msg));
                    }
                    self.metas.push(meta.to_owned());
                    Action::Meta(meta.to_owned())
                }
                (Some((k, _)), 1) if k.starts_with('$') => {
                    self.mode(true, name)?;
                    Action::Computed(OwnedValue::from_raw(value)?)
                }
                _ => {
                    for (k, v) in d.iter() {
                        self.field(&format!("{path}.{k}"), k, v)?;
                    }
                    return Ok(());
                }
            },
            v @ (RawValue::Boolean(_)
            | RawValue::Int32(_)
            | RawValue::Int64(_)
            | RawValue::Double(_)
            | RawValue::Decimal128(_)) => {
                let include = truthy(&v);
                if path != ops::ID {
                    self.mode(include, name)?;
                }
                if include { Action::Include } else { Action::Exclude }
            }
            v => {
                self.mode(true, name)?;
                Action::Computed(OwnedValue::from_raw(v)?)
            }
        };
        self.tree.insert(&segs, action, path)
    }

    /// `path: {$slice: arg}`: the `find` form, or else the expression `$slice`, which 010
    /// evaluates; when neither reads it, RefDb's error names both.
    fn slice(&mut self, path: &str, segs: &[&str], arg: RawValue<'_>) -> Result<()> {
        let find = find_slice(arg).and_then(|a| self.tree.insert(segs, a, path));
        let Err(e) = find else {
            self.slice = true;
            return Ok(());
        };
        let args = match arg {
            RawValue::Array(a) => a.iter().count(),
            _ => 1,
        };
        if (2..=3).contains(&args) {
            self.mode(true, path)?;
            let expr = Document::new().with(ops::SLICE, Value::from(arg));
            let action = Action::Computed(OwnedValue::new(Value::Document(expr))?);
            return self.tree.insert(segs, action, path);
        }
        let msg = format!(
            "Invalid $slice syntax. The given syntax {{ $slice: {} }} did not match the find() syntax because :: {}: {} :: The given syntax did not match the expression $slice syntax. :: caused by :: Expression $slice takes at least 2 arguments, and at most 3, but {args} were passed in.",
            crate::fmt::shell(arg),
            e.code.name,
            e.message
        );
        Err(err(code::SLICE_SYNTAX, msg))
    }

    fn positional(&mut self, path: &str, base: &str, value: RawValue<'_>) -> Result<()> {
        if base.split('.').any(|s| s == "$") {
            return Err(err(
                code::POSITIONAL_IN_THE_MIDDLE,
                "As of 4.4, it's illegal to specify positional operator in the middle of a path.",
            ));
        }
        if let RawValue::Document(_) = value {
            return Err(err(
                code::POSITIONAL_SUB_OBJECT,
                "positional projection cannot be used with an expression or sub object",
            ));
        }
        if !matches!(value, RawValue::Boolean(_)) && !is_number(&value) {
            return Err(err(
                code::POSITIONAL_LITERAL,
                "positional projection cannot be used with a literal",
            ));
        }
        if !truthy(&value) {
            return Err(err(
                code::POSITIONAL_EXCLUSION,
                "positional projection cannot be used with exclusion",
            ));
        }
        if self.elem_match {
            return Err(err(
                code::ELEM_MATCH_THEN_POSITIONAL,
                "Cannot specify positional operator and $elemMatch.",
            ));
        }
        if self.positional {
            return Err(err(
                code::MORE_THAN_ONE_POSITIONAL,
                "Cannot specify more than one positional projection per query.",
            ));
        }
        self.positional = true;
        self.mode(true, path)?;
        let segs = segments(base)?;
        self.tree.insert(&segs, Action::Positional, base)
    }
}

/// The `find` form of `$slice`: a number, or `[skip, limit]` with a positive limit; numbers are
/// truncated (measured).
fn find_slice(arg: RawValue<'_>) -> Result<Action> {
    let int = |v: RawValue<'_>| is_number(&v).then(|| truncate_i64(&v)).flatten();
    match arg {
        RawValue::Array(arr) => {
            let items: Vec<RawValue<'_>> = arr.iter().collect();
            let [skip, limit] = items.as_slice() else {
                let msg = "$slice array argument should be of form [skip, limit]";
                return Err(err(code::SLICE_FORM, msg));
            };
            let (Some(skip), Some(limit)) = (int(*skip), int(*limit)) else {
                let msg = "$slice array argument should be of form [skip, limit]";
                return Err(err(code::SLICE_FORM, msg));
            };
            if limit <= 0 {
                return Err(bad_value("$slice limit must be positive"));
            }
            Ok(Action::Slice { skip: Some(skip), limit })
        }
        v => int(v).map(|limit| Action::Slice { skip: None, limit }).ok_or_else(|| {
            err(code::SLICE_TYPE, "$slice only supports numbers and [skip, limit] arrays")
        }),
    }
}

fn elem_match(path: &str, arg: RawValue<'_>) -> Result<Pred> {
    if !matches!(arg, RawValue::Document(_)) {
        let msg = format!(
            "elemMatch: Invalid argument, object required, but got {}",
            crate::types::name_of(arg.element_type())
        );
        return Err(err(code::ELEM_MATCH_NOT_OBJECT, msg));
    }
    let doc = Document::new().with(path, Document::new().with(ops::ELEM_MATCH, Value::from(arg)));
    let owned = crate::value::OwnedDoc::new(&doc)?;
    match parse::parse(owned.get(), FilterUse::Nested)? {
        Expr::Field { pred, .. } => Ok(pred),
        _ => Err(bad_value("$elemMatch needs an Object")),
    }
}

impl Projection {
    /// Reads a projection.
    pub fn parse(spec: RawDoc<'_>) -> Result<Projection> {
        let mut b = Builder {
            tree: Tree::default(),
            mode: None,
            positional: false,
            elem_match: false,
            slice: false,
            metas: Vec::new(),
        };
        for (k, v) in spec.iter() {
            b.field(k, k, v)?;
        }
        // Without an inclusion or an exclusion: `$slice` excludes, `$elemMatch` includes, and
        // `{_id: 1}` alone includes (measured).
        let inclusion = b.mode.unwrap_or_else(|| {
            !b.slice
                && (b.elem_match
                    || matches!(b.tree.get(ops::ID), Some(Node::Leaf(Action::Include))))
        });
        if inclusion && b.tree.get(ops::ID).is_none() {
            b.tree.fields.insert(0, (ops::ID.to_owned(), Node::Leaf(Action::Include)));
        }
        Ok(Projection { tree: b.tree, inclusion, positional: b.positional, metas: b.metas })
    }

    /// The error of a `$meta` the query cannot give: `text_search` and `near` say whether the
    /// filter has `$text` and `$near`, `sorted` whether there is a sort (measured on a `refrouter`
    /// 9.0.4, which checks the search scores first).
    pub fn check_metadata(&self, text_search: bool, near: bool, sorted: bool) -> Result<()> {
        let unavailable = |what: &str| {
            let msg = format!("query requires {what} metadata, but it is not available");
            Err(err(code::META_UNAVAILABLE, msg))
        };
        let has = |name: &str| self.metas.iter().any(|m| m == name);
        for (name, what) in [
            ("searchScore", "$search score"),
            ("vectorSearchScore", "$vectorSearch score"),
            ("score", "score"),
            ("scoreDetails", "scoreDetails"),
        ] {
            if has(name) {
                return unavailable(what);
            }
        }
        if has("textScore") && !text_search {
            return unavailable("text score");
        }
        for (name, what) in
            [("geoNearDistance", "$geoNear distance"), ("geoNearPoint", "$geoNear point")]
        {
            if has(name) && !near {
                return unavailable(what);
            }
        }
        if has("sortKey") && !sorted {
            return Err(bad_value("cannot use sortKey $meta projection without a sort"));
        }
        Ok(())
    }

    /// Whether the projection uses `$` and so needs the filter's match position.
    pub fn needs_position(&self) -> bool {
        self.positional
    }

    /// The projected document.
    pub fn apply(&self, doc: RawDoc<'_>, ctx: &ProjectionContext<'_>) -> Result<Document> {
        let run = Run { ctx, root: doc };
        let mut out = if self.inclusion {
            run.include(&self.tree, doc, false)?
        } else {
            run.exclude(&self.tree, doc)?
        };
        run.computed(&self.tree, &mut out, self.inclusion)?;
        Ok(out)
    }
}

struct Run<'c, 'a, 'd> {
    ctx: &'c ProjectionContext<'a>,
    root: RawDoc<'d>,
}

impl Run<'_, '_, '_> {
    /// The included fields of `doc`; `done` once `$` has taken its element.
    fn include(&self, tree: &Tree, doc: RawDoc<'_>, done: bool) -> Result<Document> {
        let mut out = Document::new();
        for (k, v) in doc.iter() {
            let Some(node) = tree.get(k) else { continue };
            match node {
                Node::Leaf(Action::Include) => out.push(k, Value::from(v)),
                Node::Leaf(Action::Slice { skip, limit }) => out.push(k, sliced(v, *skip, *limit)),
                Node::Leaf(Action::Positional) if done => out.push(k, Value::from(v)),
                Node::Leaf(Action::Positional) => out.push(k, self.positional(v)?),
                Node::Leaf(_) => {}
                Node::Sub(sub) => match v {
                    RawValue::Document(d) => out.push(k, self.include(sub, d, done)?),
                    // `a.b.$`: the element of the first array on the path, projected.
                    RawValue::Array(arr) if !done && has_positional(sub) => {
                        let projected = match self.element(arr)? {
                            RawValue::Document(d) => Value::Document(self.include(sub, d, true)?),
                            RawValue::Array(inner) => {
                                Value::Array(self.include_array(sub, inner, true)?)
                            }
                            _ => return Err(mismatch()),
                        };
                        out.push(k, vec![projected]);
                    }
                    RawValue::Array(arr) => out.push(k, self.include_array(sub, arr, done)?),
                    _ => {}
                },
            }
        }
        Ok(out)
    }

    fn include_array(&self, tree: &Tree, arr: RawArray<'_>, done: bool) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for e in arr.iter() {
            match e {
                RawValue::Document(d) => out.push(Value::Document(self.include(tree, d, done)?)),
                RawValue::Array(inner) => {
                    out.push(Value::Array(self.include_array(tree, inner, done)?))
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn exclude(&self, tree: &Tree, doc: RawDoc<'_>) -> Result<Document> {
        let mut out = Document::new();
        for (k, v) in doc.iter() {
            match tree.get(k) {
                Some(Node::Leaf(Action::Exclude)) => {}
                Some(Node::Leaf(Action::Slice { skip, limit })) => {
                    out.push(k, sliced(v, *skip, *limit))
                }
                // In an exclusion `$elemMatch` keeps the field's place, or removes it.
                Some(Node::Leaf(Action::ElemMatch(pred))) => {
                    if let Some(e) = self.first_match(pred, v)? {
                        out.push(k, vec![e]);
                    }
                }
                Some(Node::Sub(sub)) => match v {
                    RawValue::Document(d) => out.push(k, self.exclude(sub, d)?),
                    RawValue::Array(arr) => out.push(k, self.exclude_array(sub, arr)?),
                    v => out.push(k, Value::from(v)),
                },
                _ => out.push(k, Value::from(v)),
            }
        }
        Ok(out)
    }

    fn exclude_array(&self, tree: &Tree, arr: RawArray<'_>) -> Result<Vec<Value>> {
        arr.iter()
            .map(|e| match e {
                RawValue::Document(d) => Ok(Value::Document(self.exclude(tree, d)?)),
                RawValue::Array(inner) => Ok(Value::Array(self.exclude_array(tree, inner)?)),
                v => Ok(Value::from(v)),
            })
            .collect()
    }

    /// Computed fields, `$meta` and, in an inclusion, `$elemMatch`, evaluated on the whole
    /// document after the rest, in the projection's order.
    fn computed(&self, tree: &Tree, out: &mut Document, inclusion: bool) -> Result<()> {
        for (k, node) in &tree.fields {
            match node {
                Node::Leaf(Action::ElemMatch(pred)) if inclusion => {
                    let v = self.root.get(k.as_str());
                    if let Some(e) = v.map(|v| self.first_match(pred, v)).transpose()?.flatten() {
                        out.insert(k.as_str(), vec![e]);
                    }
                }
                Node::Leaf(Action::Meta(name)) if name == SORT_KEY => {
                    if let Some(key) = self.ctx.sort_key {
                        out.insert(k.as_str(), key.to_vec());
                    }
                }
                Node::Leaf(Action::Computed(expr)) => {
                    let v = self.ctx.matcher.ext.eval_projection(
                        expr.get(),
                        self.root,
                        self.ctx.matcher.vars,
                    )?;
                    // An expression that gives nothing leaves the field out, as RefDb does.
                    if let Some(v) = v {
                        out.insert(k.as_str(), v);
                    }
                }
                Node::Leaf(Action::Meta(name)) => {
                    if let Some(v) = self.ctx.matcher.ext.meta(name, self.root)? {
                        out.insert(k.as_str(), v);
                    }
                }
                Node::Sub(sub) if has_computed(sub) => {
                    let inner = self.computed_in(sub, out.get(k).cloned(), inclusion)?;
                    out.insert(k.as_str(), inner);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The fields of `tree` computed into `v`: into a document, into each element of an array,
    /// or into a new document in place of anything else (measured).
    fn computed_in(&self, tree: &Tree, v: Option<Value>, inclusion: bool) -> Result<Value> {
        match v {
            Some(Value::Array(items)) => Ok(Value::Array(
                items
                    .into_iter()
                    .map(|e| self.computed_in(tree, Some(e), inclusion))
                    .collect::<Result<Vec<_>>>()?,
            )),
            Some(Value::Document(mut d)) => {
                self.computed(tree, &mut d, inclusion)?;
                Ok(Value::Document(d))
            }
            _ => {
                let mut d = Document::new();
                self.computed(tree, &mut d, inclusion)?;
                Ok(Value::Document(d))
            }
        }
    }

    fn first_match(&self, pred: &Pred, v: RawValue<'_>) -> Result<Option<Value>> {
        let RawValue::Array(arr) = v else { return Ok(None) };
        let eval = Eval::new(&self.ctx.matcher, false);
        for e in arr.iter() {
            let single = crate::value::OwnedValue::new(Value::Array(vec![Value::from(e)]))?;
            if eval.leaf(pred, Some(single.get()), None, false)? {
                return Ok(Some(Value::from(e)));
            }
        }
        Ok(None)
    }

    /// The element of `arr` at the position the filter matched.
    fn element<'v>(&self, arr: RawArray<'v>) -> Result<RawValue<'v>> {
        let Some(pos) = self.ctx.position else {
            return Err(err(
                code::POSITIONAL_NO_MATCH,
                "positional operator '.$' couldn't find a matching element in the array",
            ));
        };
        arr.iter().nth(pos).ok_or_else(mismatch)
    }

    /// `a.$`: the matched element of an array, or the value itself when it is not one.
    fn positional(&self, v: RawValue<'_>) -> Result<Value> {
        match v {
            RawValue::Array(arr) => Ok(Value::Array(vec![Value::from(self.element(arr)?)])),
            other => Ok(Value::from(other)),
        }
    }
}

fn mismatch() -> MqlError {
    err(code::POSITIONAL_MISMATCH, "positional operator '.$' element mismatch")
}

fn has_positional(tree: &Tree) -> bool {
    tree.fields.iter().any(|(_, n)| match n {
        Node::Leaf(Action::Positional) => true,
        Node::Sub(sub) => has_positional(sub),
        Node::Leaf(_) => false,
    })
}

fn has_computed(tree: &Tree) -> bool {
    tree.fields.iter().any(|(_, n)| match n {
        Node::Leaf(Action::Computed(_) | Action::Meta(_) | Action::ElemMatch(_)) => true,
        Node::Sub(sub) => has_computed(sub),
        Node::Leaf(_) => false,
    })
}

/// `$slice` of an array; anything else is kept.
fn sliced(v: RawValue<'_>, skip: Option<i64>, limit: i64) -> Value {
    let RawValue::Array(arr) = v else { return Value::from(v) };
    let items: Vec<RawValue<'_>> = arr.iter().collect();
    let n = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let (start, take) = match skip {
        None if limit >= 0 => (0, limit),
        None => ((n + limit).max(0), n),
        Some(s) if s >= 0 => (s.min(n), limit),
        Some(s) => ((n + s).max(0), limit),
    };
    let start = usize::try_from(start).unwrap_or(usize::MAX);
    let take = usize::try_from(take).unwrap_or(usize::MAX);
    Value::Array(items.into_iter().skip(start).take(take).map(Value::from).collect())
}
