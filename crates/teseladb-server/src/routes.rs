//! The node's routing table for the client workers (007 FR-003): core 0 writes it from its copy
//! of Group 0 whenever the ranges change, and a worker reads it to send each request to the core
//! of the range that holds its key. That core's router finds the range's leader. Core 0 also
//! publishes every node's tags, which reads with a `readPreference` match (FR-013), and the
//! cluster's write gates, which every write looks at (FR-029).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use teseladb_meta::group0::RangeDesc;
use teseladb_router::table::{RouteEntry, RouteTable};
use teseladb_types::{GroupId, NodeId};
use teseladb_wire::commands::write_block::WriteGate;

use crate::hosting::Hosting;

/// Every node's tags, by node.
pub type NodeTags = BTreeMap<NodeId, Vec<(String, String)>>;

/// The routing table shared by core 0 and the client workers, and the nodes' tags.
#[derive(Clone, Debug, Default)]
pub struct SharedRoutes {
    table: Arc<RwLock<Arc<RouteTable>>>,
    tags: Arc<RwLock<Arc<NodeTags>>>,
    /// Which cores of the node hold each range: a range on its way to another core is sent to
    /// the core that holds it (007 FR-011).
    hosting: Hosting,
    /// The `fsync` locks and the user write block as this node's copy of Group 0 has them.
    gate: Arc<RwLock<Arc<WriteGate>>>,
    /// The highest feature compatibility version every node runs, when one runs less than this
    /// binary's latest (FR-027).
    fcv_limit: Arc<RwLock<Arc<Option<String>>>>,
}

/// Replaces what `lock` holds with `v`; a writer that panicked left a whole value behind, which
/// is replaced all the same.
fn replace<T>(lock: &RwLock<Arc<T>>, v: Arc<T>) {
    match lock.write() {
        Ok(mut g) => *g = v,
        Err(p) => *p.into_inner() = v,
    }
}

/// What `lock` holds.
fn read<T>(lock: &RwLock<Arc<T>>) -> Arc<T> {
    match lock.read() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

impl SharedRoutes {
    /// The table that sends each range to the core of the node that holds it in `hosting`.
    pub fn with_hosting(self, hosting: Hosting) -> SharedRoutes {
        SharedRoutes { hosting, ..self }
    }

    /// `e` with the core that holds its range on this node, when only one does.
    fn local(&self, mut e: RouteEntry) -> RouteEntry {
        if let Some(core) = self.hosting.single(e.group) {
            e.core = core;
        }
        e
    }

    /// Replaces the table with one built from Group 0's descriptors.
    pub fn publish(&self, ranges: &[RangeDesc]) {
        let mut t = RouteTable::default();
        t.refresh_ranges(ranges.iter());
        replace(&self.table, Arc::new(t));
    }

    /// Replaces the nodes' tags with `tags`.
    pub fn publish_tags(&self, tags: NodeTags) {
        replace(&self.tags, Arc::new(tags));
    }

    /// Every node's tags.
    pub fn tags(&self) -> Arc<NodeTags> {
        read(&self.tags)
    }

    /// Replaces the write gates with `gate`.
    pub fn publish_gate(&self, gate: WriteGate) {
        replace(&self.gate, Arc::new(gate));
    }

    /// The write gates.
    pub fn gate(&self) -> WriteGate {
        *read(&self.gate)
    }

    /// Replaces the nodes' feature compatibility version limit with `limit`.
    pub fn publish_fcv_limit(&self, limit: Option<String>) {
        replace(&self.fcv_limit, Arc::new(limit));
    }

    /// The nodes' feature compatibility version limit.
    pub fn fcv_limit(&self) -> Option<String> {
        (*read(&self.fcv_limit)).clone()
    }

    fn table(&self) -> Arc<RouteTable> {
        read(&self.table)
    }

    /// The range that holds `key`, as the table knows it.
    pub fn lookup(&self, key: &[u8]) -> Option<RouteEntry> {
        self.table().lookup(key).cloned().map(|e| self.local(e))
    }

    /// The entry of range `g`.
    pub fn entry(&self, g: GroupId) -> Option<RouteEntry> {
        self.table().entry(g).cloned().map(|e| self.local(e))
    }

    /// The ranges in key order.
    pub fn entries(&self) -> Vec<RouteEntry> {
        self.table().entries().cloned().map(|e| self.local(e)).collect()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_meta::group0::ReplicaRole;
    use teseladb_types::NodeId;

    fn range(g: u64, start: &[u8], end: &[u8]) -> RangeDesc {
        RangeDesc {
            group: GroupId(g),
            start: start.to_vec(),
            end: end.to_vec(),
            core: 1,
            replicas: vec![(NodeId(1), ReplicaRole::Voter)],
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
        }
    }

    #[test]
    fn a_table_left_by_a_writer_that_panicked_is_still_read_and_replaced() {
        let routes = SharedRoutes::default();
        assert!(routes.lookup(&[1]).is_none());
        routes.publish(&[range(1, &[1], &[2])]);
        let held = routes.clone();
        let poisoned = std::thread::spawn(move || {
            let _guard = held.table.write();
            panic!("a writer dies holding the lock");
        });
        assert!(poisoned.join().is_err());
        assert_eq!(routes.lookup(&[1, 5]).map(|e| e.group), Some(GroupId(1)));
        routes.publish(&[range(1, &[1], &[2]), range(2, &[2], &[4])]);
        assert_eq!(routes.entry(GroupId(2)).map(|e| e.start), Some(vec![2]));
        assert_eq!(routes.entries().len(), 2);
        assert!(routes.tags().is_empty());
        let tags = NodeTags::from([(NodeId(2), vec![("dc".to_owned(), "east".to_owned())])]);
        routes.publish_tags(tags.clone());
        assert_eq!(*routes.tags(), tags);
    }

    #[test]
    fn a_range_on_its_way_to_another_core_goes_to_the_core_that_holds_it() {
        let hosting = Hosting::default();
        let routes = SharedRoutes::default().with_hosting(hosting.clone());
        routes.publish(&[range(1, &[1], &[2])]);
        let core = |r: &SharedRoutes| r.lookup(&[1, 5]).map(|e| e.core);
        assert_eq!(core(&routes), Some(1), "the descriptor's core");
        hosting.add(GroupId(1), 1);
        hosting.add(GroupId(1), 3);
        assert_eq!(core(&routes), Some(1), "both hold it: the descriptor's core");
        hosting.remove(GroupId(1), 1);
        assert_eq!(core(&routes), Some(3));
        assert_eq!(routes.entry(GroupId(1)).map(|e| e.core), Some(3));
        assert_eq!(routes.entries()[0].core, 3);
    }
}
