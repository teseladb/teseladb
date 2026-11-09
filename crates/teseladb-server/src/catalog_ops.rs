//! The catalog's requests on core 0 (006 FR-027): reading the node's copy of Group 0's catalog
//! and changing it through Group 0's leader. A change proposed here, or forwarded to the leader,
//! is answered once this node's copy applied it, so the next command of the client sees it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use teseladb_meta::catalog::{Catalog, CatalogCommand, CollectionEntry};
use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::{Group0Error, Group0Result};
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_meta::security::Principal;
use teseladb_multiraft::{Done, OpError, OpId};
use teseladb_types::{Bytes, DecodeError, Decoder, Encoder, NodeId};

use crate::coresm::CoreOutput;

/// A catalog request of the node's handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogReq {
    /// This node's copy of the catalog.
    Read,
    /// A change.
    Write(CatalogCommand),
    /// This node's copy of the cluster settings.
    Settings,
    /// The secondary indexes of a collection in this node's copy (009).
    Indexes(u32),
    /// A change of the secondary indexes (009).
    Index(IndexCommand),
    /// The search indexes of a collection in this node's copy (011).
    SearchIndexes(u32),
    /// Settings changed together, each as it was read (`config.settings`, 007 FR-017).
    SetSettings(Vec<teseladb_meta::command::SettingChange>),
    /// A change of a cluster setting.
    SetSetting {
        /// Key.
        key: Vec<u8>,
        /// Value.
        value: Vec<u8>,
    },
    /// The document of a user or role in this node's copy (013).
    Principal {
        /// User or role.
        kind: Principal,
        /// Database.
        db: String,
        /// Name.
        name: String,
    },
    /// The document of `kind` whose `_id` has key `id` (an API key by its hash, 019 FR-016).
    PrincipalById {
        /// User, role or API key.
        kind: Principal,
        /// The key of its `_id`.
        id: Vec<u8>,
    },
    /// The documents of the users or roles of a database, or of all (013).
    Principals {
        /// User or role.
        kind: Principal,
        /// Database; `None` for all.
        db: Option<String>,
    },
    /// A batch of changes of the users and roles, made idempotent by the caller (013).
    Security(Group0Command),
    /// The documents of a collection of `config` generated from this node's copy (007).
    Config(String),
    /// The catalog and the index definitions of this node's copy, encoded as
    /// `RestoreCatalog` carries them, for a backup (007 FR-021).
    Dump,
    /// The tenants of this node's copy (007 FR-025).
    Tenants,
    /// The catalog changes of this node's copy above a mark (014 FR-014).
    Ddl(teseladb_hlc::Hlc),
    /// An empty Group 0 entry, so that this node's copy reaches a newer mark (014 FR-014).
    Tick,
}

/// The answer to a [`CatalogReq`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogAnswer {
    /// The catalog, shared with Group 0's state machine.
    Catalog(Arc<Catalog>),
    /// The cluster settings.
    Settings(BTreeMap<Vec<u8>, Vec<u8>>),
    /// The secondary indexes of a collection, and the pin of the read, which the operation
    /// releases when it ends (009 FR-006).
    Indexes(Vec<IndexEntry>, u64),
    /// The search indexes of a collection, by name.
    SearchIndexes(Vec<teseladb_meta::indexes::SearchIndexEntry>),
    /// Documents of users or roles.
    Principals(Vec<Vec<u8>>),
    /// The catalog and the index definitions, encoded (`CatalogReq::Dump`).
    Dump(Vec<u8>),
    /// Documents of a generated collection of `config`, in BSON.
    Documents(Vec<Vec<u8>>),
    /// The tenants, by number, with their names and states, and whether every node reads the
    /// entries that change a state (007 FR-027).
    Tenants(Vec<(u32, String, teseladb_meta::tenants::TenantState)>, bool),
    /// The catalog changes above a mark, the mark below which they are incomplete and the mark
    /// of the last entry applied.
    Ddl(Vec<teseladb_meta::ddl_log::DdlRecord>, teseladb_hlc::Hlc, teseladb_hlc::Hlc),
    /// The change was applied: the collection it created or found.
    Done(Option<CollectionEntry>),
    /// Group 0 refused the change.
    Refused(Group0Error),
    /// No answer: no leader, a timeout or the core stopping; the caller tries again.
    Unavailable,
}

/// Where the answer goes.
pub type CatalogSender = Sender<CatalogAnswer>;

/// What applying a change gave, with Group 0's index of the change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// Index of the entry.
    pub index: u64,
    /// Its result.
    pub result: Result<Option<CollectionEntry>, Group0Error>,
}

const DONE: u8 = 0;
const REFUSED: u8 = 1;
const UNAVAILABLE: u8 = 2;

fn error_code(e: Group0Error) -> u8 {
    match e {
        Group0Error::NamespaceExists => 0,
        Group0Error::NamespaceNotFound => 1,
        Group0Error::PrincipalExists(Principal::User) => 3,
        Group0Error::PrincipalExists(Principal::Role) => 4,
        Group0Error::PrincipalNotFound(Principal::User) => 5,
        Group0Error::PrincipalNotFound(Principal::Role) => 6,
        Group0Error::SecurityConflict => 7,
        Group0Error::DatabaseDifferCase => 8,
        Group0Error::TenantExists => 9,
        Group0Error::TenantNotFound => 10,
        Group0Error::InvalidTenantName => 11,
        Group0Error::NotEmpty => 12,
        Group0Error::InvalidOptions => 13,
        Group0Error::NotEveryNodeRunsIt => 14,
        _ => 2,
    }
}

fn error_of(c: u8) -> Group0Error {
    match c {
        0 => Group0Error::NamespaceExists,
        1 => Group0Error::NamespaceNotFound,
        3 => Group0Error::PrincipalExists(Principal::User),
        4 => Group0Error::PrincipalExists(Principal::Role),
        5 => Group0Error::PrincipalNotFound(Principal::User),
        6 => Group0Error::PrincipalNotFound(Principal::Role),
        7 => Group0Error::SecurityConflict,
        8 => Group0Error::DatabaseDifferCase,
        9 => Group0Error::TenantExists,
        10 => Group0Error::TenantNotFound,
        11 => Group0Error::InvalidTenantName,
        12 => Group0Error::NotEmpty,
        13 => Group0Error::InvalidOptions,
        14 => Group0Error::NotEveryNodeRunsIt,
        _ => Group0Error::NotBootstrapped,
    }
}

/// Encodes the leader's answer to a forwarded change (`None`: it could not take it).
pub fn encode_applied(a: Option<&Applied>) -> Vec<u8> {
    let mut e = Encoder::new();
    match a {
        None => {
            e.u8(UNAVAILABLE);
        }
        Some(Applied { index, result: Ok(entry) }) => {
            e.u8(DONE).u64(*index).bool(entry.is_some());
            if let Some(c) = entry {
                e.u32(c.id).raw(&c.uuid).bytes(&c.options).u64(c.created_index);
            }
        }
        Some(Applied { index, result: Err(err) }) => {
            e.u8(REFUSED).u64(*index).u8(error_code(*err));
        }
    }
    e.finish()
}

/// Reads what [`encode_applied`] wrote.
pub fn decode_applied(b: &[u8]) -> Result<Option<Applied>, DecodeError> {
    let mut d = Decoder::new(b);
    let at = d.position();
    let a = match d.u8()? {
        UNAVAILABLE => None,
        DONE => {
            let index = d.u64()?;
            let entry = if d.bool()? {
                let id = d.u32()?;
                let mut uuid = [0u8; 16];
                uuid.copy_from_slice(d.raw(16)?);
                Some(CollectionEntry {
                    id,
                    uuid,
                    options: d.bytes()?.to_vec(),
                    created_index: d.u64()?,
                    tenant: 0,
                })
            } else {
                None
            };
            Some(Applied { index, result: Ok(entry) })
        }
        REFUSED => Some(Applied { index: d.u64()?, result: Err(error_of(d.u8()?)) }),
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    };
    d.finish()?;
    Ok(a)
}

/// The result of a proposal of a catalog change that ended.
fn applied(index: u64, r: Result<Done<CoreOutput>, OpError>) -> Option<Applied> {
    match r {
        Ok(Done::Applied(CoreOutput::Group0(res))) => {
            let result = match res {
                Group0Result::Collection(c) => Ok(Some(c)),
                Group0Result::Err(e) => Err(e),
                // A catalog change is never an order on a descriptor: `Meta1` never answers it.
                Group0Result::Ok
                | Group0Result::Node(_)
                | Group0Result::Range(_)
                | Group0Result::Index(_)
                | Group0Result::Meta1(_)
                | Group0Result::Tenant(_) => Ok(None),
            };
            Some(Applied { index, result })
        }
        Ok(_) | Err(_) => None,
    }
}

/// The catalog requests in flight on core 0.
#[derive(Default)]
pub struct CatalogOps {
    /// Proposals of this node's handle.
    local: BTreeMap<OpId, CatalogSender>,
    /// Forwards to Group 0's leader of this node's handle.
    forwarded: BTreeMap<OpId, CatalogSender>,
    /// Proposals for another node's forwarded change.
    remote: BTreeMap<OpId, (NodeId, u64)>,
    /// Answers that wait for this node's copy to apply their index, and for the authorization
    /// fence to pass it when the flag is set (013 FR-012).
    waiting: Vec<(u64, bool, CatalogAnswer, CatalogSender)>,
    /// The last operation of the handle, and those whose answer waits for the fence.
    last: Option<OpId>,
    fenced: std::collections::BTreeSet<OpId>,
    /// When each change of the handle started, and whether its stall was reported.
    since: BTreeMap<OpId, (Instant, bool)>,
    /// When each answer started to wait, by its index, and whether its stall was reported.
    waiting_since: BTreeMap<u64, (Instant, bool)>,
}

/// A change of the catalog that waits longer than this is written to the log once, with what it
/// waits for: its operation in Group 0, or this node's copy and the authorization fence.
const STALL: Duration = Duration::from_secs(5);

/// What core 0 does for a change: propose it as Group 0's leader, or forward it.
pub enum Route {
    /// This core leads Group 0.
    Propose,
    /// Forward to this node.
    Forward(NodeId),
    /// No leader known.
    Nowhere,
}

impl CatalogOps {
    /// The payload of a change.
    pub fn payload(cmd: CatalogCommand) -> Vec<u8> {
        Group0Command::Catalog(cmd).encode()
    }

    /// Whether operation `op` is one of this module's.
    pub fn owns(&self, op: OpId) -> bool {
        self.local.contains_key(&op)
            || self.forwarded.contains_key(&op)
            || self.remote.contains_key(&op)
    }

    /// A change of the handle, proposed (`Ok(op)` of a proposal) or forwarded.
    pub fn local(&mut self, op: Result<OpId, OpError>, forwarded: bool, to: CatalogSender) {
        self.last = op.as_ref().ok().copied();
        if let Ok(op) = &op {
            self.since.insert(*op, (Instant::now(), false));
        }
        match op {
            Ok(op) if forwarded => drop(self.forwarded.insert(op, to)),
            Ok(op) => drop(self.local.insert(op, to)),
            Err(_) => drop(to.send(CatalogAnswer::Unavailable)),
        }
    }

    /// The last change of the handle answers only once the authorization fence passes its index.
    pub fn fence_last(&mut self) {
        if let Some(op) = self.last.take() {
            self.fenced.insert(op);
        }
    }

    /// A change another node forwarded, proposed here as Group 0's leader (`None`: not leader).
    pub fn remote(&mut self, op: Option<OpId>, from: (NodeId, u64)) -> Option<Vec<u8>> {
        match op {
            Some(op) => {
                self.remote.insert(op, from);
                None
            }
            None => Some(encode_applied(None)),
        }
    }

    /// An operation of the core ended (`commit` is Group 0's commit index now); the forwarded
    /// answer to send, if it was one of another node's. `None` if it is not this module's.
    pub fn on_done(
        &mut self,
        op: OpId,
        r: Result<Done<CoreOutput>, OpError>,
        commit: u64,
    ) -> Option<Option<(NodeId, u64, Bytes)>> {
        let fence = self.fenced.remove(&op);
        self.since.remove(&op);
        // A change goes to its client only through `poll`, which runs after core 0 published
        // its copy (H-017): an answer sent here came before the copy, and the client's next read
        // missed the change.
        if let Some(to) = self.local.remove(&op) {
            match applied(commit, r) {
                Some(a) => self.waiting.push((a.index, fence, answer(Some(a)), to)),
                None => drop(to.send(answer(None))),
            }
            return Some(None);
        }
        if let Some((node, req)) = self.remote.remove(&op) {
            let body = encode_applied(applied(commit, r).as_ref());
            return Some(Some((node, req, Bytes::from_vec(body))));
        }
        let to = self.forwarded.remove(&op)?;
        let a = match r {
            Ok(Done::Forwarded(body)) => decode_applied(&body).ok().flatten(),
            Ok(_) | Err(_) => None,
        };
        match a {
            Some(a) => {
                let index = a.index;
                self.waiting.push((index, fence, answer(Some(a)), to));
            }
            None => drop(to.send(answer(None))),
        }
        Some(None)
    }

    /// Sends the answers whose index this node's copy applied and, for those that wait for it,
    /// the authorization fence has passed (`fenced`).
    pub fn poll(&mut self, applied_here: u64, fenced: u64) {
        let (ready, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|(i, fence, _, _)| *i <= applied_here && (!fence || *i <= fenced));
        self.waiting = rest;
        for (i, _, a, to) in ready {
            self.waiting_since.remove(&i);
            let _ = to.send(a);
        }
        for line in self.stalls(applied_here, fenced) {
            crate::log::defer(line);
        }
    }

    /// The changes that wait longer than [`STALL`] and were not reported yet, as log lines.
    fn stalls(&mut self, applied_here: u64, fenced: u64) -> Vec<crate::log::Pending> {
        let mut out = Vec::new();
        for (op, (at, told)) in &mut self.since {
            if !*told && at.elapsed() >= STALL {
                *told = true;
                let forwarded = self.forwarded.contains_key(op);
                out.push(crate::log::Pending {
                    id: 0,
                    msg: "a catalog change waits for Group 0".to_owned(),
                    attr: format!(
                        "{{\"forwarded\":{forwarded},\"waitedMillis\":{}}}",
                        at.elapsed().as_millis()
                    ),
                });
            }
        }
        for (index, fence, _, _) in &self.waiting {
            let (at, told) = self.waiting_since.entry(*index).or_insert((Instant::now(), false));
            if !*told && at.elapsed() >= STALL {
                *told = true;
                out.push(crate::log::Pending {
                    id: 0,
                    msg: "a catalog change waits for this node's copy or the authorization fence".to_owned(),
                    attr: format!(
                        "{{\"index\":{index},\"fence\":{fence},\"applied\":{applied_here},\"fenced\":{fenced}}}"
                    ),
                });
            }
        }
        out
    }
}

fn answer(a: Option<Applied>) -> CatalogAnswer {
    match a {
        None => CatalogAnswer::Unavailable,
        Some(Applied { result: Ok(e), .. }) => CatalogAnswer::Done(e),
        Some(Applied { result: Err(e), .. }) => CatalogAnswer::Refused(e),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_change_that_waits_too_long_is_reported_once() {
        let mut ops = CatalogOps::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        ops.local(Ok(OpId(7)), true, tx.clone());
        let long_ago = Instant::now().checked_sub(STALL).unwrap();
        ops.since.insert(OpId(7), (long_ago, false));
        ops.waiting.push((12, true, CatalogAnswer::Unavailable, tx));
        ops.waiting_since.insert(12, (long_ago, false));
        let lines = ops.stalls(12, 3);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].attr.contains("\"forwarded\":true"), "{}", lines[0].attr);
        assert!(lines[1].attr.contains("\"fenced\":3"), "{}", lines[1].attr);
        assert!(ops.stalls(12, 3).is_empty(), "once");
        // The answer goes once the fence passes it, and nothing of it is left.
        ops.poll(12, 12);
        assert!(ops.waiting.is_empty() && ops.waiting_since.is_empty());
    }

    #[test]
    fn a_change_is_answered_only_once_the_copy_was_published() {
        // H-017: core 0 publishes its copy of the catalog before it answers any change of it;
        // `poll` runs after that publication, so every answer waits for it (a resharding read
        // its new collection right after creating it and did not find it).
        let mut ops = CatalogOps::default();
        let (tx, rx) = std::sync::mpsc::channel();
        ops.local(Ok(OpId(5)), false, tx);
        let done = Ok(Done::Applied(CoreOutput::Group0(Group0Result::Ok)));
        assert_eq!(ops.on_done(OpId(5), done, 10), Some(None));
        assert!(rx.try_recv().is_err(), "answered before the copy was published");
        ops.poll(10, 0);
        assert!(rx.try_recv().is_ok());
        // A change forwarded to the leader, already applied here, waits for the same poll.
        let (tx, rx) = std::sync::mpsc::channel();
        ops.local(Ok(OpId(6)), true, tx);
        let body = encode_applied(Some(&Applied { index: 11, result: Ok(None) }));
        let done = Ok(Done::Forwarded(Bytes::from_vec(body)));
        assert_eq!(ops.on_done(OpId(6), done, 11), Some(None));
        assert!(rx.try_recv().is_err(), "answered before the copy was published");
        ops.poll(11, 0);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn fencing_nothing_changes_nothing() {
        let mut ops = CatalogOps::default();
        ops.fence_last();
        assert!(ops.fenced.is_empty());
    }

    #[test]
    fn answers_read_back() {
        let entry = CollectionEntry {
            id: 3,
            uuid: [4; 16],
            options: vec![5, 0, 0, 0, 0],
            created_index: 9,
            tenant: 0,
        };
        let cases = [
            None,
            Some(Applied { index: 7, result: Ok(Some(entry)) }),
            Some(Applied { index: 7, result: Ok(None) }),
            Some(Applied { index: 8, result: Err(Group0Error::NamespaceExists) }),
            Some(Applied { index: 8, result: Err(Group0Error::NamespaceNotFound) }),
            Some(Applied { index: 9, result: Err(Group0Error::PrincipalExists(Principal::User)) }),
            Some(Applied { index: 9, result: Err(Group0Error::PrincipalExists(Principal::Role)) }),
            Some(Applied {
                index: 9,
                result: Err(Group0Error::PrincipalNotFound(Principal::User)),
            }),
            Some(Applied {
                index: 9,
                result: Err(Group0Error::PrincipalNotFound(Principal::Role)),
            }),
            Some(Applied { index: 9, result: Err(Group0Error::SecurityConflict) }),
        ];
        for c in cases {
            assert_eq!(decode_applied(&encode_applied(c.as_ref())), Ok(c));
        }
        let other = Applied { index: 1, result: Err(Group0Error::BadSplit) };
        let back = decode_applied(&encode_applied(Some(&other))).unwrap().unwrap();
        assert_eq!(back.result, Err(Group0Error::NotBootstrapped));
        assert!(decode_applied(&[]).is_err());
        assert!(decode_applied(&[9]).is_err());
        assert!(decode_applied(&[UNAVAILABLE, 0]).is_err());
    }
}
