//! TeselaDB's per-core Multi-Raft engine (specification 001, contract `state-machine-api.md`),
//! sans-I/O: the Raft groups of one core over its log (`teseladb-raftlog`), with proposals and
//! their tracking, lease and ReadIndex reads, chunked snapshot transfer, configuration changes
//! with learners, grouped heartbeats and forwarded requests, for any [`StateMachine`]. It knows
//! nothing of what the state machines hold.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod chunks;
mod config;
mod core;
pub mod envelope;
mod error;
mod frame;
mod lease_clock;
pub mod log_store;
mod state_machine;
mod terms;

pub use crate::config::{
    CoreConfig, DEFAULT_TIMEOUT, GroupDesc, MAX_PENDING_PROPOSALS, NodeClock, ReadMode,
    SNAPSHOT_ENTRIES, SNAPSHOT_MIN_RATE, SNAPSHOT_TIMEOUT_BASE, snapshot_timeout,
};
pub use crate::core::{
    Actions, Done, Durable, Forwarded, GroupImport, MultiRaftCore, Observation, OpId, ProposeAt,
    first_request_id,
};
pub use crate::error::OpError;
pub use crate::frame::{
    GroupHeartbeat, GroupHeartbeatResp, PeerFrame, coalesce, expand_heartbeat_resps,
    expand_heartbeats, route_stray,
};
pub use crate::state_machine::{ApplyCtx, ApplyError, RestoreError, StateMachine};

/// Batches of a core's Raft log in flight at once (001 FR-004, D-159), for the stores of the server
/// and of the simulator: a write that reaches a core whose batch waits for its sync goes in the
/// drive's next flush instead of the one after (H-048 of the native tests).
pub const INFLIGHT_BATCHES: usize = 16;
