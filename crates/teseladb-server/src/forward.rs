//! What one node asks another with a forwarded request (`PeerMsg::Forward`), and the answers: a
//! Group 0 command for Group 0's leader (001 FR-022, 005 FR-016), a piece of a data range's
//! snapshot for a replica that waits for it (005 FR-012), a request for the data range's leader
//! or Group 0's (006 FR-014, FR-027), the sessions to kill on every node (006 FR-016), a replica's
//! checksum for its range's leader and the leader's order to rebuild a replica (005 FR-014), and
//! whether a node applies the cluster's write gates (007 FR-029). The first byte tells them apart.

use teseladb_meta::command::Group0Command;
use teseladb_range::checksum::Report;
use teseladb_range::snapshot::Chunk;
use teseladb_wire::commands::session::KillScope;
use teseladb_wire::commands::session::{SessionMatch, Uid};
use teseladb_wire::commands::write_block::WriteGate;
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId};

/// Bytes of a session owner's `uid`.
const UID_LEN: usize = std::mem::size_of::<Uid>();

/// Tag of a Group 0 command.
const GROUP0: u8 = 0;
/// Tag of a snapshot fetch.
const FETCH: u8 = 1;
/// Tag of a request for the data range's leader.
const DATA: u8 = 2;
/// Tag of a catalog command for Group 0's leader, which answers with its result.
const CATALOG: u8 = 3;
/// Tag of the sessions to kill on the node that receives it.
const KILL_SESSIONS: u8 = 4;
/// What a [`KILL_SESSIONS`] request kills: the sessions its owners' matches reach, every session,
/// or one operation of the node that receives it.
const KILL_LISTED: u8 = 0;
const KILL_ALL: u8 = 1;
const KILL_OP: u8 = 2;
/// Tag of a replica's checksum for its range's leader.
const CHECKSUM: u8 = 5;
/// Tag of the order to rebuild the replica of a range on the node that receives it.
const REBUILD: u8 = 6;
/// Tag of a merge's question to a node about its replica of the range absorbed.
const MERGE_PROBE: u8 = 7;
/// Tag of [`Request::Meta2`].
const META2: u8 = 8;
/// Tag of [`Request::Gate`].
const GATE: u8 = 9;
/// Tag of [`Request::CurrentOps`].
const CURRENT_OPS: u8 = 10;
/// Tag of [`Request::Http`].
const HTTP: u8 = 11;
/// The answer of a node to [`Request::Gate`] that applies the gates.
pub const GATE_HELD: u8 = 1;
/// The answer of a node to [`Request::Gate`] that does not apply them yet.
pub const GATE_BEHIND: u8 = 0;
/// Tag of the answer "not ready yet".
const NOT_READY: u8 = 0;
/// Tag of the answer with a chunk.
const CHUNK: u8 = 1;
/// Tag of the answer "that snapshot is older than the range's last split".
const STALE: u8 = 2;

/// A piece of a range's snapshot: file `file` from byte `offset`, of the export for `index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    /// Range.
    pub group: GroupId,
    /// Index of the snapshot the replica waits for.
    pub index: u64,
    /// File of the export.
    pub file: u32,
    /// Where in the file.
    pub offset: u64,
}

/// A forwarded request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A command for Group 0's leader.
    Group0(Group0Command),
    /// A piece of a snapshot.
    Fetch(Fetch),
    /// A write or scan for the data range's leader (`crate::data_wire::RemoteReq`, encoded).
    Data(Vec<u8>),
    /// A catalog command for Group 0's leader (a `Group0Command`, encoded).
    Catalog(Vec<u8>),
    /// Sessions whose cursors die on the node that receives it (`killSessions`).
    KillSessions(KillScope),
    /// A replica's sum for the leader of its range.
    Checksum(Report),
    /// The leader of the range marked the replica of the node that receives it for rebuild.
    Rebuild(GroupId),
    /// Whether the replica of `range` on the node that receives it is frozen for `merge` with its
    /// data on disk (007 FR-009): the answer is one byte, 1 if so.
    MergeProbe {
        /// The range being absorbed.
        range: GroupId,
        /// The merge.
        merge: u64,
    },
    /// An entry for the leader of `Meta2` range `group` (a `Meta2Command`, encoded; 007 FR-014).
    Meta2 {
        /// The `Meta2` range.
        group: GroupId,
        /// The entry.
        entry: Vec<u8>,
    },
    /// Whether the node that receives it applies write gates `want` (007 FR-029): the answer is
    /// [`GATE_HELD`] if so, [`GATE_BEHIND`] if not yet.
    Gate(WriteGate),
    /// The operations in progress on the node that receives it (`currentOp`, 010 FR-016): the
    /// answer is a BSON document whose `ops` array holds each one as `currentOp` describes it.
    CurrentOps,
    /// A request of the HTTP port for a cursor or a session the node that receives it opened
    /// (019 FR-008, FR-020), with the caller's certificate user (`crate::http_service::Forwarded`,
    /// encoded): the answer is the HTTP response, encoded the same way.
    Http(Vec<u8>),
}

impl Request {
    /// Its encoding.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Request::Group0(cmd) => [&[GROUP0][..], &cmd.encode()].concat(),
            Request::Fetch(f) => {
                let mut e = Encoder::new();
                e.u8(FETCH).u64(f.group.0).u64(f.index).u32(f.file).u64(f.offset);
                e.finish()
            }
            Request::Data(b) => [&[DATA][..], b].concat(),
            Request::Catalog(b) => [&[CATALOG][..], b].concat(),
            Request::KillSessions(scope) => {
                let mut e = Encoder::new();
                e.u8(KILL_SESSIONS);
                match scope {
                    KillScope::All => {
                        e.u8(KILL_ALL);
                    }
                    KillScope::Sessions(matches) => {
                        e.u8(KILL_LISTED).var(matches.len() as u64);
                        for m in matches {
                            e.raw(&m.uid);
                            match m.lsid {
                                Some(l) => e.bool(true).u128(l),
                                None => e.bool(false),
                            };
                        }
                    }
                    KillScope::Op { node, op } => {
                        e.u8(KILL_OP).u64(*node).u32(*op);
                    }
                }
                e.finish()
            }
            Request::Checksum(r) => [&[CHECKSUM][..], &r.encode()].concat(),
            Request::Rebuild(g) => {
                let mut e = Encoder::new();
                e.u8(REBUILD).u64(g.0);
                e.finish()
            }
            Request::MergeProbe { range, merge } => {
                let mut e = Encoder::new();
                e.u8(MERGE_PROBE).u64(range.0).u64(*merge);
                e.finish()
            }
            Request::Meta2 { group, entry } => {
                let mut e = Encoder::new();
                e.u8(META2).u64(group.0).bytes(entry);
                e.finish()
            }
            Request::Gate(g) => {
                let mut e = Encoder::new();
                e.u8(GATE).u32(g.fsync_locks).u8(u8::from(g.user_writes_blocked));
                e.finish()
            }
            Request::CurrentOps => vec![CURRENT_OPS],
            Request::Http(b) => [&[HTTP][..], b].concat(),
        }
    }

    /// Decodes what [`Request::encode`] wrote.
    pub fn decode(b: &[u8]) -> Result<Request, DecodeError> {
        let mut d = Decoder::new(b);
        match d.u8()? {
            GROUP0 => Group0Command::decode(&b[1..]).map(Request::Group0),
            FETCH => {
                let f = Fetch {
                    group: GroupId(d.u64()?),
                    index: d.u64()?,
                    file: d.u32()?,
                    offset: d.u64()?,
                };
                d.finish()?;
                Ok(Request::Fetch(f))
            }
            DATA => Ok(Request::Data(b[1..].to_vec())),
            HTTP => Ok(Request::Http(b[1..].to_vec())),
            CATALOG => Ok(Request::Catalog(b[1..].to_vec())),
            KILL_SESSIONS => {
                let scope = match d.u8()? {
                    KILL_ALL => KillScope::All,
                    KILL_LISTED => {
                        let n = d.var()?;
                        // Each one is a `uid` and a flag at least.
                        if n > d.remaining() as u64 / (UID_LEN as u64 + 1) {
                            return Err(DecodeError::InvalidValue { offset: 2 });
                        }
                        let mut matches = Vec::new();
                        for _ in 0..n {
                            let mut uid = [0u8; UID_LEN];
                            uid.copy_from_slice(d.raw(UID_LEN)?);
                            let lsid = if d.bool()? { Some(d.u128()?) } else { None };
                            matches.push(SessionMatch { uid, lsid });
                        }
                        KillScope::Sessions(matches)
                    }
                    KILL_OP => KillScope::Op { node: d.u64()?, op: d.u32()? },
                    _ => return Err(DecodeError::InvalidValue { offset: 1 }),
                };
                d.finish()?;
                Ok(Request::KillSessions(scope))
            }
            CHECKSUM => Report::decode(&b[1..]).map(Request::Checksum),
            REBUILD => {
                let g = GroupId(d.u64()?);
                d.finish()?;
                Ok(Request::Rebuild(g))
            }
            MERGE_PROBE => {
                let (range, merge) = (GroupId(d.u64()?), d.u64()?);
                d.finish()?;
                Ok(Request::MergeProbe { range, merge })
            }
            META2 => {
                let (group, entry) = (GroupId(d.u64()?), d.bytes()?.to_vec());
                d.finish()?;
                Ok(Request::Meta2 { group, entry })
            }
            GATE => {
                let fsync_locks = d.u32()?;
                let user_writes_blocked = match d.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(DecodeError::InvalidValue { offset: 5 }),
                };
                d.finish()?;
                Ok(Request::Gate(WriteGate { fsync_locks, user_writes_blocked }))
            }
            CURRENT_OPS => {
                d.finish()?;
                Ok(Request::CurrentOps)
            }
            _ => Err(DecodeError::InvalidValue { offset: 0 }),
        }
    }
}

/// The answer to a [`Fetch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchReply {
    /// This replica cannot give that piece yet (it has not applied the snapshot's index, or its
    /// export is being written): ask again later or another replica.
    NotReady,
    /// The piece.
    Chunk(Chunk),
    /// This replica applied a split after the snapshot's index: no replica serves that snapshot
    /// any more (D-067), and the one that waits for it needs a newer one from the leader.
    Stale,
}

impl FetchReply {
    /// Its encoding.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            FetchReply::NotReady => vec![NOT_READY],
            FetchReply::Chunk(c) => [&[CHUNK][..], &c.encode()].concat(),
            FetchReply::Stale => vec![STALE],
        }
    }

    /// Decodes what [`FetchReply::encode`] wrote.
    pub fn decode(b: &[u8]) -> Result<FetchReply, DecodeError> {
        match b.split_first() {
            Some((&NOT_READY, [])) => Ok(FetchReply::NotReady),
            Some((&CHUNK, rest)) => Chunk::decode(rest).map(FetchReply::Chunk),
            Some((&STALE, [])) => Ok(FetchReply::Stale),
            _ => Err(DecodeError::InvalidValue { offset: 0 }),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_types::NodeId;

    #[test]
    fn requests_and_answers_read_back() {
        let reqs = [
            Request::Group0(Group0Command::NodeBoot { node: NodeId(3), boot_epoch: 2 }),
            Request::Fetch(Fetch { group: GroupId(1), index: 9, file: 2, offset: 1 << 20 }),
            Request::Data(vec![1, 2]),
            Request::Catalog(vec![3]),
            Request::KillSessions(KillScope::All),
            Request::KillSessions(KillScope::Sessions(vec![
                SessionMatch::one(1, [7; 32]),
                SessionMatch::owner([9; 32]),
                SessionMatch::one(u128::MAX, [0; 32]),
            ])),
            Request::KillSessions(KillScope::Op { node: 3, op: u32::MAX }),
            Request::Checksum(Report {
                range: GroupId(4),
                index: 12,
                at: teseladb_hlc::Hlc { physical: 7, logical: 3 },
                node: NodeId(2),
                sum: Some([5; 32]),
            }),
            Request::Rebuild(GroupId(4)),
            Request::Gate(WriteGate { fsync_locks: u32::MAX, user_writes_blocked: true }),
            Request::Gate(WriteGate::default()),
        ];
        for r in reqs {
            assert_eq!(Request::decode(&r.encode()), Ok(r));
        }
        // A rebuild order cut short, or with bytes after it.
        let rebuild = Request::Rebuild(GroupId(4)).encode();
        assert!(Request::decode(&rebuild[..rebuild.len() - 1]).is_err());
        assert!(Request::decode(&[&rebuild[..], &[0]].concat()).is_err());
        let chunk = Chunk {
            range: GroupId(1),
            index: 9,
            file: 0,
            files: 1,
            offset: 0,
            size: 3,
            data: vec![1, 2, 3],
        };
        for a in [FetchReply::NotReady, FetchReply::Chunk(chunk), FetchReply::Stale] {
            assert_eq!(FetchReply::decode(&a.encode()), Ok(a));
        }
    }

    #[test]
    fn what_is_not_a_request_or_an_answer_is_refused() {
        assert!(Request::decode(&[]).is_err());
        assert!(Request::decode(&[7]).is_err());
        assert!(Request::decode(&[FETCH, 1]).is_err());
        let mut long =
            Request::Fetch(Fetch { group: GroupId(1), index: 1, file: 0, offset: 0 }).encode();
        long.push(0);
        assert!(Request::decode(&long).is_err());
        assert!(Request::decode(&[GROUP0, 0xff, 0xff]).is_err());
        // A kill of no known kind.
        assert!(Request::decode(&[KILL_SESSIONS, 3]).is_err());
        assert!(Request::decode(&[KILL_SESSIONS]).is_err());
        assert!(Request::decode(&[KILL_SESSIONS, 0, 2, 1]).is_err());
        // A match whose flag promises an `lsid` that is not there, or a flag that is no flag.
        let uid = [3u8; UID_LEN];
        assert!(Request::decode(&[&[KILL_SESSIONS, 0, 1][..], &uid, &[1]].concat()).is_err());
        assert!(Request::decode(&[&[KILL_SESSIONS, 0, 1][..], &uid, &[2]].concat()).is_err());
        assert!(Request::decode(&[KILL_SESSIONS, 1, 0]).is_err());
        // Gates with a flag that is no bool, cut short, or with bytes after them.
        assert!(Request::decode(&[GATE, 0, 0, 0, 1, 2]).is_err());
        assert_eq!(Request::decode(&Request::CurrentOps.encode()), Ok(Request::CurrentOps));
        let http = Request::Http(b"GET /v1".to_vec());
        assert_eq!(Request::decode(&http.encode()), Ok(http));
        assert!(Request::decode(&[CURRENT_OPS, 0]).is_err());
        assert!(Request::decode(&[GATE, 0, 0, 0, 1]).is_err());
        assert!(Request::decode(&[GATE, 0, 0, 0, 1, 1, 0]).is_err());
        assert!(FetchReply::decode(&[]).is_err());
        assert!(FetchReply::decode(&[NOT_READY, 1]).is_err());
        assert!(FetchReply::decode(&[STALE, 1]).is_err());
        assert!(FetchReply::decode(&[CHUNK]).is_err());
        assert!(FetchReply::decode(&[9]).is_err());
    }
}
