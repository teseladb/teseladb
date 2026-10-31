//! The node's memory (012 FR-013): `--memory-limit`, or three quarters of the memory the process
//! has by default (the lowest of the physical memory and its cgroup's limit), split evenly between
//! the cores and, in each core, between the parts that hold data in memory. Each part flushes or
//! drops what it holds when it reaches its share: the memtables freeze and flush to disk, the
//! block cache evicts. The split is published in the metrics.

use std::fmt::Write;

use teseladb_lsm::LsmConfig;

/// A part of a core's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Component {
    /// The LSM's memtables, the active one and those frozen while they flush.
    Memtables,
    /// The LSM's block cache (data, index and filter blocks).
    BlockCache,
    /// Search index segments kept in memory (011).
    SearchSegments,
    /// The batches of open cursors.
    Cursors,
    /// Sorts, groups and other blocking stages of aggregations.
    Aggregations,
}

/// Each part's share of a core's memory, in percent; they add up to 100.
pub const SHARES: [(Component, u64); 5] = [
    (Component::Memtables, 20),
    (Component::BlockCache, 50),
    (Component::SearchSegments, 10),
    (Component::Cursors, 10),
    (Component::Aggregations, 10),
];

impl Component {
    /// Its label in the metrics.
    pub fn label(self) -> &'static str {
        match self {
            Component::Memtables => "memtables",
            Component::BlockCache => "block_cache",
            Component::SearchSegments => "search_segments",
            Component::Cursors => "cursors",
            Component::Aggregations => "aggregations",
        }
    }
}

/// The memory of a node of `cores` cores, `total` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBudget {
    /// Bytes of the whole node.
    pub total: u64,
    /// Cores that share it.
    pub cores: u16,
}

impl MemoryBudget {
    /// The budget of `total` bytes over `cores` cores (at least one).
    pub fn new(total: u64, cores: u16) -> MemoryBudget {
        MemoryBudget { total, cores: cores.max(1) }
    }

    /// Bytes of each core.
    pub fn per_core(&self) -> u64 {
        self.total / u64::from(self.cores)
    }

    /// Bytes of part `c` in each core.
    pub fn part(&self, c: Component) -> u64 {
        let pct = SHARES.iter().find(|(k, _)| *k == c).map_or(0, |(_, p)| *p);
        let core = self.per_core();
        core / 100 * pct + core % 100 * pct / 100
    }

    /// `base` with the memtables and the block cache in their parts, never larger than `base`
    /// makes them: the block cache takes its part, and the memtables (the active one and up to
    /// `max_frozen` flushing) share theirs.
    pub fn lsm(&self, base: &LsmConfig) -> LsmConfig {
        let tables = base.max_frozen as u64 + 1;
        let memtable = (self.part(Component::Memtables) / tables).max(1).min(base.memtable_bytes);
        LsmConfig {
            memtable_bytes: memtable,
            cache_bytes: self.part(Component::BlockCache).min(base.cache_bytes),
            ..base.clone()
        }
    }

    /// The split in Prometheus' text format.
    pub fn metrics(&self, out: &mut String) {
        let _ = writeln!(out, "# HELP teseladb_memory_limit_bytes Memory the node may use.");
        let _ = writeln!(out, "# TYPE teseladb_memory_limit_bytes gauge");
        let _ = writeln!(out, "teseladb_memory_limit_bytes {}", self.total);
        let _ = writeln!(
            out,
            "# HELP teseladb_memory_part_bytes Memory of each part of each core of the node."
        );
        let _ = writeln!(out, "# TYPE teseladb_memory_part_bytes gauge");
        for core in 0..self.cores {
            for (c, _) in SHARES {
                let _ = writeln!(
                    out,
                    "teseladb_memory_part_bytes{{core=\"{core}\",part=\"{}\"}} {}",
                    c.label(),
                    self.part(c)
                );
            }
        }
    }
}

/// A size as `--memory-limit` takes it: bytes, or a number with `K`, `M`, `G` or `T` (powers of
/// 1024, optionally followed by `B` or `iB`, in any case).
pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (number, unit) = t.split_at(digits);
    let n: u64 = number.parse().map_err(|_| format!("{text:?} is not a size"))?;
    let unit = unit.to_ascii_uppercase();
    let unit = unit.strip_suffix("IB").or_else(|| unit.strip_suffix('B')).unwrap_or(&unit);
    let shift = match unit {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(format!("{text:?}: the unit is K, M, G or T")),
    };
    n.checked_mul(1 << shift).filter(|&b| b > 0).ok_or_else(|| format!("{text:?} is out of range"))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_shares_add_up_to_the_whole_core() {
        assert_eq!(SHARES.iter().map(|(_, p)| p).sum::<u64>(), 100);
        let b = MemoryBudget::new(8 << 30, 4);
        assert_eq!(b.per_core(), 2 << 30);
        assert_eq!(b.part(Component::BlockCache), 1 << 30);
        let sum: u64 = SHARES.iter().map(|(c, _)| b.part(*c)).sum();
        assert!(sum <= b.per_core() && b.per_core() - sum < 5, "{sum}");
        assert_eq!(MemoryBudget::new(10, 0).cores, 1, "at least one core");
    }

    #[test]
    fn the_lsm_takes_its_parts_and_a_memtable_keeps_its_size_at_most() {
        let base = LsmConfig { cache_bytes: u64::MAX, ..LsmConfig::default() };
        let big = MemoryBudget::new(64 << 30, 2).lsm(&base);
        assert_eq!(big.memtable_bytes, base.memtable_bytes, "never past the configured size");
        assert_eq!(big.cache_bytes, 16 << 30);
        let capped = LsmConfig { cache_bytes: 1 << 20, ..LsmConfig::default() };
        assert_eq!(MemoryBudget::new(64 << 30, 2).lsm(&capped).cache_bytes, 1 << 20);
        let small = MemoryBudget::new(256 << 20, 1);
        let lsm = small.lsm(&base);
        let tables = base.max_frozen as u64 + 1;
        assert_eq!(lsm.memtable_bytes, small.part(Component::Memtables) / tables);
        assert!(lsm.memtable_bytes * tables <= small.part(Component::Memtables));
        assert_eq!(lsm.cache_bytes, 128 << 20);
        assert_eq!(MemoryBudget::new(1, 1).lsm(&base).memtable_bytes, 1, "never zero");
    }

    #[test]
    fn the_metrics_give_the_limit_and_every_part_of_every_core() {
        let mut out = String::new();
        MemoryBudget::new(1 << 30, 2).metrics(&mut out);
        assert!(out.contains("teseladb_memory_limit_bytes 1073741824\n"), "{out}");
        assert!(out.contains("teseladb_memory_part_bytes{core=\"1\",part=\"cursors\"} 53687091\n"));
        assert_eq!(out.matches("teseladb_memory_part_bytes{").count(), 10);
    }

    #[test]
    fn sizes_read_with_their_units() {
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("8GiB"), Ok(8 << 30));
        assert_eq!(parse_size("2gb"), Ok(2 << 30));
        assert_eq!(parse_size("1t"), Ok(1 << 40));
        assert_eq!(parse_size("64k"), Ok(64 << 10));
        assert!(parse_size("").is_err());
        assert!(parse_size("G").is_err());
        assert!(parse_size("3X").unwrap_err().contains("K, M, G or T"));
        assert!(parse_size("0").is_err());
        assert!(parse_size("99999999999T").unwrap_err().contains("out of range"));
    }
}
