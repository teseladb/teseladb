//! The primary shard of each database (018 FR-005): `enableSharding` with `primaryShard` and
//! `movePrimary` keep it, and `config.databases` shows it with its `lastMod`. TeselaDB keeps no
//! data on a database's primary (every collection lives in its ranges), so the primary is a Group
//! 0 setting, `dbPrimary.<db>` with the node and the version ([`Primary`]); a database without
//! one has the first active node, version 1.
//!
//! Messages and order of the checks of the reference router 9.0.4 of two shards (measured on
//! 02-11-2026, `c9.js` and `s2.js` in the lane's notes): `movePrimary` runs only on `admin`, needs
//! `to`, refuses `admin` and `config` (72), an invalid name (73), a database that does not exist
//! (26) before a shard that does not exist (70), and moving to the primary it has is `ok`.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};
use teseladb_meta::command::SettingChange;
use teseladb_types::NodeId;

use super::{Backend, Ctx, admin};
use crate::error::{
    CommandError, INVALID_NAMESPACE, INVALID_OPTIONS, NAMESPACE_NOT_FOUND, SHARD_NOT_FOUND,
    WRITE_CONFLICT,
};
use crate::types::wrong_type;

/// `movePrimary`.
pub const MOVE_PRIMARY: &str = "movePrimary";
/// The prefix of the settings with a database's primary, followed by its name.
pub const PREFIX: &[u8] = b"dbPrimary.";
/// How many times a change goes again when another one changed the primaries meanwhile.
const ATTEMPTS: usize = 16;

/// A database's primary as Group 0 keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Primary {
    /// The node.
    pub node: NodeId,
    /// `config.databases`' `version.lastMod`, one more at each move.
    pub last_mod: u32,
}

impl Primary {
    /// Its setting's value: the node and the version, big-endian.
    pub fn encode(&self) -> Vec<u8> {
        [self.node.0.to_be_bytes().as_slice(), self.last_mod.to_be_bytes().as_slice()].concat()
    }

    /// The primary of a setting's value.
    pub fn decode(v: &[u8]) -> Option<Primary> {
        let node = u64::from_be_bytes(v.get(..8)?.try_into().ok()?);
        let last_mod = u32::from_be_bytes(v.get(8..12)?.try_into().ok()?);
        Some(Primary { node: NodeId(node), last_mod })
    }
}

/// The setting of database `db`'s primary.
pub fn key(db: &str) -> Vec<u8> {
    [PREFIX, db.as_bytes()].concat()
}

/// Every database's primary in Group 0's settings `settings`.
pub fn of_settings(settings: &BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeMap<String, Primary> {
    settings
        .range(PREFIX.to_vec()..)
        .take_while(|(k, _)| k.starts_with(PREFIX))
        .filter_map(|(k, v)| {
            let db = std::str::from_utf8(&k[PREFIX.len()..]).ok()?;
            Some((db.to_owned(), Primary::decode(v)?))
        })
        .collect()
}

/// The node of shard `name`, if it is one of the cluster's shards.
pub fn shard_node<B: Backend + ?Sized>(
    backend: &mut B,
    name: &str,
) -> Result<Option<NodeId>, CommandError> {
    let shards = backend.config_documents(super::config_db::SHARDS)?;
    let listed = shards.iter().any(|d| matches!(d.get("_id"), Some(Value::String(s)) if s == name));
    let number = name.strip_prefix(super::SHARD_PREFIX).and_then(|n| n.parse::<u64>().ok());
    Ok(number.filter(|_| listed).map(NodeId))
}

/// The primary database `db` has: its setting, or the first shard, version 1.
fn current<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
) -> Result<(Option<Vec<u8>>, Primary), CommandError> {
    let stored = backend.group0_settings(&key(db))?.remove(&key(db));
    if let Some(p) = stored.as_deref().and_then(Primary::decode) {
        return Ok((stored, p));
    }
    let first = backend
        .config_documents(super::config_db::SHARDS)?
        .iter()
        .find_map(|d| match d.get("_id") {
            Some(Value::String(s)) => s.strip_prefix(super::SHARD_PREFIX)?.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(1);
    Ok((stored, Primary { node: NodeId(first), last_mod: 1 }))
}

/// Sets `db`'s primary to `node` (a new version when it changes; version 1 for a database
/// created with it, `created`), unless another change came first, which goes again.
fn set<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    node: NodeId,
    created: bool,
) -> Result<(), CommandError> {
    for _ in 0..ATTEMPTS {
        let (stored, now) = current(backend, db)?;
        if stored.is_some() && now.node == node {
            return Ok(());
        }
        let last_mod = match created || now.node == node {
            true => now.last_mod,
            false => now.last_mod + 1,
        };
        let value = Primary { node, last_mod }.encode();
        let change = SettingChange { key: key(db), expect: stored, value: Some(value) };
        if backend.group0_settings_write(vec![change])? {
            return Ok(());
        }
    }
    Err(CommandError::new(WRITE_CONFLICT, "the database's primary kept changing"))
}

/// `enableSharding`'s `primaryShard`: the new database's primary, refused when the database has
/// another one already. The database exists from then on.
pub fn enable_with_primary<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    db: &str,
) -> Result<(), CommandError> {
    let wanted = match ctx.cmd.get("primaryShard") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => return Err(wrong_type("enableSharding.primaryShard", v, "string")),
    };
    let could_not =
        |why: String| format!("Database {db} could not be created :: caused by :: {why}");
    let node = match &wanted {
        Some(name) => Some(shard_node(ctx.backend, name)?.ok_or_else(|| {
            CommandError::new(SHARD_NOT_FOUND, could_not(format!("Shard {name} not found")))
        })?),
        None => None,
    };
    let exists = ctx.backend.databases()?.iter().any(|d| d == db);
    if let (Some(node), true) = (node, exists) {
        let (_, now) = current(ctx.backend, db)?;
        if now.node != node {
            let name = wanted.unwrap_or_default();
            let why =
                format!("database already created on a primary which is different from {name}");
            return Err(CommandError::new(crate::error::NAMESPACE_EXISTS, could_not(why)));
        }
        return Ok(());
    }
    super::catalog::ensure_database(ctx.backend, db)?;
    match node {
        Some(node) => set(ctx.backend, db, node, true),
        None => Ok(()),
    }
}

/// `movePrimary`.
pub fn move_primary<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(MOVE_PRIMARY, ctx.db)?;
    let db = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{MOVE_PRIMARY}.{MOVE_PRIMARY}"), v, "string")),
    };
    let to = match super::args::string(ctx.cmd, MOVE_PRIMARY, "to")? {
        Some(s) => s.to_owned(),
        None => return Err(super::args::missing(MOVE_PRIMARY, "to")),
    };
    if db == admin::ADMIN_DB || db == super::config_db::CONFIG_DB {
        let msg = format!("cannot move primary of internal database {db}");
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    if db.is_empty() {
        return Err(CommandError::new(INVALID_NAMESPACE, format!("{db} is not a valid db name")));
    }
    if db.contains('.') {
        let msg = format!("'.' is an invalid character in a db name: {db}");
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    if !ctx.backend.databases()?.contains(&db) {
        return Err(CommandError::new(NAMESPACE_NOT_FOUND, format!("database {db} not found")));
    }
    // A cluster of one shard moves no data, whatever the target (`assertDataMovementAllowed` of
    // the reference server r9.0.4, before the target is looked up).
    if ctx.backend.config_documents(super::config_db::SHARDS)?.len() < 2 {
        return Err(CommandError::new(
            crate::error::ILLEGAL_OPERATION,
            "Cannot migrate data in a cluster before a second shard has been successfully added",
        ));
    }
    let node = shard_node(ctx.backend, &to)?.ok_or_else(|| {
        let msg = format!(
            "requested primary shard {to} does not exist :: caused by :: Shard {to} not found"
        );
        CommandError::new(SHARD_NOT_FOUND, msg)
    })?;
    set(ctx.backend, &db, node, false)?;
    Ok(Document::new().with("ok", 1.0))
}

/// The change that forgets dropped database `db`'s primary, if it has one.
pub fn forget<B: Backend + ?Sized>(backend: &mut B, db: &str) -> Result<(), CommandError> {
    for _ in 0..ATTEMPTS {
        let Some(stored) = backend.group0_settings(&key(db))?.remove(&key(db)) else {
            return Ok(());
        };
        let change = SettingChange { key: key(db), expect: Some(stored), value: None };
        if backend.group0_settings_write(vec![change])? {
            return Ok(());
        }
    }
    Err(CommandError::new(WRITE_CONFLICT, "the database's primary kept changing"))
}
