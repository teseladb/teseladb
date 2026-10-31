//! The cluster's first data range (005 FR-016): the founder creates Group 0 and, once `Bootstrap`
//! is applied, proposes `CreateRange` for the whole data space, spaces `01` to `03` (`[01, 04)`),
//! with itself as the only replica. Being the first range Group 0 creates, it gets group 1, and
//! Group 0 places it on core `1 mod cores` of every node, as for any range.
//!
//! Every node prepares its replica of that range at start: the founder as its only voter, any
//! other node as a learner of a configuration whose only voter is the founder, which is the
//! configuration the range had before the first entry of its log. A node the range leader adds
//! with `AddLearner` then replays the log from its first entry, or receives a snapshot that brings
//! the configuration; one it never adds keeps an idle replica that never votes.

use teseladb_keys::space;
use teseladb_types::{GroupId, NodeId};

use crate::command::Group0Command;
use crate::group0::{Group0Sm, RangeDesc};

/// The data range: the first range Group 0 creates.
pub const DATA_RANGE: GroupId = GroupId(1);

/// First key of the data range: the start of space `01`.
pub const DATA_START: [u8; 1] = [space::DATA];

/// First key past the data range: the start of space `04`, Group 0's meta-ranges (007).
pub const DATA_END: [u8; 1] = [space::META];

/// The founder's `CreateRange` of the data range.
pub fn create_data_range(founder: NodeId) -> Group0Command {
    Group0Command::CreateRange {
        start: DATA_START.to_vec(),
        end: DATA_END.to_vec(),
        replicas: vec![founder],
    }
}

/// Whether the cluster exists and no range holds the data space yet: the founder has to propose
/// [`create_data_range`].
pub fn needs_data_range(sm: &Group0Sm) -> bool {
    sm.bootstrapped() && sm.range_for(&DATA_START).is_none()
}

/// The data range's descriptor, once Group 0 created it.
pub fn data_range(sm: &Group0Sm) -> Option<&RangeDesc> {
    sm.ranges.get(&DATA_RANGE)
}

/// The core of every node that holds the data range (Group 0's rule: group modulo cores).
pub fn data_range_core(cores: u16) -> u16 {
    (DATA_RANGE.0 % u64::from(cores.max(1))) as u16
}

/// The configuration `me` creates its replica of the data range with: `(voters, learners)`.
pub fn initial_replica(founder: NodeId, me: NodeId) -> (Vec<NodeId>, Vec<NodeId>) {
    if me == founder { (vec![founder], Vec::new()) } else { (vec![founder], vec![me]) }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::command::NodeAddrs;
    use crate::group0::Group0Result;

    fn founded() -> Group0Sm {
        let mut sm = Group0Sm::default();
        let first = NodeAddrs { name: b"n1".to_vec(), cores: 2, ..NodeAddrs::default() };
        let boot = Group0Command::Bootstrap {
            cluster_id: [9; 16],
            first_node: first,
            cert_fingerprint: [0; 32],
        };
        assert_eq!(sm.apply_command(1, &boot), Group0Result::Node(NodeId(1)));
        sm
    }

    #[test]
    fn the_founders_command_creates_the_data_range_once() {
        assert!(!needs_data_range(&Group0Sm::default()), "no cluster, no range");
        let mut sm = founded();
        assert!(needs_data_range(&sm));
        assert_eq!(data_range(&sm), None);
        let cmd = create_data_range(NodeId(1));
        assert_eq!(sm.apply_command(2, &cmd), Group0Result::Range(DATA_RANGE));
        assert!(!needs_data_range(&sm));
        let r = data_range(&sm).expect("created");
        assert_eq!((r.start.as_slice(), r.end.as_slice()), (&DATA_START[..], &DATA_END[..]));
        assert_eq!(r.voters(), [NodeId(1)]);
        assert_eq!(r.core, data_range_core(2));
        // Applied twice (a founder that proposed it again after a restart), it changes nothing.
        assert!(matches!(sm.apply_command(3, &cmd), Group0Result::Err(_)));
        assert_eq!(sm.ranges.len(), 1);
    }

    #[test]
    fn the_data_range_covers_spaces_01_to_03_on_core_one_modulo_cores() {
        let mut sm = founded();
        sm.apply_command(2, &create_data_range(NodeId(1)));
        let r = data_range(&sm).expect("created");
        for space in [space::DATA, space::TXN, space::SYSTEM] {
            assert!(r.contains(&[space, 7]), "space {space:02x}");
        }
        assert!(!r.contains(&[space::RANGE_LOCAL, 1]) && !r.contains(&[space::META]));
        assert_eq!((data_range_core(0), data_range_core(1)), (0, 0));
        assert_eq!((data_range_core(2), data_range_core(8)), (1, 1));
    }

    #[test]
    fn a_node_that_joins_starts_its_replica_as_a_learner_of_the_founder() {
        assert_eq!(initial_replica(NodeId(1), NodeId(1)), (vec![NodeId(1)], vec![]));
        assert_eq!(initial_replica(NodeId(1), NodeId(3)), (vec![NodeId(1)], vec![NodeId(3)]));
    }
}
