//! Services with one instance per shard.
//!
//! The engine uses multiple cores by *sharding*: each core runs its own reactor, with its own
//! memory, network stack and services, and the shards communicate by explicit messages instead of
//! locks. [`Sharded<S>`] holds one instance of the service `S` per shard: [`Sharded::start`]
//! constructs an instance on every shard, [`Sharded::invoke_on`], [`Sharded::invoke_on_all`] and
//! the map-reduce methods run functions on the instances (each on its own shard), and
//! [`Sharded::stop`] stops and destroys them.
//!
//! Constructor and call arguments are captured by the closures passed to these methods. A
//! [`ShardedParameter`] among them is evaluated on the target shard with
//! [`ShardedParameter::evaluate`]; [`EitherShardedOrLocal`](internal::EitherShardedOrLocal) gives
//! a callee either the local instance or the whole `Sharded` object.
//!
//! A service type implements [`ShardedService`]; its provided methods cover services with no
//! `stop`, no release tracking ([`AsyncShardedService`]) and no access to their peers
//! ([`PeeringShardedService`]).
//!
//! A started `Sharded` must not move: the instances and the cross-shard calls keep its address,
//! so every such method takes it pinned.

use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr::{self, NonNull};

use crate::engine::core::future::{
    ExceptionPtr, Future, Futurize, InvalidArgument, Promise, make_exception_future,
    make_ready_future,
};
use crate::engine::core::loop_::parallel_for_each;
use crate::engine::core::map_reduce::{ReduceResult, Reducer, map_reduce, map_reduce_initial};
pub use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::shared_ptr::{SharedPtr, SharedPtrCountBase, make_shared};
use crate::engine::core::smp::{Smp, SmpSubmitToOptions};
use crate::engine::core::sstring::Sstring;

/// Internal helpers of `sharded`.
pub mod internal {
    use super::*;

    /// A function run once per shard id, returning a future.
    pub type OnEachShardFunc<'a> = &'a mut dyn FnMut(u32) -> Future<()>;

    /// Gives a callee either the local instance or the whole `Sharded` object, whichever it asks
    /// for.
    pub struct EitherShardedOrLocal<'a, Service: ShardedService> {
        _sharded: Pin<&'a mut Sharded<Service>>,
    }

    impl<'a, Service: ShardedService> EitherShardedOrLocal<'a, Service> {
        /// Wraps `s`.
        #[inline]
        pub fn new(s: Pin<&'a mut Sharded<Service>>) -> Self {
            Self { _sharded: s }
        }

        /// Returns the whole `Sharded` object.
        #[inline]
        pub fn sharded(&mut self) -> Pin<&mut Sharded<Service>> {
            self._sharded.as_mut()
        }

        /// Returns the instance of the current shard.
        ///
        /// # Safety
        /// As for [`Sharded::local`].
        #[inline]
        pub unsafe fn local(&mut self) -> &mut Service {
            // SAFETY: the caller upholds the contract of Sharded::local.
            unsafe { self._sharded.as_mut().local() }
        }
    }

    /// Wraps a `Sharded` object passed as an argument, for a callee on another shard.
    #[inline]
    pub fn unwrap_sharded_ref<Service: ShardedService>(
        arg: Pin<&mut Sharded<Service>>,
    ) -> EitherShardedOrLocal<'_, Service> {
        EitherShardedOrLocal::new(arg)
    }

    /// Evaluates a [`ShardedParameter`] on the current shard.
    #[inline]
    pub fn unwrap_sharded_parameter<F, R>(sp: &ShardedParameter<F>) -> R
    where
        F: Fn() -> R,
    {
        sp.evaluate()
    }

    /// Calls `on_each_shard` for every shard id below `nr_shards`, concurrently; the future
    /// resolves when all have, and fails if any failed (after all have finished).
    #[inline]
    pub fn sharded_parallel_for_each<F>(nr_shards: u32, on_each_shard: F) -> Future<()>
    where
        F: FnMut(u32) -> Future<()> + 'static,
    {
        parallel_for_each(0..nr_shards, on_each_shard)
    }

    /// Stops one instance with [`ShardedService::stop`].
    #[inline]
    pub fn stop_sharded_instance<Service: ShardedService>(instance: &mut Service) -> Future<()> {
        instance.stop()
    }
}

/// A service managed by [`Sharded`].
///
/// Every method has a default, so a service implements only what it needs.
pub trait ShardedService: Sized + 'static {
    /// Stops the instance; called by [`Sharded::stop`] on each shard before the instance is
    /// released. The default does nothing.
    #[inline]
    fn stop(&mut self) -> Future<()> {
        make_ready_future(())
    }

    /// Returns the [`AsyncShardedService`] the service embeds: [`Sharded::stop`] then waits until
    /// every reference to the instance is released, and the instance can take pointers to
    /// itself. The default, `None`, does not wait.
    #[inline]
    fn async_service(&mut self) -> Option<&mut AsyncShardedService<Self>> {
        None
    }

    /// Receives the address of the `Sharded` object that holds the instance, right after it is
    /// constructed. A service embedding a [`PeeringShardedService`] stores it there; the default
    /// ignores it.
    #[inline]
    fn set_container(&mut self, _container: *mut Sharded<Self>) {}
}

/// Makes [`Sharded::stop`] wait until every reference to a service instance is released.
///
/// Embed it in the service and return it from [`ShardedService::async_service`]. The service
/// must still track its own references in asynchronous code, by taking
/// [`shared_from_this`](Self::shared_from_this) and keeping the pointer while it uses the
/// instance.
pub struct AsyncShardedService<Service> {
    _freed: Promise<()>,
    // The counter and the object of the block `Sharded` placed the service in, null until then.
    // Not an owner: the service holds this value, not the other way round.
    _counter: *mut SharedPtrCountBase,
    _service: *mut Service,
}

impl<Service> AsyncShardedService<Service> {
    /// Returns a tracker whose release nobody waits for yet, of a service no `Sharded` holds yet.
    #[inline]
    pub fn new() -> Self {
        Self { _freed: Promise::new(), _counter: ptr::null_mut(), _service: ptr::null_mut() }
    }

    /// Returns a future that resolves once the service is destroyed.
    #[inline]
    fn freed(&mut self) -> Future<()> {
        self._freed.get_future()
    }

    /// Records the block that `holder` owns as the one of the service that embeds `self`.
    #[inline]
    fn placed(&mut self, holder: &SharedPtr<Service>) {
        self._counter = holder.counter();
        self._service = holder.get().map_or(ptr::null_mut(), NonNull::as_ptr);
    }

    /// Returns a shared pointer to the service that embeds `self`. Keeping it delays the
    /// service's destruction, and with it the end of [`Sharded::stop`].
    ///
    /// The pointer shares the counter of the block `Sharded` placed the service in, which
    /// `Sharded` recorded here: it is not found from a reference to the service, through which
    /// nothing outside the service may be written.
    ///
    /// # Panics
    /// If no `Sharded` holds the service.
    ///
    /// # Safety
    /// `self` is still inside the service instance that returned it from
    /// [`ShardedService::async_service`] (it was not moved out of it), and the call is on that
    /// instance's shard.
    #[inline]
    pub unsafe fn shared_from_this(&self) -> SharedPtr<Service> {
        assert!(!self._counter.is_null(), "no Sharded holds this service");
        // SAFETY: by the caller `self` is inside the block `placed` recorded, which is therefore
        // alive, on its shard.
        unsafe { SharedPtr::from_parts(self._counter, self._service) }
    }
}

impl<Service> Default for AsyncShardedService<Service> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Service> Drop for AsyncShardedService<Service> {
    fn drop(&mut self) {
        self._freed.set_value(());
    }
}

/// Gives a sharded service access to its peers on other shards.
///
/// Embed it in the service and forward [`ShardedService::set_container`] to
/// [`set_container`](Self::set_container); [`container`](Self::container) then returns the
/// [`Sharded`] object, through which the instance can call its peers.
pub struct PeeringShardedService<Service: ShardedService> {
    _container: *mut Sharded<Service>,
}

impl<Service: ShardedService> PeeringShardedService<Service> {
    /// Returns a value not yet attached to a container.
    #[inline]
    pub const fn new() -> Self {
        Self { _container: std::ptr::null_mut() }
    }

    /// Records the container; call it from [`ShardedService::set_container`].
    #[inline]
    pub fn set_container(&mut self, container: *mut Sharded<Service>) {
        self._container = container;
    }

    /// Returns the `Sharded` object holding this service, as a pointer: every shard's instance
    /// holds the same one, so handing out `&mut` here would alias.
    #[inline]
    pub fn container(&self) -> *mut Sharded<Service> {
        self._container
    }

    /// Returns the `Sharded` object holding this service.
    ///
    /// # Safety
    /// The `Sharded` object must be alive and not moved since it was started (it outlives its
    /// instances), and no other reference to it may be used while the returned one is live (this
    /// includes the same call on another shard).
    #[inline]
    pub unsafe fn container_mut<'a>(&self) -> Pin<&'a mut Sharded<Service>> {
        // SAFETY: the caller's contract; the container is set before the service is published,
        // and a started `sharded` object is pinned.
        unsafe { Pin::new_unchecked(&mut *self._container) }
    }
}

impl<Service: ShardedService> Default for PeeringShardedService<Service> {
    fn default() -> Self {
        Self::new()
    }
}

/// The error returned when a [`Sharded`] object has no instance on the current shard.
#[derive(Debug, Clone)]
pub struct NoShardedInstanceException {
    _msg: Sstring,
}

impl NoShardedInstanceException {
    /// Returns the error with the message "sharded instance does not exist".
    pub fn new() -> Self {
        Self { _msg: "sharded instance does not exist".into() }
    }

    /// Returns the error with the message "sharded instance does not exist: `type_info`".
    pub fn with_type(type_info: &str) -> Self {
        Self { _msg: format!("sharded instance does not exist: {}", type_info).into() }
    }
}

impl Default for NoShardedInstanceException {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for NoShardedInstanceException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self._msg.as_str())
    }
}

impl std::error::Error for NoShardedInstanceException {}

/// One shard's slot: its instance, once started.
struct Entry<Service> {
    service: Option<SharedPtr<Service>>,
}

impl<Service: ShardedService> Entry<Service> {
    /// Returns the future of the instance's destruction, or a ready one for services that do not
    /// report it.
    fn track_deletion(&mut self) -> Future<()> {
        if let Some(service) = &self.service {
            // SAFETY: the instance is this shard's and stop() has finished with it; no other
            // reference to it is live during the call.
            if let Some(tracker) = unsafe { service.get_mut_unchecked() }.async_service() {
                return tracker.freed();
            }
        }
        make_ready_future(())
    }
}

impl<Service> Default for Entry<Service> {
    fn default() -> Self {
        Self { service: None }
    }
}

/// A service distributed across all shards: one instance of `Service` per shard.
///
/// It creates an instance on each shard, runs functions on each shard's instance, and stops the
/// service. `Service` implements [`ShardedService`], whose `stop` is called on each instance when
/// the service is stopped.
///
/// The object cannot be copied or moved once started: the instances point back at it and the
/// cross-shard calls keep its address. It is `!Unpin`, and every operation that hands out its
/// address takes it pinned (`Pin<&mut Sharded<S>>`). It must be stopped before it is dropped.
pub struct Sharded<Service: ShardedService> {
    _instances: Vec<Entry<Service>>,
    _pinned: PhantomPinned,
}

/// A raw pointer to the `Sharded` object, captured by the cross-shard closures.
///
/// On a target shard it is only used through [`This::slot`], which reaches the shard's own
/// element of `_instances` without forming a reference to the whole object (each shard
/// touches a distinct element); [`This::get`] is for the continuations
/// that run back on the shard that owns the `sharded` object, once no cross-shard call is in
/// flight.
struct This<Service: ShardedService>(*mut Sharded<Service>);

// SAFETY: the pointer crosses shards only to reach each shard's own `_instances` element (see
// `slot`); the vector is resized only by start()/stop() on the owner shard, before any and
// after every cross-shard call, so the element addresses are stable while it is shared.
unsafe impl<Service: ShardedService> Send for This<Service> {}

impl<Service: ShardedService> Clone for This<Service> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Service: ShardedService> Copy for This<Service> {}

impl<Service: ShardedService> This<Service> {
    /// The whole object, on its owner shard.
    ///
    /// # Safety
    /// Only on the owner shard, with no cross-shard call of this object in flight and no
    /// other reference to it live (a `Sharded` object outlives every operation it starts:
    /// its destructor asserts that it was stopped, and it is not moved while operations run).
    #[inline(always)]
    unsafe fn get(self) -> &'static mut Sharded<Service> {
        // SAFETY: the caller guarantees the object is alive and not otherwise referenced.
        unsafe { &mut *self.0 }
    }

    /// The service slot of the current shard.
    ///
    /// # Safety
    /// The object must be alive with `_instances` sized for this shard (between the resize in
    /// start() and the clear in stop()), and the slot must be used only from this shard.
    #[inline(always)]
    unsafe fn slot(self) -> *mut Option<SharedPtr<Service>> {
        // SAFETY: by the caller the object is alive; only the vector header is read (shared),
        // and the element of this shard is reached by pointer arithmetic without a reference
        // to the whole object or the vector.
        unsafe {
            let v = std::ptr::addr_of!((*self.0)._instances);
            let base = (*v).as_ptr().cast_mut();
            std::ptr::addr_of_mut!((*base.add(this_shard_id() as usize)).service)
        }
    }

    /// Returns the instance of the current shard, or the "no sharded instance" error.
    ///
    /// # Safety
    /// As for [`This::slot`].
    #[inline]
    unsafe fn local_service(self) -> Result<SharedPtr<Service>, ExceptionPtr> {
        // SAFETY: the caller's contract; the length is read through a shared reference.
        let len = unsafe { (*std::ptr::addr_of!((*self.0)._instances)).len() };
        if (this_shard_id() as usize) < len {
            // SAFETY: as above; the slot is this shard's and only read here.
            if let Some(s) = unsafe { &*self.slot() } {
                return Ok(s.clone());
            }
        }
        Err(ExceptionPtr::new(NoShardedInstanceException::with_type(
            std::any::type_name::<Service>(),
        )))
    }

    /// Stores a new instance in this shard's slot, after giving it the container's address.
    ///
    /// # Safety
    /// As for [`This::slot`].
    #[inline]
    unsafe fn set_local_service(self, service: Service) {
        let s = make_shared(service);
        // SAFETY: `s` was just created and is not shared yet.
        let instance = unsafe { s.get_mut_unchecked() };
        instance.set_container(self.0);
        if let Some(tracker) = instance.async_service() {
            tracker.placed(&s);
        }
        // SAFETY: the caller guarantees the slot is this shard's and the vector is sized.
        unsafe { *self.slot() = Some(s) };
    }
}

/// Calls `func` on the local instance of a cross-shard call.
///
/// The callback gets `&mut Service`. Safe code reaches a shard's
/// instance only through these calls, which run one at a time on that shard and whose
/// results are `'static` (they cannot keep the reference); the accessors that hand the
/// instance out otherwise ([`Sharded::local`], [`Sharded::local_shared`],
/// [`Sharded::get_local_service`], [`PeeringShardedService::container_mut`]) are `unsafe`
/// with the contract that none of their references is live while a call runs here.
///
/// # Safety
/// As for [`This::slot`].
#[inline]
unsafe fn call_local<Service, F, R>(this: This<Service>, func: F) -> Future<R::Output>
where
    Service: ShardedService,
    F: FnOnce(&mut Service) -> R,
    R: Futurize,
{
    // SAFETY: the caller guarantees the object is started and this is the target shard.
    match unsafe { this.local_service() } {
        // SAFETY: the instance is this shard's, reached from this shard only; no other
        // reference to it is live during the call (see above), so the callback's is unique.
        Ok(inst) => func(unsafe { inst.get_mut_unchecked() }).convert(),
        Err(e) => make_exception_future(e),
    }
}

/// What the constructor passed to [`Sharded::start`] returns: the service, or
/// `Result<Service, ExceptionPtr>` for a constructor that may fail.
pub trait ServiceCtorResult<Service> {
    /// Returns the constructed service, or the constructor's error.
    fn into_result(self) -> Result<Service, ExceptionPtr>;
}

impl<Service> ServiceCtorResult<Service> for Service {
    #[inline(always)]
    fn into_result(self) -> Result<Service, ExceptionPtr> {
        Ok(self)
    }
}

impl<Service> ServiceCtorResult<Service> for Result<Service, ExceptionPtr> {
    #[inline(always)]
    fn into_result(self) -> Result<Service, ExceptionPtr> {
        self
    }
}

impl<Service: ShardedService> Sharded<Service> {
    /// Runs `func` for every shard the instances are sized for, concurrently.
    fn sharded_parallel_for_each<F>(&self, func: F) -> Future<()>
    where
        F: FnMut(u32) -> Future<()> + 'static,
    {
        internal::sharded_parallel_for_each(self._instances.len() as u32, func)
    }

    /// Returns the object's address for the cross-shard closures.
    fn this(self: Pin<&mut Self>) -> This<Service> {
        // SAFETY: only the address is taken; the object is not moved out of the pin.
        This(unsafe { self.get_unchecked_mut() })
    }

    /// The vector of instances, for the writes of start().
    #[inline]
    fn instances_mut(self: Pin<&mut Self>) -> &mut Vec<Entry<Service>> {
        // SAFETY: the vector is not structurally pinned (its elements are reached by address
        // only while it is not resized); the object itself is not moved.
        unsafe { &mut self.get_unchecked_mut()._instances }
    }

    /// Creates an empty object; no instance of the service is created until
    /// [`start`](Self::start).
    #[inline]
    pub const fn new() -> Self {
        Self { _instances: Vec::new(), _pinned: PhantomPinned }
    }

    /// Starts the service by calling a clone of `ctor` on every shard to construct that shard's
    /// instance.
    ///
    /// The returned future resolves when every instance has been constructed. The object must
    /// stay alive and in place until it is stopped. `ctor` runs on every shard, so it is `Send`;
    /// arguments are passed by capturing them in it.
    ///
    /// # Errors
    /// If a constructor fails (returns `Err`), the instances already built are stopped and the
    /// future fails with that error.
    pub fn start<C, O>(mut self: Pin<&mut Self>, ctor: C) -> Future<()>
    where
        C: FnOnce() -> O + Clone + Send + 'static,
        O: ServiceCtorResult<Service>,
    {
        self.as_mut().instances_mut().resize_with(Smp::count() as usize, Entry::default);
        let this = self.as_mut().this();
        self.sharded_parallel_for_each(move |c: u32| {
            let ctor = ctor.clone();
            Smp::submit_to(c, move || -> Result<(), ExceptionPtr> {
                let service = ctor().into_result()?;
                // SAFETY: `_instances` was sized above and stays so until stop().
                unsafe { this.set_local_service(service) };
                Ok(())
            })
        })
        .then_wrapped(move |f: Future<()>| {
            if !f.failed() {
                return make_ready_future(());
            }
            let e = f.get_exception();
            // SAFETY: back on the owner shard, every construction has completed.
            unsafe { Pin::new_unchecked(this.get()) }
                .stop()
                .then(move |()| make_exception_future::<()>(e))
        })
    }

    /// Starts the service with a single instance, constructed by `ctor` on shard 0.
    ///
    /// The returned future resolves when the instance has been constructed.
    ///
    /// # Errors
    /// If the constructor fails, the future fails with its error.
    ///
    /// # Panics
    /// Panics if the service is already started.
    pub fn start_single<C, O>(mut self: Pin<&mut Self>, ctor: C) -> Future<()>
    where
        C: FnOnce() -> O + Send + 'static,
        O: ServiceCtorResult<Service>,
    {
        assert!(self._instances.is_empty());
        self.as_mut().instances_mut().resize_with(1, Entry::default);
        let this = self.as_mut().this();
        Smp::submit_to(0, move || -> Result<(), ExceptionPtr> {
            let service = ctor().into_result()?;
            // SAFETY: `_instances` was sized above (one slot, shard 0's) until stop().
            unsafe { this.set_local_service(service) };
            Ok(())
        })
        .then_wrapped(move |f: Future<()>| {
            if !f.failed() {
                return make_ready_future(());
            }
            let e = f.get_exception();
            // SAFETY: back on the owner shard, the construction has completed.
            unsafe { Pin::new_unchecked(this.get()) }
                .stop()
                .then(move |()| make_exception_future::<()>(e))
        })
    }

    /// Stops all started instances and destroys them.
    ///
    /// For every started instance, [`ShardedService::stop`] is called on its shard; then every
    /// instance is released, and the returned future resolves once all are destroyed (services
    /// that embed an [`AsyncShardedService`] are waited for). The future
    /// fails with the first error of a `stop`, after all instances have been released.
    pub fn stop(mut self: Pin<&mut Self>) -> Future<()> {
        let this = self.as_mut().this();
        self.sharded_parallel_for_each(move |c: u32| {
            Smp::submit_to(c, move || {
                // SAFETY: stop() runs over the shards `_instances` was sized for.
                let inst = unsafe { (*this.slot()).clone() };
                match inst {
                    None => make_ready_future(()),
                    Some(inst) => internal::stop_sharded_instance(
                        // SAFETY: the instance is this shard's, used from this shard only, and
                        // its stop() gets the only reference for the call.
                        unsafe { inst.get_mut_unchecked() },
                    ),
                }
            })
        })
        .then_wrapped(move |fut: Future<()>| {
            // SAFETY: back on the owner shard, after the first pass completed.
            unsafe { this.get() }
                .sharded_parallel_for_each(move |c: u32| {
                    Smp::submit_to(c, move || {
                        // SAFETY: as above; the slot is this shard's.
                        let slot = unsafe { &mut *this.slot() };
                        let mut entry = Entry { service: slot.take() };
                        let fut = entry.track_deletion();
                        entry.service = None;
                        fut
                    })
                })
                .finally(move || {
                    // SAFETY: back on the owner shard; every cross-shard call has completed.
                    // Assigning a new vector drops the elements and releases the storage.
                    let s = unsafe { this.get() };
                    s._instances = Vec::new();
                    fut
                })
        })
    }

    /// Calls a clone of `func` on every shard's instance, each on its own shard.
    ///
    /// The returned future resolves when every call has completed, and fails if any failed.
    /// `options` is passed to the cross-shard submission ([`Smp::submit_to_with`]); arguments are
    /// passed by capturing them in `func`.
    pub fn invoke_on_all_with<F, R>(
        mut self: Pin<&mut Self>,
        options: SmpSubmitToOptions,
        func: F,
    ) -> Future<()>
    where
        F: FnOnce(&mut Service) -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        let this = self.as_mut().this();
        self.sharded_parallel_for_each(move |c: u32| {
            let func = func.clone();
            Smp::submit_to_with(c, options, move || -> Future<()> {
                // SAFETY: the object is started (its slots are sized) while calls are in flight.
                unsafe { call_local(this, func) }
            })
        })
    }

    /// Like [`invoke_on_all_with`](Self::invoke_on_all_with), with the default submission
    /// options.
    pub fn invoke_on_all<F, R>(self: Pin<&mut Self>, func: F) -> Future<()>
    where
        F: FnOnce(&mut Service) -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        self.invoke_on_all_with(SmpSubmitToOptions::default(), func)
    }

    /// Calls a clone of `func` on every shard's instance except the current shard's.
    ///
    /// `func` returns nothing or a future of nothing. The returned future resolves when every
    /// call has completed. `options` is passed to the cross-shard submission.
    pub fn invoke_on_others_with<F, R>(
        self: Pin<&mut Self>,
        options: SmpSubmitToOptions,
        func: F,
    ) -> Future<()>
    where
        F: FnOnce(&mut Service) -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        let orig = this_shard_id();
        self.invoke_on_all_with(options, move |s: &mut Service| -> Future<()> {
            if this_shard_id() == orig { make_ready_future(()) } else { func(s).convert() }
        })
    }

    /// Like [`invoke_on_others_with`](Self::invoke_on_others_with), with the default submission
    /// options.
    pub fn invoke_on_others<F, R>(self: Pin<&mut Self>, func: F) -> Future<()>
    where
        F: FnOnce(&mut Service) -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        self.invoke_on_others_with(SmpSubmitToOptions::default(), func)
    }

    /// Calls `func` on the instance of shard `id`, on that shard, and returns a future of its
    /// result.
    ///
    /// `func` returns a value or a future of a value. `options` is passed to the cross-shard
    /// submission; arguments are passed by capturing them in `func`. The future fails with the
    /// "no sharded instance" error if shard `id` has no instance.
    pub fn invoke_on_with<F, R>(
        mut self: Pin<&mut Self>,
        id: u32,
        options: SmpSubmitToOptions,
        func: F,
    ) -> Future<R::Output>
    where
        F: FnOnce(&mut Service) -> R + Send + 'static,
        R: Futurize + 'static,
        R::Output: Send,
    {
        let this = self.as_mut().this();
        Smp::submit_to_with(id, options, move || -> Future<R::Output> {
            // SAFETY: the object is started (its slots are sized) while calls are in flight.
            unsafe { call_local(this, func) }
        })
    }

    /// Like [`invoke_on_with`](Self::invoke_on_with), with the default submission options.
    pub fn invoke_on<F, R>(self: Pin<&mut Self>, id: u32, func: F) -> Future<R::Output>
    where
        F: FnOnce(&mut Service) -> R + Send + 'static,
        R: Futurize + 'static,
        R::Output: Send,
    {
        self.invoke_on_with(id, SmpSubmitToOptions::default(), func)
    }

    /// Calls a clone of `func` on the instance of every shard id in `range`, concurrently.
    ///
    /// The returned future resolves when every call has completed. `options` is passed to the
    /// cross-shard submission.
    ///
    /// # Errors
    /// The future fails with an invalid-argument error for a shard id that does not exist, and
    /// with the error of any call that failed.
    pub fn invoke_on_range_with<I, F, R>(
        mut self: Pin<&mut Self>,
        range: I,
        options: SmpSubmitToOptions,
        func: F,
    ) -> Future<()>
    where
        I: IntoIterator<Item = u32>,
        F: FnOnce(&mut Service) -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        let this = self.as_mut().this();
        parallel_for_each(range, move |s: u32| -> Future<()> {
            if s >= Smp::count() {
                return make_exception_future(ExceptionPtr::new(InvalidArgument::new(format!(
                    "Invalid shard id in range: {}. Must be in range [0,{})",
                    s,
                    Smp::count()
                ))));
            }
            let func = func.clone();
            Smp::submit_to_with(s, options, move || -> Future<()> {
                // SAFETY: the object is started (its slots are sized) while calls are in flight.
                unsafe { call_local(this, func) }
            })
        })
    }

    /// Like [`invoke_on_range_with`](Self::invoke_on_range_with), with the default submission
    /// options.
    pub fn invoke_on_range<I, F, R>(self: Pin<&mut Self>, range: I, func: F) -> Future<()>
    where
        I: IntoIterator<Item = u32>,
        F: FnOnce(&mut Service) -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        self.invoke_on_range_with(range, SmpSubmitToOptions::default(), func)
    }

    /// Calls `func` on every shard's instance and combines the results with the reducer `r`
    /// (see [`crate::engine::core::map_reduce::map_reduce`]).
    ///
    /// Every shard calls the same `func`, which lives until the whole reduction has resolved, so
    /// it is `Fn` and `Sync`.
    pub fn map_reduce<Red, F, R, K>(
        mut self: Pin<&mut Self>,
        r: Red,
        func: F,
    ) -> Future<Red::Result>
    where
        F: Fn(&mut Service) -> R + Send + Sync + 'static,
        R: Futurize + 'static,
        R::Output: Send + 'static,
        Red: Reducer<R::Output, K> + 'static,
    {
        let this = self.as_mut().this();
        let n = self._instances.len() as u32;
        map_reduce(
            0..n,
            move |c: u32| {
                // `func` is borrowed by address: the mapper is held by map_reduce's state, which does
                // not move, until every call it starts has resolved.
                let func_addr = &func as *const F as usize;
                Smp::submit_to(c, move || -> Future<R::Output> {
                    // SAFETY: the object is started (its slots are sized) while calls are in flight;
                    // `func` outlives this call and its future (see above) and is only read (`F: Sync`).
                    unsafe { call_local(this, &*(func_addr as *const F)) }
                })
            },
            r,
        )
    }

    /// Calls `map` on every shard's instance, in parallel, and left-folds the results into
    /// `initial` with `reduce`.
    ///
    /// `map` returns a value or a future of one, and each call runs on the instance's shard. The
    /// returned future resolves with the final accumulator, or fails with the first error.
    pub fn map_reduce0<M, R, Initial, Reduce, O>(
        mut self: Pin<&mut Self>,
        map: M,
        initial: Initial,
        reduce: Reduce,
    ) -> Future<Initial>
    where
        M: Fn(&mut Service) -> R + Clone + Send + 'static,
        R: Futurize + 'static,
        R::Output: Send + 'static,
        Initial: 'static,
        Reduce: FnMut(Initial, R::Output) -> O + 'static,
        O: ReduceResult<Initial>,
    {
        let this = self.as_mut().this();
        let wrapped_map = move |c: u32| {
            let map = map.clone();
            Smp::submit_to(c, move || -> Future<R::Output> {
                // This clone of `map` belongs to the submitted function, which Smp::submit_to
                // keeps until the call's future resolves.
                // SAFETY: the object is started (its slots are sized) while calls are in flight.
                let f = unsafe { call_local(this, &map) };
                f.then_wrapped(move |f: Future<R::Output>| {
                    drop(map);
                    f
                })
            })
        };
        map_reduce_initial(Smp::all_cpus(), wrapped_map, initial, reduce)
    }

    /// Calls `mapper` on every shard's instance, in parallel, and returns a future of the results
    /// indexed by shard id.
    ///
    /// `mapper` returns a value or a future of one; each call runs on the instance's shard. The
    /// future fails with the first error.
    pub fn map<M, R>(mut self: Pin<&mut Self>, mapper: M) -> Future<Vec<R::Output>>
    where
        M: Fn(&mut Service) -> R + Send + Sync + 'static,
        R: Futurize + 'static,
        R::Output: Default + Send + 'static,
    {
        // The results and the mapper live on the heap until the returned future resolves.
        struct State<V, M> {
            vec: Vec<V>,
            mapper: M,
        }
        let n = self._instances.len();
        let mut vec = Vec::with_capacity(n);
        vec.resize_with(n, R::Output::default);
        let state = Box::into_raw(Box::new(State { vec, mapper }));
        let state_addr = state as usize;
        let this = self.as_mut().this();
        parallel_for_each(0..n as u32, move |c: u32| {
            // Every shard calls the one mapper held in the state.
            // SAFETY: the state stays alive until the final continuation below frees it.
            let mapper_addr =
                unsafe { std::ptr::addr_of!((*(state_addr as *const State<R::Output, M>)).mapper) }
                    as usize;
            Smp::submit_to(c, move || -> Future<R::Output> {
                // SAFETY: the object is started (its slots are sized) while calls are in flight;
                // the mapper lives in the state until the final continuation, after every call
                // has resolved, and is only read (`M: Sync`).
                unsafe { call_local(this, &*(mapper_addr as *const M)) }
            })
            .then(move |res: R::Output| {
                // SAFETY: as above; this continuation runs on the calling shard and writes
                // its own slot, `c < n == vec.len()` (the range is 0..n).
                unsafe {
                    let st = &mut *(state_addr as *mut State<R::Output, M>);
                    *st.vec.get_unchecked_mut(c as usize) = res;
                }
            })
        })
        .then_wrapped(move |f: Future<()>| {
            // SAFETY: every continuation using the state has run; this one owns it now.
            let st = unsafe { Box::from_raw(state_addr as *mut State<R::Output, M>) };
            if f.failed() {
                return make_exception_future(f.get_exception());
            }
            make_ready_future(st.vec)
        })
    }

    /// Gets a reference to the local instance.
    ///
    /// # Safety
    /// While the reference is live, no call of this object may run on the local instance:
    /// the callbacks of `invoke_on*`, `map*` and `stop` already submitted run as tasks on this
    /// shard and get their own `&mut Service`, so the reference is not held across a
    /// suspension point while one of them is in flight.
    ///
    /// # Panics
    /// Panics if the current shard has no instance.
    #[inline]
    pub unsafe fn local(self: Pin<&mut Self>) -> &mut Service {
        let this = self.into_ref().get_ref();
        assert!(this.local_is_initialized());
        match &this._instances[this_shard_id() as usize].service {
            // SAFETY: the pinned `&mut` excludes other access through this object, and by the
            // caller no cross-shard call uses the instance while the reference is live.
            Some(s) => unsafe { s.get_mut_unchecked() },
            // Invariant: checked by local_is_initialized() just above.
            None => unreachable!("sharded instance does not exist"),
        }
    }

    /// Gets a shared pointer to the local instance.
    ///
    /// # Safety
    /// As for [`Sharded::local`], for every reference obtained through the returned pointer
    /// (the calls of this object hand the instance out as `&mut Service`).
    ///
    /// # Panics
    /// Panics if the current shard has no instance.
    #[inline]
    pub unsafe fn local_shared(&self) -> SharedPtr<Service> {
        assert!(self.local_is_initialized());
        match &self._instances[this_shard_id() as usize].service {
            Some(s) => s.clone(),
            // Invariant: checked by local_is_initialized() just above.
            None => unreachable!("sharded instance does not exist"),
        }
    }

    /// Checks whether the local instance has been initialized.
    #[inline]
    pub fn local_is_initialized(&self) -> bool {
        let id = this_shard_id() as usize;
        self._instances.len() > id && self._instances[id].service.is_some()
    }

    /// Fails with the "no sharded instance" error if the local instance is not initialized.
    fn check_local(&self) -> Result<(), ExceptionPtr> {
        if !self.local_is_initialized() {
            return Err(ExceptionPtr::new(NoShardedInstanceException::with_type(
                std::any::type_name::<Service>(),
            )));
        }
        Ok(())
    }

    /// Returns a shared pointer to the local instance, or the "no sharded instance" error.
    ///
    /// # Safety
    /// As for [`Sharded::local_shared`].
    pub unsafe fn get_local_service(&self) -> Result<SharedPtr<Service>, ExceptionPtr> {
        self.check_local()?;
        match &self._instances[this_shard_id() as usize].service {
            Some(s) => Ok(s.clone()),
            None => Err(ExceptionPtr::new(NoShardedInstanceException::with_type(
                std::any::type_name::<Service>(),
            ))),
        }
    }
}

impl<Service: ShardedService> Default for Sharded<Service> {
    fn default() -> Self {
        Self::new()
    }
}

/// Destroys the object, which must not be started (stop it first).
///
/// # Panics
/// Panics if instances still exist.
impl<Service: ShardedService> Drop for Sharded<Service> {
    fn drop(&mut self) {
        assert!(self._instances.is_empty());
    }
}

/// Lets a `Sharded` object be used with the stop helpers of
/// [`closeable`](crate::engine::util::closeable). Stopping an object that is not started does
/// nothing.
impl<Service: ShardedService> crate::engine::util::closeable::Stoppable for Sharded<Service> {
    fn stop(&mut self) -> Future<()> {
        if self._instances.is_empty() {
            // Never started, or already stopped: there is nothing to stop, and no pointer to
            // the object may outlive this call.
            return make_ready_future(());
        }
        // SAFETY: `_instances` is only sized by start()/start_single(), which take the object
        // pinned; a pinned `!Unpin` value never moves again until it is dropped, so it is
        // still pinned here and stays in place while stop() runs.
        unsafe { Pin::new_unchecked(self) }.stop()
    }
}

/// A parameter of a [`Sharded`] call whose value depends on the shard: it is evaluated on the
/// target shard just before being passed to the local instance. Useful for the constructor of
/// [`Sharded::start`].
///
/// The function's own parameters are captured by `func`.
#[derive(Clone)]
pub struct ShardedParameter<F> {
    _func: F,
}

impl<F, R> ShardedParameter<F>
where
    F: Fn() -> R,
{
    /// Creates a parameter computed by `func` on the shard where it is evaluated. To pass the
    /// local instance of another `Sharded` object, capture
    /// [`EitherShardedOrLocal`](internal::EitherShardedOrLocal) in `func`.
    #[inline]
    pub fn new(func: F) -> Self {
        Self { _func: func }
    }

    /// Evaluates the parameter on the current shard.
    #[inline]
    pub fn evaluate(&self) -> R {
        (self._func)()
    }
}

/// A smart-pointer wrapper that is safe to move to another shard.
///
/// Because the engine avoids locking, most objects must be destroyed on the shard that created
/// them (their destructors may unlink them from that shard's containers), and the engine's shared
/// pointers use non-atomic reference counts, so they cannot be used on several shards at once.
///
/// `ForeignPtr` wraps such a pointer (`SharedPtr`, `LwSharedPtr`, `Box` or similar) and remembers
/// the shard it was created on. When it is dropped elsewhere, it sends the pointer back to that
/// shard to be destroyed there. It can be moved but not cloned (see [`copy`](Self::copy)). A null
/// `ForeignPtr` holds `None`.
pub struct ForeignPtr<P: std::ops::Deref + 'static> {
    _value: Option<P>,
    _cpu: u32,
}

// SAFETY: moving a foreign_ptr to another shard is what it exists for. The wrapped pointer's
// own state (a non-atomic count, the object's destructor) is only touched on its owner shard:
// it is destroyed there (`destroy_on`), copied there (`copy`), and taken out only through the
// unsafe `release`. The object is reached from another shard only by shared reference, and
// only when `P::Target: Sync` (`get`, `Deref`); on its owner shard, through `get_local`.
unsafe impl<P: std::ops::Deref + 'static> Send for ForeignPtr<P> {}

/// A value that only crosses shards to be dropped or used on its owner shard.
struct OwnerShardOnly<T>(T);

// SAFETY: every use below moves the value to its owner shard before touching it. One window
// remains: if `submit_to` fails before running the closure (a timed-out service group), the closure and the value in it are dropped on the sending shard.
unsafe impl<T> Send for OwnerShardOnly<T> {}

impl<P: std::ops::Deref + 'static> ForeignPtr<P> {
    /// Destroys `p` on shard `cpu` without waiting; a pending or failed destruction is left to run
    /// in the background.
    fn destroy_value(p: Option<P>, cpu: u32) {
        // Called from Drop and other synchronous methods (like `reset`) that cannot wait for
        // this future.
        let f = Self::destroy_on(p, cpu);
        if !f.available() || f.failed() {
            crate::engine::core::internal::run_in_background::run_in_background(f);
        }
    }

    /// Destroys `p` on shard `cpu`: at once if that is the current shard, otherwise by a message
    /// to it; the future resolves when it is done.
    fn destroy_on(p: Option<P>, cpu: u32) -> Future<()> {
        if let Some(p) = p {
            if cpu != this_shard_id() {
                let v = OwnerShardOnly(p);
                return Smp::submit_to(cpu, move || {
                    // Drop the pointer explicitly here, on the owner shard: the closure itself is
                    // dropped on the shard that submitted it.
                    drop(v);
                });
            } else {
                drop(p);
            }
        }
        make_ready_future(())
    }

    /// Returns a null pointer.
    #[inline]
    pub fn null() -> Self {
        Self { _value: None, _cpu: this_shard_id() }
    }

    /// Wraps `value` and records the current shard as its owner.
    #[inline]
    pub fn new(value: P) -> Self {
        Self { _value: Some(value), _cpu: this_shard_id() }
    }

    /// Returns a future of a copy of this pointer (a clone of the wrapped pointer, made on the
    /// owner shard).
    ///
    /// On the owner shard the copy is made at once; elsewhere it is made on the owner shard.
    ///
    /// # Safety
    /// When called away from the owner shard, `self` must stay alive, in place and unmodified
    /// until the returned future resolves (the owner shard reads the pointer through its
    /// address).
    pub unsafe fn copy(&self) -> Future<ForeignPtr<P>>
    where
        P: Clone,
    {
        if self._cpu == this_shard_id() {
            return make_ready_future(ForeignPtr { _value: self._value.clone(), _cpu: self._cpu });
        }
        let this = OwnerShardOnly(self as *const Self);
        Smp::submit_to(self._cpu, move || {
            let this = this;
            // SAFETY: by the caller `self` is alive and unmodified; the clone runs on the
            // owner shard, the only one that touches the pointer's count.
            let v = unsafe { &*this.0 }._value.clone();
            Ok::<ForeignPtr<P>, ExceptionPtr>(ForeignPtr { _value: v, _cpu: this_shard_id() })
        })
    }

    /// Returns the wrapped object, or `None` for a null pointer.
    ///
    /// From any shard, so the object must be shareable between threads (`P::Target: Sync`);
    /// see [`ForeignPtr::get_local`] for any object on its owner shard.
    #[inline]
    pub fn get(&self) -> Option<&P::Target>
    where
        P::Target: Sync,
    {
        self._value.as_deref()
    }

    /// Returns the wrapped object when called on its owner shard (`None` elsewhere, or when null).
    #[inline]
    pub fn get_local(&self) -> Option<&P::Target> {
        if self._cpu == this_shard_id() { self._value.as_deref() } else { None }
    }

    /// Returns the wrapped object from any shard, whatever its type.
    ///
    /// # Safety
    /// While the reference is live, nothing writes to the object (through a cell or
    /// otherwise) on any shard, and on a shard other than the owner the reference is only used
    /// to read: it is not used to clone or drop the pointers the object holds (their counts
    /// are not atomic).
    #[inline]
    pub unsafe fn get_unchecked(&self) -> Option<&P::Target> {
        self._value.as_deref()
    }

    /// Returns the owner shard of this pointer.
    ///
    /// It changes with [`assign`](Self::assign) and [`reset`](Self::reset).
    #[inline]
    pub fn get_owner_shard(&self) -> u32 {
        self._cpu
    }

    /// Checks whether the wrapped pointer is non-null.
    #[inline]
    pub fn is_some(&self) -> bool {
        self._value.is_some()
    }

    /// Replaces this pointer with `other`; the previous one is destroyed on its owner shard.
    pub fn assign(&mut self, mut other: ForeignPtr<P>) {
        Self::destroy_value(self._value.take(), self._cpu);
        self._value = other._value.take();
        self._cpu = other._cpu;
    }

    /// Takes the wrapped pointer out, leaving this one null.
    ///
    /// The caller becomes responsible for destroying the pointer on its owner shard; it is best
    /// called on the owner shard.
    ///
    /// # Safety
    /// The returned pointer is used, copied and destroyed only on the owner shard
    /// (`get_owner_shard()` before the call), unless `P` itself may cross threads.
    #[inline]
    pub unsafe fn release(&mut self) -> Option<P> {
        self._value.take()
    }

    /// Replaces the wrapped pointer with `new_ptr`, owned by the current shard.
    ///
    /// The previous pointer is destroyed on its owner shard.
    pub fn reset(&mut self, new_ptr: Option<P>) {
        let old_ptr = std::mem::replace(&mut self._value, new_ptr);
        let old_cpu = self._cpu;
        self._cpu = this_shard_id();
        Self::destroy_value(old_ptr, old_cpu);
    }

    /// Destroys the wrapped pointer, leaving this one null.
    ///
    /// Returns a future that resolves when the pointer has been destroyed on its owner shard.
    pub fn destroy(&mut self) -> Future<()> {
        Self::destroy_on(self._value.take(), self._cpu)
    }
}

impl<P: std::ops::Deref + 'static> Default for ForeignPtr<P> {
    fn default() -> Self {
        Self::null()
    }
}

/// Accesses the wrapped object (from any shard, so `P::Target: Sync`, as for
/// [`ForeignPtr::get`]).
///
/// # Panics
/// Panics on a null pointer.
impl<P: std::ops::Deref + 'static> std::ops::Deref for ForeignPtr<P>
where
    P::Target: Sync,
{
    type Target = P::Target;

    #[inline]
    fn deref(&self) -> &P::Target {
        match &self._value {
            Some(p) => p,
            // `Deref` cannot report an error, so a null pointer panics.
            None => panic!("dereferencing a null foreign_ptr"),
        }
    }
}

/// Destroys the wrapped pointer on its owner shard (at once if that is the current shard,
/// otherwise by a message to it).
impl<P: std::ops::Deref + 'static> Drop for ForeignPtr<P> {
    fn drop(&mut self) {
        Self::destroy_value(self._value.take(), self._cpu);
    }
}

/// Wraps a pointer in a [`ForeignPtr`] owned by the current shard.
#[inline]
pub fn make_foreign<T: std::ops::Deref + 'static>(ptr: T) -> ForeignPtr<T> {
    ForeignPtr::new(ptr)
}

/// `ForeignPtr` is a smart pointer.
impl<P: std::ops::Deref + 'static> crate::engine::util::is_smart_ptr::IsSmartPtr for ForeignPtr<P> {}
