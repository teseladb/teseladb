//! Registry of the simulator's workloads and checkers (002 FR-008, FR-010, data model §3): each
//! specification registers its own by name without touching the framework.

use crate::checkers::Violation;
use crate::history::History;
use crate::workloads::{
    Workload, indexes::Indexes, kv::KvWorkload, lsm::LsmWorkload, meta2::Meta2Workload,
    multirange::MultiRangeWorkload, range_docs::RangeDocsWorkload, relocate::RelocateWorkload,
    special_collections::SpecialCollections, splits::SplitsWorkload, storage::StorageWorkload,
    txn_append::Append, txn_bank::Bank,
};
use crate::world::World;

/// Builds a workload for `nodes` nodes of `cores` cores.
pub type WorkloadFactory = fn(nodes: u32, cores: u16) -> Box<dyn Workload>;

/// A registered checker.
#[derive(Clone, Copy, Debug)]
pub struct Checker {
    /// Unique name; it is the `checker` of the violations it reports.
    pub name: &'static str,
    /// Specification that adds it.
    pub owner: &'static str,
    /// Called after every event with the nodes' state.
    pub on_step: Option<fn(&World) -> Result<(), Violation>>,
    /// Called at the end with the clients' history, for workloads that have one.
    pub on_history: Option<fn(&History) -> Result<(), Violation>>,
}

/// Workloads and checkers by name.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    workloads: Vec<(&'static str, WorkloadFactory)>,
    checkers: Vec<Checker>,
}

impl Registry {
    /// The workloads of 001 (`storage`, `testkv`), 004 (`lsm`), 005 (`range-docs`), 007
    /// (`multirange`, `splits`, `split-merge`, `follower-reads`, `relocate`, `meta2`), 008 (`txn-append`), 009 (`indexes`) and 015 (`special-collections`), in the order `--workload all` runs them, and no
    /// extra checkers: the generic ones live in the workloads and the world.
    pub fn builtin() -> Registry {
        let mut r = Registry::default();
        r.register_workload("storage", StorageWorkload::boxed).expect("a new name");
        r.register_workload("testkv", KvWorkload::boxed).expect("a new name");
        r.register_workload("lsm", LsmWorkload::boxed).expect("a new name");
        r.register_workload("range-docs", RangeDocsWorkload::boxed).expect("a new name");
        r.register_workload("multirange", MultiRangeWorkload::boxed).expect("a new name");
        r.register_workload("splits", SplitsWorkload::boxed).expect("a new name");
        r.register_workload("split-merge", MultiRangeWorkload::split_merge).expect("a new name");
        r.register_workload("follower-reads", RangeDocsWorkload::follower_reads)
            .expect("a new name");
        r.register_workload("relocate", RelocateWorkload::boxed).expect("a new name");
        r.register_workload("meta2", Meta2Workload::boxed).expect("a new name");
        r.register_workload("txn-bank", Bank::boxed).expect("a new name");
        r.register_workload("txn-append", Append::boxed).expect("a new name");
        r.register_workload("indexes", Indexes::boxed).expect("a new name");
        r.register_workload("special-collections", SpecialCollections::boxed).expect("a new name");
        r
    }

    /// Registers a workload; a name registers once.
    pub fn register_workload(
        &mut self,
        name: &'static str,
        f: WorkloadFactory,
    ) -> Result<(), String> {
        if self.workloads.iter().any(|(n, _)| *n == name) {
            return Err(format!("workload {name} is already registered"));
        }
        self.workloads.push((name, f));
        Ok(())
    }

    /// Registers a checker; a name registers once.
    pub fn register_checker(&mut self, c: Checker) -> Result<(), String> {
        if self.checkers.iter().any(|x| x.name == c.name) {
            return Err(format!("checker {} is already registered", c.name));
        }
        self.checkers.push(c);
        Ok(())
    }

    /// A new workload by name.
    pub fn workload(&self, name: &str, nodes: u32, cores: u16) -> Option<Box<dyn Workload>> {
        self.workloads.iter().find(|(n, _)| *n == name).map(|(_, f)| f(nodes, cores))
    }

    /// Workload names, in registration order.
    pub fn workload_names(&self) -> Vec<&'static str> {
        self.workloads.iter().map(|(n, _)| *n).collect()
    }

    /// Checker names, in registration order.
    pub fn checker_names(&self) -> Vec<&'static str> {
        self.checkers.iter().map(|c| c.name).collect()
    }

    /// The registered checkers.
    pub fn checkers(&self) -> &[Checker] {
        &self.checkers
    }
}
