//! Errors of the operations (contract `state-machine-api.md`).

use core::fmt;

use teseladb_types::NodeId;

/// Why an operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpError {
    /// This replica does not lead the group; `hint` is the known leader (0 = none).
    NotLeader {
        /// Known leader.
        hint: NodeId,
    },
    /// The group is not on this core.
    GroupNotFound,
    /// `create_group` of a group that already has a state machine.
    GroupExists,
    /// The group has too many proposals waiting, or Raft too much uncommitted.
    Overloaded,
    /// The entry is above Raft's maximum.
    TooLarge,
    /// Another leader's entry took the index of the proposal.
    ProposalDropped,
    /// No result within the deadline; the proposal or read is no longer awaited.
    Timeout,
    /// The group stopped: the log refused a write, or an entry or snapshot could not be applied.
    Unavailable,
    /// The leader has not committed an entry of its term yet (configuration changes wait for it).
    NotReady,
    /// A configuration change is still in flight.
    ConfChangePending,
    /// The configuration change is not valid for the current configuration.
    InvalidConfChange,
    /// Something of the group is in flight with the disk (`close_group`): asked again later.
    Busy,
    /// A request forwarded to a node whose connection dropped and that has not been heard from
    /// since: nothing was sent.
    Unreachable,
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::NotLeader { hint } => write!(f, "not the leader (leader: {})", hint.0),
            OpError::GroupNotFound => f.write_str("group not on this core"),
            OpError::GroupExists => f.write_str("group already exists"),
            OpError::Overloaded => f.write_str("too many operations in flight"),
            OpError::TooLarge => f.write_str("entry too large"),
            OpError::ProposalDropped => f.write_str("another leader took the proposal's index"),
            OpError::Timeout => f.write_str("no result within the deadline"),
            OpError::Unavailable => f.write_str("group stopped"),
            OpError::NotReady => f.write_str("the leader has not committed an entry of its term"),
            OpError::ConfChangePending => f.write_str("a configuration change is in flight"),
            OpError::InvalidConfChange => f.write_str("invalid configuration change"),
            OpError::Busy => f.write_str("the group has disk work in flight"),
            OpError::Unreachable => f.write_str("the node is unreachable"),
        }
    }
}

impl std::error::Error for OpError {}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_message() {
        let all = [
            OpError::NotLeader { hint: NodeId(2) },
            OpError::GroupNotFound,
            OpError::GroupExists,
            OpError::Overloaded,
            OpError::TooLarge,
            OpError::ProposalDropped,
            OpError::Timeout,
            OpError::Unavailable,
            OpError::NotReady,
            OpError::ConfChangePending,
            OpError::InvalidConfChange,
            OpError::Busy,
            OpError::Unreachable,
        ];
        for e in all {
            assert!(!e.to_string().is_empty());
        }
    }
}
