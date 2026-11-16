//! The three disk classes as the engine's scheduling groups (023 FR-012): the engine's I/O queue
//! takes the priority class of a request from the scheduling group it is submitted in, and shares
//! each device's bandwidth among the classes in proportion to their shares. The log's class goes
//! ahead, the data's (the clients' reads) behind it and the background's (flushes and compactions)
//! with the least, so the log does not wait behind the data nor a client's read behind a
//! compaction. The groups also account the CPU time of the operations' continuations.

use std::sync::OnceLock;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::create_scheduling_group;
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::shared_future::SharedFuture;
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

/// The shard that creates the groups: every first call, from any shard, waits for its one
/// creation.
const CREATING_SHARD: u32 = 0;

thread_local! {
    /// On [`CREATING_SHARD`], the creation under way or done, which later calls share.
    static CREATION: std::cell::RefCell<Option<SharedFuture<IoGroups>>> =
        const { std::cell::RefCell::new(None) };
}

impl IoGroups {
    /// The groups of the process, creating them on every shard the first time. Callable from any
    /// shard at any time: the first calls, wherever they run, share one creation on
    /// [`CREATING_SHARD`]. Two creations would make two groups of each name, whose metrics the
    /// engine refuses to register twice (the node's cores, its log and its outlets open their
    /// disks at once on start).
    pub fn get_or_create() -> Future<IoGroups> {
        if let Some(g) = GROUPS.get() {
            return make_ready_future(*g);
        }
        Smp::submit_to(CREATING_SHARD, || {
            if let Some(g) = GROUPS.get() {
                return make_ready_future(*g);
            }
            CREATION.with(|c| {
                c.borrow_mut().get_or_insert_with(|| SharedFuture::new(Self::create())).get_future()
            })
        })
    }

    /// Creates the three groups on every shard; on [`CREATING_SHARD`] only, once.
    fn create() -> Future<IoGroups> {
        let make = |name: &str, shares| {
            create_scheduling_group(Sstring::from(name), Sstring::from(name), shares)
        };
        make("disk-log", LOG_SHARES).then(move |log: SchedulingGroup| {
            make("disk-data", DATA_SHARES).then(move |data: SchedulingGroup| {
                make("disk-background", BACKGROUND_SHARES).then(
                    move |background: SchedulingGroup| {
                        Ok::<_, ExceptionPtr>(*GROUPS.get_or_init(|| IoGroups {
                            log,
                            data,
                            background,
                        }))
                    },
                )
            })
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
