//! Raft without I/O: one [`RawNode`] per group receives
//! events (`tick`, `step`, `propose`, `read_index`) and returns batches of actions ([`Ready`]) the
//! caller executes: persist the state and the entries, send messages (those in
//! `messages_after_persist` only after persisting) and apply what is committed. It does not read
//! the clock or generate randomness: the caller passes `now` and an `Rng`.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod conf;
mod config;
mod log;
mod message;
mod progress;
mod raft;
mod ready;

pub use conf::{ConfChange, ConfChangeKind, ConfState};
pub use config::{MAX_ENTRY_BYTES, RaftConfig};
pub use log::{LogGap, LogStore, MemLogStore};
pub use message::{
    DecodeError, Entry, EntryKind, HardState, Message, MessageType, Snapshot, SnapshotMeta,
};
pub use raft::{ProposalId, ProposeError, RawNode, Role, StepError};
pub use ready::{ReadState, Ready, SoftState, Status};
pub use teseladb_types::{GroupId, NodeId};
