//! Range snapshots (FR-012, plan «Instantáneas de un rango»). The snapshot `teseladb-multiraft`
//! keeps carries only the range's local state; a replica without the data receives the export of
//! the range's two intervals (local keys and data) from the leader, as SST files cut into chunks
//! of at most 1 MiB, and ingests them with the snapshot's index as the range's flushed index.
//! The chunks of a file go in order and the files one after another; a gap, a chunk of another
//! snapshot or a file announced above [`MAX_FILE_BYTES`] is refused, and the sender starts over.
//! The export comes from any live replica that applied up to the snapshot (plan «Instantáneas de
//! un rango»).

use std::path::Path;

use teseladb_lsm::FileRef;
use teseladb_raftlog::{FileId, IoOp};
use teseladb_types::{Bytes, DecodeError, Decoder, Encoder, GroupId};

/// Largest chunk.
pub const CHUNK_BYTES: usize = 1 << 20;
/// Largest exported file a replica accepts (the LSM writes files of 16 MiB, 004 FR-012).
pub const MAX_FILE_BYTES: u64 = 1 << 30;

/// The interval of `range`'s local keys (space `00`, data model §1).
pub fn local_interval(range: GroupId) -> (Vec<u8>, Vec<u8>) {
    let start = [&[0u8][..], &range.0.to_be_bytes()].concat();
    let end = match range.0.checked_add(1) {
        Some(next) => [&[0u8][..], &next.to_be_bytes()].concat(),
        None => vec![1],
    };
    (start, end)
}

/// A piece of one exported file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Range.
    pub range: GroupId,
    /// Index of the snapshot.
    pub index: u64,
    /// File the piece belongs to.
    pub file: u32,
    /// Files of the snapshot.
    pub files: u32,
    /// Where the piece goes in its file.
    pub offset: u64,
    /// Size of its file.
    pub size: u64,
    /// The bytes.
    pub data: Vec<u8>,
}

impl Chunk {
    /// Encodes it for a frame.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u64(self.range.0).u64(self.index).u32(self.file).u32(self.files);
        e.u64(self.offset).u64(self.size).bytes(&self.data);
        e.finish()
    }

    /// Decodes a chunk written by [`Chunk::encode`].
    pub fn decode(b: &[u8]) -> Result<Chunk, DecodeError> {
        let mut d = Decoder::new(b);
        let chunk = Chunk {
            range: GroupId(d.u64()?),
            index: d.u64()?,
            file: d.u32()?,
            files: d.u32()?,
            offset: d.u64()?,
            size: d.u64()?,
            data: d.bytes()?.to_vec(),
        };
        d.finish()?;
        Ok(chunk)
    }
}

/// Cuts the files of the snapshot of `range` at `index` into chunks; an empty file is one empty
/// chunk, and no files at all is one empty chunk with `files` = 0.
pub fn split(range: GroupId, index: u64, files: &[Vec<u8>]) -> Vec<Chunk> {
    let n = files.len() as u32;
    let chunk = |file, offset: usize, size: usize, data: &[u8]| Chunk {
        range,
        index,
        file,
        files: n,
        offset: offset as u64,
        size: size as u64,
        data: data.to_vec(),
    };
    if files.is_empty() {
        return vec![chunk(0, 0, 0, &[])];
    }
    let mut out = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if f.is_empty() {
            out.push(chunk(i as u32, 0, 0, &[]));
        }
        for (k, piece) in f.chunks(CHUNK_BYTES).enumerate() {
            out.push(chunk(i as u32, k * CHUNK_BYTES, f.len(), piece));
        }
    }
    out
}

/// Why a chunk was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// It belongs to another range or another snapshot.
    Other,
    /// It does not continue where the previous one ended.
    Gap,
    /// Its file is announced above [`MAX_FILE_BYTES`].
    TooLarge,
}

/// Rebuilds the files of one snapshot from its chunks.
#[derive(Clone, Debug)]
pub struct Assembler {
    range: GroupId,
    index: u64,
    /// Files so far, each with the size its chunks announce.
    files: Vec<(Vec<u8>, u64)>,
}

impl Assembler {
    /// Waits for the snapshot of `range` at `index`.
    pub fn new(range: GroupId, index: u64) -> Assembler {
        Assembler { range, index, files: Vec::new() }
    }

    /// Takes the next chunk; once the last one arrives, every file.
    pub fn add(&mut self, c: Chunk) -> Result<Option<Vec<Vec<u8>>>, SnapshotError> {
        if c.range != self.range || c.index != self.index {
            return Err(SnapshotError::Other);
        }
        if c.files == 0 {
            return Ok(Some(Vec::new()));
        }
        if c.size > MAX_FILE_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        let continues = match self.files.last() {
            // The file being filled.
            Some((data, size)) if self.files.len() == c.file as usize + 1 => {
                data.len() as u64 == c.offset && *size == c.size
            }
            // The next file, once the previous one is whole.
            Some((data, size)) => {
                data.len() as u64 == *size && self.files.len() == c.file as usize && c.offset == 0
            }
            None => c.file == 0 && c.offset == 0,
        };
        if !continues || c.offset + c.data.len() as u64 > c.size {
            return Err(SnapshotError::Gap);
        }
        if self.files.len() == c.file as usize {
            self.files.push((Vec::new(), c.size));
        }
        let (data, size) = self.files.last_mut().expect("pushed above");
        data.extend_from_slice(&c.data);
        let whole = c.file + 1 == c.files && data.len() as u64 == *size;
        Ok(whole.then(|| std::mem::take(&mut self.files).into_iter().map(|(d, _)| d).collect()))
    }
}

/// The disk operations that leave `files` in `dir`, ready for `Lsm::ingest_range`, with their
/// references; their handles start at `first`.
pub fn write_ops(files: &[Vec<u8>], dir: &Path, first: FileId) -> (Vec<IoOp>, Vec<FileRef>) {
    let mut ops = Vec::new();
    let mut refs = Vec::new();
    for (i, data) in files.iter().enumerate() {
        let file = FileId(first.0 + i as u32);
        let path = dir.join(format!("snapshot-{i:06}.sst"));
        ops.push(IoOp::Create { file, path: path.clone(), prealloc: 0 });
        ops.push(IoOp::WriteAt { file, offset: 0, data: Bytes::from_vec(data.clone()), batch: 0 });
        ops.push(IoOp::Sync { file, batch: 0 });
        ops.push(IoOp::Close { file });
        refs.push(FileRef { path, size: data.len() as u64 });
    }
    (ops, refs)
}
