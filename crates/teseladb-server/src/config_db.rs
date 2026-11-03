//! The collections of `config` that show the cluster (007 FR-017, contract `config-db.md`),
//! generated from this node's copy of Group 0 with the fields of a `refrouter` 9.0.4 (measured):
//!
//! - `shards`: one document per node that is not removed, `_id` its shard name (`node<n>`, the
//!   one of a transaction's `recoveryToken`), `host` its client address, `state` 1 and `node` its
//!   number; a node being drained says `draining: true`;
//! - `databases`: one per database of the catalog, in creation order, with `primary` the first
//!   active node (018 gives each database its own) and `version` with the mark that created it;
//! - `refrouter`: one per node that is not removed, as a router;
//! - `version`: `{_id: 1, clusterId}`, the cluster's identifier as an ObjectId;
//! - `settings`: the documents Group 0 keeps for `config.settings` (the defaults of
//!   `setDefaultRWConcern` among them), which [`teseladb_wire::commands::config_settings`]
//!   writes;
//! - `changelog`: the splits, merges, replica changes and core changes Group 0 recorded (T045),
//!   with the fields of a `refrouter` 9.0.4 (measured): a split is `split` with its range `before`
//!   and its `left` and `right` halves, a merge `merge` with the `min` and `max` of the range
//!   it left, a replica change `moveChunk.commit` with the shards `added` and `removed`, and a
//!   core change `relocate` with its `core`; a range's bounds are its keys as binary data, the
//!   ranges of TeselaDB not being chunks of a shard key (D-008);
//! - `collections`, `chunks` and `tags`: the collections with a shard key, their chunks and
//!   zones (018); `tags` lists the zones' intervals (`_id`, `ns`, `min`, `max`, `tag`, measured on
//!   9.0.4) and `shards` the zones of each node in `tags`, in the order they were added.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::changelog::{Change, ChangeKind};
use teseladb_meta::command::{NodeState, SettingChange};
use teseladb_meta::group0::Group0Sm;
use teseladb_meta::meta2::Meta2Sm;
use teseladb_meta::zones::Zones;
use teseladb_types::NodeId;
use teseladb_wire::commands::cluster_admin::RW_DEFAULTS;
use teseladb_wire::commands::config_db::COLLECTIONS;
use teseladb_wire::commands::config_settings::{SETTINGS, SettingsWrite};
use teseladb_wire::commands::diagnostics::ANNOUNCED_VERSION;
use teseladb_wire::commands::shard_name;
use teseladb_wire::commands::sharding;
use teseladb_wire::error::{BAD_VALUE, CommandError};

use teseladb_meta::config_settings::PREFIX as SETTINGS_PREFIX;

use crate::backend::PARAMETER_PREFIX;

/// The documents of collection `coll` of `config`, in their order, from this node's copies of
/// Group 0 and of the `Meta2` ranges (007 FR-014: they record the changes of the descriptors
/// they hold).
pub fn documents_with(sm: &Group0Sm, meta2: &[&Meta2Sm], coll: &str) -> Vec<Document> {
    if coll == CHUNKS {
        return chunks(&crate::ranges::Ranges::of_all(sm, meta2).descs, sm);
    }
    if coll != "changelog" {
        return documents(sm, coll);
    }
    let mut all: Vec<&Change> = sm.changelog.changes.iter().collect();
    all.extend(meta2.iter().flat_map(|m| m.changelog.changes.iter()));
    all.sort_by_key(|c| c.time_ms);
    all.into_iter().map(|c| change(sm, c)).collect()
}

/// The documents of collection `coll` of `config`, in their order.
pub fn documents(sm: &Group0Sm, coll: &str) -> Vec<Document> {
    match coll {
        "shards" => shards(sm),
        "databases" => databases(sm),
        COLLECTIONS => sharded_collections(sm),
        "refrouter" => routers(sm),
        teseladb_wire::commands::query_analysis::QUERY_ANALYZERS => {
            teseladb_wire::commands::query_analysis::of_settings(&sm.settings)
        }
        "version" => vec![version(sm)],
        SETTINGS => settings(sm),
        "tags" => tags(sm),
        CHUNKS => chunks(&sm.ranges.values().cloned().collect::<Vec<_>>(), sm),
        "changelog" => sm.changelog.changes.iter().map(|c| change(sm, c)).collect(),
        _ => Vec::new(),
    }
}

/// `config.chunks`.
const CHUNKS: &str = "chunks";

/// The replica set presentation as node `me` takes part in it (018 FR-007), from this node's
/// copy of Group 0: the set over the live nodes and the member `me` is; `None` in the `refrouter`
/// presentation, or when `me` is not a member.
pub fn replica_member(
    sm: &Group0Sm,
    me: NodeId,
) -> Option<(teseladb_topology::replset::ReplicaSet, teseladb_topology::replset::MemberId)> {
    let p = presentation(sm)?;
    if p.mode != teseladb_topology::replset::PresentationMode::ReplicaSet {
        return None;
    }
    let hosts: Vec<String> = live(sm).map(|n| text(&n.addrs.native_addr)).collect();
    let rs = p.replica_set(&hosts).ok()?;
    let host = text(&sm.nodes.get(&me)?.addrs.native_addr);
    let member = rs.member(&host)?;
    Some((rs, member))
}

/// The Group 0 setting of the cluster parameter `topologyPresentation`, read at every tick
/// without building its key.
const PRESENTATION_KEY: &[u8] = b"clusterParameter.topologyPresentation";

/// The stored `topologyPresentation`, if it reads.
pub fn presentation(sm: &Group0Sm) -> Option<teseladb_wire::commands::presentation::Presentation> {
    use teseladb_wire::commands::presentation::Presentation;
    let bytes = sm.settings.get(PRESENTATION_KEY)?;
    Presentation::from_document(&Document::from(RawDoc::from_bytes(bytes).ok()?)).ok()
}

/// `config.chunks`: the chunks of every collection with a shard key over ranges `descs`, with
/// their bounds in shard key values (018 FR-006).
fn chunks(descs: &[teseladb_meta::group0::RangeDesc], sm: &Group0Sm) -> Vec<Document> {
    let entries: Vec<teseladb_router::table::RouteEntry> =
        descs.iter().map(teseladb_router::table::RouteEntry::from_desc).collect();
    teseladb_wire::commands::chunks::config_chunks(&entries, sm.catalog.collections.iter())
}

/// The key of the Group 0 setting that keeps the document of `config.settings` with `_id` `id`.
fn setting_key(id: &str) -> Vec<u8> {
    match id == RW_DEFAULTS {
        true => [PARAMETER_PREFIX, RW_DEFAULTS.as_bytes()].concat(),
        false => [SETTINGS_PREFIX, id.as_bytes()].concat(),
    }
}

/// A document as Group 0 keeps it: the defaults without their `_id`, the others whole.
fn stored(id: &str, doc: &Document) -> Result<Vec<u8>, CommandError> {
    if id != RW_DEFAULTS {
        return Ok(doc.to_bytes()?);
    }
    let mut d = doc.clone();
    d.remove("_id");
    Ok(d.to_bytes()?)
}

/// The Group 0 change of a document of `config.settings` that a command changed.
pub fn setting_change(w: &SettingsWrite) -> Result<SettingChange, CommandError> {
    let Value::String(id) = &w.id else {
        return Err(CommandError::new(BAD_VALUE, "a document of config.settings without a name"));
    };
    let as_stored = |d: &Option<Document>| d.as_ref().map(|d| stored(id, d)).transpose();
    Ok(SettingChange {
        key: setting_key(id),
        expect: as_stored(&w.read)?,
        value: as_stored(&w.now)?,
    })
}

fn settings(sm: &Group0Sm) -> Vec<Document> {
    let doc = |id: &str, bytes: &[u8]| -> Option<Document> {
        let d = Document::from(RawDoc::from_bytes(bytes).ok()?);
        Some(match id == RW_DEFAULTS {
            true => {
                let mut with_id = Document::new().with("_id", RW_DEFAULTS);
                for (k, v) in d.iter() {
                    with_id.push(k, v.clone());
                }
                with_id
            }
            false => d,
        })
    };
    let defaults = sm.settings.get(&setting_key(RW_DEFAULTS)).and_then(|b| doc(RW_DEFAULTS, b));
    let others = sm.settings.iter().filter_map(|(k, v)| {
        let id = std::str::from_utf8(k.strip_prefix(SETTINGS_PREFIX)?).ok()?;
        doc(id, v)
    });
    defaults.into_iter().chain(others).collect()
}

/// The bound `key` of a range: its key as binary data, `MaxKey` for the end of the last range.
fn bound(key: &[u8], end: bool) -> Value {
    match end && key.is_empty() {
        true => Value::MaxKey,
        false => Value::Binary { subtype: 0, bytes: key.to_vec() },
    }
}

fn span(start: &[u8], end: &[u8]) -> Document {
    Document::new().with("min", bound(start, false)).with("max", bound(end, true))
}

fn shard_of(n: Option<NodeId>) -> String {
    n.map_or_else(|| "config".to_owned(), |n| shard_name(n.0))
}

/// The `config.changelog` document of change `c`.
fn change(sm: &Group0Sm, c: &Change) -> Document {
    let server = sm
        .nodes
        .get(&c.leader.unwrap_or(NodeId(0)))
        .map_or_else(|| "config".to_owned(), |n| text(&n.addrs.name));
    let time = crate::log::timestamp(c.time_ms).replace('Z', "+00:00");
    let id = format!("{server}-{time}-{:08x}{:016x}", c.time_ms / 1_000, c.seq);
    let owner = shard_of(c.leader);
    let (what, details) = match &c.kind {
        ChangeKind::Split { at, .. } => (
            "split",
            Document::new()
                .with("before", span(&c.start, &c.end))
                .with("left", span(&c.start, at))
                .with("right", span(at, &c.end))
                .with("owningShard", owner),
        ),
        ChangeKind::Merge { .. } => (
            "merge",
            Document::new()
                .with("owningShard", owner)
                .with("min", bound(&c.start, false))
                .with("max", bound(&c.end, true))
                .with("numChunks", 2),
        ),
        ChangeKind::Replicas { added, removed } => {
            let names = |ns: &[NodeId]| -> Vec<Value> {
                ns.iter().map(|n| Value::from(shard_name(n.0))).collect()
            };
            let d =
                span(&c.start, &c.end).with("added", names(added)).with("removed", names(removed));
            ("moveChunk.commit", d)
        }
        ChangeKind::Core { core } => (
            "relocate",
            span(&c.start, &c.end).with("owningShard", owner).with("core", i32::from(*core)),
        ),
    };
    let ms = i64::try_from(c.time_ms).unwrap_or(i64::MAX);
    Document::new()
        .with("_id", id)
        .with("server", server)
        .with("shard", "config")
        .with("clientAddr", "")
        .with("time", Value::DateTime(ms))
        .with("what", what)
        .with("ns", "")
        .with("details", details)
}

fn live(sm: &Group0Sm) -> impl Iterator<Item = &teseladb_meta::group0::NodeInfo> {
    sm.nodes.values().filter(|n| n.state != NodeState::Removed)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn shards(sm: &Group0Sm) -> Vec<Document> {
    let zones = Zones::from_settings(&sm.settings);
    live(sm)
        .map(|n| {
            let mut d = Document::new()
                .with("_id", shard_name(n.id.0))
                .with("host", text(&n.addrs.native_addr))
                .with("state", 1)
                .with("node", n.id.0 as i64);
            if n.state == NodeState::Draining {
                d.push("draining", true);
            }
            if let Some(tags) = zones.shards.get(&n.id).filter(|t| !t.is_empty()) {
                d.push(
                    "tags",
                    Value::Array(tags.iter().map(|t| Value::from(t.as_str())).collect()),
                );
            }
            d
        })
        .collect()
}

/// `config.tags`: every zone's interval.
fn tags(sm: &Group0Sm) -> Vec<Document> {
    let doc = |b: &[u8]| RawDoc::from_bytes(b).map(Document::from).unwrap_or_default();
    Zones::from_settings(&sm.settings)
        .ranges
        .iter()
        .map(|r| {
            Document::new()
                .with("_id", Value::ObjectId(r.id))
                .with("ns", r.ns.as_str())
                .with("min", doc(&r.min))
                .with("max", doc(&r.max))
                .with("tag", r.zone.as_str())
        })
        .collect()
}

fn databases(sm: &Group0Sm) -> Vec<Document> {
    let first = sm.active_nodes().first().map_or_else(|| shard_name(1), |n| shard_name(n.0));
    let primaries = teseladb_wire::commands::primaries::of_settings(&sm.settings);
    sm.catalog
        .databases_by_creation()
        .into_iter()
        .map(|db| {
            let (primary, last_mod) = match primaries.get(&db) {
                Some(p) => (shard_name(p.node.0), i32::try_from(p.last_mod).unwrap_or(i32::MAX)),
                None => (first.clone(), 1),
            };
            let zero = teseladb_hlc::Hlc { physical: 0, logical: 0 };
            let mark = sm.catalog.db_marks.get(&db).copied().unwrap_or(zero);
            let time = u32::try_from(mark.physical / 1_000).unwrap_or(u32::MAX);
            let version = Document::new()
                .with("timestamp", Value::Timestamp { time, increment: mark.logical })
                .with("lastMod", last_mod);
            Document::new().with("_id", db).with("primary", primary).with("version", version)
        })
        .collect()
}

/// The collections with a shard key (018 FR-001), in namespace order.
fn sharded_collections(sm: &Group0Sm) -> Vec<Document> {
    sm.catalog
        .collections
        .iter()
        .filter_map(|(ns, c)| sharding::tracked_entry(ns, c).ok().flatten())
        .collect()
}

fn routers(sm: &Group0Sm) -> Vec<Document> {
    let now = i64::try_from(sm.cluster_time_ms).unwrap_or(i64::MAX);
    live(sm)
        .map(|n| {
            Document::new()
                .with("_id", text(&n.addrs.native_addr))
                .with("advisoryHostFQDNs", Value::Array(Vec::new()))
                .with("refVersion", ANNOUNCED_VERSION)
                .with("ping", Value::DateTime(now))
                .with("waiting", true)
        })
        .collect()
}

fn version(sm: &Group0Sm) -> Document {
    let mut id = [0u8; 12];
    id.copy_from_slice(&sm.cluster_id[..12]);
    Document::new().with("_id", 1).with("clusterId", Value::ObjectId(id))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_meta::catalog::{CatalogCommand, Namespace};
    use teseladb_meta::command::{Group0Command, NodeAddrs};
    use teseladb_meta::group0::Group0Result;

    fn addrs(i: u64) -> NodeAddrs {
        NodeAddrs {
            name: format!("n{i}").into_bytes(),
            peer_addr: format!("n{i}:7421").into_bytes(),
            http_addr: format!("n{i}:8080").into_bytes(),
            native_addr: format!("n{i}:27017").into_bytes(),
            cores: 2,
        }
    }

    fn cluster() -> Group0Sm {
        let mut sm = Group0Sm::default();
        let boot = Group0Command::Bootstrap {
            cluster_id: [9; 16],
            first_node: addrs(1),
            cert_fingerprint: [1; 32],
        };
        sm.apply_command(1, &boot);
        for i in 2..=3u64 {
            let add = Group0Command::AddNode { node: addrs(i), cert_fingerprint: [i as u8; 32] };
            sm.apply_command(i, &add);
        }
        let ns = Namespace { db: "app".into(), coll: "c".into() };
        let create =
            CatalogCommand::Create { ns, uuid: [3; 16], options: Vec::new(), if_absent: false };
        sm.apply_command(4, &Group0Command::Catalog(create));
        sm
    }

    #[test]
    fn a_collection_with_a_shard_key_is_listed_in_config_collections() {
        let mut sm = cluster();
        let shard_key =
            Document::new().with("key", Document::new().with("tienda", 1)).with("unique", false);
        let options = Document::new().with(sharding::SHARD_KEY, shard_key).to_bytes().unwrap();
        let ns = Namespace { db: "app".into(), coll: "s".into() };
        let create = CatalogCommand::Create { ns, uuid: [5; 16], options, if_absent: false };
        sm.apply_command(5, &Group0Command::Catalog(create));
        let listed = documents(&sm, COLLECTIONS);
        assert_eq!(listed.len(), 1, "app.c has no shard key");
        assert_eq!(listed[0].get("_id"), Some(&Value::from("app.s")));
        assert_eq!(listed[0].get("key"), Some(&Value::Document(Document::new().with("tienda", 1))));
        assert_eq!(listed[0].get("unique"), Some(&Value::Boolean(false)));
    }

    #[test]
    fn each_node_finds_its_member_in_the_replica_set_presentation() {
        use teseladb_wire::commands::presentation::Presentation;
        let mut sm = cluster();
        assert!(presentation(&sm).is_none());
        assert!(replica_member(&sm, NodeId(1)).is_none(), "a refrouter without the parameter");
        let mut p = Presentation {
            mode: teseladb_topology::replset::PresentationMode::ReplicaSet,
            set_name: "rs0".into(),
            primary: Some("n2:27017".into()),
            ..Presentation::default()
        };
        sm.settings.insert(PRESENTATION_KEY.to_vec(), p.to_document().to_bytes().unwrap());
        let (rs, me) = replica_member(&sm, NodeId(1)).expect("a member");
        assert_eq!(rs.members().len(), 3);
        assert_eq!(rs.primary(), Some("n2:27017"));
        assert_eq!(rs.member("n1:27017"), Some(me));
        assert!(replica_member(&sm, NodeId(9)).is_none(), "not a node of the cluster");
        p.mode = teseladb_topology::replset::PresentationMode::RefRouter;
        sm.settings.insert(PRESENTATION_KEY.to_vec(), p.to_document().to_bytes().unwrap());
        assert!(replica_member(&sm, NodeId(1)).is_none());
        assert_eq!(presentation(&sm).map(|p| p.term), Some(1));
    }

    #[test]
    fn the_presentation_key_is_the_parameter_setting() {
        let name = teseladb_wire::commands::presentation::TOPOLOGY_PRESENTATION;
        assert_eq!(PRESENTATION_KEY, [PARAMETER_PREFIX, name.as_bytes()].concat().as_slice());
    }

    #[test]
    fn config_chunks_lists_the_ranges_of_a_collection_with_a_shard_key() {
        // 018 FR-006: the chunks in shard key values, the shard the range's leader.
        use teseladb_meta::group0::{RangeDesc, ReplicaRole};
        let mut sm = cluster();
        let shard_key = Document::new().with("key", Document::new().with("a", 1));
        let options = Document::new().with(sharding::SHARD_KEY, shard_key).to_bytes().unwrap();
        let ns = Namespace { db: "app".into(), coll: "s".into() };
        let create = CatalogCommand::Create { ns, uuid: [5; 16], options, if_absent: false };
        sm.apply_command(5, &Group0Command::Catalog(create));
        let desc = RangeDesc {
            group: teseladb_types::GroupId(1),
            start: Vec::new(),
            end: Vec::new(),
            core: 0,
            replicas: vec![(NodeId(2), ReplicaRole::Voter)],
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
            moved_ms: 0,
        };
        sm.insert_range(desc);
        for listed in [documents(&sm, "chunks"), documents_with(&sm, &[], "chunks")] {
            assert_eq!(listed.len(), 1, "app.c has no shard key");
            let min = Document::new().with("a", Value::MinKey);
            assert_eq!(listed[0].get("min"), Some(&Value::Document(min)));
            assert_eq!(listed[0].get("shard"), Some(&Value::from("node2")));
        }
    }

    #[test]
    fn config_databases_shows_the_primary_that_move_primary_left() {
        // 018 FR-005: the setting of `movePrimary`, with its version.
        use teseladb_wire::commands::primaries::{Primary, key};
        let mut sm = cluster();
        let before = documents(&sm, "databases");
        assert_eq!(before[0].get("primary"), Some(&Value::from("node1")));
        let p = Primary { node: NodeId(3), last_mod: 2 };
        sm.settings.insert(key("app"), p.encode());
        let after = documents(&sm, "databases");
        assert_eq!(after[0].get("primary"), Some(&Value::from("node3")));
        let Some(Value::Document(v)) = after[0].get("version") else { panic!("{after:?}") };
        assert_eq!(v.get("lastMod"), Some(&Value::Int32(2)));
    }

    #[test]
    fn zones_show_in_tags_and_in_the_shards() {
        use teseladb_meta::zones::{ZoneRange, shard_key};
        let mut sm = cluster();
        let n1 = teseladb_types::NodeId(1);
        sm.settings.insert(shard_key(n1, "EU"), vec![2]);
        sm.settings.insert(shard_key(n1, ""), vec![3]);
        let min = Document::new().with("a", 0).to_bytes().unwrap();
        let max = Document::new().with("a", 10).to_bytes().unwrap();
        let r =
            ZoneRange { ns: "app.c".into(), id: [7; 12], min, max, zone: "EU".into(), keys: None };
        sm.settings.insert(r.key(), r.encode());
        let shards = documents(&sm, "shards");
        let tags = Value::Array(vec![Value::from("EU"), Value::from("")]);
        assert_eq!(shards[0].get("tags"), Some(&tags));
        assert_eq!(shards[1].get("tags"), None);
        let want = Document::new()
            .with("_id", Value::ObjectId([7; 12]))
            .with("ns", "app.c")
            .with("min", Document::new().with("a", 0))
            .with("max", Document::new().with("a", 10))
            .with("tag", "EU");
        assert_eq!(documents(&sm, "tags"), [want]);
    }

    #[test]
    fn every_node_is_a_shard_and_a_router_and_each_database_is_listed() {
        let mut sm = cluster();
        let drain = Group0Command::SetNodeState {
            node: teseladb_types::NodeId(2),
            state: NodeState::Draining,
        };
        sm.apply_command(5, &drain);
        let remove = Group0Command::SetNodeState {
            node: teseladb_types::NodeId(3),
            state: NodeState::Removed,
        };
        sm.apply_command(6, &remove);
        let shards = documents(&sm, "shards");
        assert_eq!(shards.len(), 2, "the removed node is no shard");
        assert_eq!(shards[0].get("_id"), Some(&Value::from("node1")));
        assert_eq!(shards[0].get("host"), Some(&Value::from("n1:27017")));
        assert_eq!(shards[0].get("draining"), None);
        assert_eq!(shards[1].get("draining"), Some(&Value::Boolean(true)));
        let dbs = documents(&sm, "databases");
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].get("_id"), Some(&Value::from("app")));
        assert_eq!(dbs[0].get("primary"), Some(&Value::from("node1")));
        let routers = documents(&sm, "refrouter");
        assert_eq!(routers.len(), 2);
        assert_eq!(routers[1].get("refVersion"), Some(&Value::from(ANNOUNCED_VERSION)));
        let v = documents(&sm, "version");
        assert_eq!(v[0].get("clusterId"), Some(&Value::ObjectId([9; 12])));
        for none in [COLLECTIONS, "chunks", "tags", "changelog", SETTINGS] {
            assert!(documents(&sm, none).is_empty(), "{none}");
        }
    }

    fn details(d: &Document) -> &Document {
        match d.get("details") {
            Some(Value::Document(d)) => d,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_changelog_shows_each_change_as_a_refrouter_writes_it() {
        let mut sm = cluster();
        let replicas = vec![NodeId(1), NodeId(2)];
        let create = Group0Command::CreateRange { start: vec![1], end: vec![4], replicas };
        let Group0Result::Range(g) = sm.apply_command(10, &create) else { panic!() };
        let hint = Group0Command::SetLeaderHint { group: g, node: NodeId(2), term: 1 };
        sm.apply_command(11, &hint);
        sm.apply_command(12, &Group0Command::SplitRange { group: g, at: vec![2] });
        let complete = Group0Command::CompleteSplit { group: g, at: vec![2], at_index: 0 };
        let Group0Result::Range(right) = sm.apply_command(13, &complete) else { panic!() };
        let add = Group0Command::SetReplicas {
            group: right,
            epoch: 1,
            add: vec![NodeId(3)],
            remove: vec![],
        };
        sm.apply_command(14, &add);
        let core = 1 - sm.ranges[&g].core;
        sm.apply_command(15, &Group0Command::RelocateRange { group: g, core });
        sm.apply_command(16, &Group0Command::CompleteRelocate { group: g, core });
        sm.apply_command(
            17,
            &Group0Command::SetReplicas { group: right, epoch: 1, add: vec![], remove: vec![] },
        );
        let log = documents(&sm, "changelog");
        let what: Vec<&Value> = log.iter().filter_map(|d| d.get("what")).collect();
        assert_eq!(
            what,
            [&Value::from("split"), &Value::from("moveChunk.commit"), &Value::from("relocate")]
        );
        let split = &log[0];
        assert_eq!(split.get("server"), Some(&Value::from("n2")), "the leader's node");
        assert_eq!(split.get("shard"), Some(&Value::from("config")));
        assert_eq!(split.get("ns"), Some(&Value::from("")));
        let Some(Value::String(id)) = split.get("_id") else { panic!() };
        assert!(id.starts_with("n2-1970-01-01T00:00:00.000+00:00-"), "{id}");
        let d = details(split);
        assert_eq!(d.get("owningShard"), Some(&Value::from("node2")));
        let left = match d.get("left") {
            Some(Value::Document(l)) => l,
            other => panic!("{other:?}"),
        };
        assert_eq!(left.get("max"), Some(&Value::Binary { subtype: 0, bytes: vec![2] }));
        let moved = details(&log[1]);
        assert_eq!(moved.get("added"), Some(&Value::Array(vec![Value::from("node3")])));
        let relocated = details(&log[2]);
        assert_eq!(relocated.get("core"), Some(&Value::Int32(i32::from(core))));
        // A merge, of a range whose leader is unknown, over the last range: up to `MaxKey`.
        let mut sm = cluster();
        let create = Group0Command::CreateRange {
            start: vec![1],
            end: Vec::new(),
            replicas: vec![NodeId(1)],
        };
        let Group0Result::Range(g) = sm.apply_command(10, &create) else { panic!() };
        sm.apply_command(11, &Group0Command::SplitRange { group: g, at: vec![2] });
        let complete = Group0Command::CompleteSplit { group: g, at: vec![2], at_index: 0 };
        let Group0Result::Range(right) = sm.apply_command(12, &complete) else { panic!() };
        sm.apply_command(13, &Group0Command::MergeRanges { left: g, right });
        sm.apply_command(14, &Group0Command::CompleteMerge { left: g, right });
        let log = documents(&sm, "changelog");
        assert_eq!(log[1].get("what"), Some(&Value::from("merge")));
        assert_eq!(log[1].get("server"), Some(&Value::from("config")));
        assert_eq!(details(&log[1]).get("max"), Some(&Value::MaxKey));
        assert_eq!(details(&log[1]).get("owningShard"), Some(&Value::from("config")));
    }

    #[test]
    fn settings_show_the_defaults_first_and_skip_what_does_not_read() {
        let mut sm = cluster();
        let defaults = Document::new().with("defaultWriteConcern", Document::new().with("w", 1));
        let key = [PARAMETER_PREFIX, RW_DEFAULTS.as_bytes()].concat();
        sm.settings.insert(key, defaults.to_bytes().unwrap());
        let chunks = Document::new().with("_id", "chunksize").with("value", 8);
        sm.settings.insert([SETTINGS_PREFIX, b"chunksize"].concat(), chunks.to_bytes().unwrap());
        sm.settings.insert([SETTINGS_PREFIX, b"broken"].concat(), vec![1]);
        let docs = documents(&sm, SETTINGS);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].get("_id"), Some(&Value::from(RW_DEFAULTS)));
        assert_eq!(docs[1], chunks);
        // A write whose document has no name is refused.
        let w = SettingsWrite { id: Value::Int32(1), read: None, now: Some(Document::new()) };
        assert_eq!(setting_change(&w).unwrap_err().code, BAD_VALUE);
        // The defaults are kept without their `_id`.
        let w = SettingsWrite {
            id: Value::from(RW_DEFAULTS),
            read: None,
            now: Some(Document::new().with("_id", RW_DEFAULTS).with("x", 1)),
        };
        let change = setting_change(&w).unwrap();
        assert_eq!(change.value, Some(Document::new().with("x", 1).to_bytes().unwrap()));
    }

    #[test]
    fn without_active_nodes_the_primary_is_node_one() {
        let mut sm = cluster();
        for n in 1..=3u64 {
            let off = Group0Command::SetNodeState {
                node: teseladb_types::NodeId(n),
                state: NodeState::Draining,
            };
            sm.apply_command(10 + n, &off);
        }
        let dbs = documents(&sm, "databases");
        assert_eq!(dbs[0].get("primary"), Some(&Value::from("node1")));
    }
}
