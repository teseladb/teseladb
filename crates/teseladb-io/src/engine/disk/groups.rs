//! The three disk classes as the engine's scheduling groups (023 FR-012): the engine's I/O queue
//! takes the priority class of a request from the scheduling group it is submitted in, and shares
//! each device's bandwidth among the classes in proportion to their shares. The log's class goes
//! ahead, the data's (the clients' reads) behind it and the background's (flushes and compactions)
//! with the least, so the log does not wait behind the data nor a client's read behind a
//! compaction. The groups also account the CPU time of the operations' continuations.
//!
//! The groups belong to the engine that made them, not to the process: a scheduling group has a
//! task queue on each reactor of its engine, and an engine stopped and started again in one
//! process (a node restarted, the tests) has new reactors with no queue for the groups of the
//! previous one. So the groups are made once per engine, and destroyed when that engine exits,
//! which also gives their ids back (the engine has a fixed number of them for the whole process).

use std::cell::{Cell, RefCell};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_exception_future, make_ready_future};
use teseladb::engine::core::reactor::{
    create_scheduling_group, destroy_scheduling_group, engine_logger, internal,
};
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::shared_future::SharedPromise;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::util::log::FormatInfo;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;

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

/// On [`CREATOR_SHARD`], where the groups of the running engine are.
enum Creation {
    /// Not asked for yet in this engine.
    Absent,
    /// Being made: the callers that come meanwhile wait for it.
    Making(SharedPromise<IoGroups>),
    /// Made, until the engine exits.
    Made(IoGroups),
    /// The engine is exiting and the groups are gone (or going): nothing makes them again.
    Gone,
}

thread_local! {
    /// On [`CREATOR_SHARD`], the groups of the engine whose shard this thread is.
    static CREATION: RefCell<Creation> = const { RefCell::new(Creation::Absent) };
    /// On [`CREATOR_SHARD`], whether the engine of this thread destroys the groups at its exit.
    static EXIT_HOOKED: Cell<bool> = const { Cell::new(false) };
    /// On every shard, the groups this shard already got from [`CREATOR_SHARD`]. Forgotten when
    /// the engine exits and again when the shard's reactor is destroyed, so that a later engine on
    /// the same thread never sees the groups of an earlier one.
    static KNOWN: Cell<Option<IoGroups>> = const { Cell::new(None) };
    /// Whether this shard's reactor forgets [`KNOWN`] when it is destroyed.
    static FORGET_HOOKED: Cell<bool> = const { Cell::new(false) };
}

impl IoGroups {
    /// The groups of the running engine, creating them on every shard the first time. Callers
    /// that come while they are being made, from any shard, wait for that one creation: a node's
    /// start asks from several tasks at once (the clock's keeper, the device log's writer and the
    /// cores' disks), and a second creation would make each group again under the same name,
    /// which the engine refuses when it registers the group's metrics a second time.
    ///
    /// Fails once the engine is exiting (the groups are destroyed then).
    pub fn get_or_create() -> Future<IoGroups> {
        if let Some(g) = KNOWN.get() {
            return make_ready_future(g);
        }
        Smp::submit_to(CREATOR_SHARD, create_once).then(|g: IoGroups| {
            remember(g);
            Ok::<_, ExceptionPtr>(g)
        })
    }

    /// Ties the groups to the life of the running engine before any is made: they are destroyed
    /// when it exits. Called on [`CREATOR_SHARD`] when the engine starts, while it is not stopping
    /// (the engine takes exit hooks only then). [`IoGroups::get_or_create`] ties them by itself
    /// when nothing did; this call only makes sure a creation that starts while the engine is
    /// already stopping finds the hook in place.
    pub fn tie_to_engine() {
        debug_assert_eq!(teseladb::engine::core::shard_id::this_shard_id(), CREATOR_SHARD);
        hook_exit();
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

    fn all(self) -> [SchedulingGroup; 3] {
        [self.log, self.data, self.background]
    }
}

/// Keeps `g` on the calling shard until its engine exits or its reactor goes.
fn remember(g: IoGroups) {
    KNOWN.set(Some(g));
    if !FORGET_HOOKED.replace(true) {
        internal::at_destroy(|| {
            KNOWN.set(None);
            FORGET_HOOKED.set(false);
        });
    }
}

/// Registers, once per engine, the destruction of the groups at the engine's exit.
fn hook_exit() {
    if EXIT_HOOKED.replace(true) {
        return;
    }
    internal::at_exit(NoncopyableFunction::<fn() -> Future<()>>::new(destroy_at_exit));
    // The next engine on this thread starts from nothing.
    internal::at_destroy(|| {
        CREATION.with(|c| *c.borrow_mut() = Creation::Absent);
        EXIT_HOOKED.set(false);
    });
}

/// On [`CREATOR_SHARD`]: the groups once made, the creation under way, or a new one that the
/// callers until it ends share. A creation that fails is not kept: the next caller tries again.
fn create_once() -> Future<IoGroups> {
    let waiting = CREATION.with(|c| match &*c.borrow() {
        Creation::Absent => None,
        Creation::Making(p) => Some(p.get_shared_future()),
        Creation::Made(g) => Some(make_ready_future(*g)),
        Creation::Gone => Some(make_exception_future(ExceptionPtr::from(std::io::Error::other(
            "the engine is exiting: its disk groups are gone",
        )))),
    });
    if let Some(f) = waiting {
        return f;
    }
    hook_exit();
    let promise = SharedPromise::new();
    let made = promise.get_shared_future();
    CREATION.with(|c| *c.borrow_mut() = Creation::Making(promise));
    drop(create().then_wrapped(|done: Future<IoGroups>| {
        let result = done.get();
        let kept = match &result {
            Ok(g) => Creation::Made(*g),
            Err(_) => Creation::Absent,
        };
        // Only this continuation ends a creation: the exit hook waits for it on its promise.
        let ended = CREATION.with(|c| std::mem::replace(&mut *c.borrow_mut(), kept));
        if let Creation::Making(mut promise) = ended {
            match result {
                Ok(g) => promise.set_value(g),
                Err(e) => promise.set_exception(e),
            }
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

/// The engine's exit hook, on [`CREATOR_SHARD`] after the engine drained its background work
/// and before its other shards stop: the shards forget the groups, then each group is destroyed
/// on every shard (its task queue goes after the tasks already in it ran) and its id goes back.
fn destroy_at_exit() -> Future<()> {
    let making = CREATION.with(|c| {
        let mut c = c.borrow_mut();
        // A creation under way: wait for it (its callers still get the groups), then destroy.
        if let Creation::Making(p) = &*c {
            return Some(p.get_shared_future());
        }
        match std::mem::replace(&mut *c, Creation::Gone) {
            Creation::Made(g) => Some(make_ready_future(g)),
            Creation::Absent | Creation::Gone | Creation::Making(_) => None,
        }
    });
    let Some(groups) = making else { return make_ready_future(()) };
    groups.then_wrapped(|done: Future<IoGroups>| {
        CREATION.with(|c| *c.borrow_mut() = Creation::Gone);
        match done.get() {
            Ok(g) => destroy_now(g),
            // Nothing was made: nothing to destroy.
            Err(_) => make_ready_future(()),
        }
    })
}

/// Forgets `g` on every shard and destroys its groups. A failure is logged: the engine's exit goes
/// on (an exit hook that failed would leave the other shards running).
fn destroy_now(g: IoGroups) -> Future<()> {
    make_coroutine(async move {
        if let Err(e) = Smp::invoke_on_all(|| KNOWN.set(None)).await {
            log_failure(&e);
        }
        for sg in g.all() {
            if let Err(e) = destroy_scheduling_group(sg).await {
                log_failure(&e);
            }
        }
        Ok::<(), ExceptionPtr>(())
    })
}

fn log_failure(e: &ExceptionPtr) {
    engine_logger().error(
        FormatInfo::new("the disk groups were not destroyed at the engine's exit: {}"),
        format_args!("the disk groups were not destroyed at the engine's exit: {e}"),
    );
}
