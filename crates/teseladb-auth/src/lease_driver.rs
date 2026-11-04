//! The authorization fence's protocol on one node (013 FR-012, D-073, `spec/tla/AuthzFence.tla`
//! and `spec/tla/AuthzFenceLeaders.tla`), without I/O: the server runs it on core 0 and the
//! simulator on its nodes, each over its replica of Group 0 through [`Group0Host`].
//!
//! Every `ask_every_ns` the node asks Group 0's leader for an authorization lease with the index
//! of its copy. The leader, the node itself included, holds each request ([`HeldAsks`]) until a
//! majority of the voters acknowledges a heartbeat round it sent no earlier than the request
//! arrived, then records it in its [`FenceTable`] and answers with its commit index and the
//! index up to which every change of users and roles is fenced. A leader takes over its table
//! once its copy holds every committed entry, assuming each node it knows may hold a lease of
//! the previous term until one lease from then.

use crate::fence::{AuthLease, FenceTable, HeldAsk, HeldAsks};

/// Nanoseconds in a millisecond.
const NANOS_PER_MS: u64 = 1_000_000;

fn ms(ns: u64) -> u64 {
    ns / NANOS_PER_MS
}

/// What the node's replica of Group 0 says, at the moment of a tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Group0View {
    /// The replica leads Group 0 and has committed and applied the first entry of its term, so
    /// its commit index covers every entry an earlier leader committed.
    pub leads: bool,
    /// The leader it knows (0 for none).
    pub leader: u64,
    /// The replica's term of Group 0.
    pub term: u64,
    /// Group 0's commit index on the replica.
    pub commit: u64,
    /// The index of the node's copy of Group 0.
    pub applied: u64,
}

/// The node's replica of Group 0, as the protocol uses it.
pub trait Group0Host {
    /// Its state, `None` if the node holds no replica.
    fn view(&self) -> Option<Group0View>;
    /// Every node a leader of Group 0 may have granted a lease to: those Group 0 records and its
    /// replicas.
    fn known_nodes(&self) -> Vec<u64>;
    /// As leader, the send time of the latest heartbeat round of its term a majority of the
    /// voters acknowledged, in nanoseconds of the node's clock.
    fn quorum_heard_at_ns(&self) -> Option<u64>;
    /// As leader, sends a heartbeat round now.
    fn send_confirm_round(&mut self, now_ns: u64);
}

/// A message of the protocol, to node `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceMsg {
    /// A request for a lease, to Group 0's leader.
    Ask {
        /// Destination.
        to: u64,
        /// The node that asks.
        node: u64,
        /// The request's number.
        seq: u64,
        /// The index of its copy it acknowledges.
        applied: u64,
    },
    /// The leader's answer.
    Grant {
        /// Destination, the node that asked.
        to: u64,
        /// The request's number.
        seq: u64,
        /// The leader's commit index: the lease holds only on a copy at or past it.
        commit: u64,
        /// Every change of users and roles up to this index is fenced.
        fenced: u64,
    },
}

/// The lease of a node and, while it leads Group 0, the table of the others'.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeaseDriver {
    ask_every_ns: u64,
    next_ns: u64,
    lease: AuthLease,
    /// While it leads: its table and the term it leads.
    table: Option<(FenceTable, u64)>,
    fenced: u64,
    held: HeldAsks,
}

impl LeaseDriver {
    /// A driver that asks for a lease every `ask_every_ns`.
    pub fn new(ask_every_ns: u64) -> LeaseDriver {
        LeaseDriver { ask_every_ns, ..LeaseDriver::default() }
    }

    /// Whether the node holds its lease at `now_ns`.
    pub fn holds(&self, now_ns: u64) -> bool {
        self.lease.valid(ms(now_ns))
    }

    /// When the node's lease ends, in milliseconds of its clock (0 without one).
    pub fn until_ms(&self) -> u64 {
        self.lease.until_ms()
    }

    /// The index up to which changes of users and roles are fenced.
    pub fn fenced(&self) -> u64 {
        self.fenced
    }

    /// Whether the node keeps a table as Group 0's leader.
    pub fn leads(&self) -> bool {
        self.table.is_some()
    }

    /// The node restarted: no lease and no table until the protocol gives them again.
    pub fn restart(&mut self) {
        *self = LeaseDriver::new(self.ask_every_ns);
    }

    /// A tick of node `me` at `now_ns`: its request, which the leader holds like any other, and
    /// the answers to the requests a majority's acknowledgement confirms. `acked` gives the index
    /// the node acknowledges with its copy at the index it is given (the server holds it back for
    /// its running operations, 009 FR-006). Returns what to send.
    pub fn tick(
        &mut self,
        host: &mut dyn Group0Host,
        me: u64,
        now_ns: u64,
        acked: &dyn Fn(u64) -> u64,
    ) -> Vec<FenceMsg> {
        let Some(v) = host.view() else { return Vec::new() };
        let now = ms(now_ns);
        // A table belongs to one term: a replica that led, lost the term and won a later one
        // between two ticks starts afresh, since another leader may have granted leases between.
        if !v.leads || self.table.as_ref().is_some_and(|(_, term)| *term != v.term) {
            self.table = None;
            self.held.clear();
        }
        if v.leads && self.table.is_none() && v.applied >= v.commit {
            // It takes over once its copy holds every committed entry, so that it knows every
            // node a previous leader may have granted a lease to.
            self.table = Some((FenceTable::take_over(&host.known_nodes(), now), v.term));
            // Its own request goes now: a node that has just been elected does not wait out
            // the period without a lease.
            self.next_ns = now_ns;
        }
        if let Some((table, _)) = &self.table {
            self.fenced = self.fenced.max(table.fenced(v.commit, now));
        }
        let mut out = Vec::new();
        if now_ns >= self.next_ns {
            self.next_ns = now_ns.saturating_add(self.ask_every_ns);
            let seq = self.lease.ask(now);
            let applied = acked(v.applied);
            if self.table.is_some() {
                self.held.hold(HeldAsk { node: me, seq, applied, at_ns: now_ns });
            } else if v.leader != 0 && v.leader != me {
                out.push(FenceMsg::Ask { to: v.leader, node: me, seq, applied });
            }
        }
        out.extend(self.answer(host, me, v, now_ns));
        out
    }

    /// As leader, sends a round for the requests that arrived since the last one and answers
    /// those a majority confirmed.
    fn answer(
        &mut self,
        host: &mut dyn Group0Host,
        me: u64,
        v: Group0View,
        now_ns: u64,
    ) -> Vec<FenceMsg> {
        let Some((table, _)) = &mut self.table else { return Vec::new() };
        if self.held.round_due() {
            host.send_confirm_round(now_ns);
            self.held.round_sent(now_ns);
        }
        let now = ms(now_ns);
        let mut out = Vec::new();
        for a in self.held.release(host.quorum_heard_at_ns()) {
            let commit = table.request(a.node, a.applied, v.commit, now);
            if a.node == me {
                self.lease.granted(a.seq, commit, v.applied);
            } else {
                let fenced = table.fenced(commit, now);
                out.push(FenceMsg::Grant { to: a.node, seq: a.seq, commit, fenced });
            }
        }
        out
    }

    /// Node `node` asked at `now_ns` with its copy at `applied`: held while the node leads,
    /// dropped otherwise.
    pub fn on_ask(&mut self, node: u64, seq: u64, applied: u64, now_ns: u64) {
        if self.table.is_some() {
            self.held.hold(HeldAsk { node, seq, applied, at_ns: now_ns });
        }
    }

    /// The leader answered request `seq`; the node's copy is at `applied`.
    pub fn on_grant(&mut self, seq: u64, commit: u64, fenced: u64, applied: u64) {
        self.lease.granted(seq, commit, applied);
        self.fenced = self.fenced.max(fenced);
    }
}
