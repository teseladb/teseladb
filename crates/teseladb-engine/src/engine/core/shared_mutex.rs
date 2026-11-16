//! A shared/exclusive mutex for fibers of one shard, and RAII lock helpers.
//!
//! [`SharedMutex`] grants either many shared holders or one exclusive holder, queueing waiters
//! in FIFO order. [`with_shared`] and [`with_lock`] run a function under the mutex;
//! [`get_shared_lock`] and [`get_unique_lock`] return guards ([`SharedLock`], [`UniqueLock`])
//! that unlock on drop, for any type implementing the lockable traits of [`internal`].

use crate::engine::core::chunked_fifo::ChunkedFifo;
use crate::engine::core::do_with::Lockable;
use crate::engine::core::future::{
    Future, Futurize, FuturizeT, Promise, futurize_invoke, make_ready_future,
};

struct Waiter {
    pr: Promise<()>,
    for_write: bool,
}

/// Shared/exclusive mutual exclusion.
///
/// Like a reader-writer lock, this type provides protection
/// for a shared resource, with two levels of access protection: shared
/// and exclusive.  Shared access allows multiple tasks to access the
/// shared resource concurrently, while exclusive access allows just
/// one task to access the resource at a time.
///
/// Note that many engine tasks do not require protection at all,
/// since the engine scheduler is not preemptive; however tasks that do
/// (by waiting on a future) may require explicit locking.
///
/// [`with_shared`] and [`with_lock`] provide wrappers that release the mutex whether the
/// function succeeds or fails.
///
/// Waiters are served in FIFO order: a waiting exclusive locker blocks shared lockers that
/// arrive after it. See the semaphore for simpler mutual exclusion.
#[derive(Default)]
pub struct SharedMutex {
    readers: u32,
    writer: bool,
    waiters: ChunkedFifo<Waiter>,
}

impl SharedMutex {
    /// Creates an unlocked mutex.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the mutex for shared access.
    ///
    /// Returns a future that becomes ready when no exclusive access is granted to anyone.
    pub fn lock_shared(&mut self) -> Future<()> {
        if self.try_lock_shared() {
            return make_ready_future(());
        }
        self.waiters.push_back(Waiter { pr: Promise::new(), for_write: false });
        self.waiters.back_mut().pr.get_future()
    }

    /// Tries to lock the mutex for shared access without waiting.
    ///
    /// Returns true if and only if the lock was acquired for shared access.
    #[inline]
    pub fn try_lock_shared(&mut self) -> bool {
        if !self.writer && self.waiters.empty() {
            self.readers += 1;
            return true;
        }
        false
    }

    /// Unlocks the mutex after a previous call to [`SharedMutex::lock_shared`], waking
    /// waiters that can now proceed.
    #[inline]
    pub fn unlock_shared(&mut self) {
        assert!(self.readers > 0);
        self.readers -= 1;
        self.wake();
    }

    /// Locks the mutex for exclusive access.
    ///
    /// Returns a future that becomes ready when no access, shared or exclusive,
    /// is granted to anyone.
    pub fn lock(&mut self) -> Future<()> {
        if self.try_lock() {
            return make_ready_future(());
        }
        self.waiters.push_back(Waiter { pr: Promise::new(), for_write: true });
        self.waiters.back_mut().pr.get_future()
    }

    /// Tries to lock the mutex for exclusive access without waiting.
    ///
    /// Returns true if and only if the lock was acquired for exclusive access.
    #[inline]
    pub fn try_lock(&mut self) -> bool {
        if self.readers == 0 && !self.writer {
            self.writer = true;
            return true;
        }
        false
    }

    /// Unlocks the mutex after a previous call to [`SharedMutex::lock`], waking waiters that
    /// can now proceed.
    #[inline]
    pub fn unlock(&mut self) {
        assert!(self.writer);
        self.writer = false;
        self.wake();
    }

    fn wake(&mut self) {
        while !self.waiters.empty() {
            let readers = self.readers;
            let w = self.waiters.front_mut();
            // note: there is no exclusive holder when wake() runs
            if w.for_write {
                if readers == 0 {
                    w.pr.set_value(());
                    self.writer = true;
                    self.waiters.pop_front();
                }
                break;
            } else {
                // for read
                w.pr.set_value(());
                self.readers += 1;
                self.waiters.pop_front();
            }
        }
    }
}

impl Lockable for SharedMutex {
    #[inline]
    fn lock(&mut self) -> Future<()> {
        SharedMutex::lock(self)
    }
    #[inline]
    fn unlock(&mut self) {
        SharedMutex::unlock(self);
    }
}

/// Executes a function while holding shared access to a resource.
///
/// Executes a function while holding shared access to a resource.  When
/// the function's future resolves, the mutex is automatically unlocked.
///
/// `sm` guards access to the shared resource and `func` is invoked while it is held for
/// shared access. Returns whatever `func` returns, as a future.
///
/// # Safety
/// The mutex must outlive the returned future and stay at its address meanwhile: the
/// continuation unlocks it through a pointer.
pub unsafe fn with_shared<F, R>(sm: &mut SharedMutex, func: F) -> FuturizeT<R>
where
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    let p: *mut SharedMutex = sm;
    sm.lock_shared().then(move |()| {
        // SAFETY: the caller keeps the mutex alive until the returned future resolves.
        futurize_invoke(func).finally(move || unsafe { (*p).unlock_shared() })
    })
}

/// Executes a function while holding exclusive access to a resource.
///
/// Executes a function while holding exclusive access to a resource.  When
/// the function's future resolves, the mutex is automatically unlocked.
///
/// `sm` guards access to the shared resource and `func` is invoked while it is held for
/// exclusive access. Returns whatever `func` returns, as a future.
///
/// # Safety
/// The mutex must outlive the returned future and stay at its address meanwhile: the
/// continuation unlocks it through a pointer.
pub unsafe fn with_lock<F, R>(sm: &mut SharedMutex, func: F) -> FuturizeT<R>
where
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    let p: *mut SharedMutex = sm;
    sm.lock().then(move |()| {
        // SAFETY: the caller keeps the mutex alive until the returned future resolves.
        futurize_invoke(func).finally(move || unsafe { (*p).unlock() })
    })
}

/// The lockable traits accepted by [`get_unique_lock`] and [`get_shared_lock`].
pub mod internal {
    use super::*;

    /// An object that can be locked exclusively, asynchronously, and unlocked synchronously.
    pub trait FutureBasicLockable {
        /// Returns a future that resolves when the lock is taken.
        fn lock(&mut self) -> Future<()>;
        /// Releases the lock; must not fail.
        fn unlock(&mut self);
    }

    /// An object that can be locked in shared mode, asynchronously, and unlocked
    /// synchronously. It is called `basic` because it does not require a non-waiting
    /// `try_lock_shared`.
    pub trait FutureBasicSharedLockable {
        /// Returns a future that resolves when the shared lock is taken.
        fn lock_shared(&mut self) -> Future<()>;
        /// Releases the shared lock; must not fail.
        fn unlock_shared(&mut self);
    }

    impl FutureBasicLockable for SharedMutex {
        #[inline]
        fn lock(&mut self) -> Future<()> {
            SharedMutex::lock(self)
        }
        #[inline]
        fn unlock(&mut self) {
            SharedMutex::unlock(self);
        }
    }

    impl FutureBasicSharedLockable for SharedMutex {
        #[inline]
        fn lock_shared(&mut self) -> Future<()> {
            SharedMutex::lock_shared(self)
        }
        #[inline]
        fn unlock_shared(&mut self) {
            SharedMutex::unlock_shared(self);
        }
    }
}
/// A guard holding a shared lock on `T`; dropping it unlocks.
pub struct SharedLock<T: internal::FutureBasicSharedLockable> {
    t: *mut T,
}

impl<T: internal::FutureBasicSharedLockable> Drop for SharedLock<T> {
    fn drop(&mut self) {
        // SAFETY: the caller keeps the lockable alive until the lock is destroyed.
        unsafe { (*self.t).unlock_shared() };
    }
}

/// A guard holding an exclusive lock on `T`; dropping it unlocks.
pub struct UniqueLock<T: internal::FutureBasicLockable> {
    t: *mut T,
}

impl<T: internal::FutureBasicLockable> Drop for UniqueLock<T> {
    fn drop(&mut self) {
        // SAFETY: the caller keeps the lockable alive until the lock is destroyed.
        unsafe { (*self.t).unlock() };
    }
}

/// Locks `t` in shared mode and returns a future of a RAII [`SharedLock`] that unlocks it when
/// dropped.
///
/// # Safety
/// The lockable must outlive the returned future and the lock it gives, and stay at its
/// address meanwhile: the guard unlocks it through a pointer.
pub unsafe fn get_shared_lock<T: internal::FutureBasicSharedLockable + 'static>(
    t: &mut T,
) -> Future<SharedLock<T>> {
    let p: *mut T = t;
    t.lock_shared().then(move |()| make_ready_future(SharedLock { t: p }))
}

/// Locks `t` exclusively and returns a future of a RAII [`UniqueLock`] that unlocks it when
/// dropped.
///
/// # Safety
/// The lockable must outlive the returned future and the lock it gives, and stay at its
/// address meanwhile: the guard unlocks it through a pointer.
pub unsafe fn get_unique_lock<T: internal::FutureBasicLockable + 'static>(
    t: &mut T,
) -> Future<UniqueLock<T>> {
    let p: *mut T = t;
    t.lock().then(move |()| make_ready_future(UniqueLock { t: p }))
}
