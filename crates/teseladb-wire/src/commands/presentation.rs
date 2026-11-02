//! The cluster parameter `topologyPresentation` (018 FR-007, contract `topology-modes.md`): how
//! the cluster shows itself to the drivers, kept in Group 0 as a BSON document
//! `{mode, setName, primary, setVersion, term, members}`. The members of the replica set are the
//! cluster's nodes (`config.shards`), in their order, with the attributes `replSetReconfig`
//! stored in `members` (`{host, priority, votes, hidden, tags}`).

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_topology::replset::{ConfigError, Member, PresentationMode, ReplicaSet};

use super::Backend;
use crate::error::{BAD_VALUE, CommandError};

/// The parameter's name.
pub const TOPOLOGY_PRESENTATION: &str = "topologyPresentation";
/// The term before any designation: the `t` of the oplog's entries and the term of the first
/// `electionId`.
pub const FIRST_TERM: i64 = 1;
/// `setVersion` of a new replica set.
pub const FIRST_SET_VERSION: i32 = 1;
/// The field with the mode.
pub const MODE: &str = "mode";
/// The field with the set's name.
pub const SET_NAME: &str = "setName";
/// The field with the designated primary's host.
pub const PRIMARY: &str = "primary";
/// The field with `setVersion`.
pub const SET_VERSION: &str = "setVersion";
/// The field with the term.
pub const TERM: &str = "term";
/// The field with the members' attributes.
pub const MEMBERS: &str = "members";

/// The stored presentation.
#[derive(Clone, Debug, PartialEq)]
pub struct Presentation {
    /// `refrouter` or `replicaset`.
    pub mode: PresentationMode,
    /// `setName`.
    pub set_name: String,
    /// The designated primary's host.
    pub primary: Option<String>,
    /// `setVersion`.
    pub set_version: i32,
    /// The term.
    pub term: i64,
    /// The members' attributes, by host.
    pub members: Vec<Member>,
}

impl Default for Presentation {
    fn default() -> Presentation {
        Presentation {
            mode: PresentationMode::RefRouter,
            set_name: String::new(),
            primary: None,
            set_version: FIRST_SET_VERSION,
            term: FIRST_TERM,
            members: Vec::new(),
        }
    }
}

fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::new(BAD_VALUE, msg)
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(d) => Some(*d),
        _ => None,
    }
}

/// A member's attributes from `{host, priority, votes, hidden, tags}`.
pub fn member_of(d: &Document) -> Result<Member, CommandError> {
    let Some(Value::String(host)) = d.get("host") else {
        return Err(bad("a member needs its host"));
    };
    let mut m = Member::new(host.as_str());
    if let Some(p) = d.get("priority") {
        m = m.with_priority(number(p).ok_or_else(|| bad("priority must be a number"))?);
    }
    if let Some(v) = d.get("votes") {
        let votes = number(v).ok_or_else(|| bad("votes must be a number"))?;
        m = m.with_votes(votes as i32);
    }
    if let Some(Value::Boolean(true)) = d.get("hidden") {
        m = m.hidden();
    }
    if let Some(Value::Document(t)) = d.get("tags") {
        let mut tags = Vec::new();
        for (k, v) in t.iter() {
            let Value::String(s) = v else { return Err(bad("tags must be strings")) };
            tags.push((k.to_owned(), s.clone()));
        }
        m = m.with_tags(tags);
    }
    Ok(m)
}

/// A member as the parameter stores it.
pub fn member_document(m: &Member) -> Document {
    let tags = m.tags().iter().fold(Document::new(), |t, (k, v)| t.with(k, v.as_str()));
    Document::new()
        .with("host", m.host())
        .with("priority", m.priority())
        .with("votes", m.votes())
        .with("hidden", m.is_hidden())
        .with("tags", tags)
}

impl Presentation {
    /// The presentation stored as `doc`.
    pub fn from_document(doc: &Document) -> Result<Presentation, CommandError> {
        let mut p = Presentation::default();
        if let Some(Value::String(m)) = doc.get(MODE) {
            p.mode = PresentationMode::from_name(m)
                .ok_or_else(|| bad(format!("unknown topologyPresentation mode {m}")))?;
        }
        if let Some(Value::String(s)) = doc.get(SET_NAME) {
            p.set_name = s.clone();
        }
        if let Some(Value::String(s)) = doc.get(PRIMARY) {
            p.primary = Some(s.clone());
        }
        if let Some(Value::Int32(v)) = doc.get(SET_VERSION) {
            p.set_version = *v;
        }
        if let Some(Value::Int64(t)) = doc.get(TERM) {
            p.term = *t;
        }
        if let Some(Value::Array(ms)) = doc.get(MEMBERS) {
            for m in ms {
                let Value::Document(d) = m else { return Err(bad("a member must be a document")) };
                p.members.push(member_of(d)?);
            }
        }
        Ok(p)
    }

    /// The document the parameter stores.
    pub fn to_document(&self) -> Document {
        let mut d =
            Document::new().with(MODE, self.mode.name()).with(SET_NAME, self.set_name.as_str());
        if let Some(p) = &self.primary {
            d.push(PRIMARY, p.as_str());
        }
        let members: Vec<Value> =
            self.members.iter().map(|m| Value::Document(member_document(m))).collect();
        d.with(SET_VERSION, self.set_version).with(TERM, self.term).with(MEMBERS, members)
    }

    /// The replica set over the cluster's nodes `hosts`, each with its stored attributes.
    pub fn replica_set(&self, hosts: &[String]) -> Result<ReplicaSet, ConfigError> {
        let members = hosts
            .iter()
            .map(|h| {
                self.members
                    .iter()
                    .find(|m| m.host() == h)
                    .cloned()
                    .unwrap_or_else(|| Member::new(h))
            })
            .collect();
        let primary = self.primary.as_deref().filter(|p| hosts.iter().any(|h| h == p));
        ReplicaSet::new(self.set_name.as_str(), members, primary, self.set_version, self.term)
    }
}

/// The stored presentation (`refrouter` by default).
pub fn stored<B: Backend + ?Sized>(backend: &mut B) -> Result<Presentation, CommandError> {
    let params = backend.cluster_parameters()?;
    match params.get(TOPOLOGY_PRESENTATION) {
        Some(bytes) => Presentation::from_document(&Document::from(RawDoc::from_bytes(bytes)?)),
        None => Ok(Presentation::default()),
    }
}

/// The presentation's term: it only grows (contract `oplog-format.md`).
pub fn term<B: Backend + ?Sized>(backend: &mut B) -> Result<i64, CommandError> {
    Ok(stored(backend)?.term)
}

fn text(d: &Document, f: &str) -> String {
    match d.get(f) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// The hosts of the cluster's nodes, in `config.shards` order.
pub fn hosts<B: Backend + ?Sized>(backend: &mut B) -> Result<Vec<String>, CommandError> {
    let shards = backend.config_documents(super::config_db::SHARDS)?;
    Ok(shards.iter().map(|d| text(d, "host")).filter(|h| !h.is_empty()).collect())
}

/// The cluster's identifier (`config.version`), which is the `replicaSetId`.
fn cluster_id<B: Backend + ?Sized>(backend: &mut B) -> Result<[u8; 12], CommandError> {
    let version = backend.config_documents("version")?;
    Ok(match version.first().and_then(|d| d.get("clusterId")) {
        Some(Value::ObjectId(id)) => *id,
        _ => [0; 12],
    })
}

/// The replica set and its `replicaSetId`, in the replica set presentation only.
pub fn replica_set<B: Backend + ?Sized>(
    backend: &mut B,
) -> Result<Option<(ReplicaSet, [u8; 12])>, CommandError> {
    let p = stored(backend)?;
    if p.mode != PresentationMode::ReplicaSet {
        return Ok(None);
    }
    let hosts = hosts(backend)?;
    let rs = p.replica_set(&hosts).map_err(|e| bad(format!("{e:?}")))?;
    Ok(Some((rs, cluster_id(backend)?)))
}

/// The value without `setClusterParameter`: the `refrouter` presentation.
pub fn default_value() -> Document {
    Presentation::default().to_document()
}

/// The request `{mode, setName, primary}` of `setClusterParameter`, checked field by field.
pub fn parse(v: &Document) -> Result<Document, (crate::error::Code, String)> {
    let mut out = Document::new();
    for (k, x) in v.iter() {
        match (k, x) {
            (MODE, Value::String(m)) if PresentationMode::from_name(m).is_some() => {}
            (MODE, Value::String(m)) => {
                return Err((BAD_VALUE, format!("unknown topologyPresentation mode '{m}'")));
            }
            (SET_NAME | PRIMARY, Value::String(_)) => {}
            (MODE | SET_NAME | PRIMARY, other) => {
                let e = crate::types::wrong_type(
                    &format!("ClusterServerParameter.{k}"),
                    other,
                    "string",
                );
                return Err((e.code, e.errmsg));
            }
            _ => {
                let why = format!("BSON field 'ClusterServerParameter.{k}' is an unknown field.");
                return Err((crate::error::IDL_UNKNOWN_FIELD, why));
            }
        }
        out.push(k, x.clone());
    }
    if out.get(MODE).is_none() {
        let why = "BSON field 'ClusterServerParameter.mode' is missing but a required field";
        return Err((crate::error::IDL_FAILED_TO_PARSE, why.to_owned()));
    }
    Ok(out)
}

/// The presentation `request` (already [`parse`]d) leaves after `before`, over the cluster's
/// nodes `hosts`: a change of mode, name or primary is a new designation, with a new term and
/// `setVersion` (contract `topology-modes.md`); without `primary` the replica set designates the
/// live electable member with the highest priority.
pub fn next(
    before: &Presentation,
    request: &Document,
    hosts: &[String],
) -> Result<Presentation, CommandError> {
    let mode = match request.get(MODE) {
        Some(Value::String(m)) => PresentationMode::from_name(m).unwrap_or_default(),
        _ => before.mode,
    };
    let set_name = match request.get(SET_NAME) {
        Some(Value::String(s)) => s.clone(),
        _ => before.set_name.clone(),
    };
    let primary = match request.get(PRIMARY) {
        Some(Value::String(p)) => Some(p.clone()),
        _ => None,
    };
    let mut p = Presentation {
        mode,
        set_name,
        primary: None,
        set_version: before.set_version.saturating_add(1),
        term: before.term.saturating_add(1),
        members: before.members.clone(),
    };
    if mode == PresentationMode::RefRouter {
        return Ok(p);
    }
    if p.set_name.is_empty() {
        return Err(bad("the replica set presentation needs a setName"));
    }
    let rs = p.replica_set(hosts).map_err(|e| bad(format!("{e:?}")))?;
    let designated = match &primary {
        Some(host) => rs.designate(host).map_err(|e| bad(format!("{e:?}")))?,
        None => rs.reassign(&|_| true).unwrap_or(rs),
    };
    p.primary = designated.primary().map(str::to_owned);
    Ok(p)
}
