//! Metrics of the LSM (FR-019, data-model §5).

use crate::lsm::Lsm;

/// A snapshot of the LSM's metrics.
#[derive(Clone, Debug, PartialEq)]
pub struct LsmMetrics {
    /// `lsm_level_bytes{level}`: bytes of the files of each level, level 0 first.
    pub level_bytes: Vec<u64>,
    /// `lsm_level_files{level}`: files of each level, level 0 first (write pressure counts those
    /// of level 0, FR-012).
    pub level_files: Vec<usize>,
    /// `lsm_write_amp`: bytes written to files per byte written by the callers.
    pub write_amp: f64,
    /// `lsm_read_files_per_get`: files searched per point read that finished.
    pub read_files_per_get: f64,
    /// Point reads that finished, and the files they searched (for the ratio over a window).
    pub gets_total: u64,
    /// Files searched by those point reads.
    pub get_files_total: u64,
    /// `lsm_cache_hit_ratio`: block lookups found in the cache.
    pub cache_hit_ratio: f64,
    /// `lsm_compactions_total`.
    pub compactions_total: u64,
    /// `lsm_stalls_total`: writes that came while the LSM asked to slow down.
    pub stalls_total: u64,
    /// `lsm_rejects_total`: writes that came while it asked to reject proposals.
    pub rejects_total: u64,
    /// Bytes in the block cache (pinned meta included).
    pub cache_bytes: u64,
    /// Bytes in memtables, frozen ones included.
    pub memtable_bytes: u64,
    /// Files in every level.
    pub files: usize,
    /// Data blocks written since the LSM opened, by compression code (`sst-format.md`).
    pub blocks_written: [u64; 4],
    /// Bytes of the data blocks written since the LSM opened, before and after compression
    /// (framed, without the padding to the page): the compression rate of 004 SC-006.
    pub block_bytes_written: [u64; 2],
    /// Dictionaries in the manifest.
    pub dictionaries: usize,
    /// `lsm_ingest_compactions_total`: compactions that made room for an ingestion (FR-024).
    pub ingest_compactions_total: u64,
    /// `lsm_ingest_compaction_bytes_total`: bytes those compactions read and wrote, out of their
    /// own budget.
    pub ingest_compaction_bytes_total: u64,
    /// The live bytes the embedder last told ([`Lsm::set_live_bytes`]; 0: never told).
    pub live_bytes: u64,
    /// Bytes of the files after the last compaction of everything that their space asked for.
    pub full_after: u64,
}

fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 { 0.0 } else { num as f64 / den as f64 }
}

impl Lsm {
    /// The current metrics.
    pub fn metrics(&self) -> LsmMetrics {
        let v = &self.version;
        let r = &self.reader;
        let c = &self.counters;
        LsmMetrics {
            level_bytes: v.levels.iter().map(|l| l.iter().map(|t| t.size).sum()).collect(),
            level_files: v.levels.iter().map(Vec::len).collect(),
            write_amp: ratio(c.written_bytes, c.user_bytes),
            read_files_per_get: ratio(r.probes, r.gets),
            gets_total: r.gets,
            get_files_total: r.probes,
            cache_hit_ratio: ratio(r.cache.hits, r.cache.hits + r.cache.misses),
            compactions_total: c.compactions,
            stalls_total: c.stalls,
            rejects_total: c.rejects,
            cache_bytes: r.cache.used(),
            memtable_bytes: v.memtables.iter().map(|m| m.bytes()).sum(),
            files: v.tables().count(),
            blocks_written: c.blocks,
            block_bytes_written: c.block_bytes,
            dictionaries: self.state.dictionaries.len(),
            ingest_compactions_total: c.ingest_compactions,
            ingest_compaction_bytes_total: c.ingest_compaction_bytes,
            live_bytes: self.live_bytes.unwrap_or(0),
            full_after: self.full_after,
        }
    }

    /// The dictionaries in the manifest: the prefix of their collection and their version, in
    /// the order of the versions.
    pub fn dictionaries(&self) -> Vec<(Vec<u8>, u64)> {
        self.state.dictionaries.values().map(|d| (d.prefix.clone(), d.version)).collect()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_ratio_of_nothing_is_zero() {
        assert_eq!(ratio(5, 0), 0.0);
        assert_eq!(ratio(3, 4), 0.75);
    }
}
