//! TeselaDB's distributed transactions (specification 008): the coordinator that writes a
//! transaction's intents in its ranges, keeps its record alive, commits or aborts it and resolves
//! its intents, as a sans-I/O state machine its embedder drives.
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod causal;
pub mod commit_retry;
pub mod coordinator;
pub mod push_cache;
pub mod read;
pub mod reaper;
pub mod record;
pub mod session_txn;
pub mod uncertainty;
