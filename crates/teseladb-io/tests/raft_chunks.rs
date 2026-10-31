//! `RaftBatch` on the wire in pieces of at most 1 MiB (005 FR-023): the encoder spreads a batch's
//! messages over frames of at most 1 MiB, cuts a message that does not fit one into `RaftChunk`
//! pieces, and the decoder gives the batch back whole once its last piece is in the buffer.
//! Interrupted, out of order, oversized and damaged sequences are rejected without a panic.

use teseladb_io::peer::{
    FrameError, HEADER_LEN, MAX_CHUNKED_LEN, MAX_PIECE_LEN, PeerMsg, RAFT_CHUNK_KIND,
    RAFT_CHUNK_VERSION, decode_frame, encode_frame,
};
use teseladb_raft::{Entry, EntryKind, MAX_ENTRY_BYTES, Message, MessageType};
use teseladb_types::{Bytes, GroupId, NodeId};

const WALL: u64 = 1_800_000_000_321;
/// The sender's HLC of the batches.
const HLC: teseladb_keys::Hlc = teseladb_keys::Hlc { physical: 1_800_000_000_400, logical: 7 };
const RAFT_BATCH_KIND: u8 = 0x02;

/// An `AppendReq` carrying one entry of `len` bytes.
fn append(index: u64, len: usize) -> Message {
    let mut m = Message::new(MessageType::AppendReq, NodeId(1), NodeId(2), 3);
    m.index = index - 1;
    m.log_term = 3;
    m.commit = index - 1;
    let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
    m.entries =
        vec![Entry { index, term: 3, kind: EntryKind::Normal, data: Bytes::from_vec(data) }];
    m
}

fn encode(msg: &PeerMsg) -> Vec<u8> {
    let mut out = Vec::new();
    encode_frame(msg, false, &mut out).unwrap();
    out
}

/// The frames of a stream: `(kind, whole frame length)`.
fn frames(stream: &[u8]) -> Vec<(u8, usize)> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < stream.len() {
        let len = u32::from_le_bytes(stream[at..at + 4].try_into().unwrap()) as usize;
        out.push((stream[at + 4], 4 + len));
        at += 4 + len;
    }
    out
}

/// Decodes every message of a stream that arrives `step` bytes at a time.
fn decode_all(stream: &[u8], step: usize) -> Vec<PeerMsg> {
    let mut buf = Vec::new();
    let mut got = Vec::new();
    for part in stream.chunks(step) {
        buf.extend_from_slice(part);
        while let Some((m, n)) = decode_frame(&buf).unwrap() {
            got.push(m);
            buf.drain(..n);
        }
    }
    assert!(buf.is_empty(), "nothing is left over");
    got
}

/// The messages of the batches, in order, checking that every batch carries the wall clock.
fn messages(batches: Vec<PeerMsg>) -> Vec<(GroupId, Message)> {
    let mut out = Vec::new();
    for b in batches {
        let PeerMsg::RaftBatch { wall_ms, hlc, msgs } = b else { panic!("not a batch: {b:?}") };
        assert_eq!((wall_ms, hlc), (WALL, HLC));
        out.extend(msgs);
    }
    out
}

#[test]
fn a_16_mib_entry_travels_in_pieces_of_at_most_1_mib_and_comes_back_whole() {
    let msgs = vec![(GroupId(4), append(10, 16 << 20))];
    let batch = PeerMsg::RaftBatch { wall_ms: WALL, hlc: HLC, msgs: msgs.clone() };
    let stream = encode(&batch);
    let fs = frames(&stream);
    assert!(fs.len() >= 16, "{} pieces", fs.len());
    assert!(fs.iter().all(|&(kind, len)| kind == RAFT_CHUNK_KIND && len <= MAX_PIECE_LEN));
    // Arriving in reads of 64 KiB: nothing until the last piece, then the batch whole.
    let mut buf = Vec::new();
    let mut got = None;
    for part in stream.chunks(64 << 10) {
        assert!(got.is_none(), "the batch came before its last piece");
        buf.extend_from_slice(part);
        if let Some((m, n)) = decode_frame(&buf).unwrap() {
            assert_eq!(n, stream.len());
            got = Some(m);
        }
    }
    assert_eq!(got, Some(batch));
}

#[test]
fn an_entry_of_64_mib_round_trips_and_a_message_above_the_limit_is_not_encoded() {
    let batch = PeerMsg::RaftBatch {
        wall_ms: WALL,
        hlc: HLC,
        msgs: vec![(GroupId(1), append(2, MAX_ENTRY_BYTES))],
    };
    let stream = encode(&batch);
    assert!(frames(&stream).iter().all(|&(_, len)| len <= MAX_PIECE_LEN));
    assert_eq!(decode_frame(&stream), Ok(Some((batch, stream.len()))));
    let over = PeerMsg::RaftBatch {
        wall_ms: WALL,
        hlc: HLC,
        msgs: vec![(GroupId(1), append(2, MAX_CHUNKED_LEN))],
    };
    let mut out = vec![7u8];
    assert!(matches!(encode_frame(&over, false, &mut out), Err(FrameError::TooLarge { .. })));
    assert_eq!(out, vec![7u8], "nothing is written for a batch that is not sent");
}

#[test]
fn a_batch_of_many_messages_is_spread_over_frames_of_at_most_1_mib_in_order() {
    // 300 groups with 10 KiB each, a 3 MiB message in the middle and more small ones after it.
    let mut msgs: Vec<(GroupId, Message)> =
        (0..300).map(|g| (GroupId(g), append(g + 1, 10 << 10))).collect();
    msgs.insert(150, (GroupId(999), append(7, 3 << 20)));
    let batch = PeerMsg::RaftBatch { wall_ms: WALL, hlc: HLC, msgs: msgs.clone() };
    let stream = encode(&batch);
    let fs = frames(&stream);
    assert!(fs.iter().all(|&(_, len)| len <= MAX_PIECE_LEN));
    assert!(fs.iter().filter(|f| f.0 == RAFT_BATCH_KIND).count() >= 3);
    assert!(fs.iter().filter(|f| f.0 == RAFT_CHUNK_KIND).count() >= 3);
    for step in [1 << 20, 4096, 777_777] {
        assert_eq!(messages(decode_all(&stream, step)), msgs, "reads of {step} bytes");
    }
    // A small batch stays one frame, as before; an empty one too.
    let small = PeerMsg::RaftBatch { wall_ms: WALL, hlc: HLC, msgs: msgs[..3].to_vec() };
    assert_eq!(frames(&encode(&small)).len(), 1);
    let empty = PeerMsg::RaftBatch { wall_ms: WALL, hlc: HLC, msgs: vec![] };
    assert_eq!(decode_frame(&encode(&empty)), Ok(Some((empty.clone(), encode(&empty).len()))));
}

/// One `RaftChunk` frame.
fn piece(version: u8, total: u64, offset: u64, data: &[u8]) -> Vec<u8> {
    let body_len = 1 + 8 + 8 + data.len();
    let mut f = ((body_len + 4) as u32).to_le_bytes().to_vec();
    f.extend_from_slice(&[RAFT_CHUNK_KIND, 0, 0, 0, version]);
    f.extend_from_slice(&total.to_le_bytes());
    f.extend_from_slice(&offset.to_le_bytes());
    f.extend_from_slice(data);
    f
}

fn malformed(r: Result<Option<(PeerMsg, usize)>, FrameError>) -> bool {
    matches!(r, Err(FrameError::Malformed { kind: RAFT_CHUNK_KIND, .. }))
}

#[test]
fn broken_piece_sequences_are_rejected() {
    // The payload of a valid batch, cut by hand in two pieces.
    let batch =
        PeerMsg::RaftBatch { wall_ms: WALL, hlc: HLC, msgs: vec![(GroupId(2), append(5, 100))] };
    let payload = encode(&batch)[HEADER_LEN..].to_vec();
    let total = payload.len() as u64;
    let (a, b) = payload.split_at(40);
    let good =
        [piece(RAFT_CHUNK_VERSION, total, 0, a), piece(RAFT_CHUNK_VERSION, total, 40, b)].concat();
    assert_eq!(decode_frame(&good), Ok(Some((batch, good.len()))));
    // Only the first piece: more bytes are needed.
    assert_eq!(decode_frame(&piece(RAFT_CHUNK_VERSION, total, 0, a)), Ok(None));
    let first = piece(RAFT_CHUNK_VERSION, total, 0, a);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("starts past 0", piece(RAFT_CHUNK_VERSION, total, 40, b)),
        ("unknown version", piece(9, total, 0, &payload)),
        ("empty piece", piece(RAFT_CHUNK_VERSION, total, 0, &[])),
        ("past its total", piece(RAFT_CHUNK_VERSION, 10, 0, a)),
        ("a gap", [first.clone(), piece(RAFT_CHUNK_VERSION, total, 41, &b[1..])].concat()),
        ("another total", [first.clone(), piece(RAFT_CHUNK_VERSION, total + 1, 40, b)].concat()),
        (
            "interrupted by another frame",
            [first.clone(), encode(&PeerMsg::Goodbye { reason: "x".into() })].concat(),
        ),
        ("not a batch once whole", piece(RAFT_CHUNK_VERSION, 3, 0, &[1, 2, 3])),
        (
            "a header too short",
            [first.clone(), vec![3, 0, 0, 0, RAFT_CHUNK_KIND, 0, 0, 0]].concat(),
        ),
    ];
    for (what, f) in cases {
        assert!(malformed(decode_frame(&f)), "{what}: {:?}", decode_frame(&f));
    }
    let mut flagged = first.clone();
    flagged[5] = 1;
    assert_eq!(
        decode_frame(&flagged),
        Err(FrameError::BadFlags { kind: RAFT_CHUNK_KIND, flags: 1 })
    );
    // A total above what any batch reaches is refused from its first piece.
    let huge = MAX_CHUNKED_LEN as u64 + 1;
    assert_eq!(
        decode_frame(&piece(RAFT_CHUNK_VERSION, huge, 0, a)),
        Err(FrameError::TooLarge { len: huge })
    );
}

#[test]
fn batch_frames_and_pieces_above_1_mib_are_refused_from_their_first_bytes() {
    let len = (MAX_PIECE_LEN - 4 + 1) as u32;
    for kind in [RAFT_BATCH_KIND, RAFT_CHUNK_KIND] {
        let mut head = len.to_le_bytes().to_vec();
        head.push(kind);
        assert_eq!(
            decode_frame(&head),
            Err(FrameError::TooLarge { len: u64::from(len) }),
            "{kind:#x}"
        );
    }
}

#[test]
fn damaged_piece_sequences_never_panic() {
    let batch = PeerMsg::RaftBatch {
        wall_ms: WALL,
        hlc: HLC,
        msgs: vec![(GroupId(3), append(9, 2 << 20))],
    };
    let stream = encode(&batch);
    let fs = frames(&stream);
    assert!(fs.len() >= 3);
    // Every byte of each piece's header and body prefix, altered.
    let mut at = 0;
    for &(_, len) in &fs {
        for i in at..at + HEADER_LEN + 17 {
            for x in [0x01u8, 0x80, 0xff] {
                let mut s = stream.clone();
                s[i] ^= x;
                if let Ok(Some((back, n))) = decode_frame(&s) {
                    assert!(n <= s.len());
                    assert!(matches!(back, PeerMsg::RaftBatch { .. }));
                }
            }
        }
        at += len;
    }
    // Cut anywhere: more bytes are needed, never a wrong batch.
    for cut in (0..stream.len()).step_by(99_991) {
        assert_eq!(decode_frame(&stream[..cut]), Ok(None), "cut at {cut}");
    }
}
