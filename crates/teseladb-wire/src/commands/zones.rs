//! `addShardToZone`, `removeShardFromZone` and `updateZoneKeyRange` (018 FR-003): zones and their
//! shard key intervals, kept as Group 0 settings ([`teseladb_meta::zones`]), which the balancer
//! follows: the replicas of the ranges inside an interval stay on the zone's nodes. A shard is a
//! node (`node<N>`, [`super::shard_name`]).
//!
//! The replies, messages and the order of the checks are those of a `refrouter` 9.0.4 of two shards
//! (measured on 02-11-2026): an interval is stored with the shard key's missing fields as
//! `MinKey`, one of a collection without a shard key is kept as given, and removing one
//! (`zone: null`) takes the interval with exactly those bounds, if there is one.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, Value, compare_docs};
use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::command::SettingChange;
use teseladb_meta::zones::{ZoneRange, Zones, shard_key};
use teseladb_types::NodeId;

use super::names::parse_full;
use super::sharding::{ShardKey, shard_key_of};
use super::{Backend, Ctx, admin, display};
use crate::error::{
    BAD_VALUE, BOUND_FIELDS_MISMATCH, CommandError, INVALID_NAMESPACE, INVALID_OPTIONS,
    NO_SUCH_KEY, RANGE_OVERLAP_CONFLICT, SHARD_KEY_NOT_FOUND, SHARD_NOT_FOUND, TYPE_MISMATCH,
    WRITE_CONFLICT, ZONE_NOT_FOUND, ZONE_STILL_IN_USE,
};
use crate::types::wrong_type;

/// `addShardToZone`.
pub const ADD_SHARD_TO_ZONE: &str = "addShardToZone";
/// `removeShardFromZone`.
pub const REMOVE_SHARD_FROM_ZONE: &str = "removeShardFromZone";
/// `updateZoneKeyRange`.
pub const UPDATE_ZONE_KEY_RANGE: &str = "updateZoneKeyRange";
/// The prefix of every zone setting.
const PREFIX: &[u8] = b"zone.";
/// How many times a change goes again when another one changed the zones meanwhile.
const ATTEMPTS: usize = 16;

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// The zones as Group 0 has them.
pub fn zones<B: Backend + ?Sized>(backend: &mut B) -> Result<Zones, CommandError> {
    Ok(Zones::from_settings(&backend.group0_settings(PREFIX)?))
}

/// Runs `change` on the zones as they are until Group 0 takes it: `None` changes nothing.
fn change<B: Backend + ?Sized>(
    backend: &mut B,
    mut change: impl FnMut(&mut B, &Zones) -> Result<Option<Vec<SettingChange>>, CommandError>,
) -> Result<(), CommandError> {
    for _ in 0..ATTEMPTS {
        let z = zones(backend)?;
        let Some(changes) = change(backend, &z)? else { return Ok(()) };
        if backend.group0_settings_write(changes)? {
            return Ok(());
        }
    }
    Err(CommandError::new(WRITE_CONFLICT, "the zones kept changing during the command"))
}

/// The node of shard `name`, if it is one of the cluster's shards.
fn node_of<B: Backend + ?Sized>(backend: &mut B, name: &str) -> Result<NodeId, CommandError> {
    let listed = backend
        .config_documents(super::config_db::SHARDS)?
        .iter()
        .any(|d| matches!(d.get("_id"), Some(Value::String(s)) if s == name));
    let number = name.strip_prefix(super::SHARD_PREFIX).and_then(|n| n.parse::<u64>().ok());
    match number.filter(|_| listed) {
        Some(n) => Ok(NodeId(n)),
        None => Err(CommandError::new(SHARD_NOT_FOUND, format!("Shard {name} not found"))),
    }
}

/// A required string field of an IDL command.
fn required_string<'a>(
    cmd: &'a Document,
    name: &str,
    field: &str,
) -> Result<&'a str, CommandError> {
    super::args::string(cmd, name, field)?.ok_or_else(|| super::args::missing(name, field))
}

/// `addShardToZone`: the node joins the zone (it exists from then on).
pub fn add_shard_to_zone<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(ADD_SHARD_TO_ZONE, ctx.db)?;
    let shard = required_string(ctx.cmd, ADD_SHARD_TO_ZONE, ADD_SHARD_TO_ZONE)?;
    let zone = required_string(ctx.cmd, ADD_SHARD_TO_ZONE, "zone")?;
    let node = node_of(ctx.backend, shard)?;
    let order = super::new_object_id(ctx.backend, ctx.shared).to_vec();
    change(ctx.backend, |_, z| {
        if z.shards.get(&node).is_some_and(|zs| zs.iter().any(|x| x == zone)) {
            return Ok(None);
        }
        let key = shard_key(node, zone);
        Ok(Some(vec![SettingChange { key, expect: None, value: Some(order.clone()) }]))
    })?;
    Ok(ok())
}

/// `removeShardFromZone`: the node leaves the zone, unless it is its last node and the zone has
/// intervals.
pub fn remove_shard_from_zone<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(REMOVE_SHARD_FROM_ZONE, ctx.db)?;
    let shard = match ctx.cmd.get(REMOVE_SHARD_FROM_ZONE) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => return Err(wrong_type(REMOVE_SHARD_FROM_ZONE, v, "string")),
        None => String::new(),
    };
    let zone = match ctx.cmd.get("zone") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => {
            let msg = format!(
                "\"zone\" had the wrong type. Expected string, found {}",
                crate::types::type_name(v)
            );
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
        None => return Err(CommandError::new(NO_SUCH_KEY, "Missing expected field \"zone\"")),
    };
    let node = node_of(ctx.backend, &shard)?;
    change(ctx.backend, |b, z| {
        let settings = b.group0_settings(&shard_key(node, &zone))?;
        let Some(value) = settings.get(&shard_key(node, &zone)) else { return Ok(None) };
        let last = z.nodes_of(&zone).len() == 1;
        if last && z.ranges.iter().any(|r| r.zone == zone) {
            let msg = "cannot remove a shard from zone if a chunk range is associated with it";
            return Err(CommandError::new(ZONE_STILL_IN_USE, msg));
        }
        let key = shard_key(node, &zone);
        Ok(Some(vec![SettingChange { key, expect: Some(value.clone()), value: None }]))
    })?;
    Ok(ok())
}

/// A bound in messages: `{ a: 0 }`.
fn shown(d: &Document) -> String {
    display(&Value::Document(d.clone()))
}

fn compare(a: &Document, b: &Document) -> Result<Ordering, CommandError> {
    let (a, b) = (a.to_bytes()?, b.to_bytes()?);
    Ok(compare_docs(RawDoc::from_bytes(&a)?, RawDoc::from_bytes(&b)?, None))
}

/// `bound` with the fields of `key` it lacks as `MinKey`, after checking that its fields are a
/// prefix of `key` and that a hashed field holds a `NumberLong`.
fn extended(
    (which, bound): (&str, &Document),
    (ns, key): (&str, &ShardKey),
    range: (&Document, &Document),
) -> Result<Document, CommandError> {
    let names: Vec<&str> = key.key.iter().map(|(k, _)| k).collect();
    let given: Vec<&str> = bound.iter().map(|(k, _)| k).collect();
    if given.len() > names.len() || names[..given.len()] != given[..] {
        let msg = format!(
            "{which}: {} is not a prefix of the shard key {} of ns: {ns}",
            shown(bound),
            shown(&key.key)
        );
        return Err(CommandError::new(SHARD_KEY_NOT_FOUND, msg));
    }
    let mut out = Document::new();
    for (field, kind) in key.key.iter() {
        let v = bound.get(field).cloned().unwrap_or(Value::MinKey);
        let hashed = matches!(kind, Value::String(s) if s == "hashed");
        if hashed && !matches!(v, Value::Int64(_) | Value::MinKey | Value::MaxKey) {
            let msg = format!(
                "there exists a field in the range {} -->> {} not of type NumberLong, MinKey, or \
                 MaxKey which corresponds to a hashed field in the shard key pattern {}",
                shown(range.0),
                shown(range.1),
                shown(&key.key)
            );
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        }
        out.push(field, v);
    }
    Ok(out)
}

/// The key a bound starts at in `coll`'s documents: its values in the shard key's order, as they
/// are (a hashed field's bound is a hash already), before any `_id`.
fn bound_key(coll: &CollectionEntry, bound: &Document) -> Result<Vec<u8>, CommandError> {
    let holder = bound.iter().fold(Document::new(), |d, (k, v)| d.with(k, v.clone()));
    let bytes = holder.with("_id", Value::MinKey).to_bytes()?;
    let raw = RawDoc::from_bytes(&bytes)?;
    let parts: Vec<teseladb_bson::RawValue<'_>> = raw.iter().map(|(_, v)| v).collect();
    let (id, shard) = parts.split_last().ok_or_else(|| CommandError::new(BAD_VALUE, "no _id"))?;
    Ok(KeyBuilder::document_sharded(coll.tenant, coll.id, shard, *id).as_bytes().to_vec())
}

/// The keys of a zone's interval of `coll`, when it has a shard key and the bounds (extended to
/// all its fields) are of that key.
pub fn interval_keys(
    coll: &CollectionEntry,
    (min, max): (&Document, &Document),
) -> Result<Option<(Vec<u8>, Vec<u8>)>, CommandError> {
    let Some(key) = shard_key_of(coll)? else { return Ok(None) };
    // Bounds of another key (a resharding changed it) place nothing; the fields they lack are
    // `MinKey`.
    let fields = field_names(&key.key);
    let full = |d: &Document| {
        let given = field_names(d);
        (given.len() <= fields.len() && fields[..given.len()] == given[..]).then(|| {
            key.key.iter().fold(Document::new(), |out, (f, _)| {
                out.with(f, d.get(f).cloned().unwrap_or(Value::MinKey))
            })
        })
    };
    match (full(min), full(max)) {
        (Some(min), Some(max)) => Ok(Some((bound_key(coll, &min)?, bound_key(coll, &max)?))),
        _ => Ok(None),
    }
}

/// A zone's interval as Group 0 keeps it: its bounds.
fn bounds(r: &ZoneRange) -> Result<(Document, Document), CommandError> {
    let min = Document::from(RawDoc::from_bytes(&r.min)?);
    Ok((min, Document::from(RawDoc::from_bytes(&r.max)?)))
}

fn field_names(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

/// What `updateZoneKeyRange` asks.
struct Update {
    ns: String,
    min: Document,
    max: Document,
    zone: Option<String>,
}

fn parse_update(cmd: &Document) -> Result<Update, CommandError> {
    let name = UPDATE_ZONE_KEY_RANGE;
    let ns = required_string(cmd, name, name)?.to_owned();
    let min =
        super::args::doc(cmd, name, "min")?.ok_or_else(|| super::args::missing(name, "min"))?;
    let max =
        super::args::doc(cmd, name, "max")?.ok_or_else(|| super::args::missing(name, "max"))?;
    let zone = match cmd.get("zone") {
        None => return Err(super::args::missing(name, "zone")),
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => {
            let msg = format!("Could not deserialize StringOrNull with type {}", v.element_type());
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
    };
    Ok(Update { ns, min: min.clone(), max: max.clone(), zone })
}

/// `updateZoneKeyRange`: adds an interval of a zone to a namespace, or removes the one with
/// those bounds (`zone: null`).
pub fn update_zone_key_range<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(UPDATE_ZONE_KEY_RANGE, ctx.db)?;
    let u = parse_update(ctx.cmd)?;
    let ns: Namespace =
        parse_full(&u.ns).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            CommandError::new(INVALID_NAMESPACE, "invalid namespace specified for request")
        })?;
    if compare(&u.min, &u.max)? != Ordering::Less {
        let msg = format!("min: {} should be less than max: {}", shown(&u.min), shown(&u.max));
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    let coll = ctx.backend.collection(&ns)?;
    let key = coll.as_ref().map(shard_key_of).transpose()?.flatten();
    let id = super::new_object_id(ctx.backend, ctx.shared);
    let range = (&u.min, &u.max);
    change(ctx.backend, |_, z| {
        let mine: Vec<&ZoneRange> = z.ranges.iter().filter(|r| r.ns == u.ns).collect();
        if let Some(zone) = &u.zone
            && !z.exists(zone)
        {
            return Err(CommandError::new(ZONE_NOT_FOUND, format!("zone {zone} does not exist")));
        }
        let (min, max) = match &key {
            Some(k) => (
                extended(("min", &u.min), (&u.ns, k), range)?,
                extended(("max", &u.max), (&u.ns, k), range)?,
            ),
            None => (u.min.clone(), u.max.clone()),
        };
        let Some(zone) = &u.zone else {
            // The interval with exactly these bounds, if there is one.
            let mut found = None;
            for r in &mine {
                let (rmin, rmax) = bounds(r)?;
                if compare(&rmin, &min)?.is_eq() && compare(&rmax, &max)?.is_eq() {
                    found = Some(*r);
                }
            }
            return Ok(found.map(|r| {
                vec![SettingChange { key: r.key(), expect: Some(r.encode()), value: None }]
            }));
        };
        for r in &mine {
            let (rmin, rmax) = bounds(r)?;
            if key.is_none() && field_names(&rmin) != field_names(&u.min) {
                let pattern = u.min.iter().fold(Document::new(), |d, (k, _)| d.with(k, 1));
                let msg = format!(
                    "field names of bound {} do not match those of keyPattern {}",
                    shown(&rmin),
                    shown(&pattern)
                );
                return Err(CommandError::new(BOUND_FIELDS_MISMATCH, msg));
            }
            let same = compare(&rmin, &min)?.is_eq() && compare(&rmax, &max)?.is_eq();
            if same && r.zone == *zone {
                return Ok(None);
            }
            if compare(&min, &rmax)?.is_lt() && compare(&rmin, &max)?.is_lt() {
                let msg = format!(
                    "Zone range: {} -->> {}  on  {zone} is overlapping with existing: {} -->> {}  \
                     on  {}",
                    shown(&min),
                    shown(&max),
                    shown(&rmin),
                    shown(&rmax),
                    r.zone
                );
                return Err(CommandError::new(RANGE_OVERLAP_CONFLICT, msg));
            }
        }
        let keys = match &coll {
            Some(c) => interval_keys(c, (&min, &max))?,
            None => None,
        };
        let r = ZoneRange {
            ns: u.ns.clone(),
            id,
            min: min.to_bytes()?,
            max: max.to_bytes()?,
            zone: zone.clone(),
            keys,
        };
        Ok(Some(vec![SettingChange { key: r.key(), expect: None, value: Some(r.encode()) }]))
    })?;
    Ok(ok())
}

/// Gives the intervals of `ns` the keys of `coll`, its collection now (after `shardCollection`
/// or a resharding): the balancer places its ranges by them.
pub fn rekey<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    coll: &CollectionEntry,
) -> Result<(), CommandError> {
    let name = super::names::full(ns);
    change(backend, |_, z| {
        let mut changes = Vec::new();
        for r in z.ranges.iter().filter(|r| r.ns == name) {
            let (min, max) = bounds(r)?;
            let keys = interval_keys(coll, (&min, &max))?;
            if keys != r.keys {
                let now = ZoneRange { keys, ..r.clone() };
                changes.push(SettingChange {
                    key: r.key(),
                    expect: Some(r.encode()),
                    value: Some(now.encode()),
                });
            }
        }
        Ok((!changes.is_empty()).then_some(changes))
    })
}
