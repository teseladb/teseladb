//! Reads on the leader (FR-009, FR-007, DV-010). `linearizable` confirms the leadership with
//! ReadIndex; `majority`, `local` and `available` use the lease and fall back to ReadIndex
//! without it (`MultiRaftCore::read`). Once the core answers, the read at mark t checks the
//! latches of `[start, end)`: those of writes proposed at or below t, or every one for `local` and
//! `available`. A `local` or `available` read at a mark it chose reads instead just below the
//! writes in flight when none of them was answered yet ([`admit_own_read`], DV-010). An admitted read enters the read cache at t before it reads the LSM, so any write
//! proposed afterwards goes above it.

use teseladb_hlc::Hlc;
use teseladb_multiraft::ReadMode;

use crate::latch::{Latches, ReadWait, WaitFor};
use crate::tscache::TsCache;

/// RefDb's read concern of a read outside a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadConcern {
    /// `majority`.
    Majority,
    /// `linearizable`.
    Linearizable,
    /// `local`: data confirmed by the majority, with the writes already answered (DV-010).
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

    fn waits_for(self) -> WaitFor {
        match self {
            ReadConcern::Local | ReadConcern::Available => WaitFor::Every,
            ReadConcern::Majority | ReadConcern::Linearizable => WaitFor::AtOrBelow,
        }
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
    let wait = latches.read_wait(start, end, ts, concern.waits_for());
    // Faulty variant (005 T039): a `majority` or `linearizable` read does not wait for the writes in
    // flight below its mark; one applied later appears under a mark already read.
    #[cfg(teseladb_bug = "read_skips_inflight_write")]
    let wait = if concern.waits_for() == WaitFor::Every { wait } else { ReadWait::Free };
    if wait == ReadWait::Free {
        cache.record(start, end, ts);
    }
    wait
}

/// Admits a read of `[start, end)` at a mark `ts` the node chose: what it waits for and the mark
/// it reads at. A `local` or `available` read meets no wait when no write in flight on its keys
/// was answered yet, and reads just below the lowest of them (H-010 of the native tests: it
/// waited for each update of a hot key, 15,8 ms at p99 against 0,5 ms in a `refnode` 9.0.4); any
/// other read is [`admit_read`] at `ts`.
pub fn admit_own_read(
    latches: &Latches,
    cache: &mut TsCache,
    start: &[u8],
    end: &[u8],
    ts: Hlc,
    concern: ReadConcern,
) -> (ReadWait, Hlc) {
    let below = match concern {
        ReadConcern::Local | ReadConcern::Available => latches.own_read_mark(start, end, ts),
        ReadConcern::Majority | ReadConcern::Linearizable => None,
    };
    match below {
        Some(at) => {
            cache.record(start, end, ts);
            (ReadWait::Free, at)
        }
        None => (admit_read(latches, cache, start, end, ts, concern), ts),
    }
}
