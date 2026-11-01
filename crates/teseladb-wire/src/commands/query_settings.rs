//! Query settings (010 FR-023, measured on a `refrouter` 9.0.4, 01-11-2026): `setQuerySettings`
//! fixes, for one query shape, the indexes its plans may use, its query framework, whether it
//! is rejected, a comment and a time limit; `removeQuerySettings` drops them and the
//! `$querySettings` stage lists them. They live in the internal cluster parameter
//! [`PARAMETER`], which `getClusterParameter` does not show, as a list of entries sorted by
//! hash. A new setting merges with the ones the shape had; a setting at its default (`reject:
//! false`, `maxTimeMS: 0`, no index hints) is dropped, and settings left empty are refused.
//! No query knob can be set in 9.0.4: `queryKnobs` takes only `{}`.
//!
//! `find`, `aggregate` and `distinct` look their shape up before they run, and also take
//! `querySettings` for that one run; the stored settings win when both exist.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};

use super::admin::admin_only;
use super::query_shape::{self, Shape};
use super::{Backend, Ctx};
use crate::error::{
    BAD_VALUE, CommandError, IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, INVALID_INDEX_HINT,
    INVALID_INDEX_HINT_DB, INVALID_LENGTH, QUERY_KNOB_NOT_SETTABLE, QUERY_REJECTED_BY_SETTINGS,
    QUERY_SETTINGS_DEFAULTS, QUERY_SETTINGS_EMPTY, QUERY_SHAPE_UNSUPPORTED,
};
use crate::types::{wrong_type, wrong_types};

/// `setQuerySettings`.
pub const SET_QUERY_SETTINGS: &str = "setQuerySettings";
/// `removeQuerySettings`.
pub const REMOVE_QUERY_SETTINGS: &str = "removeQuerySettings";
/// The internal cluster parameter that holds the settings.
pub const PARAMETER: &str = "querySettings";
/// The stage that lists them.
pub const STAGE: &str = "$querySettings";

/// The commands whose shape can carry settings.
const SETTABLE: &[&str] = &["find", "aggregate", "distinct"];
/// The settings, in the order they are written.
const ORDER: &[&str] = &["indexHints", "queryFramework", "reject", "comment", "maxTimeMS"];
/// The list of entries inside [`PARAMETER`].
const ENTRIES: &str = "entries";
const HASH: &str = "queryShapeHash";
const SETTINGS: &str = "settings";
const REPRESENTATIVE: &str = "representativeQuery";
const DEBUG: &str = "debugQueryShape";
/// The length of a SHA-256 in hexadecimal.
const HASH_HEX_LEN: usize = 64;
/// The query frameworks a setting may name.
const FRAMEWORKS: &[&str] = &["classic", "sbe"];
/// `errmsg` of a rejected query.
pub const REJECTED: &str = "Query rejected by admin query settings";

/// One shape's settings as stored.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    hash: String,
    settings: Document,
    representative: Option<Document>,
    debug: Option<Document>,
}

impl Entry {
    fn to_document(&self, debug: bool) -> Document {
        let mut d =
            Document::new().with(HASH, self.hash.as_str()).with(SETTINGS, self.settings.clone());
        if let Some(r) = &self.representative {
            d.push(REPRESENTATIVE, r.clone());
        }
        if debug && let Some(s) = &self.debug {
            d.push(DEBUG, s.clone());
        }
        d
    }

    fn from_document(d: &Document) -> Option<Entry> {
        let Some(Value::String(hash)) = d.get(HASH) else { return None };
        let Some(Value::Document(settings)) = d.get(SETTINGS) else { return None };
        let doc = |k: &str| match d.get(k) {
            Some(Value::Document(x)) => Some(x.clone()),
            _ => None,
        };
        Some(Entry {
            hash: hash.clone(),
            settings: settings.clone(),
            representative: doc(REPRESENTATIVE),
            debug: doc(DEBUG),
        })
    }
}

/// The stored entries, by hash.
fn stored<B: Backend + ?Sized>(backend: &mut B) -> Result<BTreeMap<String, Entry>, CommandError> {
    let all = backend.cluster_parameters()?;
    let mut out = BTreeMap::new();
    let Some(bytes) = all.get(PARAMETER) else { return Ok(out) };
    let d = Document::from(RawDoc::from_bytes(bytes)?);
    if let Some(Value::Array(items)) = d.get(ENTRIES) {
        for e in items.iter().filter_map(|i| match i {
            Value::Document(x) => Entry::from_document(x),
            _ => None,
        }) {
            out.insert(e.hash.clone(), e);
        }
    }
    Ok(out)
}

fn store<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    entries: &BTreeMap<String, Entry>,
) -> Result<(), CommandError> {
    super::lock(shared).query_settings = Cache::default();
    let list: Vec<Value> = entries.values().map(|e| Value::Document(e.to_document(true))).collect();
    let d = Document::new().with(ENTRIES, Value::Array(list));
    backend.set_cluster_parameter(PARAMETER, d.to_bytes()?)
}

/// How long a node keeps the settings it read before it reads them again, as for the default
/// concerns.
const REFRESH_MS: u64 = 1_000;

/// The settings a node last read, and when.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cache {
    entries: BTreeMap<String, Entry>,
    read_ms: Option<u64>,
}

impl Cache {
    /// The entries, if read less than [`REFRESH_MS`] before `now_ms`.
    fn fresh(&self, now_ms: u64) -> Option<BTreeMap<String, Entry>> {
        let read = self.read_ms?;
        (now_ms.saturating_sub(read) < REFRESH_MS).then(|| self.entries.clone())
    }
}

/// What a target names: a hash, with the shape and query when it is a query.
struct Target {
    hash: String,
    shape: Option<Shape>,
    query: Option<Document>,
}

/// A target given as a hash: 64 hexadecimal digits, upper-cased.
fn hash_target(s: &str) -> Result<String, CommandError> {
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) || !s.len().is_multiple_of(2) {
        return Err(CommandError::new(BAD_VALUE, "Hash input is not a hex string"));
    }
    if s.len() != HASH_HEX_LEN {
        let msg = format!("Unsupported SHA256Block hash length: {}", s.len() / 2);
        return Err(CommandError::new(INVALID_LENGTH, msg));
    }
    Ok(s.to_ascii_uppercase())
}

/// The target of `cmd`'s first field.
fn target(name: &str, v: &Value) -> Result<Target, CommandError> {
    match v {
        Value::String(s) => Ok(Target { hash: hash_target(s)?, shape: None, query: None }),
        Value::Document(q) => {
            let kind = q.iter().next().map_or("", |(k, _)| k);
            if !SETTABLE.contains(&kind) {
                let bytes = q.to_bytes()?;
                let shown = teseladb_mql::shell(teseladb_bson::RawValue::Document(
                    RawDoc::from_bytes(&bytes)?,
                ));
                let msg = format!("QueryShape can not be computed for command: {shown}");
                return Err(CommandError::new(QUERY_SHAPE_UNSUPPORTED, msg));
            }
            let Some(Value::String(db)) = q.get("$db") else {
                let msg = format!(
                    "BSON field '{kind}CommandRequest.$db' is missing but a required field"
                );
                return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg).parsed());
            };
            let shape = query_shape::shape(db, q)
                .ok_or_else(|| CommandError::new(BAD_VALUE, format!("{kind} cannot be parsed")))?;
            Ok(Target { hash: shape.hash.clone(), shape: Some(shape), query: Some(q.clone()) })
        }
        other => Err(wrong_types(&format!("{name}.{name}"), other, &["string", "object"])),
    }
}

/// One index hint: `{ns: {db, coll}, allowedIndexes: [...]}`; `None` when it allows nothing.
fn index_hint(v: &Value) -> Result<Option<Document>, CommandError> {
    let Value::Document(h) = v else {
        return Err(wrong_type("IndexHintSpec", v, "object"));
    };
    let Some(Value::Document(ns)) = h.get("ns") else {
        let msg = "BSON field 'IndexHintSpec.ns' is missing but a required field";
        return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg));
    };
    let db = ns.get("db").cloned().ok_or_else(|| {
        CommandError::new(INVALID_INDEX_HINT_DB, "invalid index hint: 'ns.db' field is missing")
    })?;
    let coll = ns.get("coll").cloned().ok_or_else(|| {
        CommandError::new(INVALID_INDEX_HINT, "invalid index hint: 'ns.coll' field is missing")
    })?;
    let Some(Value::Array(allowed)) = h.get("allowedIndexes") else {
        let msg = "BSON field 'IndexHintSpec.allowedIndexes' is missing but a required field";
        return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg));
    };
    if allowed.is_empty() {
        return Ok(None);
    }
    let ns = Document::new().with("db", db).with("coll", coll);
    Ok(Some(Document::new().with("ns", ns).with("allowedIndexes", Value::Array(allowed.clone()))))
}

/// The settings given, parsed; unknown fields are left out.
fn parse_settings(given: &Document) -> Result<Document, CommandError> {
    let mut out = Document::new();
    for key in ORDER {
        let Some(v) = given.get(key) else { continue };
        let x = match (*key, v) {
            ("indexHints", Value::Array(items)) => {
                let mut list = Vec::new();
                for i in items {
                    if let Some(h) = index_hint(i)? {
                        list.push(Value::Document(h));
                    }
                }
                Value::Array(list)
            }
            ("indexHints", one) => {
                Value::Array(index_hint(one)?.map(Value::Document).into_iter().collect())
            }
            ("queryFramework", Value::String(f)) if FRAMEWORKS.contains(&f.as_str()) => v.clone(),
            ("queryFramework", other) => {
                let got = match other {
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                let msg = format!(
                    "Invalid value for 'queryFramework': expected classic or sbe, but got {got}"
                );
                return Err(CommandError::new(BAD_VALUE, msg));
            }
            ("reject", Value::Boolean(_)) => v.clone(),
            ("reject", other) => {
                let msg = format!(
                    "Field 'reject' should be a boolean value, but found: {}",
                    crate::types::type_name(other)
                );
                return Err(CommandError::new(crate::error::TYPE_MISMATCH, msg));
            }
            ("maxTimeMS", n) => Value::Int64(max_time(n)?),
            _ => v.clone(),
        };
        out.push(*key, x);
    }
    match given.get("queryKnobs") {
        None => {}
        Some(Value::Document(k)) => {
            if let Some((name, _)) = k.iter().next() {
                let msg = format!(
                    "failed to parse query knob {name}: query knob not settable via QuerySettings: {name}"
                );
                return Err(CommandError::new(QUERY_KNOB_NOT_SETTABLE, msg));
            }
            out.push("queryKnobs", Document::new());
        }
        Some(_) => {
            let msg = "invalid parameter: expected an object (queryKnobs)";
            return Err(CommandError::new(crate::error::EXPECTED_OBJECT, msg));
        }
    }
    Ok(out)
}

/// `maxTimeMS` of a setting: a number of at least 0, truncated.
fn max_time(v: &Value) -> Result<i64, CommandError> {
    let n = match v {
        Value::Int32(n) => i64::from(*n),
        Value::Int64(n) => *n,
        // A fractional time is truncated (measured: 2.5 is 2).
        Value::Double(x) if x.is_finite() && x.abs() < i64::MAX as f64 => x.trunc() as i64,
        other => return Err(wrong_type("maxTimeMS", other, "int")),
    };
    if n < 0 {
        let msg = format!("BSON field 'maxTimeMS' value must be >= 0, actual value '{n}'");
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    Ok(n)
}

/// `old` with `new` over it, without the settings at their defaults, in order.
fn merged(old: &Document, new: &Document) -> Document {
    let mut out = Document::new();
    for key in ORDER {
        let Some(v) = new.get(key).or_else(|| old.get(key)) else { continue };
        let default = match (*key, v) {
            ("reject", Value::Boolean(false)) | ("maxTimeMS", Value::Int64(0)) => true,
            ("indexHints", Value::Array(items)) => items.is_empty(),
            _ => false,
        };
        if !default {
            out.push(*key, v.clone());
        }
    }
    out
}

/// `setQuerySettings`.
pub fn set_query_settings<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(SET_QUERY_SETTINGS, ctx.db)?;
    let given = match ctx.cmd.get(SETTINGS) {
        Some(Value::Document(s)) => s.clone(),
        Some(other) => return Err(wrong_type("setQuerySettings.settings", other, "object")),
        None => {
            let msg = "BSON field 'setQuerySettings.settings' is missing but a required field";
            return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg).parsed());
        }
    };
    let t = target(SET_QUERY_SETTINGS, ctx.first())?;
    let parsed = parse_settings(&given)?;
    if parsed.is_empty() {
        let msg = "settings field in setQuerySettings command cannot be empty";
        return Err(CommandError::new(QUERY_SETTINGS_EMPTY, msg));
    }
    let mut entries = stored(ctx.backend)?;
    let old = entries.get(&t.hash).cloned();
    let settings = merged(old.as_ref().map_or(&Document::new(), |e| &e.settings), &parsed);
    if settings.is_empty() {
        let msg = "the resulting settings cannot be empty or contain only default values";
        return Err(CommandError::new(QUERY_SETTINGS_DEFAULTS, msg));
    }
    let created = old.is_none();
    let entry = Entry {
        hash: t.hash.clone(),
        settings: settings.clone(),
        representative: old.as_ref().and_then(|e| e.representative.clone()).or(t.query),
        debug: old.as_ref().and_then(|e| e.debug.clone()).or(t.shape.map(|s| s.debug)),
    };
    let mut reply = Document::new().with(HASH, t.hash.as_str()).with(SETTINGS, settings);
    if created && let Some(r) = &entry.representative {
        reply.push(REPRESENTATIVE, r.clone());
    }
    entries.insert(t.hash, entry);
    store(ctx.backend, ctx.shared, &entries)?;
    reply.push("ok", 1.0);
    Ok(reply)
}

/// `removeQuerySettings`.
pub fn remove_query_settings<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(REMOVE_QUERY_SETTINGS, ctx.db)?;
    let t = target(REMOVE_QUERY_SETTINGS, ctx.first())?;
    let mut entries = stored(ctx.backend)?;
    if entries.remove(&t.hash).is_some() {
        store(ctx.backend, ctx.shared, &entries)?;
    }
    Ok(Document::new().with("ok", 1.0))
}

/// The documents of a `$querySettings` stage with specification `spec`.
pub fn listed<B: Backend + ?Sized>(
    backend: &mut B,
    spec: &Value,
) -> Result<Vec<Document>, CommandError> {
    let Value::Document(s) = spec else {
        return Err(wrong_type(STAGE, spec, "object"));
    };
    let mut debug = false;
    for (k, v) in s.iter() {
        match (k, v) {
            ("showDebugQueryShape", Value::Boolean(b)) => debug = *b,
            ("showDebugQueryShape", other) => {
                return Err(wrong_type("$querySettings.showDebugQueryShape", other, "bool"));
            }
            _ => {
                let msg = format!("BSON field '$querySettings.{k}' is an unknown field.");
                return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg).parsed());
            }
        }
    }
    Ok(stored(backend)?.values().map(|e| e.to_document(debug)).collect())
}

/// The settings that apply to command `cmd` on `db`: the stored ones of its shape, else its own
/// `querySettings`. `None` when it has neither or no shape that takes settings.
pub fn applying<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    db: &str,
    cmd: &Document,
) -> Result<Option<Document>, CommandError> {
    let name = cmd.iter().next().map_or("", |(k, _)| k);
    if !SETTABLE.contains(&name) {
        return Ok(None);
    }
    let own = match cmd.get(PARAMETER) {
        Some(Value::Document(s)) => Some(parse_settings(s)?),
        _ => None,
    };
    let now = backend.now_ms();
    let cached = super::lock(shared).query_settings.fresh(now);
    let entries = match cached {
        Some(e) => e,
        None => {
            let e = stored(backend)?;
            super::lock(shared).query_settings = Cache { entries: e.clone(), read_ms: Some(now) };
            e
        }
    };
    if entries.is_empty() {
        return Ok(own);
    }
    let found = query_shape::shape(db, cmd).and_then(|s| entries.get(&s.hash).cloned());
    Ok(found.map(|e| e.settings).or(own))
}

/// Refuses a command its settings reject.
pub fn check_rejected(settings: Option<&Document>) -> Result<(), CommandError> {
    match settings.and_then(|s| s.get("reject")) {
        Some(Value::Boolean(true)) => Err(CommandError::new(QUERY_REJECTED_BY_SETTINGS, REJECTED)),
        _ => Ok(()),
    }
}

/// The time limit settings give a command that has none of its own.
pub fn max_time_ms(settings: Option<&Document>) -> Option<i64> {
    match settings?.get("maxTimeMS") {
        Some(Value::Int64(n)) if *n > 0 => Some(*n),
        _ => None,
    }
}

/// The indexes settings allow on `db.coll`; `None` when they do not restrict them.
pub fn allowed_indexes(settings: Option<&Document>, db: &str, coll: &str) -> Option<Vec<Value>> {
    let Some(Value::Array(hints)) = settings?.get("indexHints") else { return None };
    hints.iter().find_map(|h| match h {
        Value::Document(h) => match (h.get("ns"), h.get("allowedIndexes")) {
            (Some(Value::Document(ns)), Some(Value::Array(allowed)))
                if ns.get("db") == Some(&Value::String(db.to_owned()))
                    && ns.get("coll") == Some(&Value::String(coll.to_owned())) =>
            {
                Some(allowed.clone())
            }
            _ => None,
        },
        _ => None,
    })
}
