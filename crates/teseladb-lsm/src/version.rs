//! Versions and views (FR-017).
//!
//! A version is the set of memtables and files at one moment: the active memtable first, then
//! the frozen ones from the newest, then level 0 from the newest file and the deeper levels, each
//! sorted by key without overlaps. A [`View`] holds a version with a reference count; a file
//! leaves the disk only when no version holds it any more.
//!
//! The order of the sources is the order of precedence when two of them have the same internal
//! key (a version applied again after a restart, or a `Retract` over its intent): the earlier
//! source wins. Data only moves down, and level 0 always goes to level 1 whole, so an older copy
//! is never above a newer one.

use std::sync::Arc;

use teseladb_keys::Hlc;

use crate::memtable::Memtable;
use crate::sst::reader::Table;

/// Memtables and files at one moment.
#[derive(Debug)]
pub(crate) struct Version {
    /// The active memtable, then the frozen ones from the newest.
    pub(crate) memtables: Vec<Arc<Memtable>>,
    /// Level 0 from the newest file, then levels 1 and down sorted by key.
    pub(crate) levels: Vec<Vec<Arc<Table>>>,
}

impl Version {
    /// An empty version with `levels` levels below level 0.
    pub(crate) fn empty(levels: usize) -> Version {
        Version {
            memtables: vec![Arc::new(Memtable::default())],
            levels: vec![Vec::new(); levels + 1],
        }
    }

    /// Every file in order of precedence.
    pub(crate) fn tables(&self) -> impl Iterator<Item = &Arc<Table>> {
        self.levels.iter().flatten()
    }

    /// Every file with its history file right after it (FR-031), in order of precedence: what a
    /// read of every version, a compaction or a count of bytes walks.
    pub(crate) fn files(&self) -> impl Iterator<Item = &Arc<Table>> {
        self.tables().flat_map(with_history)
    }

    /// The files a read at mark `read` looks at, in order of precedence: every file, and the
    /// history file of those whose highest mark is above the read's (FR-031). At or above that
    /// mark the read sees, of each key with versions in the history file, the value or the
    /// deletion its file has of it, or something newer, and none of the older ones.
    pub(crate) fn files_at(&self, read: Hlc) -> impl Iterator<Item = &Arc<Table>> {
        self.tables().flat_map(move |t| with_history_at(t, read))
    }

    /// The files that may hold versions of `key`, in order of precedence: every file of level 0
    /// whose keys cover it, then in each deeper level the file whose keys cover it, found by a
    /// binary search (a compaction cuts files only between keys; were a key's versions in two,
    /// both would come). A point read
    /// looking at every file of a large LSM spent its time comparing keys (H-051 of the native
    /// tests).
    pub(crate) fn tables_for<'a>(&'a self, key: &'a [u8]) -> impl Iterator<Item = &'a Arc<Table>> {
        let covers =
            move |t: &&Arc<Table>| t.props.lo.as_slice() <= key && key <= t.props.hi.as_slice();
        let level0 = self.levels.first().into_iter().flatten().filter(covers);
        let deeper = self.levels.iter().skip(1).flat_map(move |level| {
            let first = level.partition_point(|t| t.props.hi.as_slice() < key);
            level[first..].iter().take_while(move |t| t.props.lo.as_slice() <= key)
        });
        level0.chain(deeper)
    }

    /// Files of `level` that touch `[lo, hi]`.
    pub(crate) fn overlapping(&self, level: usize, lo: &[u8], hi: &[u8]) -> Vec<Arc<Table>> {
        self.levels[level]
            .iter()
            .filter(|t| t.props.lo.as_slice() <= hi && lo <= t.props.hi.as_slice())
            .cloned()
            .collect()
    }

    /// A copy with the same memtables and files.
    pub(crate) fn clone_shallow(&self) -> Version {
        Version { memtables: self.memtables.clone(), levels: self.levels.clone() }
    }
}

/// A file and its history file.
fn with_history(t: &Arc<Table>) -> impl Iterator<Item = &Arc<Table>> {
    std::iter::once(t).chain(t.history.as_ref())
}

/// A file and, for a read at `read` below its highest mark, its history file.
fn with_history_at(t: &Arc<Table>, read: Hlc) -> impl Iterator<Item = &Arc<Table>> {
    std::iter::once(t).chain(t.history.as_ref().filter(|_| read < t.props.max_ts))
}

/// A fixed view for reads: what was there when it was taken stays readable while it lives.
#[derive(Clone, Debug)]
pub struct View {
    pub(crate) version: Arc<Version>,
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_empty_version_has_one_memtable_and_no_files() {
        let v = Version::empty(6);
        assert_eq!(v.memtables.len(), 1);
        assert_eq!(v.levels.len(), 7);
        assert_eq!(v.tables().count(), 0);
        assert!(v.overlapping(1, b"a", b"z").is_empty());
        let c = v.clone_shallow();
        assert!(Arc::ptr_eq(&c.memtables[0], &v.memtables[0]));
    }
}
