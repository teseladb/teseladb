//! Execution stages: batch calls to one function to improve instruction cache locality.
//!
//! Execution stages provide an infrastructure for processing function calls in
//! batches in order to improve instruction cache locality.
//!
//! When the application logic becomes more and more complex and the length
//! of the data processing pipeline grows it may happen that the most
//! significant bottleneck are instruction cache misses. The solution for that
//! problem may be processing similar operations in batches so that instruction
//! cache locality is improved at the cost of potentially higher latencies and
//! worse data cache locality.
//!
//! Execution stages allow batching calls to the specified function object.
//! Every time [`ConcreteExecutionStage::call`] is used the function call
//! is added to the queue and a future is returned, and a task which executes the queued
//! calls is scheduled unless one is already pending, so calls made before it runs are
//! batched. A full queue is run inline. Execution stages are also flushed when the reactor
//! polls for events.
//!
//! When calling a function that is wrapped inside an execution stage it is
//! important to remember that the actual function call will happen at some
//! later time: the arguments are moved into the queue, and anything they point to must
//! still be alive when the call runs.
//!
//! The arguments of a stage are one value (a tuple for several). A stage keeps its
//! registered state in a heap node, so the stage object itself can be moved. Stages are
//! registered, by unique name, with a per-thread manager that the reactor flushes.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ptr::{self, NonNull};

use crate::engine::core::chunked_fifo::ChunkedFifo;
use crate::engine::core::future::{
    ExceptionPtr, Futurize, FuturizeT, InvalidArgument, Promise, make_exception_ptr,
};
use crate::engine::core::make_task::make_task;
use crate::engine::core::metrics;
use crate::engine::core::preempt::internal::scheduler_need_preempt;
use crate::engine::core::scheduling::{
    SchedulingGroup, current_scheduling_group, internal as sg_internal, max_scheduling_groups,
};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::task::{TaskPtr, schedule};

/// The per-thread registry of execution stages.
pub mod internal {
    use super::*;

    /// Unwraps a queued argument before the call; arguments are plain values (references
    /// are passed as raw pointers by the caller), so this is the identity.
    #[inline]
    pub fn unwrap_for_es<T>(object: T) -> T {
        object
    }

    /// The registry of the execution stages of one thread, by name and in registration
    /// order.
    #[derive(Default)]
    pub struct ExecutionStageManager {
        execution_stages: Vec<*mut ExecutionStage>,
        stages_by_name: HashMap<String, *mut ExecutionStage>,
    }

    thread_local! {
        static INSTANCE: UnsafeCell<ExecutionStageManager> = UnsafeCell::new(ExecutionStageManager::default());
    }

    impl ExecutionStageManager {
        /// Registers `stage` under its name.
        ///
        /// # Errors
        /// Fails with an invalid-argument error if a stage with the same name exists.
        ///
        /// # Safety
        /// `stage` points to a live stage that stays in place until it unregisters.
        pub unsafe fn register_execution_stage(
            &mut self,
            stage: *mut ExecutionStage,
        ) -> Result<(), ExceptionPtr> {
            // SAFETY: guaranteed by the caller.
            let name = unsafe { (*stage).name().as_str().to_owned() };
            if self.stages_by_name.contains_key(&name) {
                return Err(make_exception_ptr(InvalidArgument(format!(
                    "Execution stage {} already exists.",
                    name
                ))));
            }
            self.stages_by_name.insert(name, stage);
            self.execution_stages.push(stage);
            Ok(())
        }

        /// Removes `stage` from the registry.
        ///
        /// # Safety
        /// `stage` points to a live stage.
        pub unsafe fn unregister_execution_stage(&mut self, stage: *mut ExecutionStage) {
            let Some(pos) = self.execution_stages.iter().position(|s| *s == stage) else {
                return; // was changed by update_execution_stage_registration
            };
            self.execution_stages.remove(pos);
            // SAFETY: guaranteed by the caller.
            let name = unsafe { (*stage).name().as_str() };
            self.stages_by_name.remove(name);
        }

        /// Replaces the registration of `old_es` by `new_es`, for a stage that moved.
        ///
        /// # Safety
        /// `new_es` points to a live stage that stays in place until it unregisters.
        pub unsafe fn update_execution_stage_registration(
            &mut self,
            old_es: *mut ExecutionStage,
            new_es: *mut ExecutionStage,
        ) {
            if let Some(it) = self.execution_stages.iter_mut().find(|s| **s == old_es) {
                *it = new_es;
            }
            // SAFETY: guaranteed by the caller.
            let name = unsafe { (*new_es).name().as_str() };
            if let Some(s) = self.stages_by_name.get_mut(name) {
                *s = new_es;
            }
        }

        /// Returns the stage registered as `name`, or null.
        pub fn get_stage(&mut self, name: &str) -> *mut ExecutionStage {
            *self.stages_by_name.entry(name.to_owned()).or_insert(std::ptr::null_mut())
        }

        /// Flushes every registered stage; returns whether any new flush task was scheduled.
        pub fn flush(&mut self) -> bool {
            let mut did_work = false;
            // ExecutionStage::flush only schedules a task: it does not touch the manager,
            // so the list is iterated in place.
            for &stage in &self.execution_stages {
                // SAFETY: registered stages are live.
                did_work |= unsafe { ExecutionStage::flush(stage) };
            }
            did_work
        }

        /// Returns whether any registered stage has queued calls.
        pub fn poll(&self) -> bool {
            // SAFETY: registered stages are live.
            self.execution_stages.iter().any(|s| unsafe { (**s).poll() })
        }

        /// Returns the thread's manager, as a raw pointer: it is valid until the thread exits, and
        /// each caller dereferences it for one call, so no two references to it overlap.
        #[inline]
        pub fn get() -> *mut ExecutionStageManager {
            INSTANCE.with(|i| i.get())
        }
    }
}
/// Usage statistics of an execution stage.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    /// Counts tasks scheduled by execution stages
    pub tasks_scheduled: u64,
    /// Counts tasks which were preempted before execution all queued operations
    pub tasks_preempted: u64,
    /// Counts function calls added to execution stages queues
    pub function_calls_enqueued: u64,
    /// Counts function calls executed by execution stages
    pub function_calls_executed: u64,
}

/// The type-independent part of an execution stage: its name, scheduling group, statistics
/// and metrics, and the function that flushes the concrete queue.
#[repr(C)]
pub struct ExecutionStage {
    empty: bool,
    flush_scheduled: bool,
    sg: SchedulingGroup,
    stats: Stats,
    name: Sstring,
    metric_group: metrics::MetricGroup,
    /// Runs the queued calls of the concrete stage.
    do_flush: unsafe fn(*mut ExecutionStage),
    /// Frees the whole stage through its base.
    free: unsafe fn(*mut ExecutionStage),
    /// The stage was dropped while its flush task was pending: the task frees it.
    orphaned: bool,
}

impl ExecutionStage {
    /// Registers the stage and its metrics; the stage must already be at its final (heap)
    /// address.
    ///
    /// # Safety
    /// `this` must point to a stage that stays in place until it is dropped.
    unsafe fn init(this: *mut ExecutionStage) -> Result<(), ExceptionPtr> {
        // SAFETY: the manager of this thread is live; the reference ends with the call.
        unsafe { (*internal::ExecutionStageManager::get()).register_execution_stage(this)? };
        // SAFETY: guaranteed by the caller.
        let name = unsafe { (*this).name.as_str().to_owned() };
        let counter = |metric: &'static str, desc: &'static str, read: fn(&Stats) -> u64| {
            let name = name.clone();
            metrics::make_counter_labeled(
                metric,
                metrics::description(desc),
                vec![metrics::LabelInstance::new("execution_stage", &name)],
                move || {
                    // SAFETY: the metric only exists while the stage is registered, on the
                    // stage's thread; each reference ends with its call.
                    unsafe {
                        let s = (*internal::ExecutionStageManager::get()).get_stage(&name);
                        read((*s).get_stats())
                    }
                },
            )
        };
        let group = metrics::MetricGroup::with_group(
            "execution_stages",
            [
                counter("tasks_scheduled", "Counts tasks scheduled by execution stages", |s| {
                    s.tasks_scheduled
                }),
                counter(
                    "tasks_preempted",
                    "Counts tasks which were preempted before execution all queued operations",
                    |s| s.tasks_preempted,
                ),
                counter(
                    "function_calls_enqueued",
                    "Counts function calls added to execution stages queues",
                    |s| s.function_calls_enqueued,
                ),
                counter(
                    "function_calls_executed",
                    "Counts function calls executed by execution stages",
                    |s| s.function_calls_executed,
                ),
            ],
        )?;
        // SAFETY: guaranteed by the caller.
        unsafe { (*this).metric_group = group };
        Ok(())
    }

    /// Returns execution stage name
    #[inline]
    pub fn name(&self) -> &Sstring {
        &self.name
    }

    /// Returns execution stage usage statistics
    #[inline]
    pub fn get_stats(&self) -> &Stats {
        &self.stats
    }

    /// Flushes execution stage
    ///
    /// Ensures that a task which would execute all queued operations is
    /// scheduled. Does not schedule a new task if there is one already pending
    /// or the queue is empty.
    ///
    /// Returns true if a new task has been scheduled.
    ///
    /// # Safety
    /// `this` must be a live, registered stage.
    pub unsafe fn flush(this: *mut ExecutionStage) -> bool {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if (*this).empty || (*this).flush_scheduled {
                return false;
            }
            (*this).stats.tasks_scheduled += 1;
            let t = make_task((*this).sg, move || {
                if (*this).orphaned {
                    // Dropped meanwhile (see the Drop of ConcreteExecutionStage): the task
                    // was the last thing referring to the stage.
                    ((*this).free)(this);
                    return;
                }
                ((*this).do_flush)(this);
                (*this).flush_scheduled = false;
            });
            schedule(TaskPtr::from_task(NonNull::new_unchecked(t)));
            (*this).flush_scheduled = true;
        }
        true
    }

    /// Checks whether there are pending operations.
    ///
    /// Returns true if there is at least one queued operation.
    #[inline]
    pub fn poll(&self) -> bool {
        !self.empty
    }
}

struct WorkItem<A, T: 'static> {
    in_: A,
    ready: Promise<T>,
}

const FLUSH_THRESHOLD: usize = 128;
const MAX_QUEUE_LENGTH: usize = 1024;

#[repr(C)]
struct ConcreteInner<A: 'static, R: Futurize> {
    base: ExecutionStage,
    queue: ChunkedFifo<WorkItem<A, R::Output>, FLUSH_THRESHOLD>,
    function: Box<dyn FnMut(A) -> R>,
}

impl<A: 'static, R: Futurize> ConcreteInner<A, R> {
    /// The base of the stage, with the provenance of the whole object.
    ///
    /// # Safety
    /// `this` must point to a live stage.
    #[inline(always)]
    unsafe fn base_ptr(this: *mut Self) -> *mut ExecutionStage {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this).base) }
    }

    /// Frees the stage.
    ///
    /// # Safety
    /// `b` must be the base of a `ConcreteInner` allocated by `ConcreteExecutionStage::new`,
    /// from [`ConcreteInner::base_ptr`], that nothing refers to any more.
    unsafe fn free(b: *mut ExecutionStage) {
        // SAFETY: guaranteed by the caller; the base is the first field of the repr(C)
        // object and `b` has the provenance of the whole box.
        unsafe { drop(Box::from_raw(b.cast::<Self>())) };
    }

    /// Runs the queued calls, yielding to the reactor when preemption is requested.
    ///
    /// # Safety
    /// `b` must be the base of a live `ConcreteInner`, from [`ConcreteInner::base_ptr`].
    unsafe fn do_flush(b: *mut ExecutionStage) {
        // SAFETY: the base is the first field of the repr(C) ConcreteInner and `b` has the
        // provenance of the whole object; each field is reached through `me`.
        unsafe {
            let me = b.cast::<Self>();
            let queue = ptr::addr_of_mut!((*me).queue);
            while let Some(wi) = (*queue).take_front() {
                let WorkItem { in_, ready } = wi;
                let f = &mut *ptr::addr_of_mut!((*me).function);
                R::invoke(|| f(internal::unwrap_for_es(in_))).forward_to(ready);
                (*b).stats.function_calls_executed += 1;
                if scheduler_need_preempt() {
                    (*b).stats.tasks_preempted += 1;
                    break;
                }
            }
            (*b).empty = (*queue).empty();
        }
    }
}

/// An execution stage batching calls to a function from `A` to `R`.
///
/// The recommended way of creating execution stages is [`make_execution_stage`]. `A` is the
/// argument (a tuple for several) and `R` what the function returns: a future or a value,
/// converted to a future.
pub struct ConcreteExecutionStage<A: 'static, R: Futurize> {
    /// The stage, at a fixed heap address (it is registered with the manager and its
    /// flush task points to it); every pointer to its base is derived from this one.
    inner: NonNull<ConcreteInner<A, R>>,
}

impl<A: 'static, R: Futurize> ConcreteExecutionStage<A, R> {
    /// Creates a stage called `name` that runs `f` under the scheduling group `sg`.
    ///
    /// # Errors
    /// Fails with an invalid-argument error if a stage with the same name already exists on
    /// this thread.
    pub fn new<F: FnMut(A) -> R + 'static>(
        name: &Sstring,
        sg: SchedulingGroup,
        f: F,
    ) -> Result<Self, ExceptionPtr> {
        let inner = Box::into_raw(Box::new(ConcreteInner {
            base: ExecutionStage {
                empty: true,
                flush_scheduled: false,
                sg,
                stats: Stats::default(),
                name: name.clone(),
                metric_group: metrics::MetricGroup::default(),
                do_flush: ConcreteInner::<A, R>::do_flush,
                free: ConcreteInner::<A, R>::free,
                orphaned: false,
            },
            queue: ChunkedFifo::new(),
            function: Box::new(f),
        }));
        // SAFETY: the stage lives in the box and stays in place until it drops.
        unsafe {
            if let Err(e) = ExecutionStage::init(ConcreteInner::base_ptr(inner)) {
                drop(Box::from_raw(inner));
                return Err(e);
            }
            (*ptr::addr_of_mut!((*inner).queue)).reserve(FLUSH_THRESHOLD);
            Ok(Self { inner: NonNull::new_unchecked(inner) })
        }
    }

    /// Creates a stage called `name` that runs `f` under the default scheduling group.
    ///
    /// # Errors
    /// As [`ConcreteExecutionStage::new`].
    pub fn with_default_group<F: FnMut(A) -> R + 'static>(
        name: &Sstring,
        f: F,
    ) -> Result<Self, ExceptionPtr> {
        Self::new(name, SchedulingGroup::default(), f)
    }

    /// The base of the stage.
    #[inline(always)]
    fn base(&self) -> *mut ExecutionStage {
        // SAFETY: the stage owns a live inner object.
        unsafe { ConcreteInner::base_ptr(self.inner.as_ptr()) }
    }

    /// Enqueues a call to the stage's function.
    ///
    /// Adds a function call with `args` to the queue; the arguments are moved in. Anything
    /// they point to must stay alive until the call runs.
    ///
    /// Returns a future of the result of the call. If the queue already holds the maximum of
    /// 1024 calls, they run inline first. A flush task is then scheduled unless one is
    /// pending.
    pub fn call(&mut self, args: A) -> FuturizeT<R> {
        let me = self.inner.as_ptr();
        let base = self.base();
        // SAFETY: the stage is live and reached through `me` and `base`, one field at a
        // time; it is a registered stage.
        unsafe {
            let queue = ptr::addr_of_mut!((*me).queue);
            if (*queue).size() >= MAX_QUEUE_LENGTH {
                ConcreteInner::<A, R>::do_flush(base);
            }
            (*queue).push_back(WorkItem { in_: args, ready: Promise::new() });
            (*base).empty = false;
            (*base).stats.function_calls_enqueued += 1;
            let f = (*queue).back_mut_unchecked().ready.get_future();
            ExecutionStage::flush(base);
            f
        }
    }

    /// Returns execution stage name
    #[inline]
    pub fn name(&self) -> &Sstring {
        // SAFETY: the stage owns a live inner object, borrowed with `self`.
        unsafe { (*self.base()).name() }
    }

    /// Returns execution stage usage statistics
    #[inline]
    pub fn get_stats(&self) -> &Stats {
        // SAFETY: as in name().
        unsafe { (*self.base()).get_stats() }
    }

    /// Flushes the stage (see [`ExecutionStage::flush`]).
    #[inline]
    pub fn flush(&mut self) -> bool {
        // SAFETY: the base is a live, registered stage.
        unsafe { ExecutionStage::flush(self.base()) }
    }

    /// Checks whether there are pending operations.
    #[inline]
    pub fn poll(&self) -> bool {
        // SAFETY: as in name().
        unsafe { (*self.base()).poll() }
    }
}

impl<A: 'static, R: Futurize> Drop for ConcreteExecutionStage<A, R> {
    /// Unregisters the stage and drops its queued calls (their promises break) now. If its
    /// flush task is still pending, the memory stays until that task runs and frees it, so
    /// the task never runs on freed memory.
    fn drop(&mut self) {
        let me = self.inner.as_ptr();
        let base = self.base();
        // SAFETY: the manager of this thread is live; the stage was registered; it is
        // freed once, here or by its pending flush task; the fields are reached through
        // the raw pointers.
        unsafe {
            (*internal::ExecutionStageManager::get()).unregister_execution_stage(base);
            if (*base).flush_scheduled {
                let queue = ptr::addr_of_mut!((*me).queue);
                while (*queue).take_front().is_some() {}
                (*base).metric_group = metrics::MetricGroup::default();
                (*base).orphaned = true;
            } else {
                ConcreteInner::<A, R>::free(base);
            }
        }
    }
}

/// Types of [`InheritingConcreteExecutionStage`], the execution stage that inherits the
/// caller's scheduling group.
pub mod inheriting_execution_stage {
    use super::*;

    /// The statistics of the stage of one scheduling group.
    #[derive(Debug, Clone, Copy)]
    pub struct PerSchedulingGroupStats {
        /// The scheduling group.
        pub sg: SchedulingGroup,
        /// The stats of its stage.
        pub stats: Stats,
    }

    /// The statistics of every per-group stage, one element per group that was used.
    pub type InheritingStats = Vec<PerSchedulingGroupStats>;
}

/// An execution stage that inherits the caller's scheduling group.
///
/// A variation of [`ConcreteExecutionStage`] that runs each call under the scheduling group
/// of its caller: it keeps one concrete stage per scheduling group, created on first use and
/// named `<name>.<group name>`. `A` and `R` are as for [`ConcreteExecutionStage`].
pub struct InheritingConcreteExecutionStage<A: 'static, R: Futurize> {
    name: Sstring,
    function: Box<dyn FnMut(A) -> R>,
    stage_for_group: Vec<Option<ConcreteExecutionStage<A, R>>>,
}

impl<A: 'static, R: Futurize> InheritingConcreteExecutionStage<A, R> {
    fn make_stage_for_group(
        &mut self,
        sg: SchedulingGroup,
    ) -> Result<ConcreteExecutionStage<A, R>, ExceptionPtr> {
        // Every per-group stage calls the one boxed function through a pointer.
        let function: *mut dyn FnMut(A) -> R = &mut *self.function;
        // SAFETY: the function is boxed and lives as long as the inheriting stage, which
        // owns the per-group stages.
        let wrapped_function = move |args: A| unsafe { (*function)(args) };
        let name = Sstring::from(format!("{}.{}", self.name.as_str(), sg.name().as_str()).as_str());
        ConcreteExecutionStage::new(&name, sg, wrapped_function)
    }

    /// Constructs an inheriting execution stage.
    ///
    /// `name` must be unique. `f` is the function called for each
    /// [`InheritingConcreteExecutionStage::call`]; the call is deferred and batched with
    /// similar calls to increase the instruction cache hit rate.
    pub fn new<F: FnMut(A) -> R + 'static>(name: &Sstring, f: F) -> Self {
        Self {
            name: name.clone(),
            function: Box::new(f),
            stage_for_group: (0..max_scheduling_groups()).map(|_| None).collect(),
        }
    }

    /// Enqueues a call to the stage's function.
    ///
    /// Adds a function call with `args` to the queue of the caller's scheduling group,
    /// creating that group's stage on first use; the caller's scheduling group is preserved
    /// across the call.
    ///
    /// Returns a future of the result of the call, or a failed future if the group's stage
    /// could not be created.
    pub fn call(&mut self, args: A) -> FuturizeT<R> {
        let sg = current_scheduling_group();
        let sg_id = sg_internal::scheduling_group_index(sg) as usize;
        if self.stage_for_group[sg_id].is_none() {
            match self.make_stage_for_group(sg) {
                Ok(s) => self.stage_for_group[sg_id] = Some(s),
                Err(e) => return R::make_exception_future(e),
            }
        }
        match self.stage_for_group[sg_id].as_mut() {
            Some(slot) => slot.call(args),
            None => unreachable_stage(),
        }
    }

    /// Returns a summary of the individual execution stage usage statistics.
    ///
    /// Returns a vector of the stats of the individual per-scheduling group
    ///     execution stages. Each element in the vector is a pair composed of
    ///     the scheduling group and the stats for the respective execution
    ///     stage. Scheduling groups that have had no respective calls enqueued
    ///     yet are omitted.
    pub fn get_stats(&self) -> inheriting_execution_stage::InheritingStats {
        let mut summary = Vec::with_capacity(max_scheduling_groups() as usize);
        for (sg_id, s) in self.stage_for_group.iter().enumerate() {
            let sg = sg_internal::scheduling_group_from_index(sg_id as u32);
            if let Some(s) = s {
                summary.push(inheriting_execution_stage::PerSchedulingGroupStats {
                    sg,
                    stats: *s.get_stats(),
                });
            }
        }
        summary
    }
}

#[cold]
fn unreachable_stage<T>() -> T {
    unreachable!("the per-group stage was just created")
}

/// Creates a new execution stage
///
/// Wraps the given function in a [`ConcreteExecutionStage`] called `name` (unique on this
/// thread) that runs under the scheduling group `sg`. The function may return a future or an
/// immediate value.
///
/// To wrap a method, pass a closure taking the object pointer as the first element of the
/// arguments.
///
/// # Errors
/// Fails with an invalid-argument error if a stage with the same name already exists.
pub fn make_execution_stage<A: 'static, R: Futurize, F: FnMut(A) -> R + 'static>(
    name: &Sstring,
    sg: SchedulingGroup,
    fn_: F,
) -> Result<ConcreteExecutionStage<A, R>, ExceptionPtr> {
    ConcreteExecutionStage::new(name, sg, fn_)
}

/// Creates a new execution stage under the default scheduling group; see
/// [`make_execution_stage`].
///
/// # Errors
/// Fails with an invalid-argument error if a stage with the same name already exists.
pub fn make_execution_stage_default<A: 'static, R: Futurize, F: FnMut(A) -> R + 'static>(
    name: &Sstring,
    fn_: F,
) -> Result<ConcreteExecutionStage<A, R>, ExceptionPtr> {
    make_execution_stage(name, SchedulingGroup::default(), fn_)
}
