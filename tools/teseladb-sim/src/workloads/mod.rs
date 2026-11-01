//! Workloads: `storage` (local logs with crashes), `testkv` (a Multi-Raft cluster under
//! compare-and-swap), `lsm` (each core's log with its LSM, 004), `lsm-ingest` (an ingestion under sustained writes, 004) and `range-docs` (replicated
//! document ranges, 005), `multirange`, `splits`, `relocate` and `meta2` (007) and `txn-bank` and `txn-append` (transactions
//! across two ranges, 008), `indexes` (documents and their index entries in two ranges, 009) and
//! `special-collections` (a capped collection, 015).

pub mod changestream;
pub mod data_ship;
pub mod indexes;
pub mod kv;
pub mod lsm;
pub mod lsm_ingest;
pub mod meta2;
pub mod multirange;
pub mod queries;
pub mod range_docs;
pub mod relocate;
pub mod special_collections;
pub mod splits;
pub mod storage;
pub mod txn;
pub mod txn_append;
pub mod txn_bank;

use teseladb_raftlog::Completions;

use crate::checkers::Violation;
use crate::world::World;

/// Workload over the simulated world.
pub trait Workload {
    /// Name (the CLI one).
    fn name(&self) -> &'static str;
    /// The nodes run cluster cores (`World::cluster`).
    fn cluster(&self) -> bool;
    /// Limits of the nodes' clocks (research R3).
    fn clocks(&self) -> crate::clock::ClockLimits {
        crate::clock::ClockLimits::default()
    }
    /// Schedules the clients' first turns.
    fn start(&mut self, w: &mut World);
    /// A client's turn.
    fn on_client(&mut self, w: &mut World, client: u32);
    /// What a core completed.
    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions);
    /// A node is about to crash.
    fn on_crash(&mut self, w: &mut World, node: u32);
    /// A node has just recovered: durability checks.
    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation>;
    /// End of the fault window.
    fn on_heal(&mut self, w: &mut World);
    /// At the end (after the final crash and recovery of every node): liveness.
    fn finish(&mut self, w: &mut World) -> Result<(), Violation>;
    /// The clients' history, if the workload has clients.
    fn history(&self) -> Option<&crate::history::History> {
        None
    }
}
