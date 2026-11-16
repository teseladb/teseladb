//! Shard identifiers.
//!
//! The runtime runs one reactor per core, and each such reactor thread is a *shard* numbered from
//! `0` to `smp::count - 1`. The number of the current thread's shard is kept in a thread-local
//! variable that the shard sets once at start-up; [`this_shard_id`] reads it. Threads that are not
//! shards (alien threads, helper threads) read `0` unless they set the slot themselves.

/// Number of a shard: the index of a reactor thread, from `0` to the shard count minus one.
pub type ShardId = u32;

/// Access to the storage behind [`this_shard_id`],
/// for the runtime that sets it.
pub mod internal {
    use super::ShardId;
    use std::cell::Cell;

    thread_local! {
        static G_THIS_SHARD_ID: Cell<ShardId> = const { Cell::new(0) };
    }

    /// Returns a pointer to the thread-local shard id of the current thread.
    ///
    /// The pointer is valid for as long as the calling thread lives and must not be sent to another
    /// thread. The reactor writes through it once when the shard starts.
    #[inline]
    pub fn this_shard_id_ptr() -> *mut ShardId {
        G_THIS_SHARD_ID.with(|c| c.as_ptr())
    }
}

/// Returns the id of the shard the calling thread runs.
///
/// A single thread-local load: cheap enough for hot paths.
#[inline]
pub fn this_shard_id() -> ShardId {
    // SAFETY: the pointer refers to the calling thread's own thread-local, which is alive for the
    // whole call and is only written by this same thread.
    unsafe { *internal::this_shard_id_ptr() }
}
