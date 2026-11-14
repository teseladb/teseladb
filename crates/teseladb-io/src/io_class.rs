//! The class of a disk operation of a core (012 FR-023): the log's operations do not wait behind
//! the data's, the clients' reads do not wait behind the LSM's background writes (012 FR-028),
//! and the core matches each result with an operation of its class.

/// Which queue of its core's executor a disk operation goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IoClass {
    /// The Raft log and the rollback files: what a write waits for before it is answered.
    Log,
    /// The LSM's reads for the clients, exports, ingestions and the rest of the core's files.
    Data,
    /// The LSM's background work (compactions and flushes) and the core changes: within the
    /// data's share of the disk, capped while a read of the data's class waits (012 FR-028).
    Background,
}

/// How many classes there are: the io_uring executor keeps a queue for each.
#[cfg(target_os = "linux")]
pub const CLASSES: usize = 3;

impl IoClass {
    /// Its position among the classes.
    pub fn index(self) -> usize {
        match self {
            IoClass::Log => 0,
            IoClass::Data => 1,
            IoClass::Background => 2,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn each_class_has_its_own_queue() {
        let all = [IoClass::Log, IoClass::Data, IoClass::Background];
        assert_eq!(all.map(IoClass::index), [0, 1, 2]);
    }
}
