//! The state machine a group replicates (contract `state-machine-api.md`): the engine applies the
//! committed entries to it in order, takes and restores its snapshots and knows nothing of what
//! it holds.

use core::fmt;

use teseladb_types::GroupId;

/// What `apply` knows about the entry besides its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyCtx {
    /// Group.
    pub group: GroupId,
    /// Index of the entry.
    pub index: u64,
    /// Term of the entry.
    pub term: u64,
    /// HLC mark the leader stamped the entry with (005 FR-018); it grows within a group.
    pub ts: teseladb_hlc::Hlc,
}

/// An entry this version cannot apply: the group stops instead of guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyError(pub String);

/// A snapshot this version cannot restore: the group stops instead of guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreError(pub String);

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entry cannot be applied: {}", self.0)
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "snapshot cannot be restored: {}", self.0)
    }
}

impl std::error::Error for ApplyError {}
impl std::error::Error for RestoreError {}

/// A deterministic state machine: the same state and the same entry give the same state and the
/// same output on every replica. It does not read the clock or generate randomness.
pub trait StateMachine {
    /// Result for whoever proposed the entry.
    type Output: Clone + fmt::Debug + PartialEq;

    /// Whether the state machine keeps its state where the log has to wait for it (005: a range
    /// over the core's LSM, whose memtables are not durable until flushed).
    const HOLDS_LOG: bool = false;

    /// With [`StateMachine::HOLDS_LOG`]: the highest index whose entries it still needs from the
    /// log (`None`: none). The core gives it to the log after applying and on every tick, before
    /// it takes a snapshot (004 FR-011).
    fn release_floor(&self) -> Option<u64> {
        None
    }

    /// How far above the cluster time of its election a leader of this group starts its read
    /// cache, in milliseconds (005 FR-005: a range's 250 ms; `None`: it keeps none). The core
    /// raises the node's clock to that minimum as it wins the election, before anything of the
    /// term goes out, so the voters that renew its lease carry it to the next leader (FR-003).
    fn read_cache_margin_ms(&self) -> Option<u64> {
        None
    }

    /// Applies a committed entry.
    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<Self::Output, ApplyError>;

    /// An entry with nothing for the state machine (a leader's `Noop` or a configuration change)
    /// was committed at `index`.
    fn skip(&mut self, index: u64);

    /// The whole state at [`StateMachine::applied_index`].
    fn snapshot(&self) -> Vec<u8>;

    /// Replaces the state with a snapshot.
    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError>;

    /// Whether the data snapshot `snap` needs is already on this node (005 FR-012: a range's data
    /// lives in the core's LSM and arrives by ingest). If not, the core keeps the snapshot, asks
    /// the layer above for the data with `Actions::snapshot_data` and installs it on
    /// `MultiRaftCore::snapshot_data_ready`.
    fn has_data_for(&self, snap: &[u8]) -> bool {
        let _ = snap;
        true
    }

    /// The lowest index a snapshot of it may have to leave this replica (0: any). The core takes
    /// a snapshot as soon as it applied past one it has below it, whatever
    /// `CoreConfig::snapshot_entries` says, and sends none below it to another replica: those
    /// wait for the new one (007: a range's data travels as it is when it is sent, so a snapshot
    /// from before the range's last split would arrive without the part that split gave away).
    fn snapshot_floor(&self) -> u64 {
        0
    }

    /// The index whose state a snapshot taken now holds: [`StateMachine::applied_index`], or more
    /// while the log replays entries whose effects the state machine already holds (007: a range
    /// after a restart holds what its LSM flushed). The core compares it, rather than the index of
    /// the snapshot's entry, with [`StateMachine::snapshot_floor`]: a snapshot taken during that
    /// replay holds the state past the floor even if its entry is below it.
    fn snapshot_state_index(&self) -> u64 {
        self.applied_index()
    }

    /// Whether this replica cannot use the log from where it is and waits for a snapshot (007: a
    /// replica added to a range born from a split, whose log lacks the data the split left in the
    /// parent's LSM). It takes no entries until one is installed: each append is answered with a
    /// failed snapshot transfer, so that the leader sends its snapshot once its log is compacted.
    fn needs_snapshot(&self) -> bool {
        false
    }

    /// The index at or below which the log no longer rebuilds this replica's state (0: the whole
    /// log does): data entered without an entry there, as a restore's ingest does (018). As the
    /// leader, the core sends a replica whose log ends below it a snapshot past it, never those
    /// entries, even while the log still holds them. It is at most
    /// [`StateMachine::snapshot_floor`], so that the snapshot sent holds that data.
    fn log_floor(&self) -> u64 {
        0
    }

    /// Whether it applies nothing more on this core (007 FR-011: a range that moved to another
    /// core). The core stops handing it committed entries; they stay in the log, which the layer
    /// above hands over with the group.
    fn halted(&self) -> bool {
        false
    }

    /// Index of the last entry applied or skipped (or of the restored snapshot).
    fn applied_index(&self) -> u64;
}
