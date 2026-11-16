//! Scheduling groups: shares of CPU time on each shard, and values kept per group.
//!
//! A [`SchedulingGroup`] tags tasks; the reactor runs the groups' task queues in proportion to their
//! shares, so a background job in a low-share group cannot starve foreground work. Groups are
//! created, renamed and destroyed globally (on all shards at once) with
//! [`create_scheduling_group`], [`rename_scheduling_group`] and [`destroy_scheduling_group`]; at
//! most [`max_scheduling_groups`] exist. Group 0 is the default ("main") group, and group 1 is used
//! for work done at exit.
//!
//! A [`SchedulingGroupKey`], created with [`scheduling_group_key_create`] from a
//! [`SchedulingGroupKeyConfig`] (usually built with [`make_scheduling_group_key_config`]), gives
//! every group of every shard its own value of a type; see
//! [`scheduling_specific`](crate::engine::core::scheduling_specific).
//!
//! The group-level operations that need the reactor (`name`, `short_name`, `set_shares`,
//! `get_shares`, `update_io_bandwidth`) and the global create/destroy/rename functions are
//! implemented in [`reactor`](crate::engine::core::reactor) and re-exported here.

use std::any::TypeId;
use std::cell::Cell;
use std::hash::{Hash, Hasher};

/// Maximum number of scheduling groups per shard, fixed at build time.
pub const ENGINE_SCHEDULING_GROUPS_COUNT: u32 = 16;

/// Returns the maximum number of scheduling groups, [`ENGINE_SCHEDULING_GROUPS_COUNT`].
pub const fn max_scheduling_groups() -> u32 {
    ENGINE_SCHEDULING_GROUPS_COUNT
}

pub use crate::engine::core::reactor::Reactor;

/// Monotonic clock used to account scheduling time.
pub type SchedClock = std::time::Instant;

/// Low-level access to group indexes, key ids and the current-group slot, for the reactor.
pub mod internal {
    use super::*;

    /// Returns the index of `sg`, between 0 and [`max_scheduling_groups`] (exclusive).
    #[inline]
    pub fn scheduling_group_index(sg: SchedulingGroup) -> u32 {
        sg.id
    }

    /// Returns the group whose index is `index`. The index is not checked.
    #[inline]
    pub fn scheduling_group_from_index(index: u32) -> SchedulingGroup {
        SchedulingGroup::from_id(index)
    }

    /// Returns the numeric id of `key`, its index in each group's table of values.
    #[inline]
    pub fn scheduling_group_key_id(key: SchedulingGroupKey) -> u64 {
        key.id()
    }

    /// Pointer to a group's value of a key; see
    /// [`scheduling_specific`](crate::engine::core::scheduling_specific).
    pub use crate::engine::core::scheduling_specific::internal::scheduling_group_get_specific_ptr;

    /// Builds a value with `args` and writes it into `pre_alocated_mem`.
    ///
    /// The constructor arguments are the captures of `args`. A panic in `args` leaves the memory
    /// unwritten.
    ///
    /// # Safety
    /// `pre_alocated_mem` must be valid for writes of `ConstructorType` and suitably aligned.
    pub unsafe fn apply_constructor<ConstructorType, F: Fn() -> ConstructorType>(
        pre_alocated_mem: *mut u8,
        args: &F,
    ) {
        // SAFETY: the caller guarantees the memory is valid for writes and aligned for
        // ConstructorType; it holds no live value, so nothing is overwritten without being dropped.
        unsafe { std::ptr::write(pre_alocated_mem as *mut ConstructorType, args()) };
    }

    // The slot has a constant initializer, which keeps every access a plain TLS load.
    crate::engine::util::initial_exec_tls::initial_exec_thread_local! {
        /// The current scheduling group of this thread. Read by every task created, so it is
        /// an initial-exec thread-local (see `util::initial_exec_tls`); all zero bits is
        /// `SchedulingGroup::new()`, the default group (id 0).
        fn sg_slot() -> Cell<SchedulingGroup>, symbol = "__engine_rs_current_scheduling_group";
    }
    // The zero initial image of sg_slot() is the default group.
    const _: () =
        assert!(SchedulingGroup::new().id == 0 && std::mem::size_of::<SchedulingGroup>() == 4);

    /// Returns a pointer to this thread's current-group slot. Valid for the lifetime of the
    /// calling thread only.
    #[inline]
    pub fn current_scheduling_group_ptr() -> *mut SchedulingGroup {
        // SAFETY: sg_slot() returns the calling thread's live slot; the pointer to its contents is
        // the Cell's own.
        unsafe { (*sg_slot()).as_ptr() }
    }

    /// Makes `sg` the current group of this thread. Called by the reactor when it switches the
    /// group whose tasks it runs.
    #[inline]
    pub fn set_current_scheduling_group(sg: SchedulingGroup) {
        // SAFETY: sg_slot() returns the calling thread's live slot, written through its Cell.
        unsafe { (*sg_slot()).set(sg) };
    }
}

/// Creates a scheduling group with a name and a number of shares.
///
/// The operation is global and affects all shards; the returned group can be used on any shard.
/// `name` identifies the group and labels its metrics. `shares` is its share of CPU time; use
/// values in the 1-1000 range (higher values are accepted). One variant also takes a short name,
/// printed in log messages next to the shard id and truncated to 4 characters.
///
/// The returned future fails if no group slot is free.
pub use crate::engine::core::reactor::{
    create_scheduling_group, create_scheduling_group_with_shares,
};

/// Destroys a scheduling group created with [`create_scheduling_group`].
///
/// The group must not be in use and must not be used afterwards. The operation is global and
/// affects all shards; the returned future resolves when the group has been torn down everywhere.
pub use crate::engine::core::reactor::destroy_scheduling_group;

/// Renames a scheduling group created with [`create_scheduling_group`].
///
/// The operation is global, affects all shards and changes the labels of the group's metrics; the
/// returned future resolves when the group has been renamed everywhere. One variant also sets a new
/// short name.
pub use crate::engine::core::reactor::{
    rename_scheduling_group, rename_scheduling_group_with_shortname,
};

/// The callable behind a [`SgValueFunction`]: a `Fn(*mut u8)` that can be sent to another shard and
/// cloned through a trait object.
pub trait SgValueCallable: Fn(*mut u8) + Send {
    /// Returns a boxed clone of the callable.
    fn clone_box(&self) -> Box<dyn SgValueCallable>;
}

impl<F: Fn(*mut u8) + Clone + Send + 'static> SgValueCallable for F {
    #[inline]
    fn clone_box(&self) -> Box<dyn SgValueCallable> {
        Box::new(self.clone())
    }
}

/// A cloneable, type-erased callback on a scheduling-group-specific value, called as `f(ptr)`.
///
/// [`scheduling_group_key_create`] copies the configuration to every shard, so the callback is
/// `Send` and each clone owns its own captures; no state is shared between shards.
pub struct SgValueFunction(Box<dyn SgValueCallable>);

impl SgValueFunction {
    /// Wraps a callable.
    #[inline]
    pub fn new<F: Fn(*mut u8) + Clone + Send + 'static>(f: F) -> Self {
        Self(Box::new(f))
    }
}

impl Clone for SgValueFunction {
    #[inline]
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::ops::Deref for SgValueFunction {
    type Target = dyn Fn(*mut u8) + Send;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// Optional type-erased callback on a scheduling-group-specific value.
pub type SgValueFn = Option<SgValueFunction>;

/// Describes the values of a scheduling group key: their size, alignment and type, and how to
/// construct, rename and destroy them.
///
/// Cloneable and `Send`, because [`scheduling_group_key_create`] copies it to every shard.
#[derive(Clone)]
pub struct SchedulingGroupKeyConfig {
    /// Bytes allocated for each value (usually `size_of::<T>()`).
    pub allocation_size: usize,
    /// Alignment of each value (usually `align_of::<T>()`).
    pub alignment: usize,
    /// The value type, checked by the accessors in debug builds.
    pub type_index: TypeId,
    /// Called on each newly allocated value to construct it.
    pub constructor: SgValueFn,
    /// Called on each value after its scheduling group is renamed.
    pub rename: SgValueFn,
    /// Called on each value before its storage is released.
    pub destructor: SgValueFn,
}

impl Default for SchedulingGroupKeyConfig {
    /// A configuration for `()` with zero size and no callbacks.
    fn default() -> Self {
        Self::with_type(TypeId::of::<()>())
    }
}

impl SchedulingGroupKeyConfig {
    /// Creates a configuration that only records the value type `type_info` (from
    /// `TypeId::of::<T>()`).
    ///
    /// Size, alignment and callbacks are left empty; the type is what debug builds check when the
    /// value is accessed. [`make_scheduling_group_key_config`] fills in the rest.
    pub fn with_type(type_info: TypeId) -> Self {
        Self {
            allocation_size: 0,
            alignment: 0,
            type_index: type_info,
            constructor: None,
            rename: None,
            destructor: None,
        }
    }
}

/// Names a value that every scheduling group of every shard holds; created with
/// [`scheduling_group_key_create`].
///
/// The only operation a user can do on a key is copy it and pass it to the accessors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulingGroupKey {
    id: u64,
}

impl SchedulingGroupKey {
    /// Creates the key with numeric id `id`; used by the reactor when it allocates keys.
    pub(crate) fn from_id(id: u64) -> Self {
        Self { id }
    }
    /// Returns the key's numeric id.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

/// Optional rename hook of a scheduling-group-specific value type.
///
/// Every type used with [`make_scheduling_group_key_config`] implements it. A type that wants to be
/// told when its group is renamed sets `HAS_RENAME` to `true` and overrides `rename`; the default
/// is no hook.
pub trait SchedulingGroupSpecificRename {
    /// Whether [`rename`](Self::rename) should be called after the group is renamed.
    const HAS_RENAME: bool = false;
    /// Called on the value after its scheduling group is renamed, if `HAS_RENAME` is `true`.
    fn rename(&mut self) {}
}

/// Implements the trait with no hook for standard types.
macro_rules! impl_no_rename {
    ($($t:ty),* $(,)?) => {
        $(impl SchedulingGroupSpecificRename for $t {})*
    };
}

impl_no_rename!(
    (),
    bool,
    char,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    f32,
    f64,
    String,
    crate::engine::core::sstring::Sstring,
);

impl<T> SchedulingGroupSpecificRename for Vec<T> {}
impl<T> SchedulingGroupSpecificRename for Option<T> {}
impl<T: ?Sized> SchedulingGroupSpecificRename for Box<T> {}
impl<T> SchedulingGroupSpecificRename for std::collections::VecDeque<T> {}
impl<K, V, S> SchedulingGroupSpecificRename for std::collections::HashMap<K, V, S> {}
impl<K, V> SchedulingGroupSpecificRename for std::collections::BTreeMap<K, V> {}

/// Builds the key configuration for values of type `T`, each constructed by calling `args`.
///
/// Size, alignment and type come from `T`; the destructor drops the value in place, and the rename
/// hook is set when `T::HAS_RENAME` is true. Pass the result to [`scheduling_group_key_create`].
pub fn make_scheduling_group_key_config<T, F>(args: F) -> SchedulingGroupKeyConfig
where
    T: SchedulingGroupSpecificRename + 'static,
    F: Fn() -> T + Clone + Send + 'static,
{
    let mut sgkc = SchedulingGroupKeyConfig::with_type(TypeId::of::<T>());
    sgkc.allocation_size = std::mem::size_of::<T>();
    sgkc.alignment = std::mem::align_of::<T>();
    sgkc.constructor = Some(SgValueFunction::new(move |p: *mut u8| {
        // SAFETY: the reactor calls the constructor only on fresh memory allocated with this
        // configuration's allocation_size and alignment, i.e. those of T.
        unsafe { internal::apply_constructor::<T, F>(p, &args) };
    }));
    sgkc.destructor = Some(SgValueFunction::new(|p: *mut u8| {
        // SAFETY: the reactor calls the destructor once, on memory that holds a T constructed by
        // `constructor`.
        unsafe { std::ptr::drop_in_place(p as *mut T) };
    }));
    if T::HAS_RENAME {
        sgkc.rename = Some(SgValueFunction::new(|p: *mut u8| {
            // SAFETY: the reactor calls the hook on memory that holds a live T constructed by
            // `constructor`, with no other reference to it in use.
            unsafe { (*(p as *mut T)).rename() };
        }));
    }
    sgkc
}

/// Creates a scheduling group key on all shards from a configuration, allocating and constructing
/// its value in every existing group; the returned future resolves with the key.
pub use crate::engine::core::reactor::scheduling_group_key_create;

/// Returns a reference to a group's value of a key; see
/// [`scheduling_specific`](crate::engine::core::scheduling_specific).
pub use crate::engine::core::scheduling_specific::scheduling_group_get_specific;

/// Identifies work that is scheduled and accounted as a group.
///
/// A scheduling group is a small copyable tag attached to tasks. The reactor gives each group CPU
/// time in proportion to its shares and keeps per-group statistics. The default value is the main
/// group (id 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulingGroup {
    pub(crate) id: u32,
}

impl Default for SchedulingGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulingGroup {
    /// Returns the group with index `id`; the index is not checked.
    pub(crate) const fn from_id(id: u32) -> Self {
        Self { id }
    }
    /// Returns the default (main) group.
    pub const fn new() -> Self {
        Self { id: 0 } // const, so that the thread-local current-group slot has a constant initializer
    }
    /// Returns whether this group is the one currently running on this thread.
    #[inline]
    pub fn active(&self) -> bool {
        *self == current_scheduling_group()
    }
    // `name()` and `short_name()` are implemented in the reactor module.
    /// Returns whether this is the main (default) group.
    #[inline]
    pub fn is_main(&self) -> bool {
        self.id == 0
    }
    /// Returns whether this is the group used for work run at exit.
    #[inline]
    pub fn is_at_exit(&self) -> bool {
        self.id == 1
    }
    /// Returns a reference to this group's value of `key`.
    ///
    /// `T` cannot be inferred and must be given.
    ///
    /// # Safety
    /// This group must exist and be initialized on this shard (otherwise the internal pointer is
    /// null), `T` must be the type the key was created with, and the reference must not outlive
    /// the group's value nor overlap another reference to it.
    pub unsafe fn get_specific<'a, T: 'static>(&self, key: SchedulingGroupKey) -> &'a mut T {
        // SAFETY: guaranteed by the caller; the reactor keeps the value alive while the group
        // exists.
        unsafe { &mut *internal::scheduling_group_get_specific_ptr::<T>(*self, key) }
    }
    // Also implemented in the reactor module: `set_shares` (shard-local change of the group's
    // shares, e.g. to raise a background job's share while its backlog grows), `get_shares`
    // (the calling shard's value), and `update_io_bandwidth` (a bytes-per-second limit shared by
    // all shards together).
}

/// Returns the scheduling group currently running on this thread.
///
/// A single thread-local load.
#[inline]
pub fn current_scheduling_group() -> SchedulingGroup {
    // SAFETY: the pointer refers to the calling thread's live slot, which only this thread writes.
    unsafe { *internal::current_scheduling_group_ptr() }
}

/// Returns the default (main) scheduling group.
#[inline]
pub fn default_scheduling_group() -> SchedulingGroup {
    SchedulingGroup::new()
}

impl Hash for SchedulingGroup {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(internal::scheduling_group_index(*self) as usize);
    }
}
