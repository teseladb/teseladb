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

use std::cell::RefCell;
use std::rc::Rc;

use crate::memtable::Memtable;
use crate::sst::reader::Table;

/// Memtables and files at one moment.
#[derive(Debug)]
pub(crate) struct Version {
    /// The active memtable, then the frozen ones from the newest.
    pub(crate) memtables: Vec<Rc<RefCell<Memtable>>>,
    /// Level 0 from the newest file, then levels 1 and down sorted by key.
    pub(crate) levels: Vec<Vec<Rc<Table>>>,
}

impl Version {
    /// An empty version with `levels` levels below level 0.
    pub(crate) fn empty(levels: usize) -> Version {
        Version {
            memtables: vec![Rc::new(RefCell::new(Memtable::default()))],
            levels: vec![Vec::new(); levels + 1],
        }
    }

    /// Every file in order of precedence.
    pub(crate) fn tables(&self) -> impl Iterator<Item = &Rc<Table>> {
        self.levels.iter().flatten()
    }

    /// Files of `level` that touch `[lo, hi]`.
    pub(crate) fn overlapping(&self, level: usize, lo: &[u8], hi: &[u8]) -> Vec<Rc<Table>> {
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

/// A fixed view for reads: what was there when it was taken stays readable while it lives.
#[derive(Clone, Debug)]
pub struct View {
    pub(crate) version: Rc<Version>,
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
        assert!(Rc::ptr_eq(&c.memtables[0], &v.memtables[0]));
    }
}
