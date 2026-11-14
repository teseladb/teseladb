//! TeselaDB's replicated document range (specification 005): its commands, state machine and
//! snapshots, on its leader the latches, the read cache, mutation evaluation, reads and write
//! acknowledgements (the host of a core), and the client that finds each range's leader.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod ack;
pub mod batch;
pub mod capped;
pub mod changes;
pub mod checksum;
pub mod client;
pub mod closed_ts;
pub mod command;
pub mod dedup;
pub mod doc_count;
pub mod fence;
pub mod gc;
pub mod host;
pub mod intents;
pub mod latch;
pub mod load;
pub mod local_ids;
pub mod local_keys;
pub mod locks;
pub mod metrics;
pub mod mutate;
pub mod onepc;
pub mod push;
pub mod read;
pub mod relocate;
pub mod rollback;
pub mod sm;
pub mod snapshot;
pub mod throttle;
pub mod tscache;
pub mod view;

/// First byte of the stored value of a version that only serves the history of its key (014): no
/// scan returns it and it is not live data.
pub use teseladb_lsm::HISTORY_VALUE;
