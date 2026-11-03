//! The `config` database as a `refrouter` shows it (007 FR-017, contract `config-db.md`): its
//! collections of the cluster's topology are generated from this node's copy of Group 0 (the
//! backend builds their documents, [`Backend::config_documents`]) and read like any collection;
//! TeselaDB cannot write them, since Group 0 is what they show (a `refrouter` takes the write and
//! its shard list breaks, measured on 9.0.4).

use teseladb_bson::{Document, Value};

use teseladb_meta::catalog::Namespace;

use super::Backend;
use crate::error::{CommandError, ILLEGAL_OPERATION};

/// The cluster's database.
pub const CONFIG_DB: &str = "config";

/// The collections of `config` generated from Group 0, in name order.
pub const GENERATED: [&str; 9] = [
    "changelog",
    "chunks",
    COLLECTIONS,
    "databases",
    "refrouter",
    super::query_analysis::QUERY_ANALYZERS,
    SHARDS,
    "tags",
    "version",
];
/// `config.collections`: one document per sharded collection with its shard key (018).
pub const COLLECTIONS: &str = "collections";
/// `config.shards`: one document per node, whose `_id` is the node's name (`super::shard_name`).
pub const SHARDS: &str = "shards";
/// The collections of `config` that `listCollections` shows, in name order: the generated ones
/// and `settings`, which can be written ([`super::config_settings`]).
pub const LISTED: [&str; 9] = [
    "changelog",
    "chunks",
    COLLECTIONS,
    "databases",
    "refrouter",
    super::config_settings::SETTINGS,
    SHARDS,
    "tags",
    "version",
];

/// The listed collections of `config` that a member of a replica set does not have (measured on
/// a replica set of `ref:9.0.4`, 02-11-2026): the replica set presentation (018) leaves them
/// out of `listCollections`.
pub const NOT_ON_A_MEMBER: [&str; 3] = ["changelog", "refrouter", super::config_settings::SETTINGS];

/// Whether `ns` is one of the generated collections of `config`.
pub fn generated(ns: &Namespace) -> bool {
    ns.db == CONFIG_DB && GENERATED.contains(&ns.coll.as_str())
}

/// Whether the node presents itself as a member of a replica set (018), whose collections of
/// `config` that show a cluster are empty and can be dropped (measured on a replica set of
/// `ref:9.0.4`, 02-11-2026).
pub fn as_member<B: Backend + ?Sized>(backend: &mut B) -> Result<bool, CommandError> {
    let mode = super::presentation::stored(backend)?.mode;
    Ok(mode == teseladb_topology::replset::PresentationMode::ReplicaSet)
}

/// The documents of `ns` when it is a generated collection of `config`: none on a member of a
/// replica set ([`as_member`]).
pub fn documents<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<Option<Vec<Document>>, CommandError> {
    if !generated(ns) && !super::config_settings::is_settings(ns) {
        return Ok(None);
    }
    if generated(ns) && as_member(backend)? {
        return Ok(Some(Vec::new()));
    }
    backend.config_documents(&ns.coll).map(Some)
}

/// The refusal of a write to a generated collection of `config` (divergence: a `refrouter` takes
/// it).
pub fn not_writable(ns: &Namespace) -> CommandError {
    CommandError::new(
        ILLEGAL_OPERATION,
        format!(
            "{}.{} shows the cluster's topology from Group 0 and cannot be written",
            ns.db, ns.coll
        ),
    )
}

/// The UUID `listCollections` gives generated collection `name`: the same in every cluster.
pub fn uuid(name: &str) -> [u8; 16] {
    let tag = LISTED.iter().position(|n| *n == name).map_or(0, |i| i as u8 + 1);
    let mut u = [0x7c; 16];
    u[6] = 0x40;
    u[8] = 0x80;
    u[15] = tag;
    u
}

/// The `listCollections` entry of generated collection `name` (`nameOnly`: its name and type).
pub fn listed(name: &str, name_only: bool) -> Document {
    let mut d = Document::new().with("name", name).with("type", "collection");
    if !name_only {
        let uuid = Value::Binary { subtype: 4, bytes: uuid(name).to_vec() };
        let id = Document::new()
            .with("v", 2)
            .with("key", Document::new().with("_id", 1))
            .with("name", "_id_");
        let options = if name == super::config_settings::SETTINGS {
            super::config_settings::options()
        } else {
            Document::new()
        };
        d.push("options", options);
        d.push("info", Document::new().with("readOnly", false).with("uuid", uuid));
        d.push("idIndex", id);
    }
    d
}
