//! The three disk classes as the engine's scheduling groups (023 FR-012): the engine's I/O queue
//! takes the priority class of a request from the scheduling group it is submitted in, and shares
//! each device's bandwidth among the classes in proportion to their shares. The log's class goes
//! ahead, the data's (the clients' reads) behind it and the background's (flushes and compactions)
//! with the least, so the log does not wait behind the data nor a client's read behind a
//! compaction. The groups also account the CPU time of the operations' continuations.
//!
//! The groups belong to the engine that made them: a scheduling group has a task queue only on
//! the reactors of the engine that created it, and an engine stopped and started again in one
//! process (a node restarted in place, the tests) has new reactors with no queue for the groups
//! of the previous one. So what an engine knows of its groups lives in the thread-locals of its
//! own shards, and goes with them: the next engine makes its own, and a late caller of a
//! stopping engine can never hand its groups to the next one.
//!
//! The groups are not destroyed when their engine stops: tasks of a stopping node (an engine
//! thread parked on a timer) may still be scheduled in them until the last shard stops, and a
//! destroyed group's tasks have no queue. Their numbers stay taken, as in a process of the
//! reference, which runs one engine; a process that starts more engines than the engine has
//! groups for runs the disk classes of the later ones in the default group.

use std::cell::{Cell, RefCell};

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::{create_scheduling_group, internal};
use teseladb::engine::core::scheduling::{SchedulingGroup, default_scheduling_group};
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

/// The scheduling group of each disk class, the same on every shard of one engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoGroups {
    log: SchedulingGroup,
    data: SchedulingGroup,
    background: SchedulingGroup,
}

/// The shard that makes the groups, so that callers on several shards share one creation.
const CREATOR_SHARD: u32 = 0;

/// On [`CREATOR_SHARD`], where the groups of its engine are.
enum Creation {
    /// Not asked for yet.
    Absent,
    /// Being made: the callers that come meanwhile wait for it.
    Making(SharedPromise<IoGroups>),
    /// Made (or, with no group left, the default group for every class).
    Made(IoGroups),
}

thread_local! {
    /// On [`CREATOR_SHARD`], the groups of the engine whose shard this thread is.
    static CREATION: RefCell<Creation> = const { RefCell::new(Creation::Absent) };
    /// On every shard, the groups this shard already got from [`CREATOR_SHARD`].
    static KNOWN: Cell<Option<IoGroups>> = const { Cell::new(None) };
    /// Whether this shard's reactor forgets its groups when it is destroyed.
    static FORGET_HOOKED: Cell<bool> = const { Cell::new(false) };
}

impl IoGroups {
    /// The groups of the running engine, creating them on every shard the first time. Callers
    /// that come while they are being made, from any shard, wait for that one creation: a node's
    /// start asks from several tasks at once (the clock's keeper, the device log's writer and the
    /// cores' disks), and a second creation would make each group again under the same name,
    /// which the engine refuses when it registers the group's metrics a second time. When the
    /// engine has no scheduling groups left, every class runs in the default group.
    pub fn get_or_create() -> Future<IoGroups> {
        if let Some(g) = KNOWN.get() {
            return make_ready_future(g);
        }
        Smp::submit_to(CREATOR_SHARD, create_once).then(|g: IoGroups| {
            remember(g);
            Ok::<_, ExceptionPtr>(g)
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

/// Keeps `g` on the calling shard until its reactor goes: an engine run again on the same thread
/// (an application's run after another) starts from nothing.
fn remember(g: IoGroups) {
    KNOWN.set(Some(g));
    hook_forget();
}

/// Registers, once per reactor, forgetting this shard's groups when the reactor is destroyed.
fn hook_forget() {
    if FORGET_HOOKED.replace(true) {
        return;
    }
    internal::at_destroy(|| {
        KNOWN.set(None);
        CREATION.with(|c| *c.borrow_mut() = Creation::Absent);
        FORGET_HOOKED.set(false);
    });
}

/// On [`CREATOR_SHARD`]: the groups once made, the creation under way, or a new one that the
/// callers until it ends share.
fn create_once() -> Future<IoGroups> {
    let ready = CREATION.with(|c| match &*c.borrow() {
        Creation::Absent => None,
        Creation::Making(p) => Some(p.get_shared_future()),
        Creation::Made(g) => Some(make_ready_future(*g)),
    });
    if let Some(f) = ready {
        return f;
    }
    hook_forget();
    let promise = SharedPromise::new();
    let made = promise.get_shared_future();
    CREATION.with(|c| *c.borrow_mut() = Creation::Making(promise));
    drop(create().then_wrapped(|done: Future<IoGroups>| {
        let g = done.get().unwrap_or_else(|_| {
            let d = default_scheduling_group();
            IoGroups { log: d, data: d, background: d }
        });
        let ended = CREATION.with(|c| std::mem::replace(&mut *c.borrow_mut(), Creation::Made(g)));
        if let Creation::Making(mut promise) = ended {
            promise.set_value(g);
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
