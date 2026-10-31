//! What the balancer reads in the nodes' loads (007 FR-010, plan «Equilibrio de réplicas y
//! líderes»): how full each disk is, and how far the busiest node's operations are from the rest.

use crate::liveness::NodeLoad;

/// A disk more used than this (in %) gives replicas away and takes none.
pub const FULL_PCT: u64 = 90;
/// A node takes a replica for space only while its disk stays below this (in %).
pub const ROOM_PCT: u64 = 80;
/// The busiest node's operations per second over the mean (in %) above which a leader moves.
pub const BUSY_PCT: u64 = 125;
/// The least gap of operations per second between the busiest node and the idlest for a move.
pub const BUSY_GAP: u64 = 100;

/// Used share of the disk in per mille, if its size is known.
pub fn used_permille(l: &NodeLoad) -> Option<u64> {
    (l.total_bytes != 0)
        .then(|| l.total_bytes.saturating_sub(l.free_bytes).saturating_mul(1000) / l.total_bytes)
}

/// The disk is more than [`FULL_PCT`] used.
pub fn full(l: &NodeLoad) -> bool {
    used_permille(l).is_some_and(|u| u > FULL_PCT * 10)
}

/// The disk is known and below [`ROOM_PCT`] used.
pub fn roomy(l: &NodeLoad) -> bool {
    used_permille(l).is_some_and(|u| u < ROOM_PCT * 10)
}

/// Whether `busiest` serves enough more operations than the mean of `all` and than `idlest` for a
/// leader to move.
pub fn too_busy(busiest: u64, idlest: u64, all: &[u64]) -> bool {
    if all.is_empty() {
        return false;
    }
    let sum: u64 = all.iter().sum();
    let mean_x100 = sum.saturating_mul(100) / all.len() as u64;
    busiest.saturating_mul(100) > mean_x100.saturating_mul(BUSY_PCT) / 100
        && busiest >= idlest.saturating_add(BUSY_GAP)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn disk(free: u64, total: u64) -> NodeLoad {
        NodeLoad { free_bytes: free, total_bytes: total, ..NodeLoad::default() }
    }

    #[test]
    fn disks_are_full_roomy_or_unknown() {
        assert_eq!(used_permille(&disk(0, 0)), None);
        assert_eq!(used_permille(&disk(250, 1000)), Some(750));
        assert!(full(&disk(50, 1000)) && !full(&disk(100, 1000)) && !full(&disk(0, 0)));
        assert!(roomy(&disk(201, 1000)) && !roomy(&disk(200, 1000)) && !roomy(&disk(0, 0)));
    }

    #[test]
    fn a_busy_node_is_well_above_the_mean_and_the_idlest() {
        assert!(too_busy(900, 100, &[900, 300, 100]));
        assert!(!too_busy(160, 100, &[160, 140, 100]), "not 25 % over the mean");
        assert!(!too_busy(190, 100, &[190, 10, 100]), "not 100 over the idlest");
        assert!(!too_busy(5, 0, &[]));
    }
}
