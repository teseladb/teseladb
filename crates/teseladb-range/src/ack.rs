//! When a write is acknowledged to the client (FR-010, D-034, model `WriteConcernW1.tla`):
//! `w: 1` once the entry is in the leader's log, after its `fdatasync` with `j: true`;
//! `w: <n>` with n ≥ 2 once n replicas have it on disk, whatever `j` says (as RefDb with
//! `writeConcernMajorityJournalDefault`); `w: "majority"` once it is committed and applied on the
//! leader; `w: 0` never. A `w` above the number of replicas fails with
//! `UnsatisfiableWriteConcern`.

use teseladb_raft::Status;

/// RefDb's code of `UnsatisfiableWriteConcern`.
pub const UNSATISFIABLE_WRITE_CONCERN: i32 = 100;

/// The `w` of a write concern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum W {
    /// `w: 0`: no answer.
    Zero,
    /// `w: <n>` (`w: 0` is [`W::Zero`]).
    Nodes(u32),
    /// `w: "majority"`.
    Majority,
}

/// A write concern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteConcern {
    /// How many replicas.
    pub w: W,
    /// `j`.
    pub j: bool,
}

/// The point a write is acknowledged at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckWhen {
    /// Never (`w: 0`).
    Never,
    /// The entry is in the leader's log.
    LeaderLog,
    /// The entry is on the leader's disk.
    LeaderDisk,
    /// The entry is on the disk of this many replicas.
    Replicas(usize),
    /// The entry is committed and applied on the leader.
    Applied,
}

/// An entry of a range's log: the term of the leader that proposed it and its index. A write
/// answers with its entry, and a command of several statements confirms them with a barrier
/// (FR-036).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId {
    /// Term.
    pub term: u64,
    /// Index.
    pub index: u64,
}

/// `w` asks for more replicas than the range has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsatisfiable;

impl AckWhen {
    /// Whether this acknowledgement of a write that changes nothing holds once the leader has
    /// applied what it read (005 FR-026): what is applied is committed by a majority, so `w: 0`,
    /// `w: 1` without `j` and `w: "majority"` hold; the leader's own journal (`j` with `w: 1`) and
    /// a number of replicas still go through Raft.
    pub fn met_by_applied(self) -> bool {
        matches!(self, AckWhen::Never | AckWhen::LeaderLog | AckWhen::Applied)
    }
}

/// Whether `wc` holds for a write that changes nothing without an entry, whatever the range's
/// replicas (005 FR-030): [`AckWhen::met_by_applied`] of its acknowledgement point.
pub fn met_without_entry(wc: WriteConcern) -> bool {
    ack_when(wc, usize::MAX).is_ok_and(AckWhen::met_by_applied)
}

/// The acknowledgement point of `wc` on a range of `replicas` replicas.
pub fn ack_when(wc: WriteConcern, replicas: usize) -> Result<AckWhen, Unsatisfiable> {
    Ok(match wc.w {
        W::Zero | W::Nodes(0) => AckWhen::Never,
        W::Nodes(n) if n as usize > replicas => return Err(Unsatisfiable),
        W::Nodes(1) => {
            if wc.j {
                AckWhen::LeaderDisk
            } else {
                AckWhen::LeaderLog
            }
        }
        W::Nodes(n) => AckWhen::Replicas(n as usize),
        W::Majority => AckWhen::Applied,
    })
}

/// Whether the write of entry `index` is acknowledged at `when`, given the leader's status and
/// whether it applied the entry.
pub fn acked(when: AckWhen, index: u64, status: &Status, applied: bool) -> bool {
    match when {
        AckWhen::Never => false,
        AckWhen::LeaderLog => status.last_index >= index,
        AckWhen::LeaderDisk => status.persisted >= index,
        AckWhen::Replicas(n) => status.durable_replicas(index) >= n,
        AckWhen::Applied => applied,
    }
}
