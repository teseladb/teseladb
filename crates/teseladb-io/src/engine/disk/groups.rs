//! The three disk classes as the engine's scheduling groups (023 FR-012): the engine's I/O queue
//! takes the priority class of a request from the scheduling group it is submitted in, and shares
//! each device's bandwidth among the classes in proportion to their shares. The log's class goes
//! ahead, the data's (the clients' reads) behind it and the background's (flushes and compactions)
//! with the least, so the log does not wait behind the data nor a client's read behind a
//! compaction. The groups also account the CPU time of the operations' continuations.

use std::cell::RefCell;
use std::sync::OnceLock;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::create_scheduling_group;
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::shared_future::SharedPromise;
use teseladb::engine::core::smp::Smp;
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

/// The groups of the process, made once: the engine has a fixed number of scheduling groups.
static GROUPS: OnceLock<IoGroups> = OnceLock::new();

/// The shard that makes the groups, so that callers on several shards share one creation.
const CREATOR_SHARD: u32 = 0;

thread_local! {
    /// On [`CREATOR_SHARD`], the creation under way: the callers that come meanwhile wait for it.
    static CREATING: RefCell<Option<SharedPromise<IoGroups>>> = const { RefCell::new(None) };
}

impl IoGroups {
    /// The groups of the process, creating them on every shard the first time. Callers that
    /// come while they are being made, from any shard, wait for that one creation: a node's
    /// start asks from several tasks at once (the clock's keeper, the device log's writer and
    /// the cores' disks), and a second creation would make each group again under the same name,
    /// which the engine refuses when it registers the group's metrics a second time.
    pub fn get_or_create() -> Future<IoGroups> {
        if let Some(g) = GROUPS.get() {
            return make_ready_future(*g);
        }
        Smp::submit_to(CREATOR_SHARD, create_once)
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

/// On [`CREATOR_SHARD`]: the groups once made, the creation under way, or a new one that the
/// callers until it ends share. A creation that fails is not kept: the next caller tries again.
fn create_once() -> Future<IoGroups> {
    if let Some(g) = GROUPS.get() {
        return make_ready_future(*g);
    }
    if let Some(waiting) =
        CREATING.with(|c| c.borrow().as_ref().map(SharedPromise::get_shared_future))
    {
        return waiting;
    }
    let promise = SharedPromise::new();
    let made = promise.get_shared_future();
    CREATING.with(|c| *c.borrow_mut() = Some(promise));
    drop(create().then_wrapped(|done: Future<IoGroups>| {
        let ended = CREATING.with(|c| c.borrow_mut().take());
        let mut promise = ended.expect("the creation under way has its promise");
        match done.get() {
            Ok(g) => promise.set_value(*GROUPS.get_or_init(|| g)),
            Err(e) => promise.set_exception(e),
        }
        make_ready_future(())
    }));
    made
}

/// Makes the three groups on every shard.
fn create() -> Future<IoGroups> {
    let make = |name: &str, shares| {
        create_scheduling_group(Sstring::from(name), Sstring::from(name), shares)
    };
    make("disk-log", LOG_SHARES).then(move |log: SchedulingGroup| {
        make("disk-data", DATA_SHARES).then(move |data: SchedulingGroup| {
            make("disk-background", BACKGROUND_SHARES).then(move |background: SchedulingGroup| {
                Ok::<_, ExceptionPtr>(IoGroups { log, data, background })
            })
        })
    })
}
