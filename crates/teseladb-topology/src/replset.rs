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

/// Highest priority a member may have.
const MAX_PRIORITY: f64 = 1000.0;
/// First bytes of an `electionId`: the term follows as a big-endian 64-bit integer.
const ELECTION_ID_PREFIX: [u8; 4] = [0x7f, 0xff, 0xff, 0xff];
/// `settings.heartbeatIntervalMillis` (RefDb's default).
const HEARTBEAT_INTERVAL_MS: i32 = 2000;
/// `settings.heartbeatTimeoutSecs`.
const HEARTBEAT_TIMEOUT_SECS: i32 = 10;
/// `settings.electionTimeoutMillis`.
const ELECTION_TIMEOUT_MS: i32 = 10_000;
/// `settings.catchUpTakeoverDelayMillis`.
const CATCH_UP_TAKEOVER_DELAY_MS: i32 = 30_000;

/// A priority as RefDb writes it: a whole number as a 32-bit integer.
fn priority_value(p: f64) -> Value {
    if p.fract() == 0.0 && p.abs() <= MAX_PRIORITY {
        Value::Int32(p as i32)
    } else {
        Value::Double(p)
    }
}

/// Milliseconds per second, for `lastWriteDate` (the seconds of the optime).
const MS_PER_SECOND: i64 = 1000;

/// A timestamp of the cluster (005 FR-024): seconds and the increment within the second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    /// Seconds.
    pub time: u32,
    /// Increment within the second.
    pub increment: u32,
}

/// The limits and versions a node announces in its `hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// `maxBsonObjectSize`.
    pub max_bson_object_size: i32,
    /// `maxMessageSizeBytes`.
    pub max_message_size: i32,
    /// `maxWriteBatchSize`.
    pub max_write_batch_size: i32,
    /// `logicalSessionTimeoutMinutes`.
    pub logical_session_timeout_minutes: i32,
    /// `minWireVersion`.
    pub min_wire_version: i32,
    /// `maxWireVersion`.
    pub max_wire_version: i32,
}

/// What a member's `hello` needs from the connection and the node, already validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloContext {
    /// `isMaster` (`ismaster` in the reply) rather than `hello`.
    pub legacy: bool,
    /// `helloOk` was asked.
    pub hello_ok: bool,
    /// The negotiated compressors' names, in order.
    pub compressors: Vec<&'static str>,
    /// `topologyVersion`: the process and its counter.
    pub process_id: [u8; 12],
    /// `topologyVersion.counter`.
    pub counter: i64,
    /// `localTime`, milliseconds since 1970.
    pub now_ms: i64,
    /// `connectionId`.
    pub connection_id: i32,
    /// The node's limits.
    pub limits: Limits,
}

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

    /// `priority`.
    pub fn priority(&self) -> f64 {
        self.priority
    }

    /// `votes`.
    pub fn votes(&self) -> i32 {
        self.votes
    }

    /// `hidden`.
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// `tags`, in order.
    pub fn tags(&self) -> &[(String, String)] {
        &self.tags
    }

    /// Whether Group 0 may designate it as primary.
    pub fn electable(&self) -> bool {
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
    pub ts: Timestamp,
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
    /// Members in maintenance mode (`replSetMaintenance`): `RECOVERING`, neither primary nor
    /// secondary to the drivers.
    recovering: Vec<usize>,
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
        let rs = ReplicaSet {
            set_name,
            members,
            primary: None,
            set_version,
            term,
            recovering: Vec::new(),
        };
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

    /// The set with the members at `hosts` in maintenance mode (`RECOVERING`); the designated
    /// primary never is.
    pub fn with_recovering(&self, hosts: &[&str]) -> ReplicaSet {
        let recovering = (0..self.members.len())
            .filter(|&i| Some(i) != self.primary && hosts.contains(&self.members[i].host.as_str()))
            .collect();
        ReplicaSet { recovering, ..self.clone() }
    }

    /// Whether member `id` is in maintenance mode.
    pub fn is_recovering(&self, id: MemberId) -> bool {
        self.recovering.contains(&id.0)
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
        let recovering = self.recovering.iter().copied().filter(|&i| i != primary).collect();
        let term = self.term + 1;
        ReplicaSet { primary: Some(primary), set_version, term, recovering, ..self.clone() }
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

    /// Priority takeover (as a member of RefDb calls an election when it has a higher priority
    /// than the primary and is caught up, measured on 9.0.4 on 02-11-2026): the live electable
    /// member with the highest priority, above the primary's, becomes primary with a new term;
    /// `None` if there is none.
    pub fn takeover(&self, alive: &dyn Fn(&str) -> bool) -> Option<ReplicaSet> {
        let current = self.primary?;
        let best = self.candidate(alive, Some(current))?;
        (self.members[best].priority > self.members[current].priority)
            .then(|| self.with_primary(best, self.set_version))
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

    /// The configuration as `replSetGetConfig` and `local.system.replset` give it (measured on
    /// RefDb 9.0.4): the settings of RefDb's defaults, `replicaSetId` the cluster's.
    pub fn config_document(&self, replica_set_id: [u8; 12]) -> Document {
        let members: Vec<Value> = self
            .members
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let tags = m.tags.iter().fold(Document::new(), |t, (k, v)| t.with(k, v.as_str()));
                Value::Document(
                    Document::new()
                        .with("_id", i32::try_from(i).unwrap_or(i32::MAX))
                        .with("host", m.host.as_str())
                        .with("arbiterOnly", false)
                        .with("buildIndexes", true)
                        .with("hidden", m.hidden)
                        .with("priority", priority_value(m.priority))
                        .with("tags", tags)
                        .with("secondaryDelaySecs", 0i64)
                        .with("votes", m.votes),
                )
            })
            .collect();
        let settings = Document::new()
            .with("chainingAllowed", true)
            .with("heartbeatIntervalMillis", HEARTBEAT_INTERVAL_MS)
            .with("heartbeatTimeoutSecs", HEARTBEAT_TIMEOUT_SECS)
            .with("electionTimeoutMillis", ELECTION_TIMEOUT_MS)
            .with("catchUpTimeoutMillis", -1)
            .with("catchUpTakeoverDelayMillis", CATCH_UP_TAKEOVER_DELAY_MS)
            .with("getLastErrorModes", Document::new())
            .with("getLastErrorDefaults", Document::new().with("w", 1).with("wtimeout", 0))
            .with("replicaSetId", Value::ObjectId(replica_set_id));
        Document::new()
            .with("_id", self.set_name.as_str())
            .with("version", self.set_version)
            .with("term", i32::try_from(self.term).unwrap_or(i32::MAX))
            .with("members", members)
            .with("protocolVersion", 1i64)
            .with("writeConcernMajorityJournalDefault", true)
            .with("settings", settings)
    }

    /// The reply of member `me` to a validated `hello` (without `$clusterTime` and
    /// `operationTime`, which the wire appends).
    pub fn hello(&self, ctx: &HelloContext, me: MemberId, last: &LastWrite) -> Document {
        let member = &self.members[me.0];
        let is_primary = self.primary == Some(me.0);
        let mut d = Document::new();
        if !ctx.compressors.is_empty() {
            let names: Vec<Value> = ctx.compressors.iter().copied().map(Value::from).collect();
            d.push("compression", names);
        }
        if ctx.hello_ok {
            d.push("helloOk", true);
        }
        let tv = Document::new()
            .with("processId", Value::ObjectId(ctx.process_id))
            .with("counter", ctx.counter);
        d.push("topologyVersion", tv);
        d.push("hosts", self.hosts(false));
        let passives = self.hosts(true);
        if !passives.is_empty() {
            d.push("passives", passives);
        }
        d.push("setName", self.set_name.as_str());
        d.push("setVersion", self.set_version);
        d.push(if ctx.legacy { "ismaster" } else { "isWritablePrimary" }, is_primary);
        d.push("secondary", !is_primary && !self.recovering.contains(&me.0));
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
        let l = &ctx.limits;
        d.push("maxBsonObjectSize", l.max_bson_object_size);
        d.push("maxMessageSizeBytes", l.max_message_size);
        d.push("maxWriteBatchSize", l.max_write_batch_size);
        d.push("localTime", Value::DateTime(ctx.now_ms));
        d.push("logicalSessionTimeoutMinutes", l.logical_session_timeout_minutes);
        d.push("connectionId", ctx.connection_id);
        d.push("minWireVersion", l.min_wire_version);
        d.push("maxWireVersion", l.max_wire_version);
        d.push("readOnly", false);
        d.push("ok", 1.0);
        d
    }
}
