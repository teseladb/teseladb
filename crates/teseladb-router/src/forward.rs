//! Where a target runs: on a core of this node, or on the leader's node through a `Forward` or
//! `ProxyOpen` frame, whose route key carries the range and the core (contract `peer-frames.md`).

use teseladb_types::{GroupId, NodeId};

use crate::route::Target;

/// Bits of the route key that hold the core.
const CORE_BITS: u32 = 16;

/// How to reach a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// The leader is this node: hand it to core `core`.
    Local {
        /// Core.
        core: u16,
        /// Range.
        group: GroupId,
    },
    /// Forward it to `node`, which hands it to its core `core`.
    Remote {
        /// Leader node.
        node: NodeId,
        /// Core.
        core: u16,
        /// Range.
        group: GroupId,
    },
}

/// How node `me` reaches `t`.
pub fn dispatch(me: NodeId, t: &Target) -> Dispatch {
    if t.node == me {
        Dispatch::Local { core: t.core, group: t.group }
    } else {
        Dispatch::Remote { node: t.node, core: t.core, group: t.group }
    }
}

/// The route key of range `g` on core `core`: the range in the high 48 bits and the core in the
/// low 16; `None` for a range number that does not fit.
pub fn route_key(g: GroupId, core: u16) -> Option<u64> {
    (g.0 >> (64 - CORE_BITS) == 0).then_some((g.0 << CORE_BITS) | u64::from(core))
}

/// The range and core of a route key.
pub fn parse_route_key(k: u64) -> (GroupId, u16) {
    (GroupId(k >> CORE_BITS), (k & u64::from(u16::MAX)) as u16)
}
