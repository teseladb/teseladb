//! Frames of the inter-node protocol: round trip of every kind, incremental reads,
//! 16 MiB limits, unknown `kind`, flags, compression and damaged payloads.

use teseladb_io::peer::{
    EnrollAccepted, EnrollRequest, FLAG_DEFLATE, FrameError, GroupHeartbeat, GroupHeartbeatResp,
    HEADER_LEN, Hello, JoinAccepted, MAX_FRAME_LEN, PeerMsg, RAFT_CHUNK_KIND, Versions,
    decode_frame, encode_frame,
};
use teseladb_raft::{Entry, EntryKind, Message, MessageType};
use teseladb_types::{Bytes, GroupId, NodeId};

/// The sender's HLC of the Raft frames.
const HLC: teseladb_keys::Hlc = teseladb_keys::Hlc { physical: 1_800_000_000_999, logical: 5 };

fn raft_msg() -> Message {
    let mut m = Message::new(MessageType::AppendReq, NodeId(1), NodeId(2), 5);
    m.index = 9;
    m.log_term = 4;
    m.commit = 8;
    m.entries = vec![
        Entry { index: 10, term: 5, kind: EntryKind::Normal, data: Bytes::from("x") },
        Entry { index: 11, term: 5, kind: EntryKind::ConfChange, data: Bytes::new() },
    ];
    m
}

fn all() -> Vec<PeerMsg> {
    vec![
        PeerMsg::Hello(Hello {
            cluster_id: [9; 16],
            node_id: NodeId(3),
            core: 1,
            cores: 4,
            peer_addr: "10.0.0.3:7421".into(),
            http_addr: "10.0.0.3:8080".into(),
            native_addr: "10.0.0.3:7422".into(),
            versions: Versions::OWN,
        }),
        PeerMsg::RaftBatch {
            wall_ms: 1_800_000_000_789,
            hlc: HLC,
            msgs: vec![
                (GroupId(1), raft_msg()),
                (GroupId(2), Message::new(MessageType::VoteResp, NodeId(2), NodeId(1), 6)),
            ],
        },
        PeerMsg::RaftBatch { wall_ms: 0, hlc: HLC, msgs: vec![] },
        PeerMsg::Heartbeats {
            wall_ms: 1_800_000_000_123,
            hlc: HLC,
            beats: vec![
                GroupHeartbeat {
                    group: GroupId(1),
                    term: 5,
                    commit: 9,
                    lease_seq: 100,
                    closed: None,
                },
                GroupHeartbeat {
                    group: GroupId(u64::MAX),
                    term: u64::MAX,
                    commit: 0,
                    lease_seq: 1,
                    closed: Some((HLC, u64::MAX)),
                },
            ],
        },
        PeerMsg::HeartbeatResps {
            wall_ms: 1_800_000_000_456,
            hlc: HLC,
            resps: vec![GroupHeartbeatResp {
                group: GroupId(1),
                term: 5,
                ok: true,
                lease_seq: 100,
            }],
        },
        PeerMsg::RangeRetire { group: GroupId(9) },
        PeerMsg::SnapshotChunk {
            group: 5,
            index: 100,
            term: 3,
            offset: 0,
            total: 3,
            data: Bytes::from("abc"),
        },
        PeerMsg::SnapshotChunk {
            group: u64::MAX,
            index: 0,
            term: 0,
            offset: 0,
            total: 0,
            data: Bytes::new(),
        },
        PeerMsg::SnapshotAck { group: 5, index: 100, offset: 3, ok: true },
        PeerMsg::Forward { req: 1, deadline_ms: 5000, body: Bytes::from("request") },
        PeerMsg::ForwardResp { req: 1, body: Bytes::from("response") },
        PeerMsg::ProxyOpen {
            stream: 9,
            route: 4,
            proto: 2,
            principal: Bytes::from("rol"),
            data: Bytes::from("GET /"),
        },
        PeerMsg::ProxyData { stream: 9, fin: true, data: Bytes::from("fin") },
        PeerMsg::ProxyClose { stream: 9, reason: "closed".into() },
        PeerMsg::NodeHeartbeat {
            node: NodeId(2),
            now_ms: 5,
            groups: 10,
            leaders: 3,
            bytes: 1 << 40,
            free_bytes: 1 << 30,
            total_bytes: 1 << 41,
            ops: 1_200,
        },
        PeerMsg::JoinRequest {
            name: "n4".into(),
            peer_addr: "a".into(),
            http_addr: "b".into(),
            native_addr: "c".into(),
            cores: 0,
        },
        PeerMsg::JoinResponse(Ok(JoinAccepted {
            node_id: NodeId(4),
            cluster_id: [1; 16],
            cores: 8,
            voters: vec![(NodeId(1), [2; 32]), (NodeId(2), [3; 32])],
            denied: vec![[4; 32]],
        })),
        PeerMsg::JoinResponse(Err("fingerprint denied".into())),
        PeerMsg::ClockReport { node: NodeId(2), offset_ms: -250 },
        PeerMsg::NodeSuspect { node: NodeId(3), suspect: true },
        PeerMsg::AuthLeaseAsk { node: NodeId(3), seq: 9, applied: 41 },
        PeerMsg::AuthLeaseGrant { node: NodeId(3), seq: 9, commit: 42, fenced: 40 },
        PeerMsg::EnrollRequest(EnrollRequest {
            token_id: [1; 16],
            secret: [2; 32],
            name: "n4".into(),
            public_key: vec![3; 91],
        }),
        PeerMsg::EnrollResponse(Ok(EnrollAccepted {
            certificate: vec![4; 300],
            ca: vec![5; 280],
            sealed: vec![6; 400],
        })),
        PeerMsg::EnrollResponse(Err("the token was used".into())),
        PeerMsg::Goodbye { reason: "shutting down".into() },
    ]
}

fn frame(m: &PeerMsg) -> Vec<u8> {
    let mut out = Vec::new();
    encode_frame(m, false, &mut out).unwrap();
    out
}

#[test]
fn every_kind_round_trips() {
    let mut kinds = std::collections::BTreeSet::new();
    for m in all() {
        let f = frame(&m);
        assert_eq!(u32::from_le_bytes(f[..4].try_into().unwrap()) as usize, f.len() - 4);
        assert_eq!(f[4], m.kind());
        // No flags; only a `Hello` uses the spare bytes, for its highest version.
        let spare = match &m {
            PeerMsg::Hello(h) => h.versions.max.to_le_bytes(),
            _ => [0, 0],
        };
        assert_eq!(&f[5..8], &[0, spare[0], spare[1]]);
        assert_eq!(decode_frame(&f), Ok(Some((m.clone(), f.len()))), "{m:?}");
        kinds.insert(m.kind());
    }
    assert_eq!(kinds.len(), 22);
    // A `RaftBatch` that does not fit a frame of 1 MiB travels in `RaftChunk` pieces (005 FR-023).
    kinds.insert(RAFT_CHUNK_KIND);
    // Every other kind is rejected as unknown: the decoder knows exactly these.
    for kind in 0..=u8::MAX {
        let unknown = decode_frame(&[4, 0, 0, 0, kind]) == Err(FrameError::UnknownKind(kind));
        assert_eq!(unknown, !kinds.contains(&kind), "kind {kind:#04x}");
    }
}

#[test]
fn incremental_reads_and_several_frames_in_a_buffer() {
    let msgs = all();
    let mut stream = Vec::new();
    for m in &msgs {
        encode_frame(m, false, &mut stream).unwrap();
    }
    // Arriving one byte at a time: nothing until each frame is complete.
    let mut buf = Vec::new();
    let mut got = Vec::new();
    for &b in &stream {
        buf.push(b);
        while let Some((m, n)) = decode_frame(&buf).unwrap() {
            got.push(m);
            buf.drain(..n);
        }
    }
    assert!(buf.is_empty());
    assert_eq!(got, msgs);
    assert_eq!(decode_frame(&[]), Ok(None));
    assert_eq!(decode_frame(&[8, 0, 0]), Ok(None));
}

#[test]
fn length_limits() {
    // An excessive length is rejected with only 4 bytes.
    let too = (MAX_FRAME_LEN as u32 + 1).to_le_bytes();
    assert_eq!(decode_frame(&too), Err(FrameError::TooLarge { len: MAX_FRAME_LEN as u64 + 1 }));
    assert_eq!(
        decode_frame(&u32::MAX.to_le_bytes()),
        Err(FrameError::TooLarge { len: u64::from(u32::MAX) })
    );
    assert_eq!(decode_frame(&[3, 0, 0, 0, 0x7f]), Err(FrameError::TooShort { len: 3 }));
    // Right at the limit: accepted.
    let max_data = MAX_FRAME_LEN - 4 - (8 * 5 + 4);
    let big = PeerMsg::SnapshotChunk {
        group: 1,
        index: 1,
        term: 1,
        offset: 0,
        total: 1,
        data: Bytes::from_vec(vec![7; max_data]),
    };
    let f = frame(&big);
    assert_eq!(f.len(), 4 + MAX_FRAME_LEN);
    assert_eq!(decode_frame(&f).unwrap().unwrap().1, f.len());
    // One more byte cannot even be written.
    let over = PeerMsg::SnapshotChunk {
        group: 1,
        index: 1,
        term: 1,
        offset: 0,
        total: 1,
        data: Bytes::from_vec(vec![7; max_data + 1]),
    };
    let mut out = Vec::new();
    assert_eq!(
        encode_frame(&over, false, &mut out),
        Err(FrameError::TooLarge { len: MAX_FRAME_LEN as u64 + 1 })
    );
    assert!(out.is_empty());
    // Compressed it does fit (the data repeats), and it comes back the same.
    encode_frame(&over, true, &mut out).unwrap();
    assert_eq!(out[5], FLAG_DEFLATE);
    assert!(out.len() < 1 << 20);
    assert_eq!(decode_frame(&out).unwrap().unwrap().0, over);
    // But not something incompressible above the limit.
    let mut x = 1u64;
    let noise: Vec<u8> = (0..MAX_FRAME_LEN)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let incompressible = PeerMsg::SnapshotChunk {
        group: 1,
        index: 1,
        term: 1,
        offset: 0,
        total: 1,
        data: Bytes::from_vec(noise),
    };
    assert!(matches!(
        encode_frame(&incompressible, true, &mut Vec::new()),
        Err(FrameError::TooLarge { .. })
    ));
}

#[test]
fn unknown_kinds_and_flags() {
    // Unknown: rejected as soon as the kind byte arrives.
    for kind in [
        0x00u8, 0x05, 0x0f, 0x10, 0x18, 0x19, 0x1b, 0x22, 0x35, 0x43, 0x4a, 0x60, 0x63, 0x66, 0x7e,
        0x80, 0xff,
    ] {
        assert_eq!(
            decode_frame(&[4, 0, 0, 0, kind]),
            Err(FrameError::UnknownKind(kind)),
            "{kind:#x}"
        );
    }
    // `reserved` is ignored; the flags are only valid to compress a SnapshotChunk.
    let mut f = frame(&PeerMsg::Goodbye { reason: "x".into() });
    f[6] = 0xaa;
    f[7] = 0x55;
    assert!(decode_frame(&f).unwrap().is_some());
    f[5] = FLAG_DEFLATE;
    assert_eq!(decode_frame(&f), Err(FrameError::BadFlags { kind: 0x7f, flags: 1 }));
    let chunk = all().into_iter().find(|m| m.kind() == 0x20).expect("a SnapshotChunk");
    let mut s = frame(&chunk);
    s[5] = 2;
    assert_eq!(decode_frame(&s), Err(FrameError::BadFlags { kind: 0x20, flags: 2 }));
    // Compressing another kind does nothing.
    let mut out = Vec::new();
    encode_frame(&PeerMsg::Goodbye { reason: "x".into() }, true, &mut out).unwrap();
    assert_eq!(out[5], 0);
}

#[test]
fn compressed_payloads_that_lie() {
    let snap = |data: Vec<u8>| {
        let mut f = ((data.len() + 4) as u32).to_le_bytes().to_vec();
        f.extend_from_slice(&[0x20, FLAG_DEFLATE, 0, 0]);
        f.extend_from_slice(&data);
        f
    };
    // Not DEFLATE.
    assert_eq!(decode_frame(&snap(vec![0xff; 20])), Err(FrameError::Inflate));
    // Bomb: 17 MiB of zeros compressed into little.
    let bomb = teseladb_deflate::deflate(&vec![0u8; MAX_FRAME_LEN + (1 << 20)], 9);
    assert!(bomb.len() < 100_000);
    assert_eq!(decode_frame(&snap(bomb)), Err(FrameError::Inflate));
    // With bytes after the stream.
    let chunk = all()
        .into_iter()
        .find(|m| matches!(m, PeerMsg::SnapshotChunk { data, .. } if !data.is_empty()))
        .unwrap();
    let mut payload = Vec::new();
    encode_frame(&chunk, true, &mut payload).unwrap();
    assert_eq!(payload[5], FLAG_DEFLATE);
    let mut body = payload[HEADER_LEN..].to_vec();
    body.push(0);
    assert_eq!(decode_frame(&snap(body)), Err(FrameError::Inflate));
    // Valid stream but with a payload that is not a SnapshotChunk.
    let junk = teseladb_deflate::deflate(b"short", 1);
    assert!(matches!(decode_frame(&snap(junk)), Err(FrameError::Malformed { kind: 0x20, .. })));
}

#[test]
fn damaged_payloads_never_panic() {
    for m in all() {
        let f = frame(&m);
        let payload = &f[HEADER_LEN..];
        let rebuild = |p: &[u8]| {
            let mut g = ((p.len() + 4) as u32).to_le_bytes().to_vec();
            g.extend_from_slice(&f[4..8]);
            g.extend_from_slice(p);
            g
        };
        // Truncated: always malformed.
        for n in 0..payload.len() {
            assert!(
                matches!(decode_frame(&rebuild(&payload[..n])), Err(FrameError::Malformed { .. })),
                "{m:?} truncated to {n}"
            );
        }
        // With extra bytes.
        let mut extra = payload.to_vec();
        extra.push(0);
        assert!(matches!(decode_frame(&rebuild(&extra)), Err(FrameError::Malformed { .. })));
        // Each altered byte: an error or a message that re-encodes the same.
        for i in 0..payload.len().min(400) {
            for x in [0x01u8, 0x80, 0xff] {
                let mut p = payload.to_vec();
                p[i] ^= x;
                let g = rebuild(&p);
                if let Ok(Some((back, n))) = decode_frame(&g) {
                    assert_eq!(n, g.len());
                    assert_eq!(frame(&back), g, "{m:?}: byte {i} ^ {x:#x}");
                }
            }
        }
    }
}

#[test]
fn a_hello_carries_the_range_of_versions_its_node_speaks() {
    let with = |min, max| PeerMsg::Hello(Hello { versions: Versions { min, max }, ..hello() });
    for (min, max) in [(4, 4), (4, 5), (5, 9)] {
        let m = with(min, max);
        let Ok(Some((got, _))) = decode_frame(&frame(&m)) else { panic!("{min} {max}") };
        assert_eq!(got, m);
    }
    // A node that only speaks 4 sends the version in the body and zeros in the header's spare
    // bytes, which reads as 4 to 4; and it still reads the body of an upgraded node's `Hello`.
    let mut old = frame(&with(4, 6));
    old[6..8].copy_from_slice(&[0, 0]);
    let Ok(Some((got, _))) = decode_frame(&old) else { panic!() };
    assert_eq!(got, with(4, 4));
    // A highest version below the lowest is no range.
    let mut f = frame(&with(5, 5));
    f[6..8].copy_from_slice(&4u16.to_le_bytes());
    assert!(matches!(decode_frame(&f), Err(FrameError::Malformed { kind: 0x01, .. })));
}

fn hello() -> Hello {
    let Some(PeerMsg::Hello(h)) = all().into_iter().next() else { panic!() };
    h
}

#[test]
fn specific_rejections() {
    let Ok(Some((PeerMsg::Hello(_), _))) = decode_frame(&frame(&all()[0])) else { panic!() };
    // Marker or version that are not ours.
    let mut f = frame(&all()[0]);
    f[HEADER_LEN] = b'X';
    assert!(matches!(decode_frame(&f), Err(FrameError::Malformed { kind: 0x01, .. })));
    let mut f = frame(&all()[0]);
    // A lowest version above the highest the header gives (versions themselves are judged by
    // the handshake, 007 FR-027).
    f[HEADER_LEN + 8] = f[HEADER_LEN + 8].wrapping_add(2);
    assert!(matches!(decode_frame(&f), Err(FrameError::Malformed { kind: 0x01, .. })));
    // A list claiming more elements than fit.
    let mut f = frame(&PeerMsg::Heartbeats { wall_ms: 7, hlc: HLC, beats: vec![] });
    // The count goes after the wall clock (8 bytes) and the HLC (12).
    f[HEADER_LEN + 20] = 100;
    assert!(matches!(decode_frame(&f), Err(FrameError::Malformed { kind: 0x03, .. })));
    // JoinResponse tag that does not exist.
    let mut f = frame(&PeerMsg::JoinResponse(Err("x".into())));
    f[HEADER_LEN] = 2;
    assert!(decode_frame(&f).is_err());
    // Readable errors.
    for e in [
        FrameError::TooLarge { len: 1 },
        FrameError::TooShort { len: 1 },
        FrameError::UnknownKind(5),
        FrameError::BadFlags { kind: 1, flags: 2 },
        FrameError::Inflate,
        FrameError::Malformed {
            kind: 1,
            err: teseladb_types::DecodeError::InvalidValue { offset: 3 },
        },
    ] {
        assert!(!e.to_string().is_empty());
    }
}

/// One mutation of `frame`: a bit flipped, a cut, a byte inserted, a byte rewritten or a length
/// field rewritten.
fn mutate(rng: &mut teseladb_types::Rng, frame: &[u8]) -> Vec<u8> {
    let mut out = frame.to_vec();
    for _ in 0..=rng.range(0, 3) {
        let len = out.len() as u64;
        match rng.range(0, 5) {
            0 if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] ^= 1 << rng.range(0, 8);
            }
            1 if len > 0 => out.resize(rng.range(0, len) as usize, 0),
            2 => {
                let i = rng.range(0, len + 1) as usize;
                out.insert(i, rng.range(0, 256) as u8);
            }
            3 if len >= 4 => {
                // A length or count somewhere: small, huge or zero.
                let i = rng.range(0, len - 3) as usize;
                let v = [0u32, 1, 0xffff_ffff, 0x7fff_ffff][rng.range(0, 4) as usize];
                out[i..i + 4].copy_from_slice(&v.to_le_bytes());
            }
            _ if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] = rng.range(0, 256) as u8;
            }
            _ => {}
        }
    }
    out
}

/// Fuzzing of the frame reader (013 T010, SC-003), which a node runs on bytes from peers that have
/// not shown a certificate yet (an enrolling node): every kind encoded, mutated and read back must
/// give a message, "more bytes" or an error, never a panic. By default a few seconds with fixed
/// seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with seeds that change every round, and
/// `TESELADB_FUZZ_SEED` replays one seed.
#[test]
fn frames_survive_mutated_bytes() {
    let mut frames = Vec::new();
    for msg in all() {
        for compress in [false, true] {
            let mut f = Vec::new();
            encode_frame(&msg, compress, &mut f).unwrap();
            frames.push(f);
        }
    }
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let start = std::time::Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_0e10);
    let mut rounds = 0u64;
    loop {
        let mut rng = teseladb_types::Rng::new(seed);
        for f in &frames {
            let input = mutate(&mut rng, f);
            if let Ok(Some((msg, _))) = decode_frame(&input) {
                let mut again = Vec::new();
                let _ = encode_frame(&msg, false, &mut again);
            }
            rounds += 1;
        }
        if replay.is_some() {
            break;
        }
        match secs {
            Some(s) if start.elapsed() < std::time::Duration::from_secs(s) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < 20_000 => seed += 1,
            None => break,
        }
    }
    eprintln!("fuzz: {rounds} frames in {:?}", start.elapsed());
}
