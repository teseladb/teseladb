//! File names of the data directory (data-model §1).

use std::path::{Path, PathBuf};

use teseladb_types::GroupId;

/// Directory of a core.
pub fn core_dir(core: u16) -> PathBuf {
    PathBuf::from(format!("core-{core:02}"))
}

/// WAL directory of a core.
pub fn wal_dir(core: u16) -> PathBuf {
    core_dir(core).join("wal")
}

/// Snapshot directory of a core.
pub fn snap_dir(core: u16) -> PathBuf {
    core_dir(core).join("snap")
}

/// WAL segment.
pub fn wal_segment(core: u16, seq: u64) -> PathBuf {
    wal_dir(core).join(format!("{seq:016x}.wal"))
}

/// Group snapshot.
pub fn group_snapshot(core: u16, group: GroupId, index: u64) -> PathBuf {
    snap_dir(core).join(format!("g{:016x}-i{index:016x}.snap", group.0))
}

/// Temporary version of a file (deleted at start-up if left behind).
pub fn tmp(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// What a file of the `snap` directory is, from its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapFileName {
    /// Group snapshot.
    Group(GroupId, u64),
    /// Temporary.
    Tmp,
}

/// Classifies a file of the `snap` directory.
pub fn classify_snap(path: &Path) -> Option<SnapFileName> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".tmp") {
        return Some(SnapFileName::Tmp);
    }
    let stem = name.strip_suffix(".snap")?;
    let hex = |h: &str| u64::from_str_radix(h, 16).ok();
    let rest = stem.strip_prefix('g')?;
    let (g, i) = rest.split_once("-i")?;
    Some(SnapFileName::Group(GroupId(hex(g)?), hex(i)?))
}

/// `seq` of a WAL segment from its name.
pub fn classify_wal(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    u64::from_str_radix(name.strip_suffix(".wal")?, 16).ok()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn names_and_classification() {
        assert_eq!(core_dir(3), PathBuf::from("core-03"));
        assert_eq!(wal_segment(1, 10), PathBuf::from("core-01/wal/000000000000000a.wal"));
        assert_eq!(classify_wal(&wal_segment(1, 10)), Some(10));
        assert_eq!(classify_wal(Path::new("x.tmp")), None);
        assert_eq!(classify_wal(Path::new("zz.wal")), None);
        assert!(
            group_snapshot(0, GroupId(2), 9).ends_with("g0000000000000002-i0000000000000009.snap")
        );
        assert_eq!(snap_dir(0), PathBuf::from("core-00/snap"));
        assert_eq!(
            classify_snap(&group_snapshot(0, GroupId(2), 9)),
            Some(SnapFileName::Group(GroupId(2), 9))
        );
        assert_eq!(classify_snap(&tmp(&group_snapshot(0, GroupId(2), 9))), Some(SnapFileName::Tmp));
        for bad in ["x.snap", "gzz-i1.snap", "g1-x.snap", "a1-1.snap", "g1-i1", "g1-izz.snap"] {
            assert_eq!(classify_snap(Path::new(bad)), None, "{bad}");
        }
    }
}
