//! The replica set presentation (018 FR-007, contract `topology-modes.md`): the configuration
//! Group 0 keeps for it, the `hello` of each member and the designation of the primary.
//!
//! The reply follows a member of a RefDb 9.0.4 replica set field by field: `compression` and
//! `helloOk` first, then the set, the member's attributes (`passive`, `hidden`, `tags`) before
//! `me`, `electionId` only on the primary, `lastWrite`, the limits, `minWireVersion` before
//! `maxWireVersion` and `readOnly`. Hidden members are in neither `hosts` nor `passives`, and
//! members with priority 0 go to `passives`. `loadBalanced` adds nothing, as on a `refnode` that
//! is not behind a load balancer.

use teseladb_bson::{Document, Value};
use teseladb_wire::compressed::Compressor;
use teseladb_wire::connection::HelloState;
use teseladb_wire::hello::{ClusterTime, HelloRequest, IS_MASTER_LOWER, NodeInfo};
use teseladb_wire::{
    LOGICAL_SESSION_TIMEOUT_MINUTES, MAX_BSON_OBJECT_SIZE, MAX_MESSAGE_SIZE, MAX_WIRE_VERSION,
    MAX_WRITE_BATCH_SIZE, MIN_WIRE_VERSION,
};

/// Highest priority a member may have.
const MAX_PRIORITY: f64 = 1000.0;
/// First bytes of an `electionId`: the term follows as a big-endian 64-bit integer.
const ELECTION_ID_PREFIX: [u8; 4] = [0x7f, 0xff, 0xff, 0xff];
/// Milliseconds per second, for `lastWriteDate` (the seconds of the optime).
const MS_PER_SECOND: i64 = 1000;

/// How the cluster shows itself to the drivers (`topologyPresentation.mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PresentationMode {
    /// As a `refrouter` (D-006), the default.
    #[default]
    RefRouter,
    /// As a replica set (D-031).
    ReplicaSet,
}

impl PresentationMode {
    /// The name in `setClusterParameter` and `teseladb admin topology set`.
    pub fn name(self) -> &'static str {
        match self {
            PresentationMode::RefRouter => "refrouter",
            PresentationMode::ReplicaSet => "replicaset",
        }
    }

    /// The mode called `name`.
    pub fn from_name(name: &str) -> Option<PresentationMode> {
        [PresentationMode::RefRouter, PresentationMode::ReplicaSet]
            .into_iter()
            .find(|m| m.name() == name)
    }
}

/// A member of the replica set: a node with the attributes of `replSetReconfig`.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    host: String,
    priority: f64,
    votes: i32,
    hidden: bool,
    tags: Vec<(String, String)>,
}

impl Member {
    /// A voting member with priority 1, as RefDb's defaults.
    pub fn new(host: impl Into<String>) -> Member {
        Member { host: host.into(), priority: 1.0, votes: 1, hidden: false, tags: Vec::new() }
    }

    /// With priority `p`.
    pub fn with_priority(self, p: f64) -> Member {
        Member { priority: p, ..self }
    }

    /// With `votes` votes.
    pub fn with_votes(self, votes: i32) -> Member {
        Member { votes, ..self }
    }

    /// Hidden from the drivers.
    pub fn hidden(self) -> Member {
        Member { hidden: true, ..self }
    }

    /// With these tags, in order.
    pub fn with_tags(self, tags: Vec<(String, String)>) -> Member {
        Member { tags, ..self }
    }

    /// `host:port` as the drivers reach it.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether Group 0 may designate it as primary.
    fn electable(&self) -> bool {
        self.priority > 0.0
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let err = |f: fn(String) -> ConfigError| Err(f(self.host.clone()));
        if !(0.0..=MAX_PRIORITY).contains(&self.priority) {
            return err(ConfigError::PriorityOutOfRange);
        }
        if !(0..=1).contains(&self.votes) {
            return err(ConfigError::InvalidVotes);
        }
        if self.votes == 0 && self.priority > 0.0 {
            return err(ConfigError::NonVoterWithPriority);
        }
        if self.hidden && self.priority > 0.0 {
            return err(ConfigError::HiddenWithPriority);
        }
        Ok(())
    }
}

/// Why a configuration is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The set has no name.
    EmptySetName,
    /// The set has no members.
    NoMembers,
    /// Two members with the same host.
    DuplicateHost(String),
    /// Priority outside 0..=1000.
    PriorityOutOfRange(String),
    /// Votes other than 0 or 1.
    InvalidVotes(String),
    /// A member without votes must have priority 0.
    NonVoterWithPriority(String),
    /// A hidden member must have priority 0.
    HiddenWithPriority(String),
    /// No member has this host.
    UnknownMember(String),
    /// The member has priority 0 and cannot be primary.
    NotElectable(String),
}

/// A member's position in the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberId(usize);

/// An optime: the timestamp of a write and the term it was written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpTime {
    /// Timestamp (005 FR-024).
    pub ts: ClusterTime,
    /// Term: the presentation's version (contract `oplog-format.md`).
    pub term: i64,
}

impl OpTime {
    /// `{ts, t}`.
    pub fn to_document(self) -> Document {
        let ts = Value::Timestamp { time: self.ts.time, increment: self.ts.increment };
        Document::new().with("ts", ts).with("t", self.term)
    }

    fn date(self) -> Value {
        Value::DateTime(i64::from(self.ts.time) * MS_PER_SECOND)
    }
}

/// `lastWrite` of a `hello`, from the closed timestamp (for `maxStalenessSeconds`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastWrite {
    /// The last write the member has applied.
    pub op_time: OpTime,
    /// The last write confirmed by a majority.
    pub majority_op_time: OpTime,
}

impl LastWrite {
    fn to_document(self) -> Document {
        Document::new()
            .with("opTime", self.op_time.to_document())
            .with("lastWriteDate", self.op_time.date())
            .with("majorityOpTime", self.majority_op_time.to_document())
            .with("majorityWriteDate", self.majority_op_time.date())
    }
}

/// The replica set presentation as Group 0 keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplicaSet {
    set_name: String,
    members: Vec<Member>,
    primary: Option<usize>,
    set_version: i32,
    term: i64,
}

impl ReplicaSet {
    /// A validated configuration with `primary` designated (none: every member is secondary).
    pub fn new(
        set_name: impl Into<String>,
        members: Vec<Member>,
        primary: Option<&str>,
        set_version: i32,
        term: i64,
    ) -> Result<ReplicaSet, ConfigError> {
        let set_name = set_name.into();
        if set_name.is_empty() {
            return Err(ConfigError::EmptySetName);
        }
        if members.is_empty() {
            return Err(ConfigError::NoMembers);
        }
        for (i, m) in members.iter().enumerate() {
            m.validate()?;
            if members[..i].iter().any(|o| o.host == m.host) {
                return Err(ConfigError::DuplicateHost(m.host.clone()));
            }
        }
        let rs = ReplicaSet { set_name, members, primary: None, set_version, term };
        let primary = match primary {
            Some(host) => Some(rs.electable(host)?),
            None => None,
        };
        Ok(ReplicaSet { primary, ..rs })
    }

    /// `setName`.
    pub fn set_name(&self) -> &str {
        &self.set_name
    }

    /// The members in configuration order.
    pub fn members(&self) -> &[Member] {
        &self.members
    }

    /// The designated primary's host.
    pub fn primary(&self) -> Option<&str> {
        self.primary.map(|i| self.members[i].host.as_str())
    }

    /// `setVersion`.
    pub fn set_version(&self) -> i32 {
        self.set_version
    }

    /// The term: it grows with every new designation.
    pub fn term(&self) -> i64 {
        self.term
    }

    /// The member with this host.
    pub fn member(&self, host: &str) -> Option<MemberId> {
        self.members.iter().position(|m| m.host == host).map(MemberId)
    }

    fn electable(&self, host: &str) -> Result<usize, ConfigError> {
        let i = self
            .members
            .iter()
            .position(|m| m.host == host)
            .ok_or_else(|| ConfigError::UnknownMember(host.to_owned()))?;
        if !self.members[i].electable() {
            return Err(ConfigError::NotElectable(host.to_owned()));
        }
        Ok(i)
    }

    /// `electionId`: `7fffffff` and the term, as RefDb builds it.
    pub fn election_id(&self) -> [u8; 12] {
        let mut id = [0u8; 12];
        id[..4].copy_from_slice(&ELECTION_ID_PREFIX);
        id[4..].copy_from_slice(&self.term.to_be_bytes());
        id
    }

    /// The live electable member with the highest priority (the first in configuration order on
    /// a tie), other than `except`.
    fn candidate(&self, alive: &dyn Fn(&str) -> bool, except: Option<usize>) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, m) in self.members.iter().enumerate() {
            if Some(i) == except || !m.electable() || !alive(&m.host) {
                continue;
            }
            if best.is_none_or(|b| m.priority > self.members[b].priority) {
                best = Some(i);
            }
        }
        best
    }

    fn with_primary(&self, primary: usize, set_version: i32) -> ReplicaSet {
        ReplicaSet { primary: Some(primary), set_version, term: self.term + 1, ..self.clone() }
    }

    /// Group 0's automatic reassignment (FR-007, `PrimaryReassign.tla`): when there is no
    /// designated primary or it is not alive, the presentation with a live member designated and
    /// a new `electionId` and `setVersion`; `None` if nothing changes.
    pub fn reassign(&self, alive: &dyn Fn(&str) -> bool) -> Option<ReplicaSet> {
        if self.primary().is_some_and(alive) {
            return None;
        }
        let next = self.candidate(alive, None)?;
        Some(self.with_primary(next, self.set_version + 1))
    }

    /// `replSetStepDown`: another live electable member becomes primary with a new term, as an
    /// election does; `None` if there is none.
    pub fn step_down(&self, alive: &dyn Fn(&str) -> bool) -> Option<ReplicaSet> {
        let next = self.candidate(alive, self.primary)?;
        Some(self.with_primary(next, self.set_version))
    }

    /// `setClusterParameter` with `primary`: designates `host` with a new term and `setVersion`.
    pub fn designate(&self, host: &str) -> Result<ReplicaSet, ConfigError> {
        let next = self.electable(host)?;
        Ok(self.with_primary(next, self.set_version + 1))
    }

    fn hosts(&self, passive: bool) -> Vec<Value> {
        self.members
            .iter()
            .filter(|m| !m.hidden && m.electable() != passive)
            .map(|m| Value::from(m.host.as_str()))
            .collect()
    }

    /// The reply of member `me` to a validated `hello` (without `$clusterTime` and
    /// `operationTime`, which `teseladb_wire::hello::with_cluster_time` appends).
    pub fn hello(
        &self,
        req: &HelloRequest,
        state: &HelloState,
        connection_id: i32,
        node: &NodeInfo,
        me: MemberId,
        last: &LastWrite,
    ) -> Document {
        let member = &self.members[me.0];
        let is_primary = self.primary == Some(me.0);
        let mut d = Document::new();
        if !state.compressors.is_empty() {
            let names: Vec<Value> =
                state.compressors.iter().copied().map(Compressor::name).map(Value::from).collect();
            d.push("compression", names);
        }
        if req.hello_ok() {
            d.push("helloOk", true);
        }
        let tv = Document::new()
            .with("processId", Value::ObjectId(node.topology.process_id))
            .with("counter", node.topology.counter);
        d.push("topologyVersion", tv);
        d.push("hosts", self.hosts(false));
        let passives = self.hosts(true);
        if !passives.is_empty() {
            d.push("passives", passives);
        }
        d.push("setName", self.set_name.as_str());
        d.push("setVersion", self.set_version);
        d.push(if req.legacy() { IS_MASTER_LOWER } else { "isWritablePrimary" }, is_primary);
        d.push("secondary", !is_primary);
        if let Some(p) = self.primary() {
            d.push("primary", p);
        }
        if !member.electable() {
            d.push("passive", true);
        }
        if member.hidden {
            d.push("hidden", true);
        }
        if !member.tags.is_empty() {
            let tags = member.tags.iter().fold(Document::new(), |t, (k, v)| t.with(k, v.as_str()));
            d.push("tags", tags);
        }
        d.push("me", member.host.as_str());
        if is_primary {
            d.push("electionId", Value::ObjectId(self.election_id()));
        }
        d.push("lastWrite", last.to_document());
        d.push("maxBsonObjectSize", MAX_BSON_OBJECT_SIZE);
        d.push("maxMessageSizeBytes", MAX_MESSAGE_SIZE as i32);
        d.push("maxWriteBatchSize", MAX_WRITE_BATCH_SIZE);
        d.push("localTime", Value::DateTime(node.now_ms));
        d.push("logicalSessionTimeoutMinutes", LOGICAL_SESSION_TIMEOUT_MINUTES);
        d.push("connectionId", connection_id);
        d.push("minWireVersion", MIN_WIRE_VERSION);
        d.push("maxWireVersion", MAX_WIRE_VERSION);
        d.push("readOnly", false);
        d.push("ok", 1.0);
        d
    }
}
