//! Reads on the leader (FR-009, FR-007, DV-010). `linearizable` confirms the leadership with
//! ReadIndex; `majority`, `local` and `available` use the lease and fall back to ReadIndex
//! without it (`MultiRaftCore::read`). Once the core answers, the read at mark t checks the
//! latches of `[start, end)`: those of writes proposed at or below t, or every one for `local` and
//! `available`. An admitted read enters the read cache at t before it reads the LSM, so any write
//! proposed afterwards goes above it.

use teseladb_hlc::Hlc;
use teseladb_multiraft::ReadMode;

use crate::latch::{Latches, ReadWait};
use crate::tscache::TsCache;

/// RefDb's read concern of a read outside a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadConcern {
    /// `majority`.
    Majority,
    /// `linearizable`.
    Linearizable,
    /// `local`: data confirmed by the majority, waiting for writes in flight (DV-010).
    Local,
    /// `available`: as `local` (DV-010).
    Available,
}

impl ReadConcern {
    /// How the core confirms the leadership for this read.
    pub fn mode(self) -> ReadMode {
        match self {
            ReadConcern::Linearizable => ReadMode::ReadIndex,
            ReadConcern::Majority | ReadConcern::Local | ReadConcern::Available => ReadMode::Lease,
        }
    }

    fn waits_for_every_latch(self) -> bool {
        matches!(self, ReadConcern::Local | ReadConcern::Available)
    }
}

/// Admits a read of `[start, end)` at `ts` once the core answered it: the write it has to wait
/// for, or `Free` after recording it in the read cache.
pub fn admit_read(
    latches: &Latches,
    cache: &mut TsCache,
    start: &[u8],
    end: &[u8],
    ts: Hlc,
    concern: ReadConcern,
) -> ReadWait {
    let wait = latches.read_wait(start, end, ts, concern.waits_for_every_latch());
    // Faulty variant (005 T039): a `majority` or `linearizable` read does not wait for the writes in
    // flight below its mark; one applied later appears under a mark already read.
    #[cfg(teseladb_bug = "read_skips_inflight_write")]
    let wait = if concern.waits_for_every_latch() { wait } else { ReadWait::Free };
    if wait == ReadWait::Free {
        cache.record(start, end, ts);
    }
    wait
}
