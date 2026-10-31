//! Online index builds (FR-006): how many replicas of a range must hold the backfill before the
//! index is ready (`commitQuorum`), and the progress of the backfill as `currentOp` shows it
//! (measured on a `refnode` 9.0.4).

/// Largest number a `commitQuorum` may be (measured on 9.0.4).
pub const MAX_COMMIT_QUORUM: u32 = 50;
/// `desc` of an index build in `currentOp`, followed by the build's number (measured on 9.0.4).
pub const BUILD_DESC: &str = "IndexBuildsCoordinatorRefNode-";
/// What `currentOp` says a build is doing while it scans the collection (measured on 9.0.4).
const SCANNING: &str = "Index Build: scanning collection";

/// `commitQuorum`: the replicas of each range of the collection that must hold the backfill
/// before the index becomes ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitQuorum {
    /// `"votingMembers"`: every voter (RefDb's default).
    VotingMembers,
    /// `"majority"`.
    Majority,
    /// A number of replicas; 0 does not wait for any replica but the leader's quorum.
    Members(u32),
}

/// A `commitQuorum` above the voters of a range (`UnsatisfiableCommitQuorum`, 278).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsatisfiable;

impl CommitQuorum {
    /// The replicas of a range with `voters` voters that must hold each write of the backfill:
    /// the quorum asked, and never fewer than the range's majority, whose writes are the only
    /// ones a change of leader keeps.
    pub fn replicas(self, voters: usize) -> Result<usize, Unsatisfiable> {
        let majority = voters / 2 + 1;
        let asked = match self {
            CommitQuorum::VotingMembers => voters,
            CommitQuorum::Majority => majority,
            CommitQuorum::Members(n) if n as usize > voters => return Err(Unsatisfiable),
            CommitQuorum::Members(n) => n as usize,
        };
        Ok(asked.max(majority))
    }
}

/// How far the backfill went: documents done of those the collection had when it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Documents done.
    pub done: u64,
    /// Documents to do.
    pub total: u64,
}

impl Progress {
    /// The percentage done, rounded down (0 for an empty collection).
    pub fn percent(self) -> u64 {
        match self.total {
            0 => 0,
            total => self.done.saturating_mul(100) / total,
        }
    }

    /// `currentOp`'s `msg` of the build.
    pub fn message(self) -> String {
        format!("{SCANNING} {SCANNING}: {}/{} {}%", self.done, self.total, self.percent())
    }
}
