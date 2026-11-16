//! Values that exist once per scheduling group, on each shard.
//!
//! A [`SchedulingGroupKey`] (created with a [`SchedulingGroupKeyConfig`] that knows the value type,
//! its size, constructor and destructor) names a slot that every scheduling group of a shard holds.
//! The values live in per-thread storage that the reactor installs; they are constructed when a
//! group's queue is created and destroyed with it. This module reads and reduces those values:
//! [`scheduling_group_get_specific`] for a given group, [`scheduling_group_get_specific_current`]
//! for the running group, and [`map_reduce_scheduling_group_specific`] and
//! [`reduce_scheduling_group_specific`] across all initialized groups of the shard.
//!
//! The accessors are `unsafe`: the caller states the value type (checked only in debug builds) and
//! guarantees that no other reference to the value is in use.

use std::collections::BTreeMap;
use std::ffi::c_void;

use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::map_reduce::map_reduce_initial;
use crate::engine::core::scheduling::{
    SchedulingGroup, SchedulingGroupKey, SchedulingGroupKeyConfig, current_scheduling_group,
    max_scheduling_groups,
};
use crate::engine::core::shared_ptr::LwSharedPtr;

/// Per-thread storage of scheduling-group-specific values.
pub mod internal {
    use super::*;

    unsafe extern "C" {
        fn free(ptr: *mut c_void);
    }

    /// Shared configuration of a key, or `None` for an empty value.
    pub type CfgPtr = Option<LwSharedPtr<SchedulingGroupKeyConfig>>;

    /// One value: `malloc()`-allocated storage, constructed and destroyed by its key's
    /// configuration. Dropping it runs the key's destructor and releases the storage.
    pub struct SpecificVal {
        // The storage and the function that releases it.
        valp: *mut c_void,
        valp_deleter: unsafe fn(*mut c_void),
        cfg: CfgPtr,
    }

    impl SpecificVal {
        /// The default deleter of the storage.
        ///
        /// # Safety
        /// `ptr` must be null or storage from `malloc()` that nothing uses any more.
        pub unsafe fn free(ptr: *mut c_void) {
            // SAFETY: the caller's contract: `ptr` is null or unused malloc()ed storage.
            unsafe { free(ptr) };
        }

        /// An empty value.
        #[inline]
        pub fn new() -> Self {
            Self { valp: std::ptr::null_mut(), valp_deleter: Self::free, cfg: None }
        }

        /// Takes ownership of `valp` and constructs the value in it.
        ///
        /// # Safety
        /// `valp` must be null or point to `cfg.allocation_size` bytes, suitably aligned for
        /// the key's type, that `valp_deleter` releases; the value takes ownership of them.
        ///
        /// # Errors
        /// If the key's constructor panics, the panic is caught and returned as the error. No value
        /// is left behind: the key's destructor does not run, and only the storage is released
        /// with `valp_deleter`.
        pub unsafe fn with(
            valp: *mut c_void,
            valp_deleter: unsafe fn(*mut c_void),
            cfg: &CfgPtr,
        ) -> Result<Self, ExceptionPtr> {
            if !valp.is_null()
                && let Some(c) = cfg
                && let Some(ctor) = &c.constructor
            {
                let p = valp.cast::<u8>();
                if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctor(p))) {
                    // SAFETY: the storage is owned by the value being built and nothing
                    // refers to it once its constructor failed.
                    unsafe { valp_deleter(valp) };
                    return Err(ExceptionPtr::from_panic(e));
                }
            }
            Ok(Self { valp, valp_deleter, cfg: cfg.clone() })
        }

        /// Returns the pointer to the value's storage (null for an empty value).
        #[inline(always)]
        pub fn get(&self) -> *mut c_void {
            self.valp
        }

        /// Runs the key's rename hook on the value, if both exist. Called when the scheduling group
        /// is renamed.
        pub fn rename(&mut self) {
            if !self.valp.is_null()
                && let Some(cfg) = &self.cfg
                && let Some(rename) = &cfg.rename
            {
                rename(self.valp.cast::<u8>());
            }
        }
    }

    impl Default for SpecificVal {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for SpecificVal {
        fn drop(&mut self) {
            if !self.valp.is_null() {
                if let Some(cfg) = &self.cfg
                    && let Some(dtor) = &cfg.destructor
                {
                    dtor(self.valp.cast::<u8>());
                }
                // SAFETY: the value owns its storage, which nothing else references now; the deleter
                // given with it is the one that releases it.
                unsafe { (self.valp_deleter)(self.valp) };
            }
        }
    }

    /// The values of one scheduling group.
    #[derive(Default)]
    pub struct PerSchedulingGroup {
        /// Whether the group's queue exists (its values are constructed).
        pub queue_is_initialized: bool,
        /// The group's values, indexed by key id. Each holds an untyped pointer that the accessors
        /// cast to the key's value type.
        pub specific_vals: Vec<SpecificVal>,
    }

    impl PerSchedulingGroup {
        /// Renames every value of the group.
        pub fn rename(&mut self) {
            for v in &mut self.specific_vals {
                v.rename();
            }
        }
    }

    /// The maximum number of scheduling groups, as an array length.
    pub const MAX_SCHEDULING_GROUPS: usize = max_scheduling_groups() as usize;

    /// The per-thread storage of all scheduling-group-specific values of a shard: the values of
    /// every group and the configuration of every key.
    pub struct SchedulingGroupSpecificThreadLocalData {
        /// The values of every scheduling group, by group index.
        pub per_scheduling_group_data: [PerSchedulingGroup; MAX_SCHEDULING_GROUPS],
        /// The configuration of every key, by key id.
        pub scheduling_group_key_configs: BTreeMap<u64, LwSharedPtr<SchedulingGroupKeyConfig>>,
    }

    impl Default for SchedulingGroupSpecificThreadLocalData {
        fn default() -> Self {
            Self {
                per_scheduling_group_data: std::array::from_fn(|_| PerSchedulingGroup::default()),
                scheduling_group_key_configs: BTreeMap::new(),
            }
        }
    }

    // Pointer to the shard's storage, installed by the reactor; null on other threads.
    thread_local! {
        static DATA: std::cell::Cell<*mut SchedulingGroupSpecificThreadLocalData> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    }

    /// Returns the address of this thread's data pointer, through which the reactor installs the
    /// storage. Valid for the lifetime of the calling thread only.
    #[inline(always)]
    pub fn get_scheduling_group_specific_thread_local_data_ptr()
    -> *mut *mut SchedulingGroupSpecificThreadLocalData {
        DATA.with(|d| d.as_ptr())
    }

    /// Returns this thread's data, or null before the reactor installs it.
    #[inline(always)]
    pub fn get_scheduling_group_specific_thread_local_data_raw()
    -> *mut SchedulingGroupSpecificThreadLocalData {
        DATA.with(|d| d.get())
    }

    /// Returns this thread's data as a mutable reference.
    ///
    /// # Safety
    /// The reactor must have installed the data on this thread, and no other reference to it
    /// may be used while the returned one is live.
    #[inline(always)]
    pub unsafe fn get_scheduling_group_specific_thread_local_data<'a>()
    -> &'a mut SchedulingGroupSpecificThreadLocalData {
        // SAFETY: by the caller, the data is installed (non-null; the reactor keeps it for the
        // thread's lifetime) and not otherwise borrowed.
        unsafe { &mut *get_scheduling_group_specific_thread_local_data_raw() }
    }

    /// Builds the error returned for a scheduling group that does not exist or is not initialized.
    pub use crate::engine::core::reactor::no_such_scheduling_group;

    /// Returns a pointer to the value of `key` in scheduling group `sg`, or null if `sg` does not
    /// exist or its queue is not initialized.
    ///
    /// `T` cannot be inferred and must be given; in debug builds it is checked against the key's
    /// type, and an unknown key fails that assertion.
    ///
    /// # Safety
    /// The reactor must have installed this thread's data (it is not checked in release builds),
    /// and `key` must be a key created on it.
    #[inline]
    pub unsafe fn scheduling_group_get_specific_ptr<T: 'static>(
        sg: SchedulingGroup,
        key: SchedulingGroupKey,
    ) -> *mut T {
        let data = get_scheduling_group_specific_thread_local_data_raw();
        debug_assert!(!data.is_null());
        // SAFETY: by the caller the data is installed; it lives for the thread's lifetime, is
        // only read here, and the values are reached through their raw storage pointers.
        let data = unsafe { &*data };
        let key_id = crate::engine::core::scheduling::internal::scheduling_group_key_id(key);
        // Debug builds check the value type; an unknown key fails the assertion too.
        #[cfg(debug_assertions)]
        {
            let cfg = data.scheduling_group_key_configs.get(&key_id);
            assert!(cfg.is_some_and(|cfg| std::any::TypeId::of::<T>() == cfg.type_index));
        }
        let sg_id = crate::engine::core::scheduling::internal::scheduling_group_index(sg) as usize;
        // The likely case: the group exists and is initialized.
        if let Some(psg) =
            data.per_scheduling_group_data.get(sg_id).filter(|psg| psg.queue_is_initialized)
        {
            debug_assert!((key_id as usize) < psg.specific_vals.len());
            // SAFETY: an initialized group holds a value for every key created, and by the caller
            // `key` is one, so the index is in bounds.
            return unsafe { psg.specific_vals.get_unchecked(key_id as usize) }.get().cast::<T>();
        }
        std::ptr::null_mut()
    }

    /// Returns an iterator over the values of `key` in the groups whose queue is initialized, as
    /// raw pointers.
    ///
    /// The thread's data is read through its pointer at each step, so no borrow of it lives across
    /// the consumer's calls (which may reach the data again).
    ///
    /// # Panics
    /// The iteration panics if `key` is out of range for an initialized group.
    ///
    /// # Safety
    /// The reactor must have installed this thread's data, and it must stay installed while
    /// the iterator is used.
    pub unsafe fn initialized_values<S>(
        key: SchedulingGroupKey,
    ) -> impl Iterator<Item = *mut S> + 'static {
        let data = get_scheduling_group_specific_thread_local_data_raw();
        let id = crate::engine::core::scheduling::internal::scheduling_group_key_id(key) as usize;
        (0..MAX_SCHEDULING_GROUPS).filter_map(move |i| {
            // SAFETY: by the caller, `data` is installed; the shared borrow of one group ends
            // within this step.
            let psg = unsafe { &*std::ptr::addr_of!((*data).per_scheduling_group_data[i]) };
            psg.queue_is_initialized.then(|| psg.specific_vals[id].get().cast::<S>())
        })
    }
}

/// Returns a reference to the value of `key` in scheduling group `sg`.
///
/// `T` cannot be inferred and must be given.
///
/// # Errors
/// Fails with an invalid-argument error if `sg` does not exist or is not initialized.
///
/// # Safety
/// The reactor must have installed this thread's data, `T` must be the type `key` was
/// created for (it is checked only in debug builds), and no other reference to the value may be
/// used while the returned one is live.
#[inline]
pub unsafe fn scheduling_group_get_specific<'a, T: 'static>(
    sg: SchedulingGroup,
    key: SchedulingGroupKey,
) -> Result<&'a mut T, ExceptionPtr> {
    // SAFETY: by the caller: the reactor installed this thread's data and `key` was created on
    // it.
    let p = unsafe { internal::scheduling_group_get_specific_ptr::<T>(sg, key) };
    if p.is_null() {
        return Err(internal::no_such_scheduling_group(sg));
    }
    // SAFETY: a non-null pointer is the initialized value of `key` in `sg`; by the caller it
    // has type T and is not otherwise borrowed.
    Ok(unsafe { &mut *p })
}

/// Returns a reference to the value of `key` in the currently running scheduling group.
///
/// `T` cannot be inferred and must be given. Unlike [`scheduling_group_get_specific`], it does not
/// check that the group's queue is initialized.
///
/// # Panics
/// Panics if `key` is unknown on this shard or `T` is not its value type.
///
/// # Safety
/// The reactor must have installed this thread's data, and no other reference to the value
/// may be used while the returned one is live.
#[inline]
pub unsafe fn scheduling_group_get_specific_current<'a, T: 'static>(
    key: SchedulingGroupKey,
) -> &'a mut T {
    // Unlike internal::scheduling_group_get_specific_ptr, this can return a reference to an
    // element whose queue_is_initialized is false.
    // SAFETY: by the caller, the data is installed; it is only read here.
    let data = unsafe { &*internal::get_scheduling_group_specific_thread_local_data_raw() };
    let key_id = crate::engine::core::scheduling::internal::scheduling_group_key_id(key);
    assert!(
        data.scheduling_group_key_configs
            .get(&key_id)
            .is_some_and(|cfg| std::any::TypeId::of::<T>() == cfg.type_index)
    );
    let sg_id = crate::engine::core::scheduling::internal::scheduling_group_index(
        current_scheduling_group(),
    ) as usize;
    debug_assert!(sg_id < data.per_scheduling_group_data.len());
    // SAFETY: the current group's index is below max_scheduling_groups, the size of
    // per_scheduling_group_data, and every group holds a value for each key created (the key
    // was found above), so both indexes are in bounds; the value of `key` has type T (asserted
    // above), and by the caller it is not otherwise borrowed.
    unsafe {
        let psg = data.per_scheduling_group_data.get_unchecked(sg_id);
        debug_assert!((key_id as usize) < psg.specific_vals.len());
        &mut *psg.specific_vals.get_unchecked(key_id as usize).get().cast::<T>()
    }
}

/// Maps every value of `key` in the shard's initialized scheduling groups with `mapper`, and folds
/// the results into `initial_val` with `reducer`.
///
/// The value type `S` must be given explicitly. The returned future resolves with the final
/// accumulator.
///
/// # Safety
/// The reactor must have installed this thread's data, `S` must be the type `key` was
/// created for, and no other reference to these values may be used until the returned
/// future resolves.
pub unsafe fn map_reduce_scheduling_group_specific<S, M, MR, R, I>(
    mut mapper: M,
    reducer: R,
    initial_val: I,
    key: SchedulingGroupKey,
) -> Future<I>
where
    S: 'static,
    M: FnMut(&mut S) -> MR + 'static,
    MR: 'static,
    R: FnMut(I, MR) -> I + 'static,
    I: 'static,
{
    let wrapped_mapper = move |v: *mut S| {
        // SAFETY: by the caller, the value has type S and nothing else borrows it; the
        // reference ends with the mapper's call.
        make_ready_future(mapper(unsafe { &mut *v }))
    };
    // SAFETY: forwarded from the caller.
    map_reduce_initial(
        unsafe { internal::initialized_values::<S>(key) },
        wrapped_mapper,
        initial_val,
        reducer,
    )
}

/// Folds a clone of every value of `key` in the shard's initialized scheduling groups into
/// `initial_val` with `reducer`.
///
/// The value type `S` must be given explicitly. The returned future resolves with the final
/// accumulator.
///
/// # Safety
/// As for [`map_reduce_scheduling_group_specific`].
pub unsafe fn reduce_scheduling_group_specific<S, R, I>(
    reducer: R,
    initial_val: I,
    key: SchedulingGroupKey,
) -> Future<I>
where
    S: Clone + 'static,
    R: FnMut(I, S) -> I + 'static,
    I: 'static,
{
    let mapper = move |v: *mut S| {
        // SAFETY: by the caller, the value has type S and nothing mutates it during the read.
        make_ready_future(unsafe { &*v }.clone())
    };
    // SAFETY: forwarded from the caller.
    map_reduce_initial(
        unsafe { internal::initialized_values::<S>(key) },
        mapper,
        initial_val,
        reducer,
    )
}
