//! The query analyzer (018 FR-017): `configureQueryAnalyzer` keeps, in Group 0, which
//! collections have their queries sampled (`config.queryAnalyzers`); each node samples the reads
//! and writes it runs on them at most `samplesPerSecond` times a second, and
//! `$listSampledQueries` lists the samples. `analyzeShardKey` reads them
//! ([`super::shard_key_analysis`]).
//!
//! A `refrouter` 9.0.4 keeps the samples in `config.sampledQueries`, a week each; here each node
//! keeps the last [`KEPT`] it took, as long as a week, and `$listSampledQueries` lists the ones
//! of the node it runs on, the newest first. The replies, messages and the form of each sample
//! are those of a `refrouter` 9.0.4 of two shards (measured on 02-11-2026, `t6.js` to `t8.js` in the
//! lane's notes).

use std::collections::{BTreeMap, VecDeque};

use teseladb_meta::catalog::Namespace;
use teseladb_meta::command::SettingChange;
use teseladb_bson::{Document, RawDoc, Value};

use super::names::{full, parse_full};
use super::{Backend, Ctx, admin, lock};
use crate::error::{
    BAD_VALUE, CommandError, INVALID_NAMESPACE, INVALID_OPTIONS, NAMESPACE_NOT_FOUND,
    WRITE_CONFLICT,
};
use crate::types::{wrong_type, wrong_types};

/// `configureQueryAnalyzer`.
pub const CONFIGURE_QUERY_ANALYZER: &str = "configureQueryAnalyzer";
/// `config.queryAnalyzers`.
pub const QUERY_ANALYZERS: &str = "queryAnalyzers";
/// Where Group 0 keeps a collection's configuration: this prefix and the namespace.
const PREFIX: &[u8] = b"queryAnalyzer.";
const MODE_FULL: &str = "full";
const MODE_OFF: &str = "off";
const SAMPLES_PER_SECOND: &str = "samplesPerSecond";
/// The most samples a second (measured).
const MAX_SAMPLES_PER_SECOND: f64 = 50.0;
/// Samples a node keeps.
const KEPT: usize = 10_000;
/// How long a sample is kept: a week (`expireAt`, measured).
const WEEK_MS: i64 = 7 * 24 * 3_600 * 1_000;
/// Times a configuration change goes again when another one came first.
const ATTEMPTS: usize = 16;
/// The commands whose runs are sampled, as `cmdName` names them.
pub const READS: [&str; 4] = ["find", "aggregate", "count", "distinct"];
/// The writes sampled.
pub const WRITES: [&str; 3] = ["update", "delete", "findAndModify"];

/// The samples a node took and when it last took one of each collection.
#[derive(Debug, Default)]
pub struct Samples {
    taken: VecDeque<Document>,
    last_ms: BTreeMap<String, u64>,
}

impl Samples {
    /// The samples kept until `now_ms`, the newest first.
    pub fn listed(&self, now_ms: i64) -> Vec<Document> {
        self.taken
            .iter()
            .rev()
            .filter(|d| matches!(d.get("expireAt"), Some(Value::DateTime(t)) if *t > now_ms))
            .cloned()
            .collect()
    }
}

fn key(ns: &str) -> Vec<u8> {
    [PREFIX, ns.as_bytes()].concat()
}

/// The `config.queryAnalyzers` documents in Group 0's settings `settings`.
pub fn of_settings(settings: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<Document> {
    settings
        .range(PREFIX.to_vec()..)
        .take_while(|(k, _)| k.starts_with(PREFIX))
        .filter_map(|(_, v)| RawDoc::from_bytes(v).ok().map(Document::from))
        .collect()
}

/// The configuration of `ns` in Group 0, with its stored bytes.
fn configuration<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &str,
) -> Result<Option<(Vec<u8>, Document)>, CommandError> {
    let k = key(ns);
    let Some(v) = backend.group0_settings(&k)?.remove(&k) else { return Ok(None) };
    let d = Document::from(RawDoc::from_bytes(&v)?);
    Ok(Some((v, d)))
}

/// `{mode, samplesPerSecond}` of a stored configuration.
fn shown(d: &Document) -> Document {
    // A stored configuration always has both: off keeps the rate it had.
    let field = |f: &str| d.get(f).cloned().unwrap_or(Value::Null);
    Document::new().with("mode", field("mode")).with(SAMPLES_PER_SECOND, field(SAMPLES_PER_SECOND))
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(x) => Some(*x),
        Value::Decimal128(d) => Some(teseladb_mql::decimal_to_f64(*d)),
        _ => None,
    }
}

/// `configureQueryAnalyzer`: starts (`full`) or stops (`off`) the sampling of a collection's
/// queries.
pub fn configure_query_analyzer<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = CONFIGURE_QUERY_ANALYZER;
    let text = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{name}.{name}"), v, "string")),
    };
    let mode = match ctx.cmd.get("mode") {
        Some(Value::String(m)) if m == MODE_FULL || m == MODE_OFF => m.clone(),
        Some(Value::String(m)) => {
            let msg =
                format!("Enumeration value '{m}' for field '{name}.mode' is not a valid value.");
            return Err(CommandError::new(BAD_VALUE, msg));
        }
        Some(v) => return Err(wrong_type(&format!("{name}.mode"), v, "string")),
        None => return Err(super::args::missing(name, "mode")),
    };
    let rate = match ctx.cmd.get(SAMPLES_PER_SECOND) {
        Some(v) if number(v).is_some() => Some(v.clone()),
        Some(v) => {
            let types = ["double", "long", "int", "decimal"];
            return Err(wrong_types(&format!("{name}.{SAMPLES_PER_SECOND}"), v, &types));
        }
        None => None,
    };
    admin::admin_only(name, ctx.db)?;
    check_rate(&mode, rate.as_ref())?;
    let ns = analyzed_ns(&text)?;
    let coll = ctx.backend.collection(&ns)?.ok_or_else(no_namespace)?;
    let now = i64::try_from(ctx.backend.now_ms()).unwrap_or(i64::MAX);
    for _ in 0..ATTEMPTS {
        let stored = configuration(ctx.backend, &text)?;
        let mut reply = Document::new();
        let next = match (&stored, mode.as_str()) {
            (None, MODE_OFF) => {
                reply.push("newConfiguration", Document::new().with("mode", MODE_OFF));
                reply.push("ok", 1.0);
                return Ok(reply);
            }
            (Some((_, old)), MODE_OFF) => {
                let mut d = old.clone();
                d.insert("mode", MODE_OFF);
                d.insert("stopTime", Value::DateTime(now));
                d
            }
            _ => Document::new()
                .with("_id", text.as_str())
                .with("collUuid", Value::Binary { subtype: 4, bytes: coll.uuid.to_vec() })
                .with("mode", MODE_FULL)
                .with(SAMPLES_PER_SECOND, rate.clone().unwrap_or(Value::Null))
                .with("startTime", Value::DateTime(now)),
        };
        // `newConfiguration` of `off` shows the mode alone (measured).
        let new = match mode.as_str() {
            MODE_OFF => Document::new().with("mode", MODE_OFF),
            _ => shown(&next),
        };
        reply.push("newConfiguration", new);
        if let Some((_, old)) = &stored {
            reply.push("oldConfiguration", shown(old));
        }
        reply.push("ok", 1.0);
        let change = SettingChange {
            key: key(&text),
            expect: stored.map(|(b, _)| b),
            value: Some(next.to_bytes()?),
        };
        if ctx.backend.group0_settings_write(vec![change])? {
            return Ok(reply);
        }
    }
    Err(CommandError::new(WRITE_CONFLICT, "the query analyzer's configuration kept changing"))
}

/// The checks of `samplesPerSecond` against `mode` (measured order).
fn check_rate(mode: &str, rate: Option<&Value>) -> Result<(), CommandError> {
    let fail = |m: &str| Err(CommandError::new(INVALID_OPTIONS, m.to_owned()));
    match (mode, rate.and_then(number)) {
        (MODE_OFF, Some(_)) => fail("Cannot specify 'samplesPerSecond' when 'mode' is \"off\""),
        (MODE_OFF, None) => Ok(()),
        (_, None) => fail("'samplesPerSecond' must be specified when 'mode' is not \"off\""),
        (_, Some(r)) if r <= 0.0 => fail("'samplesPerSecond' must be greater than 0"),
        (_, Some(r)) if r > MAX_SAMPLES_PER_SECOND => {
            fail("'samplesPerSecond' must be less than or equal to 50")
        }
        _ => Ok(()),
    }
}

/// The namespace of `configureQueryAnalyzer` and `analyzeShardKey`.
pub fn analyzed_ns(text: &str) -> Result<Namespace, CommandError> {
    parse_full(text)
        .filter(|n| !n.db.is_empty() && !n.coll.is_empty())
        .ok_or_else(|| CommandError::new(INVALID_NAMESPACE, "The namespace is invalid"))
}

/// The refusal of a namespace with no collection.
pub fn no_namespace() -> CommandError {
    CommandError::new(NAMESPACE_NOT_FOUND, "The namespace does not exist")
}

/// Samples the command of `ctx` if it is a read or a write of a collection whose analyzer is
/// `full` and the collection's last sample on this node is at least `1 / samplesPerSecond` old.
pub fn sample<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<(), CommandError> {
    let name = ctx.name().to_owned();
    if !READS.contains(&name.as_str()) && !WRITES.contains(&name.as_str()) {
        return Ok(());
    }
    let Value::String(coll) = ctx.first() else { return Ok(()) };
    let ns_text = format!("{}.{coll}", ctx.db);
    if ctx.backend.group0_settings(PREFIX)?.is_empty() {
        return Ok(());
    }
    let Some((_, conf)) = configuration(ctx.backend, &ns_text)? else { return Ok(()) };
    if conf.get("mode") != Some(&Value::from(MODE_FULL)) {
        return Ok(());
    }
    let rate = conf.get(SAMPLES_PER_SECOND).and_then(number).unwrap_or(0.0);
    let now = ctx.backend.now_ms();
    let every = (1_000.0 / rate.max(f64::MIN_POSITIVE)) as u64;
    {
        let s = lock(ctx.shared);
        if s.sampled_queries.last_ms.get(&ns_text).is_some_and(|t| now < t + every) {
            return Ok(());
        }
    }
    let id = super::new_uuid(ctx.backend);
    let sample_id = Value::Binary { subtype: 4, bytes: id.to_vec() };
    let cmd = sampled_command(&name, ctx.cmd, ctx.db, &sample_id);
    let uuid = conf.get("collUuid").cloned().unwrap_or(Value::Null);
    let at = i64::try_from(now).unwrap_or(i64::MAX);
    let doc = Document::new()
        .with("_id", sample_id)
        .with("ns", ns_text.as_str())
        .with("collectionUuid", uuid)
        .with("cmdName", name.as_str())
        .with("cmd", cmd)
        .with("expireAt", Value::DateTime(at.saturating_add(WEEK_MS)));
    let mut s = lock(ctx.shared);
    s.sampled_queries.last_ms.insert(ns_text, now);
    s.sampled_queries.taken.push_back(doc);
    while s.sampled_queries.taken.len() > KEPT {
        s.sampled_queries.taken.pop_front();
    }
    Ok(())
}

/// The `cmd` of a sample, as a `refrouter` 9.0.4 stores it: a read's filter and collation, a
/// write's command with its first statement and the sample's id.
fn sampled_command(name: &str, cmd: &Document, db: &str, id: &Value) -> Document {
    let get = |k: &str| cmd.get(k).cloned();
    let empty = || Value::Document(Document::new());
    match name {
        "find" | "count" => Document::new()
            .with(
                "filter",
                get(if name == "find" { "filter" } else { "query" }).unwrap_or_else(empty),
            )
            .with("collation", get("collation").unwrap_or_else(empty)),
        "distinct" => Document::new()
            .with("filter", get("query").unwrap_or_else(empty))
            .with("collation", get("collation").unwrap_or_else(empty)),
        "aggregate" => {
            let first = match cmd.get("pipeline") {
                Some(Value::Array(stages)) => stages.first().and_then(|s| match s {
                    Value::Document(d) => d.get("$match").cloned(),
                    _ => None,
                }),
                _ => None,
            };
            Document::new()
                .with("filter", first.unwrap_or_else(empty))
                .with(
                    "collation",
                    get("collation").unwrap_or_else(|| {
                        Value::Document(Document::new().with("locale", "simple"))
                    }),
                )
                .with("let", get("let").unwrap_or_else(empty))
        }
        "findAndModify" => {
            let mut d = Document::new().with(name, get(name).unwrap_or(Value::Null));
            for (k, default) in [
                ("query", empty()),
                ("remove", Value::Boolean(false)),
                ("update", Value::Null),
                ("upsert", Value::Boolean(false)),
                ("new", Value::Boolean(false)),
            ] {
                match get(k) {
                    Some(v) => d.push(k, v),
                    None if k != "update" => d.push(k, default),
                    None => {}
                }
            }
            d.with("sampleId", id.clone()).with("$db", db)
        }
        _ => {
            let list = if name == "update" { "updates" } else { "deletes" };
            let first = match cmd.get(list) {
                Some(Value::Array(a)) => a.first().cloned(),
                _ => None,
            };
            let statement = match first {
                Some(Value::Document(mut s)) => {
                    if name == "update" {
                        for (k, v) in [("multi", false), ("upsert", false)] {
                            if s.get(k).is_none() {
                                s.push(k, v);
                            }
                        }
                    }
                    Value::Document(s.with("sampleId", id.clone()))
                }
                _ => Value::Null,
            };
            Document::new()
                .with(name, get(name).unwrap_or(Value::Null))
                .with("bypassDocumentValidation", false)
                .with("ordered", get("ordered").unwrap_or(Value::Boolean(true)))
                .with(list, Value::Array(vec![statement]))
                .with("$db", db)
        }
    }
}

/// `$listSampledQueries`: the samples this node keeps, of `namespace` when the stage names one.
pub fn listed(shared: &super::Shared, spec: &Document, now_ms: u64) -> Vec<Document> {
    // The stage's parser took only a string.
    let wanted = match spec.get("namespace") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let all = lock(shared).sampled_queries.listed(now);
    all.into_iter()
        .filter(|d| wanted.as_ref().is_none_or(|w| d.get("ns") == Some(&Value::from(w.as_str()))))
        .collect()
}

/// The samples of collection `ns` with UUID `uuid` this node keeps.
pub fn samples_of(
    shared: &super::Shared,
    ns: &Namespace,
    uuid: [u8; 16],
    now_ms: u64,
) -> Vec<Document> {
    let text = full(ns);
    let uuid = Value::Binary { subtype: 4, bytes: uuid.to_vec() };
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    lock(shared)
        .sampled_queries
        .listed(now)
        .into_iter()
        .filter(|d| {
            d.get("ns") == Some(&Value::from(text.as_str()))
                && d.get("collectionUuid") == Some(&uuid)
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn numbers_of_every_numeric_type() {
        assert_eq!(number(&Value::Int32(2)), Some(2.0));
        assert_eq!(number(&Value::Int64(3)), Some(3.0));
        assert_eq!(number(&Value::Double(0.5)), Some(0.5));
        let dec = teseladb_mql::to_decimal128(&Value::Double(1.5)).expect("a decimal");
        assert_eq!(number(&Value::Decimal128(dec)), Some(1.5));
        assert_eq!(number(&Value::from("x")), None);
    }

    #[test]
    fn a_sample_keeps_what_the_command_has() {
        let id = Value::Int32(7);
        let stage = |v: Value| {
            Document::new().with("aggregate", "c").with("pipeline", Value::Array(vec![v]))
        };
        let other = sampled_command("aggregate", &stage(Value::Int32(1)), "db", &id);
        assert_eq!(other.get("filter"), Some(&Value::Document(Document::new())));
        let no_list = Document::new().with("aggregate", "c");
        let s = sampled_command("aggregate", &no_list, "db", &id);
        assert_eq!(s.get("filter"), Some(&Value::Document(Document::new())));
        let fam = Document::new().with("findAndModify", "c").with("remove", true);
        let s = sampled_command("findAndModify", &fam, "db", &id);
        assert_eq!(s.get("update"), None);
        assert_eq!(s.get("remove"), Some(&Value::Boolean(true)));
        let bad = Document::new().with("update", "c").with("updates", 1);
        let s = sampled_command("update", &bad, "db", &id);
        assert_eq!(s.get("updates"), Some(&Value::Array(vec![Value::Null])));
        let del = Document::new()
            .with("delete", "c")
            .with("deletes", Value::Array(vec![Value::Document(Document::new())]));
        let s = sampled_command("delete", &del, "db", &id);
        let Some(Value::Array(a)) = s.get("deletes") else { panic!("{s:?}") };
        assert_eq!(a[0], Value::Document(Document::new().with("sampleId", id.clone())));
    }
}
