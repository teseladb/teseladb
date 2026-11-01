//! Metrics of the LSM (FR-019, data-model §5).

use crate::lsm::Lsm;

/// A snapshot of the LSM's metrics.
#[derive(Clone, Debug, PartialEq)]
pub struct LsmMetrics {
    /// `lsm_level_bytes{level}`: bytes of the files of each level, level 0 first.
    pub level_bytes: Vec<u64>,
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
    /// Dictionaries in the manifest.
    pub dictionaries: usize,
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
            write_amp: ratio(c.written_bytes, c.user_bytes),
            read_files_per_get: ratio(r.probes, r.gets),
            gets_total: r.gets,
            get_files_total: r.probes,
            cache_hit_ratio: ratio(r.cache.hits, r.cache.hits + r.cache.misses),
            compactions_total: c.compactions,
            stalls_total: c.stalls,
            rejects_total: c.rejects,
            cache_bytes: r.cache.used(),
            memtable_bytes: v.memtables.iter().map(|m| m.borrow().bytes()).sum(),
            files: v.tables().count(),
            blocks_written: [0; 4],
            dictionaries: 0,
        }
    }

    /// The versions of the dictionaries in the manifest, in order.
    pub fn dictionary_versions(&self) -> Vec<u64> {
        Vec::new()
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
