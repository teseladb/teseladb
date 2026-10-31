//! Frames of the inter-node protocol (contract `peer-frames.md` of specification 007): header, handshake and messages.
//! Codec only; connections, handshake and reconnection are done by the core loop.
//!
//! 8-byte header: `len u32` (what follows, including `kind`, `flags` and `reserved`; at most
//! 16 MiB, and 1 MiB in `RaftBatch` and its pieces, see [`chunk`]), `kind u8`, `flags u8` (bit0 = payload compressed with DEFLATE, only in `SnapshotChunk`)
//! and `reserved u16` (ignored). Everything little-endian.

use std::fmt;

mod chunk;

pub use chunk::{MAX_CHUNKED_LEN, MAX_PIECE_LEN, RAFT_CHUNK_KIND, RAFT_CHUNK_VERSION};

use teseladb_keys::Hlc;
use teseladb_raft::Message;
use teseladb_types::{Bytes, DecodeError, Decoder, Encoder, GroupId, NodeId};

/// Maximum of `len` (and of a `SnapshotChunk`'s decompressed size).
pub const MAX_FRAME_LEN: usize = 16 << 20;
/// Header bytes.
pub const HEADER_LEN: usize = 8;
/// Header bytes counted by `len` (`kind`, `flags`, `reserved`).
pub(crate) const LEN_COVERED: usize = 4;
/// Handshake marker.
pub const MAGIC: &[u8; 8] = b"TSLPEER1";
/// Protocol version: 4 since a heartbeat carries its range's closed mark (007 FR-012).
pub const PROTOCOL_VERSION: u16 = 4;
/// `flags`: payload compressed with DEFLATE.
pub const FLAG_DEFLATE: u8 = 1;

/// Error reading a frame: the contract says to close the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// `len` above 16 MiB (or the payload to send does not fit).
    TooLarge {
        /// Declared or required length.
        len: u64,
    },
    /// `len` smaller than what the header covers.
    TooShort {
        /// Declared length.
        len: u32,
    },
    /// Unknown `kind`.
    UnknownKind(u8),
    /// `flags` not valid for that `kind`.
    BadFlags {
        /// Kind.
        kind: u8,
        /// Flags.
        flags: u8,
    },
    /// Invalid compressed payload or one that exceeds 16 MiB decompressed.
    Inflate,
    /// The payload does not match the kind.
    Malformed {
        /// Kind.
        kind: u8,
        /// Decoding error.
        err: DecodeError,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLarge { len } => {
                write!(f, "frame of {len} bytes (max. {MAX_FRAME_LEN})")
            }
            FrameError::TooShort { len } => write!(f, "frame with len = {len}"),
            FrameError::UnknownKind(k) => write!(f, "unknown frame kind {k:#04x}"),
            FrameError::BadFlags { kind, flags } => {
                write!(f, "flags {flags:#04x} not valid in a {kind:#04x} frame")
            }
            FrameError::Inflate => f.write_str("invalid or too large compressed payload"),
            FrameError::Malformed { kind, err } => {
                write!(f, "malformed {kind:#04x} frame: {err}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Handshake: first frame in each direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Cluster (zeros in the handshake of a `JoinRequest`).
    pub cluster_id: [u8; 16],
    /// Node (0 only for `JoinRequest`).
    pub node_id: NodeId,
    /// Core of the connection.
    pub core: u16,
    /// Cores of the node.
    pub cores: u16,
    /// Inter-node address.
    pub peer_addr: String,
    /// HTTP address.
    pub http_addr: String,
    /// Native protocol address.
    pub native_addr: String,
}

/// Coalesced heartbeat of a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupHeartbeat {
    /// Group.
    pub group: GroupId,
    /// Leader's term.
    pub term: u64,
    /// `min(commit, matched[target])`.
    pub commit: u64,
    /// Heartbeat round.
    pub lease_seq: u64,
    /// The leader's closed mark and its last log index when it closed it (007 FR-012).
    pub closed: Option<(Hlc, u64)>,
}

/// Response to a coalesced heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupHeartbeatResp {
    /// Group.
    pub group: GroupId,
    /// Term of the responder.
    pub term: u64,
    /// Accepted.
    pub ok: bool,
    /// Round it answers.
    pub lease_seq: u64,
}

/// Accepted join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinAccepted {
    /// Assigned node.
    pub node_id: NodeId,
    /// Cluster.
    pub cluster_id: [u8; 16],
    /// Cores of the cluster.
    pub cores: u16,
    /// Group 0 voters with the fingerprint of their certificate.
    pub voters: Vec<(NodeId, [u8; 32])>,
    /// Denied fingerprints.
    pub denied: Vec<[u8; 32]>,
}

/// What a node that enrolls sends (013 FR-005): its token's id and secret, its name and the
/// public key of its new certificate.
#[derive(Clone, PartialEq, Eq)]
pub struct EnrollRequest {
    /// The token's id.
    pub token_id: [u8; 16],
    /// The token's secret.
    pub secret: [u8; 32],
    /// The node's name, the certificate's common name.
    pub name: String,
    /// The node's public key, SubjectPublicKeyInfo in DER.
    pub public_key: Vec<u8>,
}

impl fmt::Debug for EnrollRequest {
    /// Without the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnrollRequest").field("name", &self.name).finish()
    }
}

/// An accepted enrollment: the node's certificate, the CA's, and the master key and the CA's key
/// sealed for the node's key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollAccepted {
    /// The node's certificate, DER.
    pub certificate: Vec<u8>,
    /// The CA's certificate, DER.
    pub ca: Vec<u8>,
    /// The master key and the CA's key, sealed for the node's public key.
    pub sealed: Vec<u8>,
}

/// Inter-node message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerMsg {
    /// `0x01`.
    Hello(Hello),
    /// `0x02`: Raft messages of several groups.
    RaftBatch {
        /// Sender's wall clock (Unix ms, 005 FR-006).
        wall_ms: u64,
        /// Sender's HLC: the receiver's clock moves past it (005 FR-003).
        hlc: Hlc,
        /// The messages.
        msgs: Vec<(GroupId, Message)>,
    },
    /// `0x03`: heartbeats of several groups with the sender's wall clock (005 FR-006).
    Heartbeats {
        /// Sender's wall clock (Unix ms).
        wall_ms: u64,
        /// Sender's HLC (005 FR-003).
        hlc: Hlc,
        /// The heartbeats.
        beats: Vec<GroupHeartbeat>,
    },
    /// `0x04`.
    HeartbeatResps {
        /// Sender's wall clock (Unix ms, 005 FR-006).
        wall_ms: u64,
        /// Sender's HLC (005 FR-003).
        hlc: Hlc,
        /// The answers.
        resps: Vec<GroupHeartbeatResp>,
    },
    /// `0x1A`: a range's leader tells a node that left the range to drop its copy.
    RangeRetire {
        /// Range.
        group: GroupId,
    },
    /// `0x20`.
    SnapshotChunk {
        /// Group.
        group: u64,
        /// Index.
        index: u64,
        /// Term.
        term: u64,
        /// Offset of the chunk.
        offset: u64,
        /// Total size.
        total: u64,
        /// Data.
        data: Bytes,
    },
    /// `0x21`.
    SnapshotAck {
        /// Group.
        group: u64,
        /// Index.
        index: u64,
        /// Acknowledged offset.
        offset: u64,
        /// Correct.
        ok: bool,
    },
    /// `0x30`: client request forwarded to the leader's node and core.
    Forward {
        /// Request.
        req: u64,
        /// Deadline.
        deadline_ms: u32,
        /// Encoded request.
        body: Bytes,
    },
    /// `0x31`.
    ForwardResp {
        /// Request.
        req: u64,
        /// Encoded response.
        body: Bytes,
    },
    /// `0x32`.
    ProxyOpen {
        /// Session.
        stream: u64,
        /// Routing key the receiver resolves to the destination (for instance, a range).
        route: u64,
        /// Client protocol of the session.
        proto: u8,
        /// Encoded principal (specification 013).
        principal: Bytes,
        /// First bytes of the request.
        data: Bytes,
    },
    /// `0x33`.
    ProxyData {
        /// Session.
        stream: u64,
        /// End of this direction.
        fin: bool,
        /// Data.
        data: Bytes,
    },
    /// `0x34`.
    ProxyClose {
        /// Session.
        stream: u64,
        /// Reason.
        reason: String,
    },
    /// `0x40`.
    NodeHeartbeat {
        /// Node.
        node: NodeId,
        /// The node's clock.
        now_ms: u64,
        /// Groups.
        groups: u64,
        /// Leaders.
        leaders: u64,
        /// Bytes used.
        bytes: u64,
        /// Free bytes of the data directory's disk (007 FR-010).
        free_bytes: u64,
        /// Size of that disk; 0 while unknown.
        total_bytes: u64,
        /// Operations per second the ranges it leads served.
        ops: u64,
    },
    /// `0x41`.
    JoinRequest {
        /// Name.
        name: String,
        /// Inter-node address.
        peer_addr: String,
        /// HTTP address.
        http_addr: String,
        /// Native protocol address.
        native_addr: String,
        /// Cores (0 = adopt the cluster's).
        cores: u16,
    },
    /// `0x42`: accepted join or the reason for the rejection.
    JoinResponse(Result<JoinAccepted, String>),
    /// `0x44`.
    ClockReport {
        /// Node.
        node: NodeId,
        /// The node's clock minus the voters' median.
        offset_ms: i64,
    },
    /// `0x46`: a node asks Group 0's leader for an authorization lease (013 FR-012).
    AuthLeaseAsk {
        /// Node.
        node: NodeId,
        /// The request's number, echoed in the answer.
        seq: u64,
        /// The index of the node's copy of Group 0.
        applied: u64,
    },
    /// `0x47`: Group 0's leader answers an [`PeerMsg::AuthLeaseAsk`].
    AuthLeaseGrant {
        /// Node.
        node: NodeId,
        /// The request's number.
        seq: u64,
        /// Group 0's commit index: the lease holds only on a copy at or past it.
        commit: u64,
        /// Every change of users and roles up to this index is fenced.
        fenced: u64,
    },
    /// `0x48`: a node enrolls with a token.
    EnrollRequest(EnrollRequest),
    /// `0x49`: the answer to an [`PeerMsg::EnrollRequest`], or why it was refused.
    EnrollResponse(Result<EnrollAccepted, String>),
    /// `0x45`.
    NodeSuspect {
        /// Node.
        node: NodeId,
        /// Becomes `Suspect` (or goes back to `Alive`).
        suspect: bool,
    },
    /// `0x7F`.
    Goodbye {
        /// Reason.
        reason: String,
    },
}

impl PeerMsg {
    /// `kind` of the frame.
    pub fn kind(&self) -> u8 {
        match self {
            PeerMsg::Hello(_) => 0x01,
            PeerMsg::RaftBatch { .. } => 0x02,
            PeerMsg::Heartbeats { .. } => 0x03,
            PeerMsg::HeartbeatResps { .. } => 0x04,
            PeerMsg::SnapshotChunk { .. } => 0x20,
            PeerMsg::SnapshotAck { .. } => 0x21,
            PeerMsg::Forward { .. } => 0x30,
            PeerMsg::ForwardResp { .. } => 0x31,
            PeerMsg::ProxyOpen { .. } => 0x32,
            PeerMsg::ProxyData { .. } => 0x33,
            PeerMsg::ProxyClose { .. } => 0x34,
            PeerMsg::NodeHeartbeat { .. } => 0x40,
            PeerMsg::JoinRequest { .. } => 0x41,
            PeerMsg::JoinResponse(_) => 0x42,
            PeerMsg::ClockReport { .. } => 0x44,
            PeerMsg::NodeSuspect { .. } => 0x45,
            PeerMsg::AuthLeaseAsk { .. } => 0x46,
            PeerMsg::AuthLeaseGrant { .. } => 0x47,
            PeerMsg::EnrollRequest(_) => 0x48,
            PeerMsg::EnrollResponse(_) => 0x49,
            PeerMsg::RangeRetire { .. } => 0x1a,
            PeerMsg::Goodbye { .. } => 0x7f,
        }
    }

    fn encode_payload(&self, e: &mut Encoder) {
        match self {
            PeerMsg::Hello(h) => {
                e.raw(MAGIC).u16(PROTOCOL_VERSION).raw(&h.cluster_id).u64(h.node_id.0);
                e.u16(h.core).u16(h.cores).str(&h.peer_addr).str(&h.http_addr).str(&h.native_addr);
            }
            PeerMsg::RaftBatch { wall_ms, hlc, msgs: ms } => {
                e.u64(*wall_ms).u64(hlc.physical).u32(hlc.logical).var(ms.len() as u64);
                for (g, m) in ms {
                    e.u64(g.0);
                    m.encode(e);
                }
            }
            PeerMsg::Heartbeats { wall_ms, hlc, beats } => {
                e.u64(*wall_ms).u64(hlc.physical).u32(hlc.logical).var(beats.len() as u64);
                for h in beats {
                    e.u64(h.group.0).var(h.term).var(h.commit).var(h.lease_seq);
                    match h.closed {
                        None => e.bool(false),
                        Some((c, index)) => e.bool(true).u64(c.physical).u32(c.logical).var(index),
                    };
                }
            }
            PeerMsg::HeartbeatResps { wall_ms, hlc, resps: rs } => {
                e.u64(*wall_ms).u64(hlc.physical).u32(hlc.logical).var(rs.len() as u64);
                for r in rs {
                    e.u64(r.group.0).var(r.term).bool(r.ok).var(r.lease_seq);
                }
            }
            PeerMsg::RangeRetire { group } => {
                e.u64(group.0);
            }
            PeerMsg::SnapshotChunk { group, index, term, offset, total, data } => {
                e.u64(*group).u64(*index).u64(*term).u64(*offset).u64(*total).bytes(data);
            }
            PeerMsg::SnapshotAck { group, index, offset, ok } => {
                e.u64(*group).u64(*index).u64(*offset).bool(*ok);
            }
            PeerMsg::Forward { req, deadline_ms, body } => {
                e.u64(*req).u32(*deadline_ms).bytes(body);
            }
            PeerMsg::ForwardResp { req, body } => {
                e.u64(*req).bytes(body);
            }
            PeerMsg::ProxyOpen { stream, route, proto, principal, data } => {
                e.u64(*stream).u64(*route).u8(*proto).bytes(principal).bytes(data);
            }
            PeerMsg::ProxyData { stream, fin, data } => {
                e.u64(*stream).bool(*fin).bytes(data);
            }
            PeerMsg::ProxyClose { stream, reason } => {
                e.u64(*stream).str(reason);
            }
            PeerMsg::NodeHeartbeat {
                node,
                now_ms,
                groups,
                leaders,
                bytes,
                free_bytes,
                total_bytes,
                ops,
            } => {
                e.u64(node.0).u64(*now_ms).var(*groups).var(*leaders).var(*bytes);
                e.var(*free_bytes).var(*total_bytes).var(*ops);
            }
            PeerMsg::JoinRequest { name, peer_addr, http_addr, native_addr, cores } => {
                e.str(name).str(peer_addr).str(http_addr).str(native_addr).u16(*cores);
            }
            PeerMsg::JoinResponse(Ok(a)) => {
                e.u8(0).u64(a.node_id.0).raw(&a.cluster_id).u16(a.cores);
                e.var(a.voters.len() as u64);
                for (n, fp) in &a.voters {
                    e.u64(n.0).raw(fp);
                }
                e.var(a.denied.len() as u64);
                for fp in &a.denied {
                    e.raw(fp);
                }
            }
            PeerMsg::JoinResponse(Err(reason)) => {
                e.u8(1).str(reason);
            }
            PeerMsg::EnrollRequest(r) => {
                e.raw(&r.token_id).raw(&r.secret).str(&r.name).bytes(&r.public_key);
            }
            PeerMsg::EnrollResponse(Ok(a)) => {
                e.u8(0).bytes(&a.certificate).bytes(&a.ca).bytes(&a.sealed);
            }
            PeerMsg::EnrollResponse(Err(reason)) => {
                e.u8(1).str(reason);
            }
            PeerMsg::ClockReport { node, offset_ms } => {
                e.u64(node.0).i64(*offset_ms);
            }
            PeerMsg::AuthLeaseAsk { node, seq, applied } => {
                e.u64(node.0).u64(*seq).u64(*applied);
            }
            PeerMsg::AuthLeaseGrant { node, seq, commit, fenced } => {
                e.u64(node.0).u64(*seq).u64(*commit).u64(*fenced);
            }
            PeerMsg::NodeSuspect { node, suspect } => {
                e.u64(node.0).bool(*suspect);
            }
            PeerMsg::Goodbye { reason } => {
                e.str(reason);
            }
        }
    }

    fn decode_payload(kind: u8, d: &mut Decoder<'_>) -> Result<PeerMsg, DecodeError> {
        Ok(match kind {
            0x01 => {
                let at = d.position();
                if d.raw(MAGIC.len())? != MAGIC {
                    return Err(DecodeError::InvalidValue { offset: at });
                }
                let at = d.position();
                if d.u16()? != PROTOCOL_VERSION {
                    return Err(DecodeError::InvalidValue { offset: at });
                }
                PeerMsg::Hello(Hello {
                    cluster_id: array(d)?,
                    node_id: NodeId(d.u64()?),
                    core: d.u16()?,
                    cores: d.u16()?,
                    peer_addr: d.str()?.to_owned(),
                    http_addr: d.str()?.to_owned(),
                    native_addr: d.str()?.to_owned(),
                })
            }
            0x02 => {
                let (wall_ms, hlc) = (d.u64()?, decode_hlc(d)?);
                let n = count(d, 9)?;
                let mut ms = Vec::with_capacity(n);
                for _ in 0..n {
                    ms.push((GroupId(d.u64()?), Message::decode(d)?));
                }
                PeerMsg::RaftBatch { wall_ms, hlc, msgs: ms }
            }
            0x03 => {
                let (wall_ms, hlc) = (d.u64()?, decode_hlc(d)?);
                let n = count(d, 12)?;
                let mut hs = Vec::with_capacity(n);
                for _ in 0..n {
                    let (group, term, commit, lease_seq) =
                        (GroupId(d.u64()?), d.var()?, d.var()?, d.var()?);
                    let closed = if d.bool()? { Some((decode_hlc(d)?, d.var()?)) } else { None };
                    hs.push(GroupHeartbeat { group, term, commit, lease_seq, closed });
                }
                PeerMsg::Heartbeats { wall_ms, hlc, beats: hs }
            }
            0x04 => {
                let (wall_ms, hlc) = (d.u64()?, decode_hlc(d)?);
                let n = count(d, 11)?;
                let mut rs = Vec::with_capacity(n);
                for _ in 0..n {
                    rs.push(GroupHeartbeatResp {
                        group: GroupId(d.u64()?),
                        term: d.var()?,
                        ok: d.bool()?,
                        lease_seq: d.var()?,
                    });
                }
                PeerMsg::HeartbeatResps { wall_ms, hlc, resps: rs }
            }
            0x20 => PeerMsg::SnapshotChunk {
                group: d.u64()?,
                index: d.u64()?,
                term: d.u64()?,
                offset: d.u64()?,
                total: d.u64()?,
                data: Bytes::copy_from_slice(d.bytes()?),
            },
            0x21 => PeerMsg::SnapshotAck {
                group: d.u64()?,
                index: d.u64()?,
                offset: d.u64()?,
                ok: d.bool()?,
            },
            0x30 => PeerMsg::Forward {
                req: d.u64()?,
                deadline_ms: d.u32()?,
                body: Bytes::copy_from_slice(d.bytes()?),
            },
            0x31 => {
                PeerMsg::ForwardResp { req: d.u64()?, body: Bytes::copy_from_slice(d.bytes()?) }
            }
            0x32 => PeerMsg::ProxyOpen {
                stream: d.u64()?,
                route: d.u64()?,
                proto: d.u8()?,
                principal: Bytes::copy_from_slice(d.bytes()?),
                data: Bytes::copy_from_slice(d.bytes()?),
            },
            0x33 => PeerMsg::ProxyData {
                stream: d.u64()?,
                fin: d.bool()?,
                data: Bytes::copy_from_slice(d.bytes()?),
            },
            0x34 => PeerMsg::ProxyClose { stream: d.u64()?, reason: d.str()?.to_owned() },
            0x40 => {
                let (node, now_ms) = (NodeId(d.u64()?), d.u64()?);
                let (groups, leaders, bytes) = (d.var()?, d.var()?, d.var()?);
                let (free_bytes, total_bytes, ops) = (d.var()?, d.var()?, d.var()?);
                PeerMsg::NodeHeartbeat {
                    node,
                    now_ms,
                    groups,
                    leaders,
                    bytes,
                    free_bytes,
                    total_bytes,
                    ops,
                }
            }
            0x41 => PeerMsg::JoinRequest {
                name: d.str()?.to_owned(),
                peer_addr: d.str()?.to_owned(),
                http_addr: d.str()?.to_owned(),
                native_addr: d.str()?.to_owned(),
                cores: d.u16()?,
            },
            0x42 => {
                let at = d.position();
                match d.u8()? {
                    0 => {
                        let node_id = NodeId(d.u64()?);
                        let cluster_id = array(d)?;
                        let cores = d.u16()?;
                        let n = count(d, 40)?;
                        let mut voters = Vec::with_capacity(n);
                        for _ in 0..n {
                            voters.push((NodeId(d.u64()?), array(d)?));
                        }
                        let n = count(d, 32)?;
                        let mut denied = Vec::with_capacity(n);
                        for _ in 0..n {
                            denied.push(array(d)?);
                        }
                        PeerMsg::JoinResponse(Ok(JoinAccepted {
                            node_id,
                            cluster_id,
                            cores,
                            voters,
                            denied,
                        }))
                    }
                    1 => PeerMsg::JoinResponse(Err(d.str()?.to_owned())),
                    _ => return Err(DecodeError::InvalidValue { offset: at }),
                }
            }
            0x44 => PeerMsg::ClockReport { node: NodeId(d.u64()?), offset_ms: d.i64()? },
            0x48 => PeerMsg::EnrollRequest(EnrollRequest {
                token_id: array(d)?,
                secret: array(d)?,
                name: d.str()?.to_owned(),
                public_key: d.bytes()?.to_vec(),
            }),
            0x49 => {
                let at = d.position();
                match d.u8()? {
                    0 => PeerMsg::EnrollResponse(Ok(EnrollAccepted {
                        certificate: d.bytes()?.to_vec(),
                        ca: d.bytes()?.to_vec(),
                        sealed: d.bytes()?.to_vec(),
                    })),
                    1 => PeerMsg::EnrollResponse(Err(d.str()?.to_owned())),
                    _ => return Err(DecodeError::InvalidValue { offset: at }),
                }
            }
            0x45 => PeerMsg::NodeSuspect { node: NodeId(d.u64()?), suspect: d.bool()? },
            0x46 => {
                PeerMsg::AuthLeaseAsk { node: NodeId(d.u64()?), seq: d.u64()?, applied: d.u64()? }
            }
            0x47 => PeerMsg::AuthLeaseGrant {
                node: NodeId(d.u64()?),
                seq: d.u64()?,
                commit: d.u64()?,
                fenced: d.u64()?,
            },
            0x1a => PeerMsg::RangeRetire { group: GroupId(d.u64()?) },
            0x7f => PeerMsg::Goodbye { reason: d.str()?.to_owned() },
            // `decode_frame` checks the kind first; this answer is for a direct call.
            _ => return Err(DecodeError::InvalidValue { offset: 0 }),
        })
    }
}

/// Kinds that exist.
fn known_kind(kind: u8) -> bool {
    matches!(
        kind,
        0x01..=0x04
            | 0x1a
            | RAFT_CHUNK_KIND
            | 0x20
            | 0x21
            | 0x30..=0x34
            | 0x40
            | 0x41
            | 0x42
            | 0x44
            | 0x45
            | 0x46
            | 0x47
            | 0x48
            | 0x49
            | 0x7f
    )
}

/// Number of elements of a list, bounded by what is left (each takes at least `min` bytes): an
/// absurd length does not reserve memory.
fn count(d: &mut Decoder<'_>, min: usize) -> Result<usize, DecodeError> {
    let at = d.position();
    let n = d.var()?;
    if n > (d.remaining() / min) as u64 {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    Ok(n as usize)
}

fn array<const N: usize>(d: &mut Decoder<'_>) -> Result<[u8; N], DecodeError> {
    let mut a = [0u8; N];
    a.copy_from_slice(d.raw(N)?);
    Ok(a)
}

/// The sender's HLC of a Raft frame: physical milliseconds and logical counter.
fn decode_hlc(d: &mut Decoder<'_>) -> Result<Hlc, DecodeError> {
    Ok(Hlc { physical: d.u64()?, logical: d.u32()? })
}

/// Writes the frame of `msg` at the end of `out`. With `compress`, a `SnapshotChunk`'s payload is
/// compressed with DEFLATE (ignored for the other kinds). A `RaftBatch` may take several frames of
/// at most 1 MiB (005 FR-023); nothing is written when the message is refused.
pub fn encode_frame(msg: &PeerMsg, compress: bool, out: &mut Vec<u8>) -> Result<(), FrameError> {
    if let PeerMsg::RaftBatch { wall_ms, hlc, msgs } = msg {
        return chunk::encode_batch(*wall_ms, *hlc, msgs, out);
    }
    let mut e = Encoder::new();
    msg.encode_payload(&mut e);
    let mut payload = e.finish();
    let mut flags = 0;
    if compress && matches!(msg, PeerMsg::SnapshotChunk { .. }) {
        // Decompressed, the payload may reach 16 MiB (what the receiver accepts).
        if payload.len() > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge { len: payload.len() as u64 });
        }
        payload = teseladb_deflate::deflate(&payload, 1);
        flags = FLAG_DEFLATE;
    }
    if payload.len() > MAX_FRAME_LEN - LEN_COVERED {
        return Err(FrameError::TooLarge { len: (payload.len() + LEN_COVERED) as u64 });
    }
    out.reserve(HEADER_LEN + payload.len());
    out.extend_from_slice(&((payload.len() + LEN_COVERED) as u32).to_le_bytes());
    out.extend_from_slice(&[msg.kind(), flags, 0, 0]);
    out.extend_from_slice(&payload);
    Ok(())
}

/// Reads a frame from the start of `buf`: `Ok(None)` if bytes are still missing; otherwise the
/// message and the bytes it took (every piece of a `RaftBatch` cut into `RaftChunk` frames). An
/// excessive length is rejected with only the first 5 bytes, without waiting for the payload.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(PeerMsg, usize)>, FrameError> {
    let Some(len) = buf.get(..4) else { return Ok(None) };
    let len = u32::from_le_bytes(len.try_into().expect("4 bytes"));
    if len as usize > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge { len: u64::from(len) });
    }
    if (len as usize) < LEN_COVERED {
        return Err(FrameError::TooShort { len });
    }
    // The kind is checked as soon as it arrives, before the payload.
    if let Some(&kind) = buf.get(4) {
        if !known_kind(kind) {
            return Err(FrameError::UnknownKind(kind));
        }
        let piece = kind == chunk::RAFT_BATCH_KIND || kind == RAFT_CHUNK_KIND;
        if piece && len as usize > MAX_PIECE_LEN - 4 {
            return Err(FrameError::TooLarge { len: u64::from(len) });
        }
        if kind == RAFT_CHUNK_KIND {
            return chunk::decode_pieces(buf);
        }
    }
    let total = 4 + len as usize;
    let Some(frame) = buf.get(..total) else { return Ok(None) };
    let (kind, flags) = (frame[4], frame[5]);
    let compressed = flags == FLAG_DEFLATE && kind == 0x20;
    if flags != 0 && !compressed {
        return Err(FrameError::BadFlags { kind, flags });
    }
    let raw = &frame[HEADER_LEN..];
    let inflated;
    let payload = if compressed {
        inflated = inflate_limited(raw)?;
        &inflated[..]
    } else {
        raw
    };
    let mut d = Decoder::new(payload);
    let msg = PeerMsg::decode_payload(kind, &mut d)
        .and_then(|m| d.finish().map(|()| m))
        .map_err(|err| FrameError::Malformed { kind, err })?;
    Ok(Some((msg, total)))
}

fn inflate_limited(data: &[u8]) -> Result<Vec<u8>, FrameError> {
    let mut inf = teseladb_deflate::Inflater::with_limit(MAX_FRAME_LEN as u64);
    let mut out = Vec::new();
    match inf.feed(data, &mut out) {
        Ok((used, teseladb_deflate::Status::Done)) if used == data.len() => Ok(out),
        _ => Err(FrameError::Inflate),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_kind_is_not_decoded() {
        assert!(PeerMsg::decode_payload(0x05, &mut Decoder::new(&[])).is_err());
    }

    #[test]
    fn count_rejects_lengths_beyond_the_buffer() {
        let b = [5u8, 0, 0];
        assert!(count(&mut Decoder::new(&b), 1).is_err());
        let b = [2u8, 0, 0];
        let mut d = Decoder::new(&b);
        assert_eq!(count(&mut d, 1), Ok(2));
        assert_eq!(d.remaining(), 2);
    }
}
