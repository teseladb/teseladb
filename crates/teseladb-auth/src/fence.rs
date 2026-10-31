//! The authorization fence (013 FR-012, `spec/tla/AuthzFence.tla`): a change of users or roles
//! that takes privileges away answers only when no node can still authorize with the state
//! before it. Every node authorizes with its own copy of Group 0, and only while it holds an
//! authorization lease. A node asks Group 0's leader for a lease with the index of its copy and
//! the time it asked; the leader records that index as acknowledged and answers with its commit
//! index; the node takes a lease of [`AUTH_LEASE_MS`] from the time it asked, and only if its copy
//! has reached that commit index. The leader assumes the lease lasts [`AUTH_LEASE_MS`] from the
//! time it answered, which is never earlier. A revocation at index `i` is released once every
//! node has acknowledged `i` or its last lease has run out. Without I/O: the node and the leader
//! drive these tables with their messages and their monotonic clocks.

use std::collections::BTreeMap;

/// How long an authorization lease lasts, in milliseconds (plan of 013: renewed every 2 s).
pub const AUTH_LEASE_MS: u64 = 2_000;
/// What a node takes off its lease for the drift between its clock and the leader's (D-004).
pub const DRIFT_MARGIN_MS: u64 = 250;

/// The leader's side: what each node acknowledged and until when it may hold a lease.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FenceTable {
    nodes: BTreeMap<u64, Granted>,
}

/// What the leader knows of one node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Granted {
    /// The highest index of Group 0 the node said it applied.
    acked: u64,
    /// The latest end of a lease it may hold, in the leader's clock.
    until_ms: u64,
}

impl FenceTable {
    /// An empty table: a leader that just took over assumes every node may hold a lease until
    /// one lease from `now_ms`, since it does not know what the previous leader granted.
    pub fn new() -> FenceTable {
        FenceTable::default()
    }

    /// The leader took over at `now_ms` knowing `nodes`: each may hold a lease granted by the
    /// previous leader until one lease from now.
    pub fn take_over(nodes: &[u64], now_ms: u64) -> FenceTable {
        let until_ms = now_ms.saturating_add(AUTH_LEASE_MS);
        FenceTable { nodes: nodes.iter().map(|n| (*n, Granted { acked: 0, until_ms })).collect() }
    }

    /// Node `node` asked for a lease with its copy at `applied`; the answer is `commit`, the
    /// leader's commit index, and the leader counts the lease as granted until one lease from
    /// `now_ms`.
    pub fn request(&mut self, node: u64, applied: u64, commit: u64, now_ms: u64) -> u64 {
        let g = self.nodes.entry(node).or_default();
        g.acked = g.acked.max(applied);
        g.until_ms = g.until_ms.max(now_ms.saturating_add(AUTH_LEASE_MS));
        commit
    }

    /// A node left the cluster: it no longer counts.
    pub fn remove(&mut self, node: u64) {
        self.nodes.remove(&node);
    }

    /// Whether a revocation at `index` may answer at `now_ms`: every node acknowledged it or its
    /// last lease has run out.
    pub fn released(&self, index: u64, now_ms: u64) -> bool {
        self.nodes.values().all(|g| g.acked >= index || g.until_ms <= now_ms)
    }

    /// The highest index up to `commit` that is released at `now_ms`: every change up to it may
    /// answer.
    pub fn fenced(&self, commit: u64, now_ms: u64) -> u64 {
        self.nodes
            .values()
            .filter(|g| g.until_ms > now_ms)
            .map(|g| g.acked)
            .min()
            .map_or(commit, |acked| acked.min(commit))
    }

    /// When a revocation at `index` will be released at the latest if no node acknowledges it:
    /// the end of the last lease granted to a node that has not.
    pub fn deadline(&self, index: u64) -> u64 {
        self.nodes.values().filter(|g| g.acked < index).map(|g| g.until_ms).max().unwrap_or(0)
    }
}

/// The node's side: its lease and the request in flight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthLease {
    until_ms: u64,
    asked: Option<(u64, u64)>,
    next_seq: u64,
}

impl AuthLease {
    /// A node without a lease.
    pub fn new() -> AuthLease {
        AuthLease::default()
    }

    /// Asks for a lease at `now_ms`: the sequence number the request carries. A new request
    /// replaces one still in flight.
    pub fn ask(&mut self, now_ms: u64) -> u64 {
        self.next_seq += 1;
        self.asked = Some((self.next_seq, now_ms));
        self.next_seq
    }

    /// The leader answered request `seq` with commit index `commit`; with the node's copy at
    /// `applied`, the lease runs from when it asked, less the drift margin. A late or unknown
    /// answer, or a copy behind the leader, gives nothing.
    pub fn granted(&mut self, seq: u64, commit: u64, applied: u64) {
        let Some((asked_seq, asked_ms)) = self.asked else { return };
        if asked_seq != seq {
            return;
        }
        self.asked = None;
        if applied < commit {
            return;
        }
        let until = asked_ms.saturating_add(AUTH_LEASE_MS - DRIFT_MARGIN_MS);
        self.until_ms = self.until_ms.max(until);
    }

    /// When the lease ends (0 without one).
    pub fn until_ms(&self) -> u64 {
        self.until_ms
    }

    /// Whether the node may authorize at `now_ms`.
    pub fn valid(&self, now_ms: u64) -> bool {
        now_ms < self.until_ms
    }

    /// The node restarted or lost its copy: no lease until the next grant.
    pub fn drop_lease(&mut self) {
        self.until_ms = 0;
        self.asked = None;
    }
}
