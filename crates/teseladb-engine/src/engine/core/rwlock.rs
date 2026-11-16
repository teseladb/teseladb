//! A read-write lock for fibers of one shard, built on a semaphore.
//!
//! [`BasicRwlock`] holds a semaphore with the maximum count: a reader takes one unit and a
//! writer takes all of them, so readers share the lock and a writer excludes everyone.
//! Waiters are served in FIFO order by the semaphore. The lock can wait with a deadline of
//! clock `C` or until an abort source fires, and [`RwlockHolder`] units release it on drop.
//! It is not a cross-shard lock.

use std::pin::Pin;

use crate::engine::core::abort_source::AbortSource;
use crate::engine::core::do_with::Lockable;
use crate::engine::core::future::Future;
use crate::engine::core::semaphore::{
    BasicSemaphore, SemaphoreDefaultExceptionFactory, SemaphoreUnits, get_units_abortable,
    get_units_until, try_get_units,
};
use crate::engine::core::timer::{SteadyClockType, TimerClock};

/// The rwlock seen as a read lock, with lock / unlock semantics usable with `with_lock`.
#[repr(transparent)]
pub struct RwlockForRead<C: TimerClock = SteadyClockType>(BasicRwlock<C>);

impl<C: TimerClock> Lockable for RwlockForRead<C> {
    #[inline]
    fn lock(&mut self) -> Future<()> {
        self.0.read_lock()
    }
    #[inline]
    fn unlock(&mut self) {
        self.0.read_unlock();
    }
}

/// The rwlock seen as a write lock, with lock / unlock semantics usable with `with_lock`.
#[repr(transparent)]
pub struct RwlockForWrite<C: TimerClock = SteadyClockType>(BasicRwlock<C>);

impl<C: TimerClock> Lockable for RwlockForWrite<C> {
    #[inline]
    fn lock(&mut self) -> Future<()> {
        self.0.write_lock()
    }
    #[inline]
    fn unlock(&mut self) {
        self.0.write_unlock();
    }
}
type SemaphoreType<C> = BasicSemaphore<SemaphoreDefaultExceptionFactory, C>;

/// Implements a read-write lock mechanism. Beware: this is not a cross-CPU
/// lock, due to the engine's sharded architecture.
/// Instead, it can be used to achieve rwlock semantics between two (or more)
/// fibers running in the same CPU that may use the same resource.
/// Acquiring the write lock will effectively cause all readers not to be executed
/// until the write part is done.
pub struct BasicRwlock<C: TimerClock = SteadyClockType> {
    sem: SemaphoreType<C>,
}

impl<C: TimerClock> Default for BasicRwlock<C> {
    fn default() -> Self {
        Self::new()
    }
}

/// The units of a held read or write lock; dropping them releases the lock.
pub type RwlockHolder<C = SteadyClockType> = SemaphoreUnits<SemaphoreDefaultExceptionFactory, C>;

impl<C: TimerClock> BasicRwlock<C> {
    const MAX_OPS: usize = SemaphoreType::<C>::max_counter();

    /// Creates an unlocked rwlock.
    #[inline]
    pub fn new() -> Self {
        Self { sem: SemaphoreType::<C>::new(Self::MAX_OPS) }
    }

    /// Cast this rwlock into read lock object with lock semantics appropriate to be used
    /// by "with_lock". The resulting object will have lock / unlock calls that, when called,
    /// will acquire / release the lock in read mode.
    #[inline]
    pub fn for_read(&mut self) -> &mut RwlockForRead<C> {
        // SAFETY: RwlockForRead is a repr(transparent) wrapper of BasicRwlock.
        unsafe { &mut *(self as *mut Self as *mut RwlockForRead<C>) }
    }

    /// Cast this rwlock into write lock object with lock semantics appropriate to be used
    /// by "with_lock". The resulting object will have lock / unlock calls that, when called,
    /// will acquire / release the lock in write mode.
    #[inline]
    pub fn for_write(&mut self) -> &mut RwlockForWrite<C> {
        // SAFETY: RwlockForWrite is a repr(transparent) wrapper of BasicRwlock.
        unsafe { &mut *(self as *mut Self as *mut RwlockForWrite<C>) }
    }

    /// Acquires this lock in read mode. Many readers are allowed, but when
    /// this future resolves, and until [`BasicRwlock::read_unlock`] is called, all fibers
    /// waiting on [`BasicRwlock::write_lock`] are guaranteed not to execute.
    #[inline]
    pub fn read_lock(&mut self) -> Future<()> {
        // A wait without a timeout arms no timer, so nothing points back at the lock.
        self.sem.wait(1)
    }

    /// Acquires this lock in read mode, failing with a semaphore timeout error if it cannot be
    /// taken by `timeout`.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn read_lock_until(&mut self, timeout: C::TimePoint) -> Future<()> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { self.sem.wait_until(timeout, 1) }
    }

    /// Acquires this lock in read mode, failing with the abort error if `as_` is aborted
    /// first.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn read_lock_abortable(&mut self, as_: Pin<&mut AbortSource>) -> Future<()> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { self.sem.wait_abortable(as_, 1) }
    }

    /// Releases the lock, which must have been taken in read mode. After this
    /// is called, one of the fibers waiting on [`BasicRwlock::write_lock`] will be allowed
    /// to proceed.
    #[inline]
    pub fn read_unlock(&mut self) {
        assert!(self.sem.current() < Self::MAX_OPS);
        self.sem.signal(1);
    }

    /// Acquires this lock in write mode. Only one writer is allowed. When
    /// this future resolves, and until [`BasicRwlock::write_unlock`] is called, all other
    /// fibers waiting on either [`BasicRwlock::read_lock`] or [`BasicRwlock::write_lock`] are guaranteed
    /// not to execute.
    #[inline]
    pub fn write_lock(&mut self) -> Future<()> {
        // A wait without a timeout arms no timer, so nothing points back at the lock.
        self.sem.wait(Self::MAX_OPS)
    }

    /// Acquires this lock in write mode, failing with a semaphore timeout error if it cannot
    /// be taken by `timeout`.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn write_lock_until(&mut self, timeout: C::TimePoint) -> Future<()> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { self.sem.wait_until(timeout, Self::MAX_OPS) }
    }

    /// Acquires this lock in write mode, failing with the abort error if `as_` is aborted
    /// first.
    #[inline]
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    pub unsafe fn write_lock_abortable(&mut self, as_: Pin<&mut AbortSource>) -> Future<()> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { self.sem.wait_abortable(as_, Self::MAX_OPS) }
    }

    /// Releases the lock, which must have been taken in write mode. After this
    /// is called, one of the other fibers waiting on [`BasicRwlock::write_lock`] or the fibers
    /// waiting on [`BasicRwlock::read_lock`] will be allowed to proceed.
    #[inline]
    pub fn write_unlock(&mut self) {
        assert!(self.sem.current() == 0);
        self.sem.signal(Self::MAX_OPS);
    }

    /// Tries to acquire the lock in read mode if and only if this can be done without
    /// waiting; returns whether it was acquired.
    #[inline]
    pub fn try_read_lock(&mut self) -> bool {
        self.sem.try_wait(1)
    }

    /// Tries to acquire the lock in write mode if and only if this can be done without
    /// waiting; returns whether it was acquired.
    #[inline]
    pub fn try_write_lock(&mut self) -> bool {
        self.sem.try_wait(Self::MAX_OPS)
    }

    /// Waits for a read lock and returns units which, when dropped, release the lock. This
    /// makes it easy to ensure that the lock is eventually released in every case, errors
    /// included. The units' `release()` method gives up their ownership of the lock and
    /// avoids the automatic unlock. Both `hold_read_lock` and `hold_write_lock` return the
    /// same type, [`RwlockHolder`].
    ///
    /// The returned future fails if the lock cannot be obtained.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn hold_read_lock(&mut self) -> Future<RwlockHolder<C>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.hold_read_lock_until(C::max_time_point()) }
    }

    /// [`BasicRwlock::hold_read_lock`] with a deadline: fails with a semaphore timeout error if
    /// the lock is not obtained by `timeout`.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn hold_read_lock_until(
        &mut self,
        timeout: C::TimePoint,
    ) -> Future<RwlockHolder<C>> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { get_units_until(&mut self.sem, 1, timeout) }
    }

    /// [`BasicRwlock::hold_read_lock`] that fails with the abort error if `as_` is aborted
    /// first.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn hold_read_lock_abortable(
        &mut self,
        as_: Pin<&mut AbortSource>,
    ) -> Future<RwlockHolder<C>> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { get_units_abortable(&mut self.sem, 1, as_) }
    }

    /// Synchronously tries to get a read lock and, if successful, returns units which, when
    /// dropped, release the lock; `None` if the lock is not available.
    #[inline]
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    pub unsafe fn try_hold_read_lock(&mut self) -> Option<RwlockHolder<C>> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { try_get_units(&mut self.sem, 1) }
    }

    /// Waits for a write lock and returns units which, when dropped, release the lock. The
    /// units' `release()` method gives up their ownership of the lock and avoids the
    /// automatic unlock. Both `hold_read_lock` and `hold_write_lock` return the same type,
    /// [`RwlockHolder`].
    ///
    /// The returned future fails if the lock cannot be obtained.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn hold_write_lock(&mut self) -> Future<RwlockHolder<C>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.hold_write_lock_until(C::max_time_point()) }
    }

    /// [`BasicRwlock::hold_write_lock`] with a deadline: fails with a semaphore timeout error
    /// if the lock is not obtained by `timeout`.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn hold_write_lock_until(
        &mut self,
        timeout: C::TimePoint,
    ) -> Future<RwlockHolder<C>> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { get_units_until(&mut self.sem, Self::MAX_OPS, timeout) }
    }

    /// [`BasicRwlock::hold_write_lock`] that fails with the abort error if `as_` is aborted
    /// first.
    ///
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    #[inline]
    pub unsafe fn hold_write_lock_abortable(
        &mut self,
        as_: Pin<&mut AbortSource>,
    ) -> Future<RwlockHolder<C>> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { get_units_abortable(&mut self.sem, Self::MAX_OPS, as_) }
    }

    /// Synchronously tries to get a write lock and, if successful, returns units which, when
    /// dropped, release the lock; `None` if the lock is not available.
    #[inline]
    /// # Safety
    /// The lock must outlive the wait and the returned units, and stay at its address
    /// meanwhile (see `semaphore::get_units`).
    pub unsafe fn try_hold_write_lock(&mut self) -> Option<RwlockHolder<C>> {
        // SAFETY: the lock owns the semaphore and must outlive the wait and the units.
        unsafe { try_get_units(&mut self.sem, Self::MAX_OPS) }
    }

    /// Checks if any read or write locks are currently held.
    #[inline]
    pub fn locked(&self) -> bool {
        self.sem.available_units() != Self::MAX_OPS as isize
    }
}

/// A read-write lock on the steady clock.
pub type Rwlock = BasicRwlock;
