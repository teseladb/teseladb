//! Raft state machine of one group.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use teseladb_types::{Bytes, GroupId, Nanos, NodeId, Rng};

use crate::conf::{ConfChange, ConfState};
use crate::config::RaftConfig;
use crate::log::{LogGap, LogStore, RaftLog};
use crate::message::{Entry, EntryKind, HardState, Message, MessageType, Snapshot};
use crate::progress::{Progress, ProgressState};
use crate::ready::{ReadState, Ready, SoftState, Status};

/// Role of a node in the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Follower.
    Follower,
    /// Asks for pre-votes without changing term.
    PreCandidate,
    /// Asks for votes.
    Candidate,
    /// Leader.
    Leader,
    /// Learner: receives the log, never votes or stands for election.
    Learner,
}

/// Identifier of a proposal: the index of its entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalId(pub u64);

/// Why a proposal or a read is not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposeError {
    /// Not the leader (with the known leader, 0 if none).
    NotLeader {
        /// Known leader.
        hint: NodeId,
    },
    /// Dropped (leadership transfer in progress or too much uncommitted).
    Dropped,
    /// Entry larger than `max_entry_bytes`.
    TooLarge,
    /// The leader has not yet committed and applied its term's entry.
    NotReady,
    /// A configuration change is already in flight.
    ConfChangePending,
    /// The change is not valid in the current configuration.
    InvalidConfChange,
}

impl fmt::Display for ProposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ProposeError {}

/// Message that cannot be processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepError {
    /// Addressed to another node.
    WrongTarget,
    /// From itself (a loop).
    FromSelf,
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for StepError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Campaign {
    Pre,
    Election { transfer_ctx: u64 },
}

/// Heartbeat round: when it was sent and who has confirmed it.
#[derive(Clone, Debug)]
struct Round {
    seq: u64,
    sent_at: Nanos,
    acks: BTreeSet<NodeId>,
    done: bool,
}

/// ReadIndex read waiting to be confirmed by a heartbeat round.
#[derive(Clone, Debug)]
struct PendingRead {
    ctx: u64,
    index: u64,
    round: u64,
    requester: NodeId,
}

/// Heartbeat rounds remembered for the lease and the reads.
const ROUNDS_KEPT: usize = 32;

/// A Raft group.
#[derive(Debug)]
pub struct RawNode {
    cfg: RaftConfig,
    group: GroupId,
    id: NodeId,
    term: u64,
    vote: NodeId,
    leader: NodeId,
    role: Role,
    log: RaftLog,
    conf: ConfState,
    prs: BTreeMap<NodeId, Progress>,
    votes: BTreeMap<NodeId, bool>,
    election_elapsed: u32,
    heartbeat_elapsed: u32,
    randomized_timeout: u32,
    ticks_since_boot: u32,
    now: Nanos,
    lease_until: Nanos,
    lease_seq: u64,
    lease_disabled: bool,
    /// The leader heard from a quorum in its last check (every election timeout).
    has_quorum: bool,
    /// Ticks the leader has gone without hearing from a quorum.
    quorum_lost_ticks: u32,
    rounds: VecDeque<Round>,
    noop_index: u64,
    pending_conf_index: u64,
    reads: Vec<PendingRead>,
    read_states: Vec<ReadState>,
    msgs: Vec<Message>,
    msgs_after: Vec<Message>,
    prev_hs: HardState,
    prev_soft: SoftState,
    ready_number: u64,
    transfer: Option<(NodeId, u32)>,
    uncommitted_bytes: usize,
    /// Quiescent: the leader sends no heartbeats and the followers count no timeout.
    quiesced: bool,
    /// Ticks the leader has been idle with every follower up to date.
    idle_ticks: u32,
    /// The node may campaign and take leadership: false while its clock is out of margin
    /// (005 FR-006).
    may_lead: bool,
    /// The state machine waits for a snapshot (its data is missing): nothing is handed out to
    /// apply, the node does not campaign, and it installs the snapshot it waits for also at or
    /// below the commit index (007 T016).
    awaiting_snapshot: bool,
    /// The index at or below which the log does not rebuild the state machine's state (data
    /// entered without an entry): a follower whose log ends below it gets a snapshot.
    log_floor: u64,
}

impl RawNode {
    /// Recovered (or new) group with its configuration and hard state.
    pub fn new(
        cfg: RaftConfig,
        group: GroupId,
        conf: ConfState,
        hs: HardState,
        store: &dyn LogStore,
        rng: &mut Rng,
    ) -> RawNode {
        cfg.validate().expect("valid RaftConfig");
        let id = cfg.id;
        let mut n = RawNode {
            group,
            id,
            term: hs.term,
            vote: hs.vote,
            leader: NodeId(0),
            role: Role::Follower,
            log: RaftLog::new(store, hs.commit),
            conf,
            prs: BTreeMap::new(),
            votes: BTreeMap::new(),
            election_elapsed: 0,
            heartbeat_elapsed: 0,
            randomized_timeout: 0,
            ticks_since_boot: 0,
            now: Nanos::ZERO,
            lease_until: Nanos::ZERO,
            has_quorum: false,
            quorum_lost_ticks: 0,
            lease_seq: 0,
            lease_disabled: false,
            rounds: VecDeque::new(),
            noop_index: 0,
            pending_conf_index: 0,
            quiesced: false,
            idle_ticks: 0,
            may_lead: true,
            awaiting_snapshot: false,
            log_floor: 0,
            reads: Vec::new(),
            read_states: Vec::new(),
            msgs: Vec::new(),
            msgs_after: Vec::new(),
            prev_hs: HardState { term: hs.term, vote: hs.vote, commit: 0 },
            prev_soft: SoftState { leader: NodeId(0), role: Role::Follower },
            ready_number: 0,
            transfer: None,
            uncommitted_bytes: 0,
            cfg,
        };
        n.prev_hs.commit = n.log.committed;
        n.role = n.follower_role();
        n.prev_soft.role = n.role;
        n.reset_timeout(rng);
        n
    }

    /// Group.
    pub fn group(&self) -> GroupId {
        self.group
    }

    fn follower_role(&self) -> Role {
        if self.conf.is_learner(self.id) { Role::Learner } else { Role::Follower }
    }

    fn promotable(&self) -> bool {
        self.may_lead && !self.awaiting_snapshot && self.conf.is_voter(self.id)
    }

    /// Whether this node may campaign and take leadership (005 FR-006: not while its clock is out
    /// of margin). It still votes and follows; a leader is not demoted by this.
    pub fn set_may_lead(&mut self, may: bool) {
        self.may_lead = may;
    }

    /// Whether the state machine waits for a snapshot (007 T016: the data of the node's own
    /// snapshot is missing after a restart). Meanwhile the node votes, appends and commits, so
    /// that its group keeps a quorum, but hands nothing out to apply and does not campaign; the
    /// snapshot it waits for is installed also at or below the commit index.
    pub fn set_awaiting_snapshot(&mut self, on: bool) {
        self.awaiting_snapshot = on;
    }

    /// The state machine's state no longer follows from the log at or below `index` (data entered
    /// without an entry, as a restore's ingest): as the leader, a follower whose log ends below it
    /// is sent the snapshot, never those entries, even while the log holds them.
    pub fn set_log_floor(&mut self, index: u64) {
        self.log_floor = index;
    }

    /// `peer` waits for a snapshot and cannot use its log (its state machine lacks the data of the
    /// log's start): the newest snapshot goes to it, however much of the log it has. A transfer on
    /// its way already answers it. Without a snapshot, probing starts again as after a failed
    /// transfer.
    pub fn request_snapshot(&mut self, peer: NodeId, store: &dyn LogStore) {
        let in_flight =
            self.prs.get(&peer).is_some_and(|p| matches!(p.state, ProgressState::Snapshot(_)));
        if self.role != Role::Leader || in_flight {
            return;
        }
        if store.snapshot_meta().index == 0 {
            self.retry_snapshot(peer, store);
            return;
        }
        if self.prs.contains_key(&peer) {
            self.send_snapshot(peer, store);
        }
    }

    /// There are committed configuration changes not yet handed out: the current configuration
    /// may be stale (e.g. after a restart), so it is not used to campaign.
    fn has_unapplied_conf_changes(&self, store: &dyn LogStore) -> bool {
        let (lo, hi) = (self.log.handed_apply + 1, self.log.committed + 1);
        lo < hi
            && self
                .log
                .entries(store, lo, hi, u64::MAX)
                .is_ok_and(|es| es.iter().any(|e| e.kind == EntryKind::ConfChange))
    }

    fn quorum(&self) -> usize {
        self.conf.quorum()
    }

    fn single_voter(&self) -> bool {
        self.conf.voters == [self.id]
    }

    fn reset_timeout(&mut self, rng: &mut Rng) {
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.randomized_timeout = rng
            .range(u64::from(self.cfg.election_ticks_min), u64::from(self.cfg.election_ticks_max))
            as u32;
    }

    fn send(&mut self, m: Message) {
        self.msgs.push(m);
    }

    fn send_after_persist(&mut self, m: Message) {
        self.msgs_after.push(m);
    }

    fn msg(&self, ty: MessageType, to: NodeId) -> Message {
        Message::new(ty, self.id, to, self.term)
    }

    // ----- role changes -----

    fn become_follower(&mut self, term: u64, leader: NodeId, rng: &mut Rng) {
        if term != self.term {
            self.term = term;
            self.vote = NodeId(0);
        }
        self.leader = leader;
        self.role = self.follower_role();
        self.prs.clear();
        self.votes.clear();
        self.reads.clear();
        self.rounds.clear();
        self.lease_until = Nanos::ZERO;
        self.lease_disabled = false;
        self.transfer = None;
        self.uncommitted_bytes = 0;
        self.reset_timeout(rng);
    }

    fn become_leader(&mut self, store: &dyn LogStore) {
        self.role = Role::Leader;
        self.leader = self.id;
        self.votes.clear();
        self.lease_until = Nanos::ZERO;
        self.lease_disabled = false;
        self.has_quorum = true;
        self.quorum_lost_ticks = 0;
        self.rounds.clear();
        self.transfer = None;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        let last = self.log.last_index(store);
        self.prs.clear();
        for m in self.conf.members().collect::<Vec<_>>() {
            if m != self.id {
                let mut p = Progress::new(last, self.conf.is_learner(m));
                p.recent_active = true;
                self.prs.insert(m, p);
            }
        }
        // Empty entry of the term: until it is committed there are no reads or configuration changes.
        self.noop_index = last + 1;
        self.pending_conf_index = last + 1;
        let noop =
            Entry { index: last + 1, term: self.term, kind: EntryKind::Normal, data: Bytes::new() };
        self.log.append(store, &[noop]);
        self.bcast_append(store);
        if self.single_voter() {
            self.maybe_commit(store);
        }
    }

    fn campaign(&mut self, kind: Campaign, store: &dyn LogStore, rng: &mut Rng) {
        if !self.promotable() || self.has_unapplied_conf_changes(store) {
            return;
        }
        let (ty, term, ctx) = match kind {
            Campaign::Pre => {
                self.role = Role::PreCandidate;
                self.leader = NodeId(0);
                (MessageType::PreVoteReq, self.term + 1, 0)
            }
            Campaign::Election { transfer_ctx } => {
                self.become_follower(self.term + 1, NodeId(0), rng);
                self.role = Role::Candidate;
                self.vote = self.id;
                (MessageType::VoteReq, self.term, transfer_ctx)
            }
        };
        self.votes.clear();
        self.votes.insert(self.id, true);
        if self.votes_won() {
            match kind {
                Campaign::Pre => self.campaign(Campaign::Election { transfer_ctx: 0 }, store, rng),
                Campaign::Election { .. } => self.become_leader(store),
            }
            return;
        }
        let (last, last_term) = (self.log.last_index(store), self.log.last_term(store));
        for v in self.conf.voters.clone() {
            if v == self.id {
                continue;
            }
            let mut m = Message::new(ty, self.id, v, term);
            m.index = last;
            m.log_term = last_term;
            m.ctx = ctx;
            // Vote request: the node's own vote has to be persisted first.
            if ty == MessageType::VoteReq {
                self.send_after_persist(m);
            } else {
                self.send(m);
            }
        }
    }

    fn votes_won(&self) -> bool {
        self.votes.iter().filter(|(n, g)| **g && self.conf.is_voter(**n)).count() >= self.quorum()
    }

    fn votes_lost(&self) -> bool {
        self.votes.iter().filter(|(n, g)| !**g && self.conf.is_voter(**n)).count() >= self.quorum()
    }

    // ----- events -----

    /// Advance of the logical clock (one global tick of the core); `now` is the monotonic clock.
    pub fn tick(&mut self, store: &dyn LogStore, rng: &mut Rng, now: Nanos) {
        self.now = now;
        self.ticks_since_boot = self.ticks_since_boot.saturating_add(1);
        if self.quiesced {
            // Nothing happens until something wakes the group.
            return;
        }
        if self.role == Role::Leader {
            self.tick_leader(store, rng);
        } else {
            self.election_elapsed += 1;
            if self.promotable() && self.election_elapsed >= self.randomized_timeout {
                let kind = if self.cfg.pre_vote {
                    Campaign::Pre
                } else {
                    Campaign::Election { transfer_ctx: 0 }
                };
                self.campaign(kind, store, rng);
            }
        }
    }

    fn tick_leader(&mut self, store: &dyn LogStore, rng: &mut Rng) {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;
        if self.election_elapsed >= self.cfg.election_ticks_min {
            self.election_elapsed = 0;
            // No quorum during the whole timeout: its lease runs out by itself and it stops
            // holding off other candidates; it abdicates once that lasted `step_down_ticks`.
            self.has_quorum = !self.cfg.check_quorum || self.quorum_active();
            if self.has_quorum {
                self.quorum_lost_ticks = 0;
            } else {
                self.quorum_lost_ticks += self.cfg.election_ticks_min;
                if self.quorum_lost_ticks >= self.cfg.step_down_ticks {
                    self.become_follower(self.term, NodeId(0), rng);
                    return;
                }
            }
            for p in self.prs.values_mut() {
                p.recent_active = false;
            }
            // A transfer that does not finish within the timeout is abandoned.
            self.transfer = None;
        }
        if self.heartbeat_elapsed >= self.cfg.heartbeat_ticks {
            self.heartbeat_elapsed = 0;
            self.bcast_heartbeat(store);
        }
        // A leader cut off from its quorum does not go quiet: it has to step down in time.
        if self.cfg.quiesce_after_ticks > 0 && self.has_quorum && self.idle(store) {
            self.idle_ticks += 1;
            if self.idle_ticks >= self.cfg.quiesce_after_ticks {
                self.quiesce();
            }
        } else {
            self.idle_ticks = 0;
        }
    }

    /// Nothing in flight: every entry committed and applied, every follower has the whole log, no
    /// read, transfer, configuration change or snapshot pending. A pending configuration change is
    /// an entry of the log (`pending_conf_index <= last`), so it is covered by `applied == last`.
    fn idle(&self, store: &dyn LogStore) -> bool {
        let last = self.log.last_index(store);
        self.log.committed == last
            && self.log.applied == last
            && self.reads.is_empty()
            && self.transfer.is_none()
            && self
                .prs
                .values()
                .all(|p| p.matched == last && !matches!(p.state, ProgressState::Snapshot(_)))
    }

    /// The leader goes quiet: one last heartbeat with round 0 tells the followers to stop their
    /// timers; it holds no lease while quiet.
    fn quiesce(&mut self) {
        self.quiesced = true;
        self.lease_until = Nanos::ZERO;
        let msgs: Vec<Message> = self
            .prs
            .iter()
            .map(|(&to, pr)| {
                let mut m = Message::new(MessageType::Heartbeat, self.id, to, self.term);
                m.commit = self.log.committed.min(pr.matched);
                m
            })
            .collect();
        self.msgs.extend(msgs);
    }

    /// Leaves quiescence: a leader announces itself again at once, a follower starts counting its
    /// timeout from zero.
    fn unquiesce(&mut self, store: &dyn LogStore) {
        if !self.quiesced {
            return;
        }
        self.quiesced = false;
        self.idle_ticks = 0;
        self.election_elapsed = 0;
        if self.role == Role::Leader {
            self.heartbeat_elapsed = 0;
            self.bcast_heartbeat(store);
        }
    }

    /// The group is quiescent.
    pub fn is_quiesced(&self) -> bool {
        self.quiesced
    }

    /// A leader that did not hear from a quorum in its last check: it keeps the role and takes
    /// writes until `step_down_ticks`, but it cannot confirm a read.
    pub fn lost_quorum(&self) -> bool {
        self.role == Role::Leader && !self.has_quorum
    }

    fn quorum_active(&self) -> bool {
        let active = self
            .conf
            .voters
            .iter()
            .filter(|&&v| v == self.id || self.prs.get(&v).is_some_and(|p| p.recent_active))
            .count();
        active >= self.quorum()
    }

    /// Processes a message from another node of the group.
    pub fn step(
        &mut self,
        m: Message,
        store: &dyn LogStore,
        rng: &mut Rng,
    ) -> Result<(), StepError> {
        if m.to != self.id {
            return Err(StepError::WrongTarget);
        }
        if m.from == self.id {
            return Err(StepError::FromSelf);
        }
        // The leader's quiet heartbeat (round 0) puts the follower to rest; a request wakes the
        // group (answers still on their way when it went quiet do not, unless they bring a newer
        // term).
        let quiet = m.ty == MessageType::Heartbeat && m.ctx == 0;
        let answer = matches!(
            m.ty,
            MessageType::AppendResp
                | MessageType::HeartbeatResp
                | MessageType::VoteResp
                | MessageType::PreVoteResp
                | MessageType::ReadIndexResp
        );
        if !quiet && (!answer || m.term > self.term) {
            self.unquiesce(store);
        }
        if m.term > self.term {
            match m.ty {
                MessageType::VoteReq | MessageType::PreVoteReq => {
                    if self.in_lease_guard(m.ctx) {
                        // It heard the leader recently: it does not acknowledge the new term.
                        let mut r = self.msg(vote_resp(m.ty), m.from);
                        r.reject = true;
                        self.send(r);
                        return Ok(());
                    }
                    if m.ty == MessageType::VoteReq {
                        self.become_follower(m.term, NodeId(0), rng);
                    }
                }
                // A granted pre-vote carries the election's term (the current one + 1).
                MessageType::PreVoteResp if !m.reject => {}
                MessageType::AppendReq | MessageType::Heartbeat | MessageType::SnapshotReq => {
                    self.become_follower(m.term, m.from, rng);
                }
                _ => self.become_follower(m.term, NodeId(0), rng),
            }
        } else if m.term < self.term {
            match m.ty {
                // An old leader: it gets an answer so that it sees the new term and abdicates.
                MessageType::AppendReq | MessageType::Heartbeat | MessageType::SnapshotReq => {
                    let r = self.msg(MessageType::AppendResp, m.from);
                    self.send(r);
                }
                MessageType::PreVoteReq => {
                    let mut r = self.msg(MessageType::PreVoteResp, m.from);
                    r.reject = true;
                    self.send(r);
                }
                _ => {}
            }
            return Ok(());
        }
        match m.ty {
            MessageType::VoteReq | MessageType::PreVoteReq => self.handle_vote(m, store),
            MessageType::VoteResp | MessageType::PreVoteResp => {
                self.handle_vote_resp(m, store, rng)
            }
            MessageType::AppendReq => {
                self.heard_leader(m.from, rng);
                self.handle_append(m, store);
            }
            MessageType::Heartbeat => {
                self.heard_leader(m.from, rng);
                let last = self.log.last_index(store);
                self.log.commit_to(m.commit.min(last));
                if quiet {
                    self.quiesced = true;
                } else {
                    let mut r = self.msg(MessageType::HeartbeatResp, m.from);
                    r.ctx = m.ctx;
                    self.send(r);
                }
            }
            MessageType::SnapshotReq => {
                self.heard_leader(m.from, rng);
                self.handle_snapshot(m, store);
            }
            MessageType::TimeoutNow => {
                if self.promotable() && self.role != Role::Leader {
                    self.campaign(Campaign::Election { transfer_ctx: m.term }, store, rng);
                }
            }
            MessageType::AppendResp => self.handle_append_resp(m, store),
            MessageType::HeartbeatResp => self.handle_heartbeat_resp(m, store),
            MessageType::ReadIndexReq => {
                if self.role == Role::Leader && self.ready_for_reads() {
                    self.register_read(m.ctx, m.from, store);
                }
            }
            MessageType::ReadIndexResp => {
                if self.role != Role::Leader {
                    self.read_states.push(ReadState { ctx: m.ctx, index: m.index });
                }
            }
        }
        Ok(())
    }

    /// Rule of not voting while hearing the current leader (guarantee 5), except a transfer from
    /// this term's leader.
    fn in_lease_guard(&self, ctx: u64) -> bool {
        let transfer = ctx != 0 && ctx == self.term;
        // The leader too: it "hears" itself while it keeps the quorum.
        let hearing = match self.role {
            Role::Leader => self.has_quorum,
            _ => self.leader.0 != 0 && self.election_elapsed < self.cfg.election_ticks_min,
        };
        // Faulty variant (005 R4): votes while the leader may still be serving lease reads.
        #[cfg(teseladb_bug = "vote_during_lease")]
        let hearing = {
            let _ = hearing; // the faulty variant ignores the real value
            false
        };
        self.cfg.check_quorum && !transfer && hearing
    }

    fn heard_leader(&mut self, from: NodeId, rng: &mut Rng) {
        if self.role != Role::Follower && self.role != Role::Learner {
            self.become_follower(self.term, from, rng);
        }
        self.leader = from;
        self.election_elapsed = 0;
    }

    fn handle_vote(&mut self, m: Message, store: &dyn LogStore) {
        let pre = m.ty == MessageType::PreVoteReq;
        let up_to_date = self.log.is_up_to_date(store, m.index, m.log_term);
        let booting = self.ticks_since_boot < self.cfg.election_ticks_max;
        // Faulty variant (005 R4): a node that has just started votes although it does not know
        // whether a leader is still serving lease reads.
        #[cfg(teseladb_bug = "vote_after_restart")]
        let booting = {
            let _ = booting; // the faulty variant ignores the real value
            false
        };
        let can_vote = self.vote.0 == 0 || self.vote == m.from;
        // Faulty variant (005 R4): votes for a second candidate in the same term.
        #[cfg(teseladb_bug = "vote_twice")]
        let can_vote = {
            let _ = can_vote; // the faulty variant ignores the real value
            true
        };
        // Neither this node nor the candidate has to vote in this node's configuration: one that
        // was just promoted may not have applied the promotion yet, and only the candidate's
        // configuration decides which votes count (guarantee 8). Requiring it can leave a group
        // without a leader for good.
        // A vote request reaches this point with this node's term: `step` adopted a higher one and
        // answered a lower one.
        // A pre-vote is for the next term: for this node's own term it would let a second
        // candidate stand in a term this node already voted in, and split the vote (as anotherproduct).
        let fresh = pre && m.term > self.term;
        let grant = !booting && up_to_date && !self.in_lease_guard(m.ctx) && (fresh || can_vote);
        let mut r = self.msg(vote_resp(m.ty), m.from);
        r.reject = !grant;
        if pre && grant {
            // The granted pre-vote carries the election's term.
            r.term = m.term;
        }
        if grant && !pre {
            self.vote = m.from;
            self.election_elapsed = 0;
        }
        if pre {
            self.send(r);
        } else {
            self.send_after_persist(r);
        }
    }

    fn handle_vote_resp(&mut self, m: Message, store: &dyn LogStore, rng: &mut Rng) {
        let pre = m.ty == MessageType::PreVoteResp;
        let expected =
            if pre { (Role::PreCandidate, self.term + 1) } else { (Role::Candidate, self.term) };
        if self.role != expected.0 || (m.term != expected.1 && !m.reject) {
            return;
        }
        self.votes.insert(m.from, !m.reject);
        if self.votes_won() {
            if pre {
                self.campaign(Campaign::Election { transfer_ctx: 0 }, store, rng);
            } else {
                self.become_leader(store);
            }
        } else if self.votes_lost() {
            self.become_follower(self.term, NodeId(0), rng);
            // Faulty variant (002 T014): the lost vote leaves the election timer disarmed, so the
            // node waits for a leader that may never come.
            #[cfg(teseladb_bug = "no_timer_after_lost_vote")]
            {
                self.randomized_timeout = u32::MAX;
            }
        }
    }

    fn handle_append(&mut self, m: Message, store: &dyn LogStore) {
        let mut r = self.msg(MessageType::AppendResp, m.from);
        // Every acknowledgement waits for the log to be durable: the commit index may cover entries
        // still being persisted, and the leader takes `index` as matched for good (guarantee 9).
        if m.index < self.log.committed {
            r.index = self.log.committed;
            self.send_after_persist(r);
            return;
        }
        match self.log.maybe_append(store, m.index, m.log_term, &m.entries) {
            Some(last_new) => {
                self.log.commit_to(m.commit.min(last_new));
                r.index = last_new;
                // Only once what was received is durable.
                self.send_after_persist(r);
            }
            None => {
                r.reject = true;
                r.index = m.index;
                r.reject_hint = self.log.last_index(store);
                self.send(r);
            }
        }
    }

    fn handle_snapshot(&mut self, m: Message, store: &dyn LogStore) {
        let Some(s) = m.snapshot else { return };
        let mut r = self.msg(MessageType::AppendResp, m.from);
        let (idx, term) = (s.meta.index, s.meta.term);
        // The state machine waits for this one: it installs it even at or below the commit index.
        if self.awaiting_snapshot && idx > self.log.handed_apply {
            self.install_awaited(s, r, store);
            return;
        }
        if idx <= self.log.committed {
            r.index = self.log.committed;
            self.send_after_persist(r);
            return;
        }
        if self.log.matches(store, idx, term) {
            // It already has those entries (maybe still being persisted): advancing the commit
            // index is enough.
            self.log.commit_to(idx);
            r.index = idx;
            self.send_after_persist(r);
            return;
        }
        self.conf = s.meta.conf.clone();
        self.role = self.follower_role();
        self.log.restore(s);
        r.index = idx;
        self.send_after_persist(r);
    }

    /// The snapshot a waiting state machine needs: at an entry the durable log has, the log past it
    /// stays (below the commit index the entry is a committed one); at or past the commit index,
    /// it replaces the log. Below the commit index at an entry not durable yet, it waits: the
    /// follower answers with its commit index and asks again.
    fn install_awaited(&mut self, s: Snapshot, mut r: Message, store: &dyn LogStore) {
        let idx = s.meta.index;
        let keep = idx <= self.log.persisted && self.log.matches(store, idx, s.meta.term);
        if !keep && idx < self.log.committed {
            r.index = self.log.committed;
            self.send_after_persist(r);
            return;
        }
        if keep {
            self.log.restore_keeping_log(s);
        } else {
            self.conf = s.meta.conf.clone();
            self.role = self.follower_role();
            self.log.restore(s);
        }
        r.index = idx;
        self.send_after_persist(r);
    }

    fn handle_append_resp(&mut self, m: Message, store: &dyn LogStore) {
        if self.role != Role::Leader {
            return;
        }
        let last = self.log.last_index(store);
        let Some(pr) = self.prs.get_mut(&m.from) else { return };
        pr.recent_active = true;
        if m.reject {
            if pr.maybe_decr(m.index, m.reject_hint) {
                if pr.state == ProgressState::Replicate {
                    pr.become_probe();
                }
                self.send_append(m.from, store);
            }
            return;
        }
        let updated = pr.maybe_update(m.index);
        match pr.state {
            ProgressState::Probe if updated => pr.become_replicate(),
            // Also without an update: a follower sent a snapshot it asked for may already have the
            // log up to it, answer with that and install nothing (007 T016), and no report of
            // the transfer comes.
            ProgressState::Snapshot(idx) if pr.matched >= idx => {
                pr.become_probe();
                pr.become_replicate();
            }
            _ => {}
        }
        if updated && self.maybe_commit(store) {
            self.bcast_append(store);
        }
        let caught_up = self.prs.get(&m.from).is_some_and(|p| p.matched >= last);
        if let Some((target, _)) = self.transfer
            && target == m.from
            && caught_up
        {
            self.send_timeout_now(target);
        }
        if !caught_up {
            self.send_append(m.from, store);
        }
    }

    fn handle_heartbeat_resp(&mut self, m: Message, store: &dyn LogStore) {
        if self.role != Role::Leader {
            return;
        }
        // The round trip of the round it answers (at tick resolution: `now` moves with `tick`).
        let sent = self.rounds.iter().find(|r| r.seq == m.ctx).map(|r| r.sent_at);
        let now = self.now;
        let Some(pr) = self.prs.get_mut(&m.from) else { return };
        pr.recent_active = true;
        pr.rtt = sent.map(|s| now.saturating_since(s)).or(pr.rtt);
        let behind = pr.matched < self.log.last_index(store);
        match pr.state {
            ProgressState::Probe => pr.probe_sent = false,
            // Window full of messages that may have been lost: one is freed so that the follower
            // can reject and probing starts again.
            ProgressState::Replicate if pr.inflight.len() >= self.cfg.max_inflight_msgs => {
                pr.inflight.pop_front();
            }
            _ => {}
        }
        if !pr.learner {
            self.ack_round(m.ctx, m.from, store);
        }
        if behind {
            self.send_append(m.from, store);
        }
    }

    // ----- replication -----

    fn bcast_append(&mut self, store: &dyn LogStore) {
        let peers: Vec<NodeId> = self.prs.keys().copied().collect();
        for p in peers {
            self.send_append(p, store);
        }
    }

    fn send_append(&mut self, to: NodeId, store: &dyn LogStore) {
        let max_inflight = self.cfg.max_inflight_msgs;
        // Callers name a peer of the configuration (voters and learners all have a progress).
        let pr = self.prs.get(&to).expect("a peer with progress");
        if pr.paused(max_inflight) {
            return;
        }
        let next = pr.next;
        if next <= self.log_floor {
            // Only a snapshot past the floor rebuilds the follower: until one is saved, it waits
            // (each answer of the follower comes back here).
            if store.snapshot_meta().index >= self.log_floor {
                self.send_snapshot(to, store);
            }
            return;
        }
        let last = self.log.last_index(store);
        let prev_term = self.log.term(store, next - 1);
        let entries = self.log.entries(store, next, last + 1, self.cfg.max_msg_bytes as u64);
        match (prev_term, entries) {
            (Ok(log_term), Ok(entries)) => {
                let mut m = self.msg(MessageType::AppendReq, to);
                m.index = next - 1;
                m.log_term = log_term;
                m.commit = self.log.committed;
                let sent_last = entries.last().map_or(next - 1, |e| e.index);
                m.entries = entries;
                self.prs.get_mut(&to).expect("follower").sent(sent_last);
                self.send(m);
            }
            (Err(LogGap::Compacted), _) | (_, Err(LogGap::Compacted)) => {
                self.send_snapshot(to, store)
            }
            _ => {}
        }
    }

    fn send_snapshot(&mut self, to: NodeId, store: &dyn LogStore) {
        let meta = store.snapshot_meta();
        let mut m = self.msg(MessageType::SnapshotReq, to);
        m.index = meta.index;
        m.log_term = meta.term;
        // The caller sends the data in separate chunks (`SnapshotChunk`).
        m.snapshot = Some(Snapshot { meta: meta.clone(), data: Bytes::new() });
        let pr = self.prs.get_mut(&to).expect("follower");
        pr.state = ProgressState::Snapshot(meta.index);
        pr.probe_sent = false;
        pr.inflight.clear();
        self.send(m);
    }

    fn bcast_heartbeat(&mut self, store: &dyn LogStore) {
        self.lease_seq += 1;
        let seq = self.lease_seq;
        let mut acks = BTreeSet::new();
        acks.insert(self.id);
        self.rounds.push_back(Round { seq, sent_at: self.now, acks, done: false });
        while self.rounds.len() > ROUNDS_KEPT {
            self.rounds.pop_front();
        }
        let msgs: Vec<Message> = self
            .prs
            .iter()
            .map(|(&to, pr)| {
                let mut m = Message::new(MessageType::Heartbeat, self.id, to, self.term);
                // Never beyond what the follower has in common with the leader.
                m.commit = self.log.committed.min(pr.matched);
                m.ctx = seq;
                m
            })
            .collect();
        self.msgs.extend(msgs);
        // With a single voter the round is confirmed when sent.
        self.check_round(self.rounds.len() - 1, store);
    }

    fn ack_round(&mut self, seq: u64, from: NodeId, store: &dyn LogStore) {
        let Some(at) = self.rounds.iter().position(|r| r.seq == seq) else { return };
        self.rounds[at].acks.insert(from);
        self.check_round(at, store);
    }

    /// If the round at position `at` of `rounds` has a majority: renews the lease from its send
    /// time and confirms the reads registered before it.
    fn check_round(&mut self, at: usize, store: &dyn LogStore) {
        let quorum = self.quorum();
        let voters = &self.conf.voters;
        let r = &mut self.rounds[at];
        let seq = r.seq;
        if r.done || r.acks.iter().filter(|n| voters.binary_search(n).is_ok()).count() < quorum {
            return;
        }
        r.done = true;
        let until = r.sent_at + self.cfg.lease_dur;
        if self.cfg.check_quorum {
            self.lease_until = self.lease_until.max(until);
        }
        let (ready, rest): (Vec<PendingRead>, Vec<PendingRead>) =
            core::mem::take(&mut self.reads).into_iter().partition(|p| p.round <= seq);
        self.reads = rest;
        for p in ready {
            self.answer_read(p, store);
        }
    }

    fn answer_read(&mut self, p: PendingRead, _store: &dyn LogStore) {
        if p.requester == self.id {
            self.read_states.push(ReadState { ctx: p.ctx, index: p.index });
        } else {
            let mut m = self.msg(MessageType::ReadIndexResp, p.requester);
            m.ctx = p.ctx;
            m.index = p.index;
            self.send(m);
        }
    }

    fn register_read(&mut self, ctx: u64, requester: NodeId, store: &dyn LogStore) {
        let index = self.log.committed;
        if self.single_voter() {
            self.answer_read(PendingRead { ctx, index, round: 0, requester }, store);
            return;
        }
        // The read rides on the next round sent, which leaves after it arrived; the reads of one
        // step share it (`send_read_round`).
        self.reads.push(PendingRead { ctx, index, round: self.lease_seq + 1, requester });
    }

    /// Whether reads registered since the last round wait for one (the caller sends it with
    /// [`RawNode::send_read_round`] once per step, so the reads of the step share it).
    pub fn read_round_due(&self) -> bool {
        self.role == Role::Leader && self.reads.last().is_some_and(|p| p.round > self.lease_seq)
    }

    /// Sends the heartbeat round that confirms the leadership for the reads registered since the
    /// last one: a single round for every read of the step, each still waiting for a majority
    /// that answers a round sent after it arrived.
    pub fn send_read_round(&mut self, store: &dyn LogStore) {
        if !self.read_round_due() {
            return;
        }
        // Faulty variant (005 FR-009): the batch is answered as soon as its round leaves, without
        // waiting for a majority to confirm it.
        #[cfg(teseladb_bug = "read_batch_without_round")]
        {
            let seq = self.lease_seq;
            let (ready, rest): (Vec<PendingRead>, Vec<PendingRead>) =
                core::mem::take(&mut self.reads).into_iter().partition(|p| p.round > seq);
            self.reads = rest;
            for p in ready {
                self.answer_read(p, store);
            }
        }
        self.bcast_heartbeat(store);
    }

    /// Advances the commit index according to the majority's matches (only entries of the own term
    /// count by counting). Returns whether it advanced.
    fn maybe_commit(&mut self, store: &dyn LogStore) -> bool {
        let mut matched: Vec<u64> = self
            .conf
            .voters
            .iter()
            .map(|&v| {
                if v == self.id {
                    self.log.persisted
                } else {
                    self.prs.get(&v).map_or(0, |p| p.matched)
                }
            })
            .collect();
        matched.sort_unstable_by(|a, b| b.cmp(a));
        // A configuration always has a voter, and `quorum() <= voters`.
        let idx = matched[self.quorum() - 1];
        if idx > self.log.committed && self.log.term(store, idx) == Ok(self.term) {
            self.uncommitted_bytes = 0;
            return self.log.commit_to(idx);
        }
        false
    }

    fn send_timeout_now(&mut self, to: NodeId) {
        // From here on the leader no longer serves lease reads in this term (guarantee 5).
        self.lease_disabled = true;
        let m = self.msg(MessageType::TimeoutNow, to);
        self.send(m);
    }

    // ----- caller requests -----

    /// Forces an election now (start-up of a single node or tests).
    pub fn campaign_now(&mut self, store: &dyn LogStore, rng: &mut Rng) {
        if self.role != Role::Leader {
            self.campaign(Campaign::Election { transfer_ctx: 0 }, store, rng);
        }
    }

    fn check_leader(&self) -> Result<(), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader { hint: self.leader });
        }
        if self.transfer.is_some() {
            return Err(ProposeError::Dropped);
        }
        Ok(())
    }

    /// Proposes data; the result arrives when the entry with that index is applied.
    pub fn propose(
        &mut self,
        data: Bytes,
        store: &dyn LogStore,
    ) -> Result<ProposalId, ProposeError> {
        self.check_leader()?;
        self.unquiesce(store);
        if data.len() > self.cfg.max_entry_bytes {
            return Err(ProposeError::TooLarge);
        }
        if self.uncommitted_bytes + data.len() > self.cfg.max_uncommitted_bytes
            && self.uncommitted_bytes > 0
        {
            return Err(ProposeError::Dropped);
        }
        self.uncommitted_bytes += data.len();
        Ok(self.append_entry(EntryKind::Normal, data, store))
    }

    fn append_entry(&mut self, kind: EntryKind, data: Bytes, store: &dyn LogStore) -> ProposalId {
        let index = self.log.last_index(store) + 1;
        self.log.append(store, &[Entry { index, term: self.term, kind, data }]);
        self.bcast_append(store);
        ProposalId(index)
    }

    /// Proposes a configuration change (at most one in flight and only after committing an entry
    /// of the own term).
    pub fn propose_conf_change(
        &mut self,
        cc: ConfChange,
        store: &dyn LogStore,
    ) -> Result<ProposalId, ProposeError> {
        self.check_leader()?;
        self.unquiesce(store);
        if self.log.committed < self.noop_index {
            return Err(ProposeError::NotReady);
        }
        if self.pending_conf_index > self.log.applied {
            return Err(ProposeError::ConfChangePending);
        }
        cc.apply(&self.conf).map_err(|_| ProposeError::InvalidConfChange)?;
        let id = self.append_entry(EntryKind::ConfChange, Bytes::from_vec(cc.to_bytes()), store);
        self.pending_conf_index = id.0;
        Ok(id)
    }

    /// Linearizable read: the index arrives in `Ready::read_states` with the same `ctx`.
    pub fn read_index(&mut self, ctx: u64, store: &dyn LogStore) -> Result<(), ProposeError> {
        self.unquiesce(store);
        match self.role {
            Role::Leader => {
                if !self.ready_for_reads() {
                    return Err(ProposeError::NotReady);
                }
                self.register_read(ctx, self.id, store);
                Ok(())
            }
            _ if self.leader.0 == 0 => Err(ProposeError::NotLeader { hint: NodeId(0) }),
            _ => {
                let mut m = self.msg(MessageType::ReadIndexReq, self.leader);
                m.ctx = ctx;
                self.send(m);
                Ok(())
            }
        }
    }

    /// Guarantee 6b: no reads until the term's `Noop` is committed and applied.
    fn ready_for_reads(&self) -> bool {
        let ready = self.log.committed >= self.noop_index && self.log.applied >= self.noop_index;
        // Faulty variant (005 R4): a new leader reads before committing its `Noop`.
        #[cfg(teseladb_bug = "read_before_term_commit")]
        let ready = {
            let _ = ready; // the faulty variant ignores the real value
            true
        };
        ready
    }

    /// Tells whether the leader can read now through the lease (guarantees 5, 6 and 6b).
    pub fn can_lease_read(&self, now: Nanos) -> bool {
        let in_lease = self.single_voter() || now < self.lease_until;
        // Faulty variant (005 R4): the lease never expires, so a leader cut off from the quorum
        // keeps serving reads until check-quorum makes it step down.
        #[cfg(teseladb_bug = "lease_without_check_quorum")]
        let in_lease = {
            let _ = in_lease; // the faulty variant ignores the real value
            true
        };
        self.role == Role::Leader
            && self.cfg.check_quorum
            && !self.lease_disabled
            && !self.quiesced
            && self.ready_for_reads()
            && in_lease
    }

    /// Transfers leadership to `to` (a voter): once it is up to date, `TimeoutNow` is sent to it.
    pub fn transfer_leader(&mut self, to: NodeId, store: &dyn LogStore) {
        if self.role != Role::Leader || to == self.id || !self.conf.is_voter(to) {
            return;
        }
        self.unquiesce(store);
        if self.transfer.is_some_and(|(t, _)| t == to) {
            return;
        }
        self.transfer = Some((to, 0));
        self.election_elapsed = 0;
        let last = self.log.last_index(store);
        if self.prs.get(&to).is_some_and(|p| p.matched >= last) {
            self.send_timeout_now(to);
        } else {
            self.send_append(to, store);
        }
    }

    /// Leaves quiescence (a request, or `NodeSuspect` about the leader on a follower: its timer
    /// starts again, and it campaigns only if it does not hear the leader in time).
    pub fn wake(&mut self, store: &dyn LogStore) {
        self.unquiesce(store);
        self.election_elapsed = 0;
    }

    /// The follower does not respond (connection down): probing starts again.
    pub fn report_unreachable(&mut self, peer: NodeId) {
        if let Some(p) = self.prs.get_mut(&peer)
            && p.state == ProgressState::Replicate
        {
            p.become_probe();
        }
    }

    /// Result of sending a snapshot to `peer`.
    pub fn report_snapshot(&mut self, peer: NodeId, ok: bool) {
        let Some(p) = self.prs.get_mut(&peer) else { return };
        if let ProgressState::Snapshot(idx) = p.state {
            if ok {
                p.matched = p.matched.max(idx);
            }
            p.become_probe();
        }
    }

    /// `peer` installed the snapshot at `index`. It may be older than the one being sent now
    /// (the follower acknowledges what it installed, and a newer transfer may have started
    /// meanwhile): only `index` counts as matched.
    pub fn snapshot_installed(&mut self, peer: NodeId, index: u64) {
        let Some(p) = self.prs.get_mut(&peer) else { return };
        if let ProgressState::Snapshot(_) = p.state {
            p.matched = p.matched.max(index);
            p.become_probe();
        }
    }

    /// The transfer of a snapshot to `peer` was given up: probing starts again at once, which
    /// sends it the newest snapshot if it still needs one. A follower that waits for the data of
    /// a snapshot answers nothing until it has it, so waiting for its answer would wait forever.
    pub fn retry_snapshot(&mut self, peer: NodeId, store: &dyn LogStore) {
        self.report_snapshot(peer, false);
        if self.role != Role::Leader {
            return;
        }
        let Some(p) = self.prs.get_mut(&peer) else { return };
        // Probing starts again from what the follower is known to have, not from past the
        // snapshot that did not arrive nor behind a probe it will not answer: a compacted log
        // sends the newest snapshot.
        p.become_probe();
        p.next = p.matched + 1;
        self.send_append(peer, store);
    }

    // ----- output -----

    fn hard_state(&self) -> HardState {
        HardState { term: self.term, vote: self.vote, commit: self.log.committed }
    }

    fn soft_state(&self) -> SoftState {
        SoftState { leader: self.leader, role: self.role }
    }

    fn apply_upto(&self) -> u64 {
        // A state machine that waits for a snapshot takes nothing until it has it.
        if self.awaiting_snapshot {
            return self.log.handed_apply;
        }
        let upto = self.log.committed.min(self.log.persisted);
        // Faulty variant (005 R4): hands out for application everything persisted, committed or not.
        #[cfg(teseladb_bug = "apply_uncommitted")]
        let upto = upto.max(self.log.persisted);
        upto
    }

    /// Tells whether there is anything to do.
    pub fn has_ready(&self) -> bool {
        self.log.pending_snapshot.is_some()
            || self.hard_state() != self.prev_hs
            || self.soft_state() != self.prev_soft
            || self.log.truncate_from.is_some()
            || self.log.handed < self.log.unstable.len()
            || !self.msgs.is_empty()
            || !self.msgs_after.is_empty()
            || self.log.handed_apply < self.apply_upto()
            || !self.read_states.is_empty()
    }

    /// Batch of actions.
    pub fn ready(&mut self, store: &dyn LogStore, rng: &mut Rng) -> Ready {
        self.ready_number += 1;
        let hs = self.hard_state();
        let hard_state = (hs != self.prev_hs).then_some(hs);
        self.prev_hs = hs;
        let soft = self.soft_state();
        let soft_state = (soft != self.prev_soft).then_some(soft);
        self.prev_soft = soft;
        let snapshot_to_apply = self.log.pending_snapshot.take();
        if let Some(s) = &snapshot_to_apply {
            self.log.unstable_snapshot = Some(s.meta.clone());
        }
        let truncate_from = self.log.truncate_from.take();
        let entries_to_persist = self.log.take_unpersisted();
        let mut committed = Vec::new();
        let upto = self.apply_upto();
        if self.log.handed_apply < upto {
            committed = self
                .log
                .entries(store, self.log.handed_apply + 1, upto + 1, 16 << 20)
                .unwrap_or_default();
            // What `apply_upto` covers is in the store or in the unstable part.
            self.log.handed_apply = committed.last().map_or(self.log.handed_apply, |e| e.index);
            for e in &committed {
                if e.kind == EntryKind::ConfChange {
                    self.apply_conf_change(e, store, rng);
                }
            }
        }
        let messages = core::mem::take(&mut self.msgs);
        let messages_after_persist = core::mem::take(&mut self.msgs_after);
        // Faulty variant (005 R4): votes and appends are acknowledged before they are persisted.
        #[cfg(teseladb_bug = "memory_raft_ack")]
        let (messages, messages_after_persist) =
            ([messages, messages_after_persist].concat(), Vec::new());
        Ready {
            number: self.ready_number,
            hard_state,
            entries_to_persist,
            truncate_from,
            snapshot_to_apply,
            messages,
            messages_after_persist,
            committed,
            read_states: core::mem::take(&mut self.read_states),
            soft_state,
        }
    }

    fn apply_conf_change(&mut self, e: &Entry, store: &dyn LogStore, rng: &mut Rng) {
        let Ok(cc) = ConfChange::from_bytes(&e.data) else { return };
        // On recovery a change already included in the initial configuration may arrive: it is ignored.
        let Ok(next) = cc.apply(&self.conf) else { return };
        self.conf = next;
        if self.role == Role::Leader {
            if !self.conf.is_voter(self.id) {
                // The leader removed itself.
                self.become_follower(self.term, NodeId(0), rng);
                return;
            }
            let last = self.log.last_index(store);
            self.prs.retain(|n, _| self.conf.is_voter(*n) || self.conf.is_learner(*n));
            for m in self.conf.members().collect::<Vec<_>>() {
                if m == self.id {
                    continue;
                }
                let learner = self.conf.is_learner(m);
                self.prs.entry(m).or_insert_with(|| Progress::new(last, learner)).learner = learner;
            }
            if self.maybe_commit(store) {
                self.bcast_append(store);
            }
        } else {
            self.role = self.follower_role();
        }
    }

    /// The caller persisted `Ready` `number` with entries up to (`last_index`, `last_term`).
    pub fn on_persisted(
        &mut self,
        number: u64,
        last_index: u64,
        last_term: u64,
        store: &dyn LogStore,
    ) {
        debug_assert!(number <= self.ready_number);
        self.log.stable_to(store, last_index, last_term);
        if self.role == Role::Leader && self.maybe_commit(store) {
            self.bcast_append(store);
        }
    }

    /// The caller applied up to `index`.
    pub fn on_applied(&mut self, index: u64) {
        self.log.applied = self.log.applied.max(index.min(self.log.handed_apply));
    }

    /// Current state.
    pub fn status(&self, store: &dyn LogStore) -> Status {
        Status {
            id: self.id,
            role: self.role,
            leader: self.leader,
            hard_state: self.hard_state(),
            last_index: self.log.last_index(store),
            persisted: self.log.persisted,
            applied: self.log.applied,
            progress: self.prs.iter().map(|(n, p)| (*n, p.matched)).collect(),
            rtts: self.prs.iter().filter_map(|(n, p)| Some((*n, p.rtt?))).collect(),
        }
    }

    /// Current role, without building the whole [`Status`].
    pub fn role(&self) -> Role {
        self.role
    }

    /// Current configuration.
    pub fn conf(&self) -> &ConfState {
        &self.conf
    }
}

fn vote_resp(req: MessageType) -> MessageType {
    if req == MessageType::PreVoteReq { MessageType::PreVoteResp } else { MessageType::VoteResp }
}
