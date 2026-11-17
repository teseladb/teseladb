//! The files of the device log (012 FR-035, D-154): `raftlog/<seq>.log`, each a header block and
//! then flush records, one per write of the writer.
//!
//! ```text
//! segment header := magic(8) · version(u32) · 0(u32) · seq(u64) · crc32c(u32) · zeros to 4 KiB
//! flush record   := head · body · zeros up to the next 4 KiB
//! head           := magic(u32) · version(u16) · 0(u16) · flush(u64) · body_len(u32) · body_crc(u32)
//!                   · head_crc(u32) · 0(u32)
//! body           := frame*
//! frame          := kind(u8) · 0(u8) · core(u16) · len(u32) · segment(u64) · arg(u64) · data(len)
//!                   · zeros up to 8 bytes
//!                 | 6(u8) · byte(u8) · core(u16) · len(u32) · segment(u64) · arg(u64)
//! ```
//!
//! A frame addresses a WAL segment of one core: `CREATE` makes it (`arg`: its reserved length),
//! `WRITE` puts `data` at offset `arg`, `FILL` puts `len` copies of `byte` at offset `arg` without
//! carrying them, `TRUNCATE` cuts it to `arg` bytes and `REMOVE` deletes it. `BOUND` names no
//! segment: `arg` is the node's HLC bound in force (005 FR-003, D-172). The CRC of a record covers
//! its whole body, so a record is whole or not there. Integers are little-endian.
//!
//! Version 2 adds `FILL`: a core's WAL pads each batch to 4 KiB, and the padding no longer goes to
//! the disk byte by byte. Version 2 reads the files and records of version 1; a reader of version 1
//! takes a record of version 2 for the end of its log, so the data directory's format says which
//! version a node may open.

use teseladb_hash::crc32c;

/// Unit of the log's writes: every record starts on a block.
pub const BLOCK: u64 = 4096;
/// Directory of the device log under the data directory.
pub const DIR: &str = "raftlog";
const SEGMENT_MAGIC: &[u8; 8] = b"TDBRLOG\0";
const RECORD_MAGIC: u32 = 0x4746_4C54;
/// Version of the format it writes.
pub const VERSION: u16 = 2;
/// The oldest version it reads.
pub const FIRST_VERSION: u16 = 1;
/// Bytes of a record's head.
pub const HEAD: usize = 32;
/// Bytes of a frame before its data.
pub const FRAME_HEAD: usize = 24;

/// What a frame does to its WAL segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// The segment exists, with at least `arg` bytes.
    Create = 1,
    /// `data` at offset `arg`.
    Write = 2,
    /// The segment ends at `arg`.
    Truncate = 3,
    /// The segment is gone.
    Remove = 4,
    /// The node's HLC bound in force, `arg` milliseconds (005 FR-003, D-172); it names no
    /// segment (core and segment 0). Every record carries one once a bound was offered.
    Bound = 5,
    /// `len` copies of one byte at offset `arg`, not carried in the record (version 2).
    Fill = 6,
}

impl FrameKind {
    fn from_u8(b: u8) -> Option<FrameKind> {
        match b {
            1 => Some(FrameKind::Create),
            2 => Some(FrameKind::Write),
            3 => Some(FrameKind::Truncate),
            4 => Some(FrameKind::Remove),
            5 => Some(FrameKind::Bound),
            6 => Some(FrameKind::Fill),
            _ => None,
        }
    }
}

/// Whether this build reads files and records of `version`.
#[must_use]
pub fn reads_version(version: u32) -> bool {
    (u32::from(FIRST_VERSION)..=u32::from(VERSION)).contains(&version)
}

/// A frame read from a record: where its data starts within the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// What it does.
    pub kind: FrameKind,
    /// The core whose WAL segment it addresses.
    pub core: u16,
    /// The WAL segment.
    pub segment: u64,
    /// Offset, reserved length or new length, by kind.
    pub arg: u64,
    /// Bytes of data (only `WRITE` carries them), or the bytes a `FILL` puts.
    pub len: u32,
    /// Where the data starts within the body.
    pub data_at: usize,
    /// The byte a `FILL` repeats (0 for the other kinds).
    pub fill: u8,
}

/// Name of segment `seq` under the data directory.
#[must_use]
pub fn segment_path(seq: u64) -> std::path::PathBuf {
    std::path::Path::new(DIR).join(format!("{seq:016x}.log"))
}

/// `seq` of a segment from its path.
#[must_use]
pub fn segment_seq(path: &std::path::Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let hex = name.strip_suffix(".log")?;
    (hex.len() == 16).then(|| u64::from_str_radix(hex, 16).ok()).flatten()
}

/// The header block of segment `seq`.
#[must_use]
pub fn segment_header(seq: u64) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK as usize];
    b[..8].copy_from_slice(SEGMENT_MAGIC);
    b[8..12].copy_from_slice(&u32::from(VERSION).to_le_bytes());
    b[16..24].copy_from_slice(&seq.to_le_bytes());
    let crc = crc32c(&b[..24]);
    b[24..28].copy_from_slice(&crc.to_le_bytes());
    b
}

/// Whether `data` starts with the header of segment `seq`, zeros to the end of its block
/// included.
#[must_use]
pub fn header_ok(data: &[u8], seq: u64) -> bool {
    data.len() >= BLOCK as usize
        && &data[..8] == SEGMENT_MAGIC
        && reads_version(u32_at(data, 8))
        && u64_at(data, 16) == seq
        && u32_at(data, 24) == crc32c(&data[..24])
        && data[28..BLOCK as usize].iter().all(|b| *b == 0)
}

/// Appends a frame to `body`.
pub fn push_frame(
    body: &mut Vec<u8>,
    kind: FrameKind,
    core: u16,
    segment: u64,
    arg: u64,
    data: &[u8],
) {
    let len = u32::try_from(data.len()).expect("a frame's data is one write of a WAL batch");
    body.push(kind as u8);
    body.push(0);
    body.extend_from_slice(&core.to_le_bytes());
    body.extend_from_slice(&len.to_le_bytes());
    body.extend_from_slice(&segment.to_le_bytes());
    body.extend_from_slice(&arg.to_le_bytes());
    body.extend_from_slice(data);
    body.resize(body.len().next_multiple_of(8), 0);
}

/// Appends to `body` a `FILL` frame: `len` copies of `byte` at offset `arg` of the segment.
pub fn push_fill(body: &mut Vec<u8>, core: u16, segment: u64, arg: u64, len: u32, byte: u8) {
    body.push(FrameKind::Fill as u8);
    body.push(byte);
    body.extend_from_slice(&core.to_le_bytes());
    body.extend_from_slice(&len.to_le_bytes());
    body.extend_from_slice(&segment.to_le_bytes());
    body.extend_from_slice(&arg.to_le_bytes());
}

/// Bytes a frame of `len` bytes of data takes in a body.
#[must_use]
pub fn frame_len(len: usize) -> usize {
    (FRAME_HEAD + len).next_multiple_of(8)
}

/// The record of flush `flush` with `body`: head, body and padding to a block.
#[must_use]
pub fn record(flush: u64, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("a flush is far below 4 GiB");
    let total = (HEAD + body.len()).next_multiple_of(BLOCK as usize);
    let mut r = Vec::with_capacity(total);
    r.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
    r.extend_from_slice(&VERSION.to_le_bytes());
    r.extend_from_slice(&[0, 0]);
    r.extend_from_slice(&flush.to_le_bytes());
    r.extend_from_slice(&len.to_le_bytes());
    r.extend_from_slice(&crc32c(body).to_le_bytes());
    let head_crc = crc32c(&r);
    r.extend_from_slice(&head_crc.to_le_bytes());
    r.extend_from_slice(&[0; 4]);
    r.extend_from_slice(body);
    r.resize(total, 0);
    r
}

/// A record read at some offset: its flush, its body and its length on disk.
#[derive(Debug, PartialEq, Eq)]
pub struct Record<'a> {
    /// Its flush number.
    pub flush: u64,
    /// Its body.
    pub body: &'a [u8],
    /// Bytes it takes, padding included.
    pub len: usize,
}

/// The record at the start of `data`, if a whole one is there.
#[must_use]
pub fn read_record(data: &[u8]) -> Option<Record<'_>> {
    let head = data.get(..HEAD)?;
    if u32_at(head, 0) != RECORD_MAGIC
        || !reads_version(u32::from(u16::from_le_bytes([head[4], head[5]])))
        || u32_at(head, 24) != crc32c(&head[..24])
    {
        return None;
    }
    let body_len = u32_at(head, 16) as usize;
    let body = data.get(HEAD..HEAD.checked_add(body_len)?)?;
    if crc32c(body) != u32_at(head, 20) {
        return None;
    }
    Some(Record {
        flush: u64_at(head, 8),
        body,
        len: (HEAD + body_len).next_multiple_of(BLOCK as usize),
    })
}

/// The frames of a body, or `None` if one is malformed (a body whose CRC matched was written
/// whole, so this means a bug, not damage).
#[must_use]
pub fn frames(body: &[u8]) -> Option<Vec<Frame>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < body.len() {
        let head = body.get(pos..pos + FRAME_HEAD)?;
        let kind = FrameKind::from_u8(head[0])?;
        let len = u32_at(head, 4);
        let data_at = pos + FRAME_HEAD;
        let carried = if kind == FrameKind::Fill { 0 } else { len as usize };
        body.get(data_at..data_at + carried)?;
        out.push(Frame {
            kind,
            core: u16::from_le_bytes([head[2], head[3]]),
            segment: u64_at(head, 8),
            arg: u64_at(head, 16),
            len,
            data_at,
            fill: if kind == FrameKind::Fill { head[1] } else { 0 },
        });
        pos += frame_len(carried);
    }
    Some(out)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_record_reads_back_whole_and_not_when_damaged() {
        let mut body = Vec::new();
        push_frame(&mut body, FrameKind::Create, 3, 9, 8192, &[]);
        push_frame(&mut body, FrameKind::Write, 3, 9, 4096, b"hello");
        push_frame(&mut body, FrameKind::Remove, 1, 2, 0, &[]);
        let r = record(7, &body);
        assert_eq!(r.len() % BLOCK as usize, 0);
        let got = read_record(&r).unwrap();
        assert_eq!((got.flush, got.len, got.body), (7, r.len(), &body[..]));
        let f = frames(got.body).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(
            (f[1].kind, f[1].core, f[1].segment, f[1].arg, f[1].len),
            (FrameKind::Write, 3, 9, 4096, 5)
        );
        assert_eq!(&got.body[f[1].data_at..f[1].data_at + 5], b"hello");
        assert_eq!(f[2].kind, FrameKind::Remove);
        // Any damaged byte of the head or the body: no record.
        for at in [0, 9, 17, HEAD + 3] {
            let mut bad = r.clone();
            bad[at] ^= 1;
            assert_eq!(read_record(&bad), None, "{at}");
        }
        assert_eq!(read_record(&vec![0; BLOCK as usize]), None);
        assert_eq!(read_record(&r[..HEAD + 4]), None);
    }

    #[test]
    fn segment_names_and_headers() {
        let p = segment_path(26);
        assert_eq!(p, std::path::PathBuf::from("raftlog/000000000000001a.log"));
        assert_eq!(segment_seq(&p), Some(26));
        assert_eq!(segment_seq(std::path::Path::new("raftlog/1a.log")), None);
        assert_eq!(segment_seq(std::path::Path::new("raftlog/000000000000001a.tmp")), None);
        let h = segment_header(26);
        assert!(header_ok(&h, 26));
        assert!(!header_ok(&h, 27));
        for at in [3, 20, 26, 28, 4000] {
            let mut bad = h.clone();
            bad[at] ^= 1;
            assert!(!header_ok(&bad, 26), "{at}");
        }
        assert!(!header_ok(&h[..100], 26));
    }

    #[test]
    fn a_fill_frame_carries_no_data_and_version_1_still_reads() {
        let mut body = Vec::new();
        push_frame(&mut body, FrameKind::Write, 2, 5, 0, b"abc");
        push_fill(&mut body, 2, 5, 3, 4093, 0x5a);
        push_frame(&mut body, FrameKind::Truncate, 2, 5, 9, &[]);
        assert_eq!(body.len(), frame_len(3) + 2 * FRAME_HEAD);
        let f = frames(&body).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!((f[1].kind, f[1].arg, f[1].len, f[1].fill), (FrameKind::Fill, 3, 4093, 0x5a));
        assert_eq!((f[0].fill, f[2].kind), (0, FrameKind::Truncate));
        // A record and a header of version 1 (no `FILL`) read as before; a later version does not.
        let mut old = record(4, &body[..frame_len(3)]);
        old[4] = 1;
        let crc = crc32c(&old[..24]);
        old[24..28].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(read_record(&old).unwrap().flush, 4);
        let mut h = segment_header(3);
        h[8] = 1;
        let crc = crc32c(&h[..24]);
        h[24..28].copy_from_slice(&crc.to_le_bytes());
        assert!(header_ok(&h, 3));
        h[8] = 3;
        let crc = crc32c(&h[..24]);
        h[24..28].copy_from_slice(&crc.to_le_bytes());
        assert!(!header_ok(&h, 3));
        assert!(!reads_version(0) && reads_version(1) && reads_version(2) && !reads_version(3));
    }

    #[test]
    fn a_malformed_body_has_no_frames() {
        assert_eq!(frames(&[9; 24]), None);
        let mut body = Vec::new();
        push_frame(&mut body, FrameKind::Write, 0, 1, 0, b"abc");
        assert_eq!(frames(&body[..FRAME_HEAD + 1]), None);
        assert_eq!(frames(&[]), Some(Vec::new()));
        assert_eq!(frame_len(3), 32);
    }
}
