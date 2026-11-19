//! What each node records of the versions it runs (007 FR-027): a Group 0 setting per node, under
//! [`PREFIX`], with the snapshot format of Group 0 it reads and writes in [`FORMAT`]. Group 0
//! writes its snapshot in the lowest format a node that is not removed records, and refuses the
//! entries of a newer format until every such node records it, so that a node of the version
//! before keeps applying the log and reading the snapshots during an upgrade.

use teseladb_bson::{RawDoc, RawValue};
use teseladb_types::NodeId;

use crate::catalog::CatalogCommand;
use crate::command::NodeState;
use crate::group0::{Group0Sm, SNAPSHOT_VERSION, SNAPSHOT_VERSION_WITHOUT_DB_MARKS};

/// Prefix of the Group 0 settings with each node's versions.
pub const PREFIX: &[u8] = b"nodeVersions.";
/// The field of a node's setting with the snapshot format it runs.
pub const FORMAT: &str = "format";

/// The setting of node `node`.
pub fn key(node: NodeId) -> Vec<u8> {
    [PREFIX, node.0.to_string().as_bytes()].concat()
}

/// The snapshot format a node's setting `value` records.
pub fn format_of(value: &[u8]) -> Option<u8> {
    match RawDoc::from_bytes(value).ok()?.get(FORMAT)? {
        RawValue::Int64(v) => u8::try_from(v).ok(),
        RawValue::Int32(v) => u8::try_from(v).ok(),
        _ => None,
    }
}

/// The format Group 0 writes its snapshot in: the lowest one recorded by a node that is not
/// removed, within the formats this binary writes; this binary's when no node is behind. A node
/// that recorded nothing yet runs this binary.
pub fn snapshot_format(sm: &Group0Sm) -> u8 {
    sm.nodes
        .values()
        .filter(|n| n.state != NodeState::Removed)
        .filter_map(|n| sm.settings.get(&key(n.id)).and_then(|v| format_of(v)))
        .min()
        .unwrap_or(SNAPSHOT_VERSION)
        .clamp(SNAPSHOT_VERSION_WITHOUT_DB_MARKS, SNAPSHOT_VERSION)
}

/// Whether every node that is not removed reads snapshot format `format` and the entries that
/// came with it.
pub fn all_run(sm: &Group0Sm, format: u8) -> bool {
    snapshot_format(sm) >= format
}

/// Whether every node that is not removed reads the entry `cmd` leaves in Group 0's log. A node
/// of the version before an entry came does not decode it and stops applying Group 0 at it, so
/// whoever would propose the entry asks this first (007 FR-027): refusing it when it is applied
/// is too late for that node. The match names every command, so that a new one says here which
/// format brought it.
pub fn all_read(sm: &Group0Sm, cmd: &CatalogCommand) -> bool {
    match cmd {
        CatalogCommand::AddFieldNames { .. } => all_run(sm, SNAPSHOT_VERSION),
        CatalogCommand::Create { .. }
        | CatalogCommand::Drop { .. }
        | CatalogCommand::Rename { .. }
        | CatalogCommand::Modify { .. }
        | CatalogCommand::DropDatabase { .. }
        | CatalogCommand::CreateDatabase { .. }
        | CatalogCommand::Purged { .. } => true,
    }
}
