//! `RaftBatch` on the wire in frames of at most 1 MiB (005 FR-023). The encoder spreads a batch's
//! messages over `RaftBatch` frames of at most [`MAX_PIECE_LEN`]; a message that does not fit one
//! (an `AppendReq` with an entry of up to 64 MiB) travels as the payload of a one-message batch
//! cut into consecutive `RaftChunk` (0x59) frames:
//!
//! ```text
//! version(u8) · total(u64) · offset(u64) · data (the rest of the frame)
//! ```
//!
//! The pieces of one batch go one after another on the connection; the decoder gives the batch
//! back once its last piece is in the buffer, reading only the headers until then.
//!
//! From protocol 6 on, a `Forward` or `ForwardResp` that does not fit a frame's 16 MiB goes the
//! same way as pieces of version [`FORWARD_CHUNK_VERSION`], whose joined data is the message's
//! kind followed by its payload: a forwarded write of a document of 16 MiB, or a range's answer
//! with the versions of one commit, can pass a frame.

use teseladb_keys::Hlc;
use teseladb_raft::{MAX_ENTRY_BYTES, Message};
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId};

use super::{FrameError, HEADER_LEN, LEN_COVERED, PeerMsg};

/// Largest `RaftBatch` or `RaftChunk` frame, header included.
pub const MAX_PIECE_LEN: usize = 1 << 20;
/// Largest batch payload rebuilt from pieces: one message with an entry of 64 MiB and its headers.
pub const MAX_CHUNKED_LEN: usize = MAX_ENTRY_BYTES + (1 << 20);
/// `kind` of a piece.
pub const RAFT_CHUNK_KIND: u8 = 0x59;
/// Version of a piece's body.
pub const RAFT_CHUNK_VERSION: u8 = 1;
/// Version of the pieces of a `Forward` or `ForwardResp` (protocol 6).
pub const FORWARD_CHUNK_VERSION: u8 = 2;
/// `kind` of a `RaftBatch`.
pub(super) const RAFT_BATCH_KIND: u8 = 0x02;
/// `kind` of a `Forward`.
pub(super) const FORWARD_KIND: u8 = 0x30;
/// `kind` of a `ForwardResp`.
pub(super) const FORWARD_RESP_KIND: u8 = 0x31;

/// Bytes of a piece's body before its data.
const PIECE_PREFIX: usize = 1 + 8 + 8;
/// Data a piece carries at most.
const PIECE_DATA: usize = MAX_PIECE_LEN - HEADER_LEN - PIECE_PREFIX;
/// Room for a batch's messages in one frame (wall clock, HLC and the longest count).
const BATCH_ROOM: usize = MAX_PIECE_LEN - HEADER_LEN - 8 - 12 - 10;

/// Writes the frames of a batch at the end of `out`; nothing is written if a message is too
/// large.
pub(super) fn encode_batch(
    wall_ms: u64,
    hlc: Hlc,
    msgs: &[(GroupId, Message)],
    out: &mut Vec<u8>,
) -> Result<(), FrameError> {
    let mut frames = Vec::new();
    let mut group: Vec<Vec<u8>> = Vec::new();
    let mut room = BATCH_ROOM;
    for (g, m) in msgs {
        let mut e = Encoder::new();
        e.u64(g.0);
        m.encode(&mut e);
        let part = e.finish();
        if part.len() > BATCH_ROOM {
            batch_frame(wall_ms, hlc, &std::mem::take(&mut group), &mut frames);
            room = BATCH_ROOM;
            pieces(&batch_payload(wall_ms, hlc, &[part]), &mut frames)?;
            continue;
        }
        if part.len() > room {
            batch_frame(wall_ms, hlc, &std::mem::take(&mut group), &mut frames);
            room = BATCH_ROOM;
        }
        room -= part.len();
        group.push(part);
    }
    // An empty batch still goes as one frame.
    if !group.is_empty() || frames.is_empty() {
        push_frame(RAFT_BATCH_KIND, &batch_payload(wall_ms, hlc, &group), &mut frames);
    }
    out.extend_from_slice(&frames);
    Ok(())
}

fn batch_payload(wall_ms: u64, hlc: Hlc, parts: &[Vec<u8>]) -> Vec<u8> {
    let mut e = Encoder::new();
    e.u64(wall_ms).u64(hlc.physical).u32(hlc.logical).var(parts.len() as u64);
    for p in parts {
        e.raw(p);
    }
    e.finish()
}

/// A `RaftBatch` frame of `parts`, if there are any.
fn batch_frame(wall_ms: u64, hlc: Hlc, parts: &[Vec<u8>], frames: &mut Vec<u8>) {
    if !parts.is_empty() {
        push_frame(RAFT_BATCH_KIND, &batch_payload(wall_ms, hlc, parts), frames);
    }
}

fn push_frame(kind: u8, payload: &[u8], frames: &mut Vec<u8>) {
    // Callers keep the payload within a piece, far below `u32::MAX`.
    let len = (payload.len() + LEN_COVERED) as u32;
    frames.extend_from_slice(&len.to_le_bytes());
    frames.extend_from_slice(&[kind, 0, 0, 0]);
    frames.extend_from_slice(payload);
}

/// Cuts a batch payload into `RaftChunk` frames.
fn pieces(payload: &[u8], frames: &mut Vec<u8>) -> Result<(), FrameError> {
    cut(RAFT_CHUNK_VERSION, payload, frames)
}

/// Cuts `payload` into `RaftChunk` frames of version `version`.
fn cut(version: u8, payload: &[u8], frames: &mut Vec<u8>) -> Result<(), FrameError> {
    if payload.len() > MAX_CHUNKED_LEN {
        return Err(FrameError::TooLarge { len: payload.len() as u64 });
    }
    let total = payload.len() as u64;
    for (i, data) in payload.chunks(PIECE_DATA).enumerate() {
        let mut e = Encoder::with_capacity(PIECE_PREFIX + data.len());
        e.u8(version).u64(total).u64((i * PIECE_DATA) as u64).raw(data);
        push_frame(RAFT_CHUNK_KIND, &e.finish(), frames);
    }
    Ok(())
}

/// Writes a forwarded request or answer of kind `kind` and payload `payload`, too large for a
/// frame, as pieces at the end of `out`; nothing is written if it passes [`MAX_CHUNKED_LEN`].
pub(super) fn encode_forward(
    kind: u8,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), FrameError> {
    let mut joined = Vec::with_capacity(1 + payload.len());
    joined.push(kind);
    joined.extend_from_slice(payload);
    let mut frames = Vec::new();
    cut(FORWARD_CHUNK_VERSION, &joined, &mut frames)?;
    out.extend_from_slice(&frames);
    Ok(())
}

/// A piece read from the buffer.
struct Piece<'a> {
    version: u8,
    total: u64,
    offset: u64,
    data: &'a [u8],
    /// Bytes of the whole frame.
    len: usize,
}

fn bad(at: usize) -> FrameError {
    FrameError::Malformed { kind: RAFT_CHUNK_KIND, err: DecodeError::InvalidValue { offset: at } }
}

/// The piece at the start of `buf`: `Ok(None)` while it is incomplete.
fn read_piece(buf: &[u8], at: usize) -> Result<Option<Piece<'_>>, FrameError> {
    let Some(head) = buf.get(..HEADER_LEN) else { return Ok(None) };
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    if head[4] != RAFT_CHUNK_KIND || !(LEN_COVERED..=MAX_PIECE_LEN - 4).contains(&len) {
        return Err(bad(at));
    }
    if head[5] != 0 {
        return Err(FrameError::BadFlags { kind: RAFT_CHUNK_KIND, flags: head[5] });
    }
    let Some(frame) = buf.get(..4 + len) else { return Ok(None) };
    let mut d = Decoder::new(&frame[HEADER_LEN..]);
    let prefix = (|| Ok::<_, DecodeError>((d.u8()?, d.u64()?, d.u64()?)))();
    let Ok((version, total, offset)) = prefix else { return Err(bad(at)) };
    let data = &frame[HEADER_LEN + PIECE_PREFIX..];
    let end = offset.checked_add(data.len() as u64);
    let known = version == RAFT_CHUNK_VERSION || version == FORWARD_CHUNK_VERSION;
    if !known || data.is_empty() || end.is_none_or(|e| e > total) {
        return Err(bad(at));
    }
    Ok(Some(Piece { version, total, offset, data, len: 4 + len }))
}

/// Reads the pieces of a batch from the start of `buf`: `Ok(None)` until the last one is there,
/// then the batch and the bytes its pieces took.
pub(super) fn decode_pieces(buf: &[u8]) -> Result<Option<(PeerMsg, usize)>, FrameError> {
    // First the headers, to know whether the batch is complete without copying anything.
    let mut at = 0;
    let mut got = 0u64;
    let mut first: Option<(u64, u8)> = None;
    let mut found: Vec<&[u8]> = Vec::new();
    while first.is_none_or(|(t, _)| got < t) {
        let Some(p) = read_piece(&buf[at..], at)? else { return Ok(None) };
        if first.is_none() && p.total > MAX_CHUNKED_LEN as u64 {
            return Err(FrameError::TooLarge { len: p.total });
        }
        if *first.get_or_insert((p.total, p.version)) != (p.total, p.version) || p.offset != got {
            return Err(bad(at));
        }
        got += p.data.len() as u64;
        found.push(p.data);
        at += p.len;
    }
    let payload = found.concat();
    let (kind, body) = match first.map(|(_, v)| v) {
        Some(FORWARD_CHUNK_VERSION) => match payload.split_first() {
            Some((&k, body)) if k == FORWARD_KIND || k == FORWARD_RESP_KIND => (k, body),
            _ => return Err(bad(0)),
        },
        _ => (RAFT_BATCH_KIND, &payload[..]),
    };
    let mut d = Decoder::new(body);
    let msg = PeerMsg::decode_payload(kind, &mut d)
        .and_then(|m| d.finish().map(|()| m))
        .map_err(|err| FrameError::Malformed { kind: RAFT_CHUNK_KIND, err })?;
    Ok(Some((msg, at)))
}
