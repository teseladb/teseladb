//! Writes to `config.settings` (007 FR-017, contract `config-db.md`), as a `refrouter` 9.0.4 takes
//! them (measured, 01-11-2026): `insert`, `update`, `delete` and `findAndModify` run the usual
//! semantics on the collection's documents, which Group 0 keeps, and every new version of a
//! document passes the collection's validator: `balancer` with its `mode`, `stopped`,
//! `activeWindow` and the other fields of the balancer and nothing else, `chunksize` with a
//! `value` from 1 to 1024 and nothing else, and `automerge`, `ReadWriteConcernDefaults` and
//! `audit` with any fields; any other document fails with `DocumentValidationFailure` (121) and
//! its `errInfo`. The whole command is worked out on a copy of the documents and goes to Group 0
//! as one change that expects each touched document as it was read; another change in between
//! makes it start again.

use teseladb_meta::catalog::Namespace;
use teseladb_mql::MatchContext;
use teseladb_mql::validation::{ValidationAction, ValidationLevel, Validator};
use teseladb_bson::{Document, RawDoc, Value};

use super::authz_writes::{Entry, Staged, statement};
use super::config_db::CONFIG_DB;
use super::crud::{Modify, with_id};
use super::mutation::{MutationResult, Stored, from_mql, stored_bytes};
use super::{Backend, Ctx};
use crate::error::{CommandError, DUPLICATE_KEY, WRITE_CONFLICT};
use crate::types::wrong_type;

/// `config.settings`.
pub const SETTINGS: &str = "settings";
/// `_id` of the balancer's settings.
pub const BALANCER: &str = "balancer";
/// `_id` of the size at which the chunks of a sharded collection split, in MiB.
pub const CHUNK_SIZE: &str = "chunksize";
/// `_id` of the switch of automatic merges.
pub const AUTO_MERGE: &str = "automerge";
/// `_id` of the default read and write concerns (`setDefaultRWConcern`).
pub const RW_CONCERN_DEFAULTS: &str = "ReadWriteConcernDefaults";
/// `_id` of the audit settings.
pub const AUDIT: &str = "audit";
/// The smallest and largest `chunksize`.
pub const CHUNK_SIZE_RANGE: (i32, i32) = (1, 1024);

/// Attempts before a command whose documents keep changing under it gives up.
const MAX_ATTEMPTS: usize = 16;

/// A document of `config.settings` changed by a command: its `_id`, as it was read and as it is
/// now (`None`: absent).
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsWrite {
    /// The document's `_id`.
    pub id: Value,
    /// As it was read.
    pub read: Option<Document>,
    /// As it is now.
    pub now: Option<Document>,
}

/// Whether `ns` is `config.settings`.
pub fn is_settings(ns: &Namespace) -> bool {
    ns.db == CONFIG_DB && ns.coll == SETTINGS
}

fn enum_of(values: &[&str]) -> Document {
    let values: Vec<Value> = values.iter().map(|v| Value::from(*v)).collect();
    Document::new().with("enum", values)
}

fn bson_type(t: &str) -> Document {
    Document::new().with("bsonType", t)
}

/// The validator of `config.settings`, as `listCollections` shows it on a `refrouter` 9.0.4.
pub fn validator_spec() -> Document {
    let balancer = Document::new()
        .with("_id", enum_of(&[BALANCER]))
        .with("mode", enum_of(&["full", "off"]))
        .with("stopped", bson_type("bool"))
        .with(
            "activeWindow",
            bson_type("object").with("required", vec![Value::from("start"), Value::from("stop")]),
        )
        .with("activeWindowDOW", bson_type("array"))
        .with(
            "_secondaryThrottle",
            Document::new().with(
                "oneOf",
                vec![Value::Document(bson_type("bool")), Value::Document(bson_type("object"))],
            ),
        )
        .with("_waitForDelete", bson_type("bool"))
        .with("attemptToBalanceJumboChunks", bson_type("bool"));
    let chunk_size = Document::new().with("_id", enum_of(&[CHUNK_SIZE])).with(
        "value",
        bson_type("number").with("minimum", CHUNK_SIZE_RANGE.0).with("maximum", CHUNK_SIZE_RANGE.1),
    );
    let others = Document::new().with("_id", enum_of(&[AUTO_MERGE, RW_CONCERN_DEFAULTS, AUDIT]));
    let closed = |props: Document| {
        Value::Document(
            Document::new().with("properties", props).with("additionalProperties", false),
        )
    };
    let one_of = vec![
        closed(balancer),
        closed(chunk_size),
        Value::Document(Document::new().with("properties", others)),
    ];
    Document::new().with("$jsonSchema", Document::new().with("oneOf", one_of))
}

/// The options of `config.settings` in `listCollections`.
pub fn options() -> Document {
    Document::new()
        .with("validator", validator_spec())
        .with("validationLevel", ValidationLevel::Strict.name())
        .with("validationAction", ValidationAction::Error.name())
}

fn validator() -> Result<Validator, CommandError> {
    let bytes = validator_spec().to_bytes()?;
    let raw = RawDoc::from_bytes(&bytes)?;
    Validator::parse(raw, ValidationLevel::Strict, ValidationAction::Error).map_err(from_mql)
}

/// The documents of `config.settings` while a command works on them.
struct Settings {
    entries: Vec<Entry>,
    validator: Validator,
    bypass: bool,
}

impl Settings {
    fn load<B: Backend + ?Sized>(backend: &mut B, bypass: bool) -> Result<Settings, CommandError> {
        let mut entries = Vec::new();
        for doc in backend.config_documents(SETTINGS)? {
            entries.push(Entry { read: Some(doc.to_bytes()?), now: Some(doc) });
        }
        Ok(Settings { entries, validator: validator()?, bypass })
    }

    /// The documents that changed, as Group 0 takes them.
    fn writes(&self) -> Result<Vec<SettingsWrite>, CommandError> {
        let mut out = Vec::new();
        for e in &self.entries {
            let read = e.read.as_deref().map(RawDoc::from_bytes).transpose()?.map(Document::from);
            let now_bytes = e.now.as_ref().map(Document::to_bytes).transpose()?;
            if e.read == now_bytes {
                continue;
            }
            let of = |d: &Option<Document>| d.as_ref().and_then(|d| d.get("_id").cloned());
            let id = of(&e.now).or_else(|| of(&read)).unwrap_or(Value::Null);
            out.push(SettingsWrite { id, read, now: e.now.clone() });
        }
        Ok(out)
    }
}

impl Staged for Settings {
    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn check(&self, bytes: &[u8], skip: Option<usize>) -> Result<(), CommandError> {
        let raw = RawDoc::from_bytes(bytes)?;
        let id = raw.get("_id").map(Value::from).unwrap_or(Value::Null);
        for (i, e) in self.entries.iter().enumerate() {
            let Some(other) = e.now.as_ref().filter(|_| Some(i) != skip) else { continue };
            if other.get("_id") == Some(&id) {
                let ns = format!("{CONFIG_DB}.{SETTINGS}");
                let pattern = Document::new().with("_id", 1);
                let d = teseladb_index::unique::duplicate_key(&ns, "_id_", &pattern, &[id])?;
                let mut err = CommandError::new(DUPLICATE_KEY, d.message);
                err.extra.push("keyPattern", d.key_pattern);
                err.extra.push("keyValue", d.key_value);
                return Err(err);
            }
        }
        let old = skip.and_then(|i| self.entries[i].read.as_deref());
        let old = old.map(RawDoc::from_bytes).transpose()?;
        let mctx =
            MatchContext { collation: None, ext: &super::engines::NodeExtensions, vars: None };
        self.validator.check(raw, old, self.bypass, &mctx).map(|_| ()).map_err(from_mql)
    }

    fn note(&mut self, _operation: &'static str, _doc: &Document) {}
}

/// Works the command out with `step` on fresh copies until Group 0 takes its change.
fn commit<B: Backend + ?Sized, T>(
    ctx: &mut Ctx<'_, B>,
    bypass: bool,
    mut step: impl FnMut(&mut B, &mut Settings) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    for _ in 0..MAX_ATTEMPTS {
        let mut settings = Settings::load(ctx.backend, bypass)?;
        let out = step(ctx.backend, &mut settings)?;
        let writes = settings.writes()?;
        if !writes.is_empty() && !ctx.backend.config_settings_write(writes)? {
            continue;
        }
        return Ok(out);
    }
    Err(CommandError::new(
        WRITE_CONFLICT,
        "config.settings changed while the command ran; try again",
    ))
}

/// `insert` of `docs`: the reply's `n` and `writeErrors`.
pub fn insert<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    docs: &[Value],
    ordered: bool,
    bypass: bool,
) -> Result<Document, CommandError> {
    let shared = ctx.shared;
    let (n, errors) = commit(ctx, bypass, |backend, settings| {
        let (mut n, mut errors) = (0i64, Vec::new());
        for (i, v) in docs.iter().enumerate() {
            let added = match v {
                Value::Document(d) => with_id(backend, shared, d).and_then(|doc| {
                    let bytes = stored_bytes(&doc, Stored::Insert)?;
                    settings.check(&bytes, None)?;
                    Ok(doc)
                }),
                other => Err(wrong_type(&format!("insert.documents.{i}"), other, "object")),
            };
            match added {
                Ok(doc) => {
                    settings.entries.push(Entry { read: None, now: Some(doc) });
                    n += 1;
                }
                Err(e) => {
                    errors.push(super::crud::write_error(i, &e));
                    if ordered {
                        break;
                    }
                }
            }
        }
        Ok((n, errors))
    })?;
    let mut reply = Document::new().with("n", super::crud::count(n));
    if !errors.is_empty() {
        reply.push("writeErrors", errors);
    }
    Ok(reply)
}

/// Runs `statements` (already checked; an error is that statement's) as one command: the result
/// of each, up to the first failure of an ordered command.
pub fn modify<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    statements: &[Result<Modify, CommandError>],
    ordered: bool,
    bypass: bool,
) -> Result<Vec<Result<MutationResult, CommandError>>, CommandError> {
    let shared = ctx.shared;
    commit(ctx, bypass, |backend, settings| {
        let mut out = Vec::new();
        for st in statements {
            let r = st.clone().and_then(|m| statement(backend, shared, settings, &m));
            let failed = r.is_err();
            out.push(r);
            if failed && ordered {
                break;
            }
        }
        Ok(out)
    })
}
