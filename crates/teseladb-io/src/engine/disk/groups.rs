//! The three disk classes as the engine's scheduling groups (023 FR-012): the engine's I/O queue
//! takes the priority class of a request from the scheduling group it is submitted in, and shares
//! each device's bandwidth among the classes in proportion to their shares. The log's class goes
//! ahead, the data's (the clients' reads) behind it and the background's (flushes and compactions)
//! with the least, so the log does not wait behind the data nor a client's read behind a
//! compaction. The groups also account the CPU time of the operations' continuations.
//!
//! The groups belong to the engine that runs: a scheduling group exists on the shards of the
//! engine that created it, and its number stays taken until it is destroyed. They are made once
//! per engine and destroyed before it stops ([`IoGroups::release`], which the node's engine
//! does), so that the next engine of the process makes its own.

use std::sync::{Mutex, PoisonError};

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::{create_scheduling_group, destroy_scheduling_group};
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::sstring::Sstring;

use crate::IoClass;

/// Shares of the log's class.
pub const LOG_SHARES: f32 = 1000.0;
/// Shares of the data's class.
pub const DATA_SHARES: f32 = 400.0;
/// Shares of the background's class.
pub const BACKGROUND_SHARES: f32 = 150.0;

/// The scheduling group of each disk class, the same on every shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoGroups {
    log: SchedulingGroup,
    data: SchedulingGroup,
    background: SchedulingGroup,
}

/// The groups of the running engine, made once: the engine has a fixed number of scheduling
/// groups.
static GROUPS: Mutex<Option<IoGroups>> = Mutex::new(None);

fn current() -> Option<IoGroups> {
    *GROUPS.lock().unwrap_or_else(PoisonError::into_inner)
}

impl IoGroups {
    /// The groups of the running engine, creating them on every shard the first time. Call it
    /// from one shard at a time, before the shards open their disks (the node does on shard 0 at
    /// start).
    pub fn get_or_create() -> Future<IoGroups> {
        if let Some(g) = current() {
            return make_ready_future(g);
        }
        let make = |name: &str, shares| {
            create_scheduling_group(Sstring::from(name), Sstring::from(name), shares)
        };
        make("disk-log", LOG_SHARES).then(move |log: SchedulingGroup| {
            make("disk-data", DATA_SHARES).then(move |data: SchedulingGroup| {
                make("disk-background", BACKGROUND_SHARES).then(
                    move |background: SchedulingGroup| {
                        let mut g = GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
                        Ok::<_, ExceptionPtr>(*g.get_or_insert(IoGroups { log, data, background }))
                    },
                )
            })
        })
    }

    /// Destroys the groups of the running engine, if it made them, once nothing runs in them:
    /// on a shard, before the engine stops.
    pub fn release() -> Future<()> {
        let Some(g) = GROUPS.lock().unwrap_or_else(PoisonError::into_inner).take() else {
            return make_ready_future(());
        };
        destroy_scheduling_group(g.background).then(move |()| {
            destroy_scheduling_group(g.data).then(move |()| destroy_scheduling_group(g.log))
        })
    }

    /// The scheduling group of `class`.
    #[must_use]
    pub fn of(&self, class: IoClass) -> SchedulingGroup {
        match class {
            IoClass::Log => self.log,
            IoClass::Data => self.data,
            IoClass::Background => self.background,
        }
    }
}
