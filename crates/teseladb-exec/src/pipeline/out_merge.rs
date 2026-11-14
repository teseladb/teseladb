//! `$out` and `$merge` (010 FR-012, T029), as a reference router 9.0.4 reads and runs them (measured
//! 01-11-2026): their arguments are checked when the pipeline is parsed, and for each document
//! that reaches `$merge` [`Merge::action`] says what becomes of the target collection's
//! document with the same `on` fields. The node writes (the stage holds no I/O): `$out` into a
//! collection that replaces the target at the end, `$merge` document by document.

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::MqlError;

use super::errors::{self, TYPE_MISMATCH, error, missing_field, unknown_field, wrong_type};
use super::{Env, Pipeline, Stage};
use crate::expr::arith::raw_type_name;
use crate::expr::{Ctx, Expr};

/// `$out`.
pub const OUT: &str = "$out";
/// `$merge`.
pub const MERGE: &str = "$merge";

/// The prefix of the collections a node keeps for itself, which neither stage writes.
const SYSTEM_PREFIX: &str = "system.";

/// A stage that writes the documents that reach it to a collection.
#[derive(Clone, Debug)]
pub enum Write {
    /// `$out`.
    Out(Out),
    /// `$merge`.
    Merge(Box<Merge>),
}

impl Write {
    /// The stage's name.
    pub fn name(&self) -> &'static str {
        match self {
            Write::Out(_) => OUT,
            Write::Merge(_) => MERGE,
        }
    }
}

/// `$out`: the collection the documents replace.
#[derive(Clone, Debug, PartialEq)]
pub struct Out {
    /// Its database, when the stage names one (the command's otherwise).
    pub db: Option<String>,
    /// Its collection.
    pub coll: String,
}

impl Out {
    /// Reads `$out`'s argument: a collection, or `{db, coll}`.
    pub fn parse(v: RawValue<'_>) -> Result<Out, MqlError> {
        let out = match v {
            RawValue::String(c) => Out { db: None, coll: c.to_owned() },
            RawValue::Document(d) => {
                let (mut db, mut coll) = (None, None);
                for (k, x) in d.iter() {
                    match (k, x) {
                        ("db", RawValue::String(s)) => db = Some(s.to_owned()),
                        ("coll", RawValue::String(s)) => coll = Some(s.to_owned()),
                        ("db" | "coll", other) => {
                            return Err(wrong_type(&format!("{OUT}.{k}"), other, "type 'string'"));
                        }
                        ("timeseries", _) => {}
                        (other, _) => return Err(unknown_field(OUT, other)),
                    }
                }
                let db = db.ok_or_else(|| missing_field(OUT, "db"))?;
                let coll = coll.ok_or_else(|| missing_field(OUT, "coll"))?;
                Out { db: Some(db), coll }
            }
            other => {
                return Err(error(
                    errors::OUT_TYPE,
                    format!(
                        "$out only supports a string or object argument, but found {}",
                        raw_type_name(other)
                    ),
                ));
            }
        };
        if out.coll.starts_with(SYSTEM_PREFIX) {
            return Err(error(
                errors::OUT_SPECIAL,
                format!("Can't $out to special collection: {}", out.coll),
            ));
        }
        Ok(out)
    }

    /// Checks the target in database `db` (the command's when the stage names none).
    pub fn check(&self, db: &str) -> Result<(), MqlError> {
        if self.coll.is_empty() {
            let db = self.db.as_deref().unwrap_or(db);
            return Err(error(
                errors::INVALID_NAMESPACE,
                format!("Invalid $out target namespace, {db}"),
            ));
        }
        Ok(())
    }
}

/// What `$merge` does with a document whose `on` fields a target document has.
#[derive(Clone, Debug)]
pub enum WhenMatched {
    /// The target document becomes the document.
    Replace,
    /// The target document stays.
    KeepExisting,
    /// The document's fields go into the target document.
    Merge,
    /// The command fails.
    Fail,
    /// The target document through this pipeline, with the document in `$$new`; and the names
    /// of its stages.
    Pipeline(Pipeline, Vec<String>),
}

/// What `$merge` does with a document no target document matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhenNotMatched {
    /// The document is inserted.
    Insert,
    /// The document is dropped.
    Discard,
    /// The command fails.
    Fail,
}

/// `$merge`.
#[derive(Clone, Debug)]
pub struct Merge {
    /// The target's database, when the stage names one.
    pub db: Option<String>,
    /// The target's collection.
    pub coll: String,
    /// The fields a document matches its target by (`_id` when not given).
    pub on: Option<Vec<String>>,
    /// What it does with a document a target document matches.
    pub when_matched: WhenMatched,
    /// What it does with a document no target document matches.
    pub when_not_matched: WhenNotMatched,
    /// The variables of the `whenMatched` pipeline besides `new`, the document.
    pub lets: Vec<(String, Expr)>,
}

/// What becomes of the target of one document that reaches `$merge`.
#[derive(Clone, Debug, PartialEq)]
pub enum MergeAction {
    /// The document is inserted.
    Insert(Document),
    /// The target document becomes this one.
    Replace(Document),
    /// Nothing is written.
    Skip,
    /// The command fails with a duplicate key on the target (`whenMatched: fail`).
    Duplicate,
}

/// The variable a `$merge` pipeline reads the document from.
const NEW: &str = "new";

/// The stages a `whenMatched` pipeline may hold (an update's).
const UPDATE_STAGES: [&str; 6] =
    ["$addFields", "$set", "$project", "$unset", "$replaceRoot", "$replaceWith"];

/// The modes `whenMatched` takes, by name.
fn when_matched(name: &str) -> Option<WhenMatched> {
    match name {
        "replace" => Some(WhenMatched::Replace),
        "keepExisting" => Some(WhenMatched::KeepExisting),
        "merge" => Some(WhenMatched::Merge),
        "fail" => Some(WhenMatched::Fail),
        _ => None,
    }
}

/// The modes `whenNotMatched` takes, by name.
fn when_not_matched(name: &str) -> Option<WhenNotMatched> {
    match name {
        "insert" => Some(WhenNotMatched::Insert),
        "discard" => Some(WhenNotMatched::Discard),
        "fail" => Some(WhenNotMatched::Fail),
        _ => None,
    }
}

/// The name of a `whenMatched` mode in the messages.
fn matched_name(m: &WhenMatched) -> &'static str {
    match m {
        WhenMatched::Replace => "replace",
        WhenMatched::KeepExisting => "keepExisting",
        WhenMatched::Merge => "merge",
        WhenMatched::Fail => "fail",
        WhenMatched::Pipeline(..) => "pipeline",
    }
}

/// The name of a `whenNotMatched` mode in the messages.
fn not_matched_name(m: WhenNotMatched) -> &'static str {
    match m {
        WhenNotMatched::Insert => "insert",
        WhenNotMatched::Discard => "discard",
        WhenNotMatched::Fail => "fail",
    }
}

/// `into`: a collection, or `{db, coll}`.
fn into(v: RawValue<'_>) -> Result<(Option<String>, String), MqlError> {
    let (db, coll) = match v {
        RawValue::String("") => {
            return Err(error(
                errors::MERGE_INTO_EMPTY,
                "$merge 'into' field cannot be an empty string",
            ));
        }
        RawValue::String(c) => (None, c.to_owned()),
        RawValue::Document(d) => {
            let text = |k: &str| match d.get(k) {
                Some(RawValue::String(s)) if !s.is_empty() => Some(s.to_owned()),
                _ => None,
            };
            let coll = text("coll").ok_or_else(|| {
                error(
                    errors::MERGE_INTO_COLL,
                    "$merge 'into' field must specify a 'coll' that is not empty, null or undefined",
                )
            })?;
            (text("db"), coll)
        }
        other => {
            return Err(error(
                errors::MERGE_INTO_TYPE,
                format!(
                    "$merge 'into' field  must be either a string or an object, but found {}",
                    raw_type_name(other)
                ),
            ));
        }
    };
    if coll.starts_with(SYSTEM_PREFIX) {
        return Err(error(
            errors::MERGE_SPECIAL,
            format!("Cannot $merge to special collection: {coll}"),
        ));
    }
    Ok((db, coll))
}

/// `on`: a field, or a list of at least one.
fn on(v: RawValue<'_>) -> Result<Vec<String>, MqlError> {
    match v {
        RawValue::String(f) => Ok(vec![f.to_owned()]),
        RawValue::Array(items) => {
            let mut fields = Vec::new();
            for i in items.iter() {
                match i {
                    RawValue::String(f) => fields.push(f.to_owned()),
                    other => {
                        return Err(error(
                            errors::MERGE_ON_ELEMENT,
                            format!(
                                "$merge 'on' array elements must be strings, but found {}",
                                raw_type_name(other)
                            ),
                        ));
                    }
                }
            }
            if fields.is_empty() {
                return Err(error(
                    errors::MERGE_ON_EMPTY,
                    "If explicitly specifying $merge 'on', must include at least one field",
                ));
            }
            Ok(fields)
        }
        other => Err(error(
            errors::MERGE_ON_TYPE,
            format!(
                "$merge 'on' field  must be either a string or an array of strings, but found {}",
                raw_type_name(other)
            ),
        )),
    }
}

/// Whether the reference server runs `$merge` with modes `m` and `n` (measured: `keepExisting` and `fail`
/// only insert what does not match).
fn supported(m: &WhenMatched, n: WhenNotMatched) -> bool {
    !matches!(
        (m, n),
        (
            WhenMatched::KeepExisting | WhenMatched::Fail,
            WhenNotMatched::Discard | WhenNotMatched::Fail
        )
    )
}

impl Merge {
    /// Reads `$merge`'s argument: a collection, or its options; `ctx`: the command's
    /// variables.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Merge, MqlError> {
        let d = match v {
            RawValue::String(_) => {
                let (db, coll) = into(v)?;
                return Ok(Merge {
                    db,
                    coll,
                    on: None,
                    when_matched: WhenMatched::Merge,
                    when_not_matched: WhenNotMatched::Insert,
                    lets: Vec::new(),
                });
            }
            RawValue::Document(d) => d,
            other => {
                return Err(error(
                    TYPE_MISMATCH,
                    format!(
                        "$merge requires a string or object argument, but found {}",
                        raw_type_name(other)
                    ),
                ));
            }
        };
        let mut target = None;
        let mut fields = None;
        let mut matched = WhenMatched::Merge;
        let mut pipeline = None;
        let mut not_matched = WhenNotMatched::Insert;
        let mut lets = None;
        for (k, x) in d.iter() {
            match k {
                "into" => target = Some(into(x)?),
                "on" => fields = Some(on(x)?),
                "whenMatched" => match x {
                    RawValue::String(s) => {
                        matched = when_matched(s).ok_or_else(|| {
                            error(
                                errors::BAD_VALUE,
                                format!(
                                    "Enumeration value '{s}' for field 'whenMatched' is not a valid value."
                                ),
                            )
                        })?;
                    }
                    RawValue::Array(a) => pipeline = Some(a.iter().collect::<Vec<_>>()),
                    other => {
                        return Err(error(
                            errors::MERGE_WHEN_MATCHED_TYPE,
                            format!(
                                "$merge 'whenMatched' field  must be either a string or an array, but found {}",
                                raw_type_name(other)
                            ),
                        ));
                    }
                },
                "whenNotMatched" => match x {
                    RawValue::String(s) => {
                        not_matched = when_not_matched(s).ok_or_else(|| {
                            error(
                                errors::BAD_VALUE,
                                format!(
                                    "Enumeration value '{s}' for field '{MERGE}.whenNotMatched' is not a valid value."
                                ),
                            )
                        })?;
                    }
                    other => {
                        let field = format!("{MERGE}.whenNotMatched");
                        return Err(wrong_type(&field, other, "type 'string'"));
                    }
                },
                "let" => match x {
                    RawValue::Document(l) => lets = Some(l),
                    other => {
                        return Err(wrong_type(&format!("{MERGE}.let"), other, "type 'object'"));
                    }
                },
                other => return Err(unknown_field(MERGE, other)),
            }
        }
        let (db, coll) = target.ok_or_else(|| missing_field(MERGE, "into"))?;
        let lets = match lets {
            Some(l) => parse_lets(l, ctx)?,
            None => Vec::new(),
        };
        let when_matched = match pipeline {
            Some(stages) => {
                let names = stages
                    .iter()
                    .map(|s| match s {
                        RawValue::Document(d) => d.iter().next().map_or("", |(k, _)| k).to_owned(),
                        _ => String::new(),
                    })
                    .collect();
                WhenMatched::Pipeline(parse_update(&stages, &lets, ctx)?, names)
            }
            None => matched,
        };
        if !supported(&when_matched, not_matched) {
            return Err(error(
                errors::MERGE_MODES,
                format!(
                    "Combination of $merge modes 'whenMatched: {}' and 'whenNotMatched: {}' is not supported",
                    matched_name(&when_matched),
                    not_matched_name(not_matched)
                ),
            ));
        }
        Ok(Merge { db, coll, on: fields, when_matched, when_not_matched: not_matched, lets })
    }

    /// The fields documents match their target by.
    pub fn on_fields(&self) -> Vec<String> {
        self.on.clone().unwrap_or_else(|| vec!["_id".to_owned()])
    }

    /// What becomes of the target of document `new`, given the target document that has its
    /// `on` fields (`existing`); the pipeline of `whenMatched` reads the command's variables
    /// from `env`.
    pub fn action(
        &self,
        existing: Option<&Document>,
        new: Document,
        env: &dyn Env,
    ) -> Result<MergeAction, MqlError> {
        let Some(old) = existing else {
            return match self.when_not_matched {
                WhenNotMatched::Insert => Ok(MergeAction::Insert(new)),
                WhenNotMatched::Discard => Ok(MergeAction::Skip),
                WhenNotMatched::Fail => Err(error(
                    errors::MERGE_NO_MATCH,
                    "$merge could not find a matching document in the target collection for at least one document in the source collection",
                )),
            };
        };
        match &self.when_matched {
            WhenMatched::Replace => Ok(MergeAction::Replace(with_id_of(old, new))),
            WhenMatched::KeepExisting => Ok(MergeAction::Skip),
            WhenMatched::Fail => Ok(MergeAction::Duplicate),
            WhenMatched::Merge => {
                let mut out = old.clone();
                for (k, v) in new.iter() {
                    out.insert(k, v.clone());
                }
                Ok(MergeAction::Replace(out))
            }
            WhenMatched::Pipeline(p, names) => {
                refuse_outside(names, &UPDATE_STAGES)?;
                let mut vars = env.vars().cloned().unwrap_or_default();
                vars.insert(NEW, new.clone());
                for (name, e) in &self.lets {
                    let scope = env.scope(&new);
                    vars.insert(name, e.eval(&new, &scope)?.unwrap_or(Value::Null));
                }
                let scoped = Scoped { inner: env, vars };
                let out = p.run(vec![old.clone()], &scoped)?;
                Ok(out.into_iter().next().map_or(MergeAction::Skip, MergeAction::Replace))
            }
        }
    }
}

/// Document `new` with the `_id` of `old` when it has none, as a replacement keeps it.
fn with_id_of(old: &Document, new: Document) -> Document {
    match (new.get("_id"), old.get("_id")) {
        (None, Some(id)) => {
            let mut out = Document::new().with("_id", id.clone());
            for (k, v) in new.iter() {
                out.push(k, v.clone());
            }
            out
        }
        _ => new,
    }
}

/// The variables of `let`.
fn parse_lets(l: RawDoc<'_>, ctx: &Ctx) -> Result<Vec<(String, Expr)>, MqlError> {
    let mut out = Vec::new();
    for (name, e) in l.iter() {
        out.push((name.to_owned(), Expr::parse_in(e, ctx)?));
    }
    Ok(out)
}

/// The `whenMatched` pipeline, parsed with `$$new` and the variables of `let`.
fn parse_update(
    stages: &[RawValue<'_>],
    lets: &[(String, Expr)],
    ctx: &Ctx,
) -> Result<Pipeline, MqlError> {
    // `$$new` is the document unless `let` names it (measured).
    let names: Vec<&str> =
        std::iter::once(NEW).chain(lets.iter().map(|(n, _)| n.as_str())).collect();
    Pipeline::parse(stages, &ctx.with(names)).map_err(|e| {
        MqlError::new(
            e.code,
            format!("Error parsing {MERGE}.whenMatched pipeline :: caused by :: {}", e.message),
        )
    })
}

/// An [`Env`] with more variables.
struct Scoped<'a> {
    inner: &'a dyn Env,
    vars: Document,
}

impl Env for Scoped<'_> {
    fn ext(&self) -> &dyn teseladb_mql::Extensions {
        self.inner.ext()
    }

    fn interrupted(&self) -> Result<(), MqlError> {
        self.inner.interrupted()
    }

    fn vars(&self) -> Option<&Document> {
        Some(&self.vars)
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        self.inner.collection(name)
    }

    fn random(&self) -> f64 {
        self.inner.random()
    }

    fn collation(&self) -> crate::collation::Collation {
        self.inner.collation()
    }
}

/// The error of a writing stage that is not the pipeline's last (`stage`).
pub(crate) fn not_last(stage: &str) -> MqlError {
    error(errors::WRITE_NOT_LAST, format!("{stage} can only be the final stage in the pipeline"))
}

/// Refuses a writing stage among sub-pipeline `stages` of `owner` (`$lookup`, `$unionWith`).
pub(crate) fn refuse_in(stages: &[RawValue<'_>], owner: &str) -> Result<(), MqlError> {
    for s in stages {
        let name = match s {
            RawValue::Document(d) => d.iter().next().map_or("", |(k, _)| k),
            _ => "",
        };
        if name == OUT || name == MERGE {
            let code = match owner {
                "$lookup" => errors::WRITE_IN_LOOKUP,
                _ => errors::WRITE_IN_UNION,
            };
            return Err(error(
                code,
                format!("{name} is not allowed within a {owner}'s sub-pipeline"),
            ));
        }
    }
    Ok(())
}

impl Pipeline {
    /// The stage that writes the pipeline's documents, last, if it has one.
    pub fn write(&self) -> Option<&Write> {
        match self.stages.last() {
            Some(Stage::Write(w)) => Some(w),
            _ => None,
        }
    }
}

/// Refuses a stage among `names` other than `allowed` (`InvalidOptions`, as an update's
/// pipeline).
fn refuse_outside(names: &[String], allowed: &[&str]) -> Result<(), MqlError> {
    match names.iter().find(|n| !allowed.contains(&n.as_str())) {
        Some(name) => Err(error(
            errors::INVALID_OPTIONS,
            format!("{name} is not allowed to be used within an update"),
        )),
        None => Ok(()),
    }
}
