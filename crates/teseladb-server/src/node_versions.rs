//! The versions each node runs (007 FR-027): at start, a node records in Group 0 the lowest and
//! highest protocol versions it speaks, its format version and the latest feature compatibility version it runs, as a
//! setting of its own; while some node that is not removed runs less than this binary's latest,
//! `setFeatureCompatibilityVersion` cannot go past it, so new features turn on once every node
//! has them.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::command::NodeState;
use teseladb_meta::group0::Group0Sm;
use teseladb_wire::commands::fcv::VERSIONS;

use teseladb_meta::node_versions::FORMAT;
pub use teseladb_meta::node_versions::{PREFIX, key};

const PROTOCOL: &str = "protocol";
const PROTOCOL_MIN: &str = "protocolMin";
const FCV: &str = "fcv";
/// The format of the range commands a node reads (007 FR-039).
const RANGE: &str = "range";
/// The document key layouts a node reads: 1 adds the `_id` order of new collections (D-153).
const KEYS: &str = "keys";
/// The layout version that brings the `_id` order of new collections.
const KEYS_ORDERED: u8 = 1;

/// What this binary runs: the peer protocol, Group 0's snapshot format and the latest feature
/// compatibility version.
pub fn ours() -> Vec<u8> {
    let d = Document::new()
        .with(PROTOCOL, i64::from(teseladb_io::peer::PROTOCOL_VERSION))
        .with(PROTOCOL_MIN, i64::from(teseladb_io::peer::PROTOCOL_MIN))
        .with(FORMAT, i64::from(teseladb_meta::group0::SNAPSHOT_VERSION))
        .with(FCV, VERSIONS[0])
        .with(RANGE, i64::from(teseladb_range::command::FORMAT))
        .with(KEYS, i64::from(KEYS_ORDERED));
    d.to_bytes().unwrap_or_default()
}

/// The latest feature compatibility version a node's setting `value` says it runs.
fn fcv_of(value: &[u8]) -> Option<String> {
    let d = Document::from(RawDoc::from_bytes(value).ok()?);
    match d.get(FCV) {
        Some(Value::String(v)) => Some(v.clone()),
        _ => None,
    }
}

/// The version a node's setting `value` records under `field`: 0 for a node that recorded its
/// versions before it recorded that one.
fn version_of(value: &[u8], field: &str) -> u8 {
    match RawDoc::from_bytes(value).ok().and_then(|d| d.get(field)) {
        Some(teseladb_bson::RawValue::Int64(v)) => u8::try_from(v).unwrap_or(0),
        Some(teseladb_bson::RawValue::Int32(v)) => u8::try_from(v).unwrap_or(0),
        _ => 0,
    }
}

/// Whether every node that is not removed reads the writes that carry their change of each
/// collection's documents (007 FR-039). A node that recorded nothing yet runs this binary.
pub fn counted_writes(sm: &Group0Sm) -> bool {
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)))
        .all(|v| version_of(v, RANGE) >= teseladb_range::command::FORMAT_COUNTED)
}

/// Whether every node that is not removed reads the writes whose idempotence records keep the
/// greatest data key they wrote (007 FR-022). A node that recorded nothing yet runs this binary.
pub fn reaching_records(sm: &Group0Sm) -> bool {
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)))
        .all(|v| version_of(v, RANGE) >= teseladb_range::command::FORMAT_REACHING)
}

/// Whether every node that is not removed reads the writes that number their new documents
/// (009 FR-023). A node that recorded nothing yet runs this binary.
pub fn local_ids(sm: &Group0Sm) -> bool {
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)))
        .all(|v| version_of(v, RANGE) >= teseladb_range::command::FORMAT_LOCAL_IDS)
}

/// Whether every node that is not removed reads the commits of transactions that carry their
/// change of each collection's documents (007 FR-039). A node that recorded nothing yet runs this
/// binary.
pub fn counted_resolutions(sm: &Group0Sm) -> bool {
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)))
        .all(|v| version_of(v, RANGE) >= teseladb_range::command::FORMAT_RESOLVE_COUNTED)
}

/// Whether every node that is not removed reads the changes of documents that carry their bytes
/// of BSON and of keys (007 FR-039, `collStats`). A node that recorded nothing yet runs this
/// binary.
pub fn sized_counts(sm: &Group0Sm) -> bool {
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)))
        .all(|v| version_of(v, RANGE) >= teseladb_range::command::FORMAT_SIZED)
}

/// Whether every node that is not removed keeps the documents of a new collection in `_id`
/// order (D-153): a node of the version before would key them by the hash bucket of their `_id`.
/// A node that recorded nothing yet runs this binary.
pub fn ordered_ids(sm: &Group0Sm) -> bool {
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)))
        .all(|v| version_of(v, KEYS) >= KEYS_ORDERED)
}

/// The highest feature compatibility version every node that is not removed runs, if one runs
/// less than this binary's latest. A node that recorded nothing yet runs this binary.
pub fn fcv_limit(sm: &Group0Sm) -> Option<String> {
    let rank = |v: &str| VERSIONS.iter().position(|x| *x == v);
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)).and_then(|v| fcv_of(v)))
        .filter(|v| rank(v).is_some_and(|r| r > 0))
        .max_by_key(|v| rank(v))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_meta::command::NodeAddrs;
    use teseladb_meta::group0::NodeInfo;
    use teseladb_types::NodeId;

    fn with_nodes(states: &[(u64, NodeState, Option<&str>)]) -> Group0Sm {
        let mut sm = Group0Sm::default();
        for (id, state, fcv) in states {
            let id = NodeId(*id);
            let info = NodeInfo {
                id,
                addrs: NodeAddrs::default(),
                state: *state,
                joined_index: 1,
                cert_fingerprint: [0; 32],
                boot_epoch: 1,
                tags: Vec::new(),
            };
            sm.nodes.insert(id, info);
            if let Some(v) = fcv {
                let value = Document::new().with(FCV, *v).to_bytes().unwrap();
                sm.settings.insert(key(id), value);
            }
        }
        sm
    }

    #[test]
    fn the_limit_is_the_lowest_version_of_the_nodes_that_are_not_removed() {
        use NodeState::{Active, Draining, Removed};
        assert_eq!(fcv_limit(&with_nodes(&[(1, Active, Some("9.0")), (2, Active, None)])), None);
        let sm = with_nodes(&[(1, Active, Some("9.0")), (2, Draining, Some("8.0"))]);
        assert_eq!(fcv_limit(&sm), Some("8.0".to_owned()));
        let sm = with_nodes(&[(1, Active, Some("8.3")), (2, Active, Some("8.0"))]);
        assert_eq!(fcv_limit(&sm), Some("8.0".to_owned()));
        let sm = with_nodes(&[(1, Active, Some("9.0")), (2, Removed, Some("8.0"))]);
        assert_eq!(fcv_limit(&sm), None, "a removed node does not count");
    }

    #[test]
    fn this_binary_records_its_own_versions() {
        assert_eq!(fcv_of(&ours()), Some(VERSIONS[0].to_owned()));
        // The range of protocol versions it speaks, from the lowest to the highest.
        let d = Document::from(RawDoc::from_bytes(&ours()).unwrap());
        let range = (d.get(PROTOCOL_MIN).cloned(), d.get(PROTOCOL).cloned());
        let (min, max) = (teseladb_io::peer::PROTOCOL_MIN, teseladb_io::peer::PROTOCOL_VERSION);
        assert_eq!(range, (Some(Value::Int64(i64::from(min))), Some(Value::Int64(i64::from(max)))));
        assert_eq!(fcv_of(b"x"), None);
        assert_eq!(key(NodeId(12)), b"nodeVersions.12".to_vec());
    }

    #[test]
    fn counted_writes_wait_for_every_node_that_is_not_removed() {
        use NodeState::{Active, Removed};
        // A node of the version before recorded its versions without the range format.
        let old = Document::new().with(FCV, "9.0").to_bytes().unwrap();
        let mut sm = with_nodes(&[(1, Active, None), (2, Active, None)]);
        assert!(counted_writes(&sm), "nodes that recorded nothing run this binary");
        sm.settings.insert(key(NodeId(1)), ours());
        sm.settings.insert(key(NodeId(2)), old.clone());
        assert!(!counted_writes(&sm));
        sm.settings.insert(key(NodeId(2)), ours());
        assert!(counted_writes(&sm));
        assert!(counted_resolutions(&sm));
        // A node of the format before reads counted writes and not counted commits.
        let format_1 = Document::new()
            .with(FCV, "9.0")
            .with(RANGE, i64::from(teseladb_range::command::FORMAT_COUNTED))
            .to_bytes()
            .unwrap();
        sm.settings.insert(key(NodeId(2)), format_1);
        assert!(counted_writes(&sm));
        assert!(!counted_resolutions(&sm));
        // A node of format 2 reads counted commits and not the records that keep their key.
        let format_2 = Document::new()
            .with(FCV, "9.0")
            .with(RANGE, i64::from(teseladb_range::command::FORMAT_RESOLVE_COUNTED))
            .to_bytes()
            .unwrap();
        sm.settings.insert(key(NodeId(2)), format_2);
        assert!(counted_resolutions(&sm));
        assert!(!reaching_records(&sm));
        assert!(!local_ids(&sm));
        // A node of format 3 keeps the records' key and does not number documents.
        let format_3 = Document::new()
            .with(FCV, "9.0")
            .with(RANGE, i64::from(teseladb_range::command::FORMAT_REACHING))
            .to_bytes()
            .unwrap();
        sm.settings.insert(key(NodeId(2)), format_3);
        assert!(reaching_records(&sm));
        assert!(!local_ids(&sm));
        assert!(!sized_counts(&sm), "format 3 does not read the bytes of the counts");
        // A node of format 4 numbers documents and does not read the bytes of the counts.
        let format_4 = Document::new()
            .with(FCV, "9.0")
            .with(RANGE, i64::from(teseladb_range::command::FORMAT_LOCAL_IDS))
            .to_bytes()
            .unwrap();
        sm.settings.insert(key(NodeId(2)), format_4);
        assert!(local_ids(&sm));
        assert!(!sized_counts(&sm));
        sm.settings.insert(key(NodeId(2)), ours());
        assert!(reaching_records(&sm));
        assert!(local_ids(&sm));
        assert!(sized_counts(&sm));
        assert!(ordered_ids(&sm));
        sm.settings.insert(key(NodeId(2)), Document::new().with(FCV, "9.0").to_bytes().unwrap());
        assert!(!ordered_ids(&sm), "a node of the version before keys documents by hash");
        let mut sm = with_nodes(&[(1, Active, None), (2, Removed, None)]);
        sm.settings.insert(key(NodeId(2)), old);
        assert!(counted_writes(&sm), "a removed node does not count");
    }
}
