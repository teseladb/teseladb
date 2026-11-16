//! Waiting on the engine from code that is written to block (023 FR-018, FR-019, FR-026): the
//! commands of the clients' port, the handlers of the node's ports and the shards' periodic tasks
//! run in engine threads ([`super::stacks`]) and wait for the cores' answers ([`super::answer`]),
//! for a notification ([`Waiter`]) or for a pause ([`sleep`]) by parking their engine thread, so
//! the shard runs its other work meanwhile. Nothing here takes a lock shared between threads: a
//! notification reaches a parked engine thread as one `smp` message to its shard.
//!
//! A thread that is not a shard (the process's own thread before and after its engine, a command
//! of the command line, the unit tests) has no engine thread to park: [`sleep`] sleeps, and a
//! [`Waiter`] looks again every [`THREAD_POLL`]; the answers it waits for go through a channel of
//! the standard library ([`super::answer`]).
//!
//! An engine thread that parks hands the shard to the other engine threads of the shard, which
//! share its thread-local state. A process sets [`set_switch_hooks`] once with what saves the
//! state of a running command before it parks and restores it when it resumes; without hooks
//! nothing is saved.
//!
//! Every engine wait here is in a non-generic function of this crate (the engine's awaits are
//! instantiated where they are written).

use std::any::Any;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How often a thread that is not a shard looks at a [`Waiter`] it waits on.
pub const THREAD_POLL: Duration = Duration::from_millis(1);

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

/// The shard of the engine thread this code runs in, if it runs in one.
pub(crate) fn engine_thread_shard() -> Option<u32> {
    rt::engine_thread_shard()
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

/// An engine thread about to park, by its number on its shard.
pub(crate) use rt::Parking;

/// Registers the running engine thread to park; it parks with [`finish_park`] or gives up with
/// [`cancel_park`]. Only in an engine thread.
pub(crate) fn begin_park() -> Parking {
    rt::begin_park()
}

/// The number on its shard that [`wake_parked`] wakes `p` by.
pub(crate) fn parking_id(p: &Parking) -> u64 {
    p.id
}

/// Parks until [`wake_parked`] names it or `until` passes; the command's thread-local state is
/// saved meanwhile.
pub(crate) fn finish_park(p: Parking, until: Option<Instant>) {
    rt::finish_park(p, until);
}

/// Gives up a park registered with [`begin_park`] without parking.
pub(crate) fn cancel_park(p: Parking) {
    rt::cancel_park(p);
}

/// Wakes the engine thread of this shard parked as `id`, if it still is.
pub(crate) fn wake_parked(id: u64) {
    rt::wake_parked(id);
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
        pub(crate) id: u64,
        wake: Rc<Wake>,
    }

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

    pub(super) fn wake_parked(id: u64) {
        let wake = PARKED.with(|p| p.borrow().1.get(&id).cloned());
        if let Some(w) = wake {
            w.wake();
        }
    }

    pub(super) fn cancel_park(p: Parking) {
        PARKED.with(|q| q.borrow_mut().1.remove(&p.id));
    }

    pub(super) fn finish_park(p: Parking, until: Option<Instant>) {
        let saved = HOOKS.get().map(|h| (h, (h.save)()));
        let mut timer = until.map(|at| {
            let w = Rc::downgrade(&p.wake);
            let mut t = Box::pin(<Timer>::with_callback(move || {
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
        cancel_park(p);
        if let Some((h, state)) = saved {
            (h.restore)(state);
        }
    }
}

/// Outside Linux there is no engine (D3): no thread is a shard, so nothing parks.
#[cfg(not(target_os = "linux"))]
mod rt {
    use std::time::Instant;

    pub(super) fn this_shard() -> Option<u32> {
        None
    }

    pub(super) fn engine_thread_shard() -> Option<u32> {
        None
    }

    pub(super) fn post(_shard: u32, _f: Box<dyn FnOnce() + Send>) -> bool {
        false
    }

    /// Never made: no thread is an engine thread.
    #[allow(dead_code, reason = "no engine thread outside Linux, so nothing parks")]
    pub(crate) struct Parking {
        pub(crate) id: u64,
    }

    pub(super) fn begin_park() -> Parking {
        unreachable!("no engine thread outside Linux")
    }

    pub(super) fn wake_parked(_id: u64) {}

    pub(super) fn cancel_park(_p: Parking) {}

    pub(super) fn finish_park(_p: Parking, _until: Option<Instant>) {
        unreachable!("no engine thread outside Linux")
    }
}

/// Bits of [`Waiter`]'s parked word that hold the parking number; the shard goes above them.
const ID_BITS: u32 = 40;
const ID_MASK: u64 = (1 << ID_BITS) - 1;

/// The parked word of the engine thread of `shard` parked as `id` (never 0: numbers start at 1).
fn parked_word(shard: u32, id: u64) -> u64 {
    (u64::from(shard) << ID_BITS) | (id & ID_MASK)
}

/// One waiting side and any number of notifying ones, on any threads (the stop and the wake of a
/// shard's task). A notification while nobody waits is kept for the next wait; one while an
/// engine thread waits goes to its shard as one message, and none when the waiter is already
/// awake. Two atomic words, no lock: `ready` says a notification came, `parked` names the engine
/// thread waiting (its shard and number), 0 when none does.
#[derive(Debug, Default)]
pub struct Waiter {
    ready: AtomicBool,
    parked: AtomicU64,
}

impl Waiter {
    /// Wakes the waiting side (or the next wait).
    pub fn notify(&self) {
        // Sequentially consistent with the waiter's store of `parked` and load of `ready`: either
        // the notifier sees the parked thread, or the waiter sees the notification.
        self.ready.store(true, Ordering::SeqCst);
        let word = self.parked.swap(0, Ordering::SeqCst);
        if word != 0 {
            let shard = u32::try_from(word >> ID_BITS).unwrap_or(u32::MAX);
            let id = word & ID_MASK;
            // A shard that is gone has nobody to wake.
            let _ = rt::post(shard, Box::new(move || rt::wake_parked(id)));
        }
    }

    /// Waits for a notification until `until` (`None`: no limit); whether one came. Takes it.
    pub fn wait(&self, until: Option<Instant>) -> bool {
        loop {
            if self.ready.swap(false, Ordering::SeqCst) {
                return true;
            }
            let now = Instant::now();
            if until.is_some_and(|u| now >= u) {
                return false;
            }
            let Some(shard) = rt::engine_thread_shard() else {
                // A thread that is not a shard looks again shortly.
                let left = until.map_or(THREAD_POLL, |u| u.saturating_duration_since(now));
                std::thread::sleep(left.min(THREAD_POLL));
                continue;
            };
            let p = rt::begin_park();
            let word = parked_word(shard, p.id);
            self.parked.store(word, Ordering::SeqCst);
            // A notification that came before the store found nobody to wake: take it back
            // unless the notifier already took the word (its wake is then on its way).
            if self.ready.load(Ordering::SeqCst)
                && self.parked.compare_exchange(word, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok()
            {
                rt::cancel_park(p);
                continue;
            }
            rt::finish_park(p, until);
            // Woken by the deadline: nobody is parked here any more.
            let _ = self.parked.compare_exchange(word, 0, Ordering::SeqCst, Ordering::SeqCst);
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
    fn a_thread_that_is_not_a_shard_parks_nobody() {
        // Its wait never names a parked engine thread, so a notification posts nothing.
        let w = Waiter::default();
        assert!(!w.wait(Some(Instant::now() + THREAD_POLL * 3)));
        assert_eq!(w.parked.load(Ordering::SeqCst), 0);
        w.notify();
        assert!(w.ready.load(Ordering::SeqCst));
    }

    #[test]
    fn the_parked_word_keeps_the_shard_and_the_number_apart() {
        let word = parked_word(7, 42);
        assert_ne!(word, 0);
        assert_eq!(word >> ID_BITS, 7);
        assert_eq!(word & ID_MASK, 42);
        assert_ne!(parked_word(0, 1), 0, "shard 0's first park is not 'nobody'");
    }

    #[test]
    fn outside_the_engine_a_sleep_sleeps_and_nothing_is_posted() {
        assert!(!in_engine_thread());
        assert_eq!(this_shard(), None);
        assert_eq!(engine_thread_shard(), None);
        let start = Instant::now();
        sleep(Duration::from_millis(5));
        assert!(start.elapsed() >= Duration::from_millis(5));
        assert!(!post(0, Box::new(|| unreachable!())));
    }
}
