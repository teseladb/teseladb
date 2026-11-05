//! A node's part of a tenant's storage quota (007 FR-031). Time is cut into epochs of
//! [`EPOCH_MS`] of the wall clock, the same on every node. Each node reads the data each tenant
//! uses once per epoch, at [`reading_at`]: [`GUARD_MS`] after the epoch starts, so that every
//! node's reading sees each write a node let through in an earlier window, whatever the difference
//! between their clocks (D-004). A node's part of the margin of a reading is
//! `(limit - used) / nodes`.
//!
//! In window `e` a write must fit the part of the reading of epoch `e - 2`, counting all the node
//! let through from that epoch's window on, and the part of the reading of `e - 1` from its window
//! on, if the node has it. The reading of `e - 2` saw every write of the windows before its own,
//! and every node has had it since window `e` started; all nodes keeping within it, they never
//! pass the quota by more than the writes in flight when a reading starts. A node without that
//! reading lets nothing grow. A node does not know what it let through before it started, so it
//! counts from the window after its start. The rules have no clock and no I/O: the server and the
//! simulator run them.

use std::collections::BTreeMap;

/// The length of an epoch, and how often each node reads the tenants' usage (FR-031: at least
/// every 5 s). A write's part comes from a reading two epochs old, so a short epoch lets
/// deletions free room soon and a restarted node write soon.
pub const EPOCH_MS: u64 = 2_000;
/// How far two nodes' clocks may differ (D-004): a write's window starts this much before its
/// epoch, and a reading starts this much after it.
pub const GUARD_MS: u64 = 250;

/// The window a write the node lets through at wall-clock `now_ms` counts in.
pub fn window_of(now_ms: u64) -> u64 {
    (now_ms + GUARD_MS) / EPOCH_MS
}

/// When, on the wall clock, the reading of epoch `epoch` starts.
pub fn reading_at(epoch: u64) -> u64 {
    epoch * EPOCH_MS + GUARD_MS
}

/// The epoch a reading of one tenant that starts at wall-clock `now_ms` may stand for: the base of the window it is
/// in. Such a reading saw every write of the tenant up to now, which the base's reading would have seen and more,
/// and the writes it saw that the node also counts in its window are counted twice, never missed. A node that has no
/// reading of a tenant it knows (one created a moment ago) reads it at once with this epoch.
pub fn base_epoch(now_ms: u64) -> u64 {
    window_of(now_ms).saturating_sub(2)
}

/// Keeps in `readings` what a reading of epoch `epoch` gave for `tenant`, and only the last `keep` epochs.
pub fn note(readings: &mut Vec<NodeReading>, epoch: u64, tenant: u32, used: u64, keep: usize) {
    match readings.iter_mut().find(|r| r.epoch == epoch) {
        Some(r) => {
            r.used.insert(tenant, used);
        }
        None => readings.push(NodeReading { epoch, used: [(tenant, used)].into_iter().collect() }),
    }
    readings.sort_by_key(|r| r.epoch);
    let extra = readings.len().saturating_sub(keep);
    readings.drain(..extra);
}

/// What a node's reading of an epoch gave for a tenant: the bytes it uses, or `None` if the
/// tenant could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The epoch of the reading.
    pub epoch: u64,
    /// The bytes the tenant uses.
    pub used: Option<u64>,
}

/// A node's reading of the tenants' data in an epoch (FR-031): the bytes of each tenant it read.
/// A tenant it did not read has no reading of that epoch: the node's copy of Group 0 may not have
/// known it yet, so it says nothing of what the tenant used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeReading {
    /// The epoch.
    pub epoch: u64,
    /// The bytes of each tenant it read.
    pub used: BTreeMap<u32, u64>,
}

impl NodeReading {
    /// What this reading gave for `tenant`.
    pub fn of(&self, tenant: u32) -> Reading {
        Reading { epoch: self.epoch, used: self.used.get(&tenant).copied() }
    }
}

/// Why a write that grows a tenant's data does not fit in its quota on a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaRefusal {
    /// The tenant uses its whole quota or more: the bytes it uses.
    Full(u64),
    /// The node has no reading of the tenant of this epoch or the one before since it started,
    /// so it does not know its part.
    Unread,
    /// The node let the tenant write its whole part of a reading: the part.
    ShareSpent(u64),
}

/// What a node let a tenant write in each window it still needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaWindow {
    written: BTreeMap<u64, u64>,
}

impl QuotaWindow {
    /// Lets a write of `bytes` through at wall-clock `now_ms` if it fits the parts of the
    /// readings in `readings` of the two epochs before, of a node that counts whole from window
    /// `since`, of a tenant with quota `limit`, among `nodes` nodes (none counts as one); it then
    /// counts in its window.
    pub fn admit(
        &mut self,
        limit: u64,
        nodes: usize,
        since: u64,
        now_ms: u64,
        readings: &[Reading],
        bytes: u64,
    ) -> Result<(), QuotaRefusal> {
        let now = window_of(now_ms);
        // The reading of two epochs before saw every write of the windows before its own, and
        // every node has had it since this window started: it bounds what the node lets through
        // from its window on. Without it this node does not know its part.
        let base = now.saturating_sub(2);
        self.written.retain(|w, _| *w >= base);
        // The faulty variant lets every write through (`tenants`, 007 T059).
        if cfg!(teseladb_bug = "quota_ignored") {
            *self.written.entry(now).or_default() += bytes;
            return Ok(());
        }
        if base < since || !readings.iter().any(|r| r.epoch == base && r.used.is_some()) {
            return Err(QuotaRefusal::Unread);
        }
        let nodes = u64::try_from(nodes.max(1)).unwrap_or(u64::MAX);
        // The newest reading first: its refusal says the most about the tenant now. The reading
        // of the epoch before, if there is one, bounds what went through from its window on.
        let mut usable: Vec<(u64, u64)> = readings
            .iter()
            .filter(|r| r.epoch >= base && r.epoch < now)
            .filter_map(|r| r.used.map(|u| (r.epoch, u)))
            .collect();
        usable.sort_by_key(|(epoch, _)| std::cmp::Reverse(*epoch));
        for (epoch, used) in usable {
            if used >= limit {
                return Err(QuotaRefusal::Full(used));
            }
            let share = (limit - used) / nodes;
            let spent: u64 = self.written.range(epoch..).map(|(_, b)| b).sum();
            if spent.saturating_add(bytes) > share {
                return Err(QuotaRefusal::ShareSpent(share));
            }
        }
        *self.written.entry(now).or_default() += bytes;
        Ok(())
    }
}
