//! Configuration of a core's LSM (plan, «Presupuestos»; FR-002, FR-012, FR-013).

use std::path::PathBuf;

pub use teseladb_crypto::at_rest::Encryption;

/// How data blocks are compressed (004 FR-022, `sst-format.md` «Bloques»): the code each block
/// carries. A block that the codec does not shrink stays plain (code 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Codec {
    /// Code 0.
    #[default]
    None,
    /// Code 1.
    Zlib,
    /// Code 2.
    Zstd,
    /// Code 3: zstd with the dictionary of the block's collection, code 2 while the collection has
    /// none (a compaction trains it).
    ZstdDict,
}

/// Configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LsmConfig {
    /// Directory of the LSM (`<data>/core-NN/lsm`).
    pub dir: PathBuf,
    /// Budget of the active memtable before it is frozen.
    pub memtable_bytes: u64,
    /// Frozen memtables waiting for their flush before writes are asked to slow down.
    pub max_frozen: usize,
    /// Budget of the block cache (index and filter blocks count too).
    pub cache_bytes: u64,
    /// Target size of a data block (4 to 32 KiB).
    pub block_bytes: usize,
    /// Target size of a file written by a compaction.
    pub target_file_bytes: u64,
    /// Compression of data blocks.
    pub codec: Codec,
    /// Largest dictionary trained for a collection ([`Codec::ZstdDict`]).
    pub dict_bytes: usize,
    /// Bytes of a collection's versions that compactions write with its dictionary before one of
    /// them trains a newer one.
    pub dict_renew_bytes: u64,
    /// Files in level 0 that start a compaction.
    pub l0_trigger: usize,
    /// Files in level 0 that ask writers to slow down.
    pub l0_slow: usize,
    /// Files in level 0 that ask the leader to reject proposals.
    pub l0_reject: usize,
    /// Size of level 1, and the smallest budget of a level above the deepest one with data
    /// (which sets the others, each a `level_ratio` part of the one below).
    pub l1_bytes: u64,
    /// Ratio between levels.
    pub level_ratio: u64,
    /// Levels below level 0.
    pub levels: usize,
    /// I/O budget of compactions per second.
    pub compaction_bytes_per_sec: u64,
    /// I/O budget per second of the compactions an ingestion needs (FR-024), apart from the one
    /// of the other compactions.
    pub ingest_compaction_bytes_per_sec: u64,
    /// Largest write or read a flush or a compaction issues at once.
    pub io_chunk_bytes: usize,
    /// Manifest size that starts a rotation.
    pub manifest_rotate_bytes: u64,
    /// Encryption of the files (013 FR-015); `None` writes them in clear.
    pub encryption: Option<Encryption>,
}

/// Default size of a dictionary: zstd's own default for trained dictionaries.
const DICT_BYTES: usize = 110 << 10;
/// Default bytes written with a dictionary before it is renewed.
const DICT_RENEW_BYTES: u64 = 1 << 30;

impl Default for LsmConfig {
    fn default() -> LsmConfig {
        LsmConfig {
            dir: PathBuf::from("lsm"),
            memtable_bytes: 64 << 20,
            max_frozen: 4,
            cache_bytes: 256 << 20,
            block_bytes: 16 << 10,
            target_file_bytes: 16 << 20,
            codec: Codec::None,
            dict_bytes: DICT_BYTES,
            dict_renew_bytes: DICT_RENEW_BYTES,
            l0_trigger: 4,
            l0_slow: 20,
            l0_reject: 36,
            l1_bytes: 64 << 20,
            level_ratio: 10,
            levels: 6,
            compaction_bytes_per_sec: 64 << 20,
            ingest_compaction_bytes_per_sec: 64 << 20,
            io_chunk_bytes: 1 << 20,
            manifest_rotate_bytes: 64 << 20,
            encryption: None,
        }
    }
}

impl LsmConfig {
    /// Size budget of level `level` (1 and up).
    pub fn level_bytes(&self, level: usize) -> u64 {
        let mut b = self.l1_bytes;
        for _ in 1..level {
            b = b.saturating_mul(self.level_ratio);
        }
        b
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn levels_grow_tenfold() {
        let c = LsmConfig::default();
        assert_eq!(c.level_bytes(1), 64 << 20);
        assert_eq!(c.level_bytes(3), 6400 << 20);
        assert_eq!(LsmConfig { l1_bytes: u64::MAX, ..c }.level_bytes(4), u64::MAX);
    }
}
