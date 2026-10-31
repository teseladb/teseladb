//! Idempotent Group 0 commands (007 contract `catalog.md`): a command wrapped with a request
//! identifier, `(origin node, id)`, is applied once; a retry of the same identifier, after a lost
//! answer or a change of leader, gets the stored result without applying anything. Each origin
//! keeps its last [`REMEMBERED_PER_ORIGIN`] results, so an origin numbers its requests upwards and
//! retries only recent ones. The table travels in Group 0's snapshot. Group 0 kind 42.

use std::collections::BTreeMap;

use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

use crate::catalog::CollectionEntry;
use crate::group0::{Group0Error, Group0Result};
use crate::indexes::{IndexCommand, IndexEntry};
use crate::security::Principal;

/// Results remembered per origin node.
pub const REMEMBERED_PER_ORIGIN: usize = 1024;

/// Identifier of a request: the node that proposes it and its number there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId {
    /// Node that proposes it.
    pub origin: NodeId,
    /// Its number on that node, growing.
    pub id: u64,
}

/// Results of the idempotent commands applied, per origin and request number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requests {
    seen: BTreeMap<NodeId, BTreeMap<u64, Group0Result>>,
}

impl Requests {
    /// The result of request `req`, if it was applied and is still remembered.
    pub fn get(&self, req: RequestId) -> Option<&Group0Result> {
        self.seen.get(&req.origin)?.get(&req.id)
    }

    /// Remembers the result of `req`, forgetting the oldest of its origin beyond the limit.
    pub fn record(&mut self, req: RequestId, result: Group0Result) {
        let of = self.seen.entry(req.origin).or_default();
        of.insert(req.id, result);
        while of.len() > REMEMBERED_PER_ORIGIN {
            of.pop_first();
        }
    }

    /// Writes the table into Group 0's snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        e.var(self.seen.len() as u64);
        for (origin, results) in &self.seen {
            e.u64(origin.0).var(results.len() as u64);
            for (id, r) in results {
                e.u64(*id);
                put_result(e, r);
            }
        }
    }

    /// Reads what [`Requests::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Requests, DecodeError> {
        let mut t = Requests::default();
        for _ in 0..d.var()? {
            let origin = NodeId(d.u64()?);
            let mut results = BTreeMap::new();
            for _ in 0..d.var()? {
                let id = d.u64()?;
                results.insert(id, get_result(d)?);
            }
            t.seen.insert(origin, results);
        }
        Ok(t)
    }
}

const ERRORS: [Group0Error; 27] = [
    Group0Error::AlreadyBootstrapped,
    Group0Error::NotBootstrapped,
    Group0Error::NameOrAddrTaken,
    Group0Error::NoSuchNode,
    Group0Error::NoSuchGroup,
    Group0Error::BadReplicas,
    Group0Error::BadBounds,
    Group0Error::RangeBusy,
    Group0Error::BadSplit,
    Group0Error::NamespaceExists,
    Group0Error::NamespaceNotFound,
    Group0Error::IndexExists,
    Group0Error::IndexNotFound,
    Group0Error::PrincipalExists(Principal::User),
    Group0Error::PrincipalExists(Principal::Role),
    Group0Error::PrincipalNotFound(Principal::User),
    Group0Error::PrincipalNotFound(Principal::Role),
    Group0Error::SecurityConflict,
    Group0Error::NoSuchMerge,
    Group0Error::ClusterTimeKeyRaced,
    Group0Error::DatabaseDifferCase,
    Group0Error::EnrollTokenUnknown,
    Group0Error::EnrollTokenUsed,
    Group0Error::EnrollTokenExpired,
    Group0Error::InvalidTags,
    Group0Error::BadCore,
    Group0Error::NoSuchRelocation,
];

/// The code of an error in the snapshot: its position in [`ERRORS`].
fn error_code(e: Group0Error) -> u8 {
    match e {
        Group0Error::AlreadyBootstrapped => 0,
        Group0Error::NotBootstrapped => 1,
        Group0Error::NameOrAddrTaken => 2,
        Group0Error::NoSuchNode => 3,
        Group0Error::NoSuchGroup => 4,
        Group0Error::BadReplicas => 5,
        Group0Error::BadBounds => 6,
        Group0Error::RangeBusy => 7,
        Group0Error::BadSplit => 8,
        Group0Error::NamespaceExists => 9,
        Group0Error::NamespaceNotFound => 10,
        Group0Error::IndexExists => 11,
        Group0Error::IndexNotFound => 12,
        Group0Error::PrincipalExists(Principal::User) => 13,
        Group0Error::PrincipalExists(Principal::Role) => 14,
        Group0Error::PrincipalNotFound(Principal::User) => 15,
        Group0Error::PrincipalNotFound(Principal::Role) => 16,
        Group0Error::SecurityConflict => 17,
        Group0Error::NoSuchMerge => 18,
        Group0Error::ClusterTimeKeyRaced => 19,
        Group0Error::DatabaseDifferCase => 20,
        Group0Error::EnrollTokenUnknown => 21,
        Group0Error::EnrollTokenUsed => 22,
        Group0Error::EnrollTokenExpired => 23,
        Group0Error::InvalidTags => 24,
        Group0Error::BadCore => 25,
        Group0Error::NoSuchRelocation => 26,
    }
}

const OK: u8 = 0;
const NODE: u8 = 1;
const RANGE: u8 = 2;
const COLLECTION: u8 = 3;
const INDEX: u8 = 4;
const ERR: u8 = 5;

fn put_result(e: &mut Encoder, r: &Group0Result) {
    match r {
        Group0Result::Ok => {
            e.u8(OK);
        }
        Group0Result::Node(n) => {
            e.u8(NODE).u64(n.0);
        }
        Group0Result::Range(g) => {
            e.u8(RANGE).u64(g.0);
        }
        Group0Result::Collection(c) => {
            e.u8(COLLECTION).u32(c.id).raw(&c.uuid).bytes(&c.options).u64(c.created_index);
        }
        Group0Result::Index(i) => {
            e.u8(INDEX).u32(i.id);
            IndexCommand::Create {
                collection: i.collection,
                name: i.name.clone(),
                spec: i.spec.clone(),
                buckets: i.buckets,
            }
            .encode(e);
        }
        Group0Result::Err(x) => {
            e.u8(ERR).u8(error_code(*x));
        }
    }
}

fn get_result(d: &mut Decoder<'_>) -> Result<Group0Result, DecodeError> {
    let at = d.position();
    Ok(match d.u8()? {
        OK => Group0Result::Ok,
        NODE => Group0Result::Node(NodeId(d.u64()?)),
        RANGE => Group0Result::Range(GroupId(d.u64()?)),
        COLLECTION => {
            let id = d.u32()?;
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(d.raw(16)?);
            let options = d.bytes()?.to_vec();
            Group0Result::Collection(CollectionEntry { id, uuid, options, created_index: d.u64()? })
        }
        INDEX => {
            let id = d.u32()?;
            let IndexCommand::Create { collection, name, spec, buckets } = IndexCommand::decode(d)?
            else {
                return Err(DecodeError::InvalidValue { offset: at });
            };
            // A result is the index as it was created: in state `DeleteOnly`.
            let state = crate::indexes::IndexState::DeleteOnly;
            Group0Result::Index(IndexEntry { id, collection, name, spec, buckets, state })
        }
        ERR => {
            let at = d.position();
            let c = usize::from(d.u8()?);
            Group0Result::Err(*ERRORS.get(c).ok_or(DecodeError::InvalidValue { offset: at })?)
        }
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    })
}
