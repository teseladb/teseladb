//! Waiting on the engine from code that is written to block (023 FR-018, FR-019): the commands
//! of the clients' port run in engine threads ([`super::stacks`]) and wait for the cores'
//! answers ([`super::answer`]) or for a pause ([`sleep`]) by parking their engine thread, so the
//! shard runs its other connections meanwhile. The same calls block an ordinary thread (the HTTP
//! and administration ports until they move to the engine, and every thread outside Linux), so
//! the code that waits does not know where it runs.
//!
//! An engine thread that parks hands the shard to the other engine threads of the shard, which
//! share its thread-local state. A process sets [`set_switch_hooks`] once with what saves the
//! state of a running command before it parks and restores it when it resumes; without hooks
//! nothing is saved.
//!
//! Every engine wait here is in a non-generic function of this crate (the engine's awaits are
//! instantiated where they are written).

use std::any::Any;
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// What saves the thread-local state of a command before its engine thread parks and gives it
/// back when it resumes.
#[derive(Clone, Copy, Debug)]
pub struct SwitchHooks {
    /// Takes the state of the command running on this thread, leaving the defaults.
    pub save: fn() -> Box<dyn Any>,
    /// Puts back what [`SwitchHooks::save`] took.
    pub restore: fn(Box<dyn Any>),
}

static HOOKS: OnceLock<SwitchHooks> = OnceLock::new();

/// Sets the hooks of the process, once; `false` if they were already set.
pub fn set_switch_hooks(hooks: SwitchHooks) -> bool {
    HOOKS.set(hooks).is_ok()
}

/// The shard this thread is, if it is one of the engine's.
pub fn this_shard() -> Option<u32> {
    rt::this_shard()
}

/// Whether this code runs in an engine thread, where a wait parks the thread instead of
/// blocking the shard.
pub fn in_engine_thread() -> bool {
    rt::engine_thread_shard().is_some()
}

/// Runs `f` on `shard`: from a shard with `smp::submit_to`, from any other thread with `alien`.
/// `false` (and `f` dropped) when no engine runs in the process.
pub fn post(shard: u32, f: Box<dyn FnOnce() + Send>) -> bool {
    rt::post(shard, f)
}

/// Pauses the caller for `d`: an engine thread parks, any other thread sleeps.
pub fn sleep(d: Duration) {
    if rt::engine_thread_shard().is_none() {
        std::thread::sleep(d);
        return;
    }
    let p = rt::begin_park();
    rt::finish_park(p, Some(Instant::now() + d));
}

#[cfg(target_os = "linux")]
mod rt {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::Instant;

    use teseladb::engine::core::alien;
    use teseladb::engine::core::coroutine::make_coroutine;
    use teseladb::engine::core::future::ExceptionPtr;
    use teseladb::engine::core::reactor::engine_is_ready;
    use teseladb::engine::core::shard_id::this_shard_id;
    use teseladb::engine::core::smp::Smp;
    use teseladb::engine::core::thread::Thread;
    use teseladb::engine::core::timer::Timer;

    use super::super::post::Wake;
    use super::HOOKS;

    pub(super) fn this_shard() -> Option<u32> {
        engine_is_ready().then(this_shard_id)
    }

    pub(super) fn engine_thread_shard() -> Option<u32> {
        (engine_is_ready() && Thread::running_in_thread()).then(this_shard_id)
    }

    pub(super) fn post(shard: u32, f: Box<dyn FnOnce() + Send>) -> bool {
        if engine_is_ready() {
            drop(Smp::submit_to(shard, f));
            return true;
        }
        if alien::internal::default_instance().is_null() {
            return false;
        }
        #[allow(deprecated)]
        alien::run_on_default(shard, f);
        true
    }

    thread_local! {
        /// The engine threads of this shard parked in a wait, by number.
        static PARKED: RefCell<(u64, HashMap<u64, Rc<Wake>>)> =
            RefCell::new((0, HashMap::new()));
    }

    /// An engine thread about to park: its number on the shard, which a notification names.
    pub(crate) struct Parking {
        pub(super) id: u64,
        wake: Rc<Wake>,
    }

    /// Registers the running engine thread to park.
    pub(super) fn begin_park() -> Parking {
        let wake = Rc::new(Wake::default());
        let id = PARKED.with(|p| {
            let mut p = p.borrow_mut();
            p.0 += 1;
            let id = p.0;
            p.1.insert(id, Rc::clone(&wake));
            id
        });
        Parking { id, wake }
    }

    /// Wakes the engine thread of this shard parked as `id`, if it still is.
    pub(super) fn wake_parked(id: u64) {
        let wake = PARKED.with(|p| p.borrow().1.get(&id).cloned());
        if let Some(w) = wake {
            w.wake();
        }
    }

    /// Parks until [`wake_parked`] or `until`; the command's thread-local state is saved
    /// meanwhile.
    pub(super) fn finish_park(p: Parking, until: Option<Instant>) {
        let saved = HOOKS.get().map(|h| (h, (h.save)()));
        let mut timer = until.map(|at| {
            let w = Rc::downgrade(&p.wake);
            let mut t = Box::pin(Timer::with_callback(move || {
                if let Some(w) = w.upgrade() {
                    w.wake();
                }
            }));
            t.as_mut().arm_in(at.saturating_duration_since(Instant::now()));
            t
        });
        let w = Rc::clone(&p.wake);
        // Woken by a delivery or by the timer: it always ends, so nothing is left waiting.
        let _ = make_coroutine(async move {
            w.wait().await;
            Ok::<(), ExceptionPtr>(())
        })
        .get();
        if let Some(t) = timer.as_mut() {
            t.as_mut().cancel();
        }
        PARKED.with(|q| q.borrow_mut().1.remove(&p.id));
        if let Some((h, state)) = saved {
            (h.restore)(state);
        }
    }
}

#[cfg(target_os = "linux")]
fn parking_id(p: &rt::Parking) -> u64 {
    p.id
}

/// Who waits on a [`Waiter`].
#[derive(Debug, Clone, Copy)]
enum Parked {
    Nobody,
    /// An ordinary thread, on the condition variable.
    Thread,
    /// An engine thread of `shard`, parked as `id`.
    Shard {
        shard: u32,
        id: u64,
    },
}

#[derive(Debug)]
struct State {
    /// Set by [`Waiter::notify`], taken by a wait that returns.
    ready: bool,
    parked: Parked,
}

/// One waiting side and any number of notifying ones, on any threads: the receiving side of an
/// answer. A notification while nobody waits is kept for the next wait; one while an engine
/// thread waits goes to its shard as one message, and none when the waiter is already awake.
#[derive(Debug)]
pub struct Waiter {
    st: Mutex<State>,
    cv: Condvar,
}

impl Default for Waiter {
    fn default() -> Waiter {
        Waiter {
            st: Mutex::new(State { ready: false, parked: Parked::Nobody }),
            cv: Condvar::new(),
        }
    }
}

impl Waiter {
    /// Wakes the waiting side (or the next wait).
    pub fn notify(&self) {
        let parked = {
            let mut st = self.st.lock().unwrap_or_else(PoisonError::into_inner);
            st.ready = true;
            std::mem::replace(&mut st.parked, Parked::Nobody)
        };
        match parked {
            Parked::Nobody => {}
            Parked::Thread => self.cv.notify_all(),
            Parked::Shard { shard, id } => {
                // A shard that is gone has nobody to wake.
                let _ = rt::post(shard, Box::new(move || rt::wake_parked(id)));
            }
        }
    }

    /// Waits for a notification until `until` (`None`: no limit); whether one came. Takes it.
    pub fn wait(&self, until: Option<Instant>) -> bool {
        loop {
            let mut st = self.st.lock().unwrap_or_else(PoisonError::into_inner);
            if std::mem::take(&mut st.ready) {
                return true;
            }
            if until.is_some_and(|u| Instant::now() >= u) {
                return false;
            }
            if let Some(shard) = rt::engine_thread_shard() {
                let p = rt::begin_park();
                let id = parking_id(&p);
                st.parked = Parked::Shard { shard, id };
                drop(st);
                rt::finish_park(p, until);
                let mut st = self.st.lock().unwrap_or_else(PoisonError::into_inner);
                if matches!(st.parked, Parked::Shard { id: i, .. } if i == id) {
                    st.parked = Parked::Nobody;
                }
                continue;
            }
            st.parked = Parked::Thread;
            let st = match until {
                None => self.cv.wait(st).unwrap_or_else(PoisonError::into_inner),
                Some(u) => {
                    let left = u.saturating_duration_since(Instant::now());
                    self.cv.wait_timeout(st, left).unwrap_or_else(PoisonError::into_inner).0
                }
            };
            let mut st = st;
            if matches!(st.parked, Parked::Thread) {
                st.parked = Parked::Nobody;
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_notification_before_the_wait_is_kept_for_it() {
        let w = Waiter::default();
        w.notify();
        assert!(w.wait(Some(Instant::now())));
        assert!(!w.wait(Some(Instant::now() + Duration::from_millis(5))));
    }

    #[test]
    fn a_thread_waits_until_another_notifies() {
        let w = Arc::new(Waiter::default());
        let other = Arc::clone(&w);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            other.notify();
        });
        let start = Instant::now();
        assert!(w.wait(None));
        assert!(start.elapsed() >= Duration::from_millis(15));
        t.join().unwrap();
    }

    #[test]
    fn a_wait_with_a_deadline_ends_without_a_notification() {
        let w = Waiter::default();
        let start = Instant::now();
        assert!(!w.wait(Some(start + Duration::from_millis(10))));
        assert!(start.elapsed() >= Duration::from_millis(10));
        // A notification after a wait that gave up is the next wait's.
        w.notify();
        w.notify();
        assert!(w.wait(None));
        assert!(!w.wait(Some(Instant::now())));
    }

    #[test]
    fn outside_the_engine_a_sleep_sleeps_and_nothing_is_posted() {
        assert!(!in_engine_thread());
        assert_eq!(this_shard(), None);
        let start = Instant::now();
        sleep(Duration::from_millis(5));
        assert!(start.elapsed() >= Duration::from_millis(5));
        assert!(!post(0, Box::new(|| unreachable!())));
    }
}
