//! The HLC's bound in its own file, `hlc.bound` (005 FR-003, D-158): two slots of 512 bytes, each
//! in a sector of its own, with a sequence number and a CRC32C. A renewal writes the next slot in
//! place and syncs once; a write torn by a crash damages only the slot it wrote, and the other one
//! keeps the bound before it, which covered every mark given out until the new one was durable.

use std::path::PathBuf;

use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp, StorageError, write_atomic};
use teseladb_types::Bytes;

/// The file's name, in the root of the data directory.
pub const BOUND_FILE: &str = "hlc.bound";
/// Bytes of a slot.
const SLOT: usize = 512;
/// The first bytes of a valid slot.
const MAGIC: &[u8; 8] = b"TSLHLCB1";
/// The bytes of a slot its CRC covers: magic, sequence and bound.
const COVERED: usize = 24;
/// The identifier the file keeps open on the clock's disk; `node.meta` is written with
/// `FileId(u32::MAX)`.
const FILE: FileId = FileId(u32::MAX - 1);

/// Slot of write number `seq` and bound `bound`.
fn encode(seq: u64, bound: u64) -> [u8; SLOT] {
    let mut b = [0u8; SLOT];
    b[..8].copy_from_slice(MAGIC);
    b[8..16].copy_from_slice(&seq.to_le_bytes());
    b[16..24].copy_from_slice(&bound.to_le_bytes());
    let crc = teseladb_hash::crc32c(&b[..COVERED]);
    b[COVERED..COVERED + 4].copy_from_slice(&crc.to_le_bytes());
    b
}

/// The sequence and bound of a valid slot.
fn decode(slot: &[u8]) -> Option<(u64, u64)> {
    let word =
        |at: usize| slot.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes);
    let crc = slot.get(COVERED..COVERED + 4)?;
    let valid =
        slot.get(..8)? == MAGIC && teseladb_hash::crc32c(&slot[..COVERED]).to_le_bytes() == crc;
    valid.then_some((word(8)?, word(16)?))
}

/// The sequence and bound of the valid slot written last; `None` without the file or without a
/// valid slot.
///
/// # Errors
///
/// [`StorageError::Io`] when the file is there and cannot be read.
pub fn read_bound(disk: &mut dyn Disk) -> Result<Option<(u64, u64)>, StorageError> {
    let data = match disk.read_all(&PathBuf::from(BOUND_FILE)) {
        Ok(d) => d,
        Err(IoErrno::NotFound) => return Ok(None),
        Err(errno) => return Err(StorageError::Io { op: "read", errno }),
    };
    Ok(data.chunks(SLOT).filter_map(decode).max_by_key(|(seq, _)| *seq))
}

fn exec(disk: &mut dyn Disk, op: IoOp) -> Result<(), StorageError> {
    match disk.execute(op) {
        IoDone::Err { op, errno } => Err(StorageError::Io { op: op.name(), errno }),
        _ => Ok(()),
    }
}

/// `hlc.bound`, open for its renewals.
#[derive(Debug)]
pub struct BoundFile {
    /// The number of the last write.
    seq: u64,
}

impl BoundFile {
    /// Opens the file, creating it with `bound` when it has no valid slot; with the bound it
    /// holds then, the higher of its own and `bound` (the one of `node.meta`).
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] or [`StorageError::DiskFull`] when it cannot be read, made or opened.
    pub fn open(disk: &mut dyn Disk, bound: u64) -> Result<(BoundFile, u64), StorageError> {
        let path = PathBuf::from(BOUND_FILE);
        let found = read_bound(disk)?;
        let (seq, own) = match found {
            Some(slot) => slot,
            None => {
                let mut data = encode(1, bound).to_vec();
                data.extend_from_slice(&[0; SLOT]);
                write_atomic(disk, &path, data, FILE)?;
                (1, bound)
            }
        };
        exec(disk, IoOp::Open { file: FILE, path })?;
        let mut file = BoundFile { seq };
        // A `node.meta` above the file (written by a binary before D-158): the file takes it.
        if bound > own {
            file.write(disk, bound)?;
        }
        Ok((file, own.max(bound)))
    }

    /// Writes `bound` to the next slot and syncs it: once it returns, a restart waits past it.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] or [`StorageError::DiskFull`] when the write or the sync fails; the
    /// slot written before still holds the bound before.
    pub fn write(&mut self, disk: &mut dyn Disk, bound: u64) -> Result<(), StorageError> {
        let seq = self.seq + 1;
        let offset = (seq % 2) * SLOT as u64;
        let data = Bytes::from_vec(encode(seq, bound).to_vec());
        exec(disk, IoOp::WriteAt { file: FILE, offset, data, batch: 0 })?;
        exec(disk, IoOp::Sync { file: FILE, batch: 0 })?;
        self.seq = seq;
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_raftlog::MemDisk;

    fn slots(disk: &mut MemDisk) -> Vec<u8> {
        disk.read_all(&PathBuf::from(BOUND_FILE)).unwrap()
    }

    #[test]
    fn a_new_file_holds_the_bound_and_each_write_takes_the_other_slot() {
        let mut disk = MemDisk::new();
        assert_eq!(read_bound(&mut disk).unwrap(), None);
        let (mut f, held) = BoundFile::open(&mut disk, 5_000).unwrap();
        assert_eq!(held, 5_000);
        assert_eq!(read_bound(&mut disk).unwrap(), Some((1, 5_000)));
        f.write(&mut disk, 6_000).unwrap();
        f.write(&mut disk, 7_000).unwrap();
        assert_eq!(read_bound(&mut disk).unwrap(), Some((3, 7_000)));
        let data = slots(&mut disk);
        assert_eq!(data.len(), 2 * SLOT);
        assert_eq!(decode(&data[SLOT..]), Some((3, 7_000)));
        assert_eq!(decode(&data[..SLOT]), Some((2, 6_000)));
    }

    #[test]
    fn a_torn_slot_leaves_the_bound_of_the_other() {
        let mut disk = MemDisk::new();
        let (mut f, _) = BoundFile::open(&mut disk, 5_000).unwrap();
        f.write(&mut disk, 6_000).unwrap();
        // The next write, to slot 1, reaches the disk half written.
        let mut torn = encode(3, 9_000);
        torn[16] ^= 0xff;
        let data = Bytes::from_vec(torn.to_vec());
        exec(&mut disk, IoOp::WriteAt { file: FILE, offset: SLOT as u64, data, batch: 0 }).unwrap();
        assert_eq!(read_bound(&mut disk).unwrap(), Some((2, 6_000)));
    }

    #[test]
    fn an_open_file_keeps_the_higher_bound_and_moves_up_to_a_higher_one() {
        let mut disk = MemDisk::new();
        let (mut f, _) = BoundFile::open(&mut disk, 5_000).unwrap();
        f.write(&mut disk, 8_000).unwrap();
        // A node whose `node.meta` holds less: the file's bound stands.
        let (_, held) = BoundFile::open(&mut disk, 3_000).unwrap();
        assert_eq!(held, 8_000);
        assert_eq!(read_bound(&mut disk).unwrap(), Some((2, 8_000)));
        // One whose `node.meta` holds more (written by a binary before D-158): the file takes it.
        let (_, held) = BoundFile::open(&mut disk, 9_000).unwrap();
        assert_eq!(held, 9_000);
        assert_eq!(read_bound(&mut disk).unwrap(), Some((3, 9_000)));
    }

    #[test]
    fn a_file_without_a_valid_slot_holds_no_bound() {
        let mut disk = MemDisk::new();
        write_atomic(&mut disk, &PathBuf::from(BOUND_FILE), vec![7; 2 * SLOT], FILE).unwrap();
        assert_eq!(read_bound(&mut disk).unwrap(), None);
        assert_eq!(decode(&[0; 4]), None);
    }
}
