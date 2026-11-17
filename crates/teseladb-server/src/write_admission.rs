//! Admission of a write by the memory it takes on its range's core: a write whose keys and values
//! the core would hold several times over (its request, its latches, its Raft entry, the memtable
//! and the files it flushes to) is refused before it starts when that is more than the core's free
//! memory, with the reference server's `ExceededMemoryLimit` (146), instead of running the core's
//! memory pool out, which aborts the process. A document of up to 16 MiB with its index entries
//! fits a core of the default size; one whose index entries take many times its size (dozens of
//! indexes on one long string) may not.

use teseladb_range::host::RangeError;
use teseladb_range::host::TxnOp;
use teseladb_range::intents::IntentOp;

use crate::data_wire::RemoteReq;

/// How many times a write's keys and values a core holds at its peak, measured with the heap
/// profiler on a document of 15 MiB with five indexes on it (lane d2-bigdoc, 17-11-2026): 862 MiB
/// over the shard's own data for 90 MiB of keys and values, without the in-memory disk of the
/// test (its request and the copies kept to forward and resolve it, the transaction's keys, the
/// latches, the Raft entry, the memtable, the flush and the compaction).
pub const FOOTPRINT_FACTOR: u64 = 10;

/// The bytes of keys and values a request writes, if it writes.
pub fn write_bytes(r: &RemoteReq) -> Option<u64> {
    let len = |b: &[u8]| b.len() as u64;
    match r {
        RemoteReq::Write { keys, mutation, .. } => {
            Some(keys.iter().map(|k| len(k)).sum::<u64>() + len(mutation))
        }
        RemoteReq::Txn {
            op: TxnOp::PutIntents { writes, .. } | TxnOp::OnePhase { writes, .. },
            ..
        } => Some(
            writes
                .iter()
                .map(|(k, op)| {
                    len(k)
                        + match op {
                            IntentOp::Put { value, .. } => len(value),
                            IntentOp::Delete => 0,
                        }
                })
                .sum(),
        ),
        _ => None,
    }
}

/// Whether request `r` may run on a core with `free` bytes of free memory (`None`: the core's
/// memory is not known, as off the engine's shards).
///
/// # Errors
///
/// [`RangeError::ExceededMemory`] with the bytes it would take and those free.
pub fn admit(r: &RemoteReq, free: Option<u64>) -> Result<(), RangeError> {
    let (Some(bytes), Some(free)) = (write_bytes(r), free) else { return Ok(()) };
    let needed = bytes.saturating_mul(FOOTPRINT_FACTOR);
    if needed > free {
        return Err(RangeError::ExceededMemory { needed, free });
    }
    Ok(())
}

/// The free memory of the calling core's pool, when it runs on one of the engine's shards.
pub fn core_free_memory() -> Option<u64> {
    let s = teseladb::engine::core::memory::stats();
    (s.total_memory() > 0).then(|| s.free_memory() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use teseladb_hlc::Hlc;
    use teseladb_range::ack::{W, WriteConcern};
    use teseladb_range::intents::TxnId;

    const AT: Hlc = Hlc { physical: 1, logical: 0 };

    fn intents(sizes: &[usize]) -> RemoteReq {
        let writes = sizes
            .iter()
            .map(|n| (vec![1; 8], IntentOp::Put { value_type: 3, value: vec![0; *n] }))
            .collect();
        let op = TxnOp::PutIntents { txn: TxnId(1), read_ts: AT, writes };
        RemoteReq::Txn { op, deadline: None }
    }

    /// A transaction's intents count their keys and values; a deletion its key.
    #[test]
    fn intents_count_their_keys_and_values() {
        assert_eq!(write_bytes(&intents(&[10, 20])), Some(16 + 30));
        let op = TxnOp::OnePhase {
            txn: TxnId(1),
            read_ts: AT,
            writes: vec![(vec![1; 5], IntentOp::Delete)],
            session: None,
        };
        assert_eq!(write_bytes(&RemoteReq::Txn { op, deadline: None }), Some(5));
        let w = RemoteReq::Write {
            keys: vec![vec![0; 4], vec![0; 6]],
            mutation: vec![0; 100],
            dedup: None,
            wc: WriteConcern { w: W::Majority, j: true },
            deadline: None,
            prepared: Default::default(),
        };
        assert_eq!(write_bytes(&w), Some(110));
        assert_eq!(write_bytes(&RemoteReq::Live), None);
    }

    /// A write that would take more than the core's free memory is refused with what it needs and
    /// what is free; one that fits, a request that writes nothing and a core of unknown memory
    /// go on.
    #[test]
    fn a_write_past_the_free_memory_is_refused_and_one_that_fits_goes_on() {
        let r = intents(&[1 << 20]);
        let needed = ((1 << 20) + 8) * FOOTPRINT_FACTOR;
        assert_eq!(admit(&r, Some(needed)), Ok(()));
        assert_eq!(
            admit(&r, Some(needed - 1)),
            Err(RangeError::ExceededMemory { needed, free: needed - 1 })
        );
        assert_eq!(admit(&r, None), Ok(()));
        assert_eq!(admit(&RemoteReq::Live, Some(0)), Ok(()));
    }

    /// A document of 15 MiB with five index entries as long as it fits a core of 2 GiB with half
    /// of it in use.
    #[test]
    fn a_document_of_15_mib_with_five_long_index_entries_fits_half_a_core_of_2_gib() {
        let r = intents(&[15 << 20; 6]);
        assert_eq!(admit(&r, Some(1 << 30)), Ok(()));
    }
}
