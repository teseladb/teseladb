//! The class of a disk operation of a core (012 FR-023): the log's operations do not wait behind
//! the data's, and the core matches each result with an operation of its class.

/// Which queue of its core's executor a disk operation goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IoClass {
    /// The Raft log and the rollback files: what a write waits for before it is answered.
    Log,
    /// The LSM, exports, ingestions and the rest of the core's files.
    Data,
}

impl IoClass {
    /// Its position among the classes.
    pub fn index(self) -> usize {
        match self {
            IoClass::Log => 0,
            IoClass::Data => 1,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn each_class_has_its_own_queue() {
        assert_eq!((IoClass::Log.index(), IoClass::Data.index()), (0, 1));
    }
}
