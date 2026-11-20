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
    /// Code 4: the LZ4 block format (FR-037), with the decompressed length before it.
    Lz4,
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
    /// Files in level 0 that send it down to level 1 whatever level 1 weighs; below them, level
    /// 0 lighter than level 1 joins its flushes in tiers of level 0 (FR-036). `None`: half of
    /// `l0_slow`, and at least `l0_trigger`.
    pub l0_down: Option<usize>,
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
    /// A value longer than this goes to a value file, and the file of its key keeps a reference
    /// (FR-034).
    pub value_separation_bytes: usize,
    /// Whether the data files store the documents of a shape of theirs packed (003 FR-024);
    /// `false` writes them whole, as the files written before.
    pub pack_documents: bool,
}

/// Default size of a dictionary: zstd's own default for trained dictionaries.
const DICT_BYTES: usize = 110 << 10;
/// Default size above which a value goes to a value file (FR-034).
const VALUE_SEPARATION_BYTES: usize = 4 << 10;
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
            l0_trigger: L0_TRIGGER,
            l0_down: Some(L0_DOWN),
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
            value_separation_bytes: VALUE_SEPARATION_BYTES,
            pack_documents: true,
        }
    }
}

/// Files in level 0 that start a compaction, by default: two. Each file of level 0 is one more
/// source for every scan, with its own version of the keys that are rewritten. In the one-node
/// load (002 runs p70c, p70t2 and p70t1, the same binary in the same hour) four files gave 3.0
/// files of level 0 for each scan and 909 million versions of keys already decided; two gave
/// 1.85 and 727 million, 2.8 % less CPU for each operation and the writes as they were, for 1.8
/// times the bytes written instead of 1.3; one joined every flush at once, wrote 37.5 times
/// the bytes and took twice the CPU.
pub const L0_TRIGGER: usize = 2;

/// Files in level 0 that send it down to level 1 whatever level 1 weighs, by default: three.
/// The tiers of level 0 are each one more source for every scan. In the one-node load (002 runs
/// p73e, p73d4 and p73d3, the same binary in the same hour) ten files, half of the slow-down,
/// gave 1.96 files of level 0 for each scan and 791 million versions of keys already decided;
/// four gave 1.42 and 679 million; three, 1.15 and 582 million, with 1.5 % less CPU for each
/// operation, the writes as they were and 14.7 kB written to disk for each operation instead of
/// 12.0 (the reference writes 17.7).
pub const L0_DOWN: usize = 3;

/// The environment variable that sets the level 0 files that start a compaction, for a diagnosis
/// run: fewer overlapping files mean fewer copies of a hot key for a scan to pass over (002 run
/// p42: 3.92 entries for each item a scan gave). Not an option of the node.
pub const L0_TRIGGER_ENV: &str = "TESELADB_L0_TRIGGER";

/// The environment variable that sets [`LsmConfig::l0_down`] for a diagnosis run: tiers of level
/// 0 are each one more source for every scan (002 runs p42-p43). Not an option of the node.
pub const L0_DOWN_ENV: &str = "TESELADB_L0_DOWN";

impl LsmConfig {
    /// The files in level 0 that send it down to level 1 ([`LsmConfig::l0_down`]).
    pub fn l0_down_files(&self) -> usize {
        self.l0_down.unwrap_or(self.l0_slow / 2).max(self.l0_trigger)
    }

    /// This configuration with [`LsmConfig::l0_down`] of [`L0_DOWN_ENV`]'s value `env`, when it is
    /// a whole number from 1 to below `l0_slow`; otherwise as it is.
    #[must_use]
    pub fn with_l0_down_from(mut self, env: Option<&str>) -> LsmConfig {
        let wanted = env.and_then(|v| v.trim().parse::<usize>().ok());
        if let Some(n) = wanted.filter(|n| (1..self.l0_slow).contains(n)) {
            self.l0_down = Some(n);
        }
        self
    }

    /// This configuration with the level 0 trigger of [`L0_TRIGGER_ENV`]'s value `env`, when it
    /// is a whole number from 1 to below `l0_slow`; otherwise as it is.
    #[must_use]
    pub fn with_l0_trigger_from(mut self, env: Option<&str>) -> LsmConfig {
        let wanted = env.and_then(|v| v.trim().parse::<usize>().ok());
        if let Some(n) = wanted.filter(|n| (1..self.l0_slow).contains(n)) {
            self.l0_trigger = n;
        }
        self
    }

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
    fn two_files_of_level_0_start_a_compaction_by_default() {
        let c = LsmConfig::default();
        assert_eq!(c.l0_trigger, 2);
        // Level 0 goes down to level 1 at three files, whatever level 1 weighs.
        assert_eq!(c.l0_down_files(), 3);
        // Without its own value, at half of the slow-down.
        assert_eq!(LsmConfig { l0_down: None, ..c }.l0_down_files(), 10);
    }

    #[test]
    fn levels_grow_tenfold() {
        let c = LsmConfig::default();
        assert_eq!(c.level_bytes(1), 64 << 20);
        assert_eq!(c.level_bytes(3), 6400 << 20);
        assert_eq!(LsmConfig { l1_bytes: u64::MAX, ..c }.level_bytes(4), u64::MAX);
    }

    #[test]
    fn the_level_0_trigger_changes_only_for_a_diagnosis_and_below_the_slow_down() {
        let c = LsmConfig::default();
        assert_eq!(c.clone().with_l0_trigger_from(None).l0_trigger, c.l0_trigger);
        assert_eq!(c.clone().with_l0_trigger_from(Some("2")).l0_trigger, 2);
        assert_eq!(c.clone().with_l0_trigger_from(Some(" 3 ")).l0_trigger, 3);
        // Zero, at or past the slow-down, or no number, leaves the default.
        for bad in ["0", "x", &c.l0_slow.to_string()] {
            assert_eq!(c.clone().with_l0_trigger_from(Some(bad)).l0_trigger, c.l0_trigger, "{bad}");
        }
    }
}
