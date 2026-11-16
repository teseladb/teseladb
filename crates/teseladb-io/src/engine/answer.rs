//! The answers of the cores to whoever asked (023 FR-018, FR-026): a channel with the methods of
//! the standard library's `mpsc` channel, any number of senders and one receiver, with no lock
//! shared between threads.
//!
//! A channel made on a shard (by a command, a handler or a task in an engine thread, or by a task
//! of the shard for one of them) keeps its answers on that shard: a sender on the shard puts its
//! answer there at once, a sender anywhere else sends it there as one `smp` message (`alien` from
//! a thread that is not a shard), and the receiver waits by parking its engine thread
//! ([`super::wait`]), so the shard runs its other work meanwhile. Its receiver is used on that
//! shard only.
//!
//! A channel made on a thread that is not a shard (the process's own thread before and after its
//! engine, a command of the command line, the unit tests) is the standard library's channel,
//! whose receiver blocks its thread.
//!
//! The receiver sees the channel closed once every sender is gone and nothing is left in it or on
//! its way; a send after the receiver is gone gives the value back.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
/// The errors of the standard library's channel, which this one gives the same way.
pub use std::sync::mpsc::{RecvError, RecvTimeoutError, SendError, TryRecvError};
use std::time::{Duration, Instant};

use super::wait;

/// What both sides of a channel kept on a shard share, wherever each runs.
#[derive(Debug)]
struct Shared {
    /// Senders alive.
    senders: AtomicUsize,
    /// Answers sent from another thread that have not reached the shard yet.
    in_flight: AtomicUsize,
    /// Whether the receiver is alive.
    receiver: AtomicBool,
    /// Whether the receiver is parked waiting: the last sender that goes wakes it.
    waiting: AtomicBool,
}

/// Where a channel keeps its answers.
enum Route<T> {
    /// On shard `shard`, in its slot `id`.
    Shard { shard: u32, id: u64, shared: Arc<Shared>, _of: PhantomData<fn(T) -> T> },
    /// In a channel of the standard library (a thread that is not a shard).
    Thread(mpsc::Sender<T>),
}

/// The sending side of an answer channel; cheap to clone, usable from any thread.
pub struct Sender<T> {
    route: Route<T>,
}

/// The receiving side of an answer channel.
pub struct Receiver<T> {
    inner: Inner<T>,
}

enum Inner<T> {
    Shard { shard: u32, id: u64, shared: Arc<Shared>, _of: PhantomData<fn(T) -> T> },
    Thread(mpsc::Receiver<T>),
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("answer::Sender")
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("answer::Receiver")
    }
}

/// The answers of the channels kept on this shard, by slot.
mod slots {
    use super::{Any, Cell, HashMap, Rc, RefCell, VecDeque};

    /// One channel's answers on its shard, and the engine thread parked on them.
    pub(super) struct Slot<T> {
        pub(super) queue: RefCell<VecDeque<T>>,
        /// The parking number of the receiver's engine thread, 0 when it is not parked.
        pub(super) parked: Rc<Cell<u64>>,
    }

    /// A slot of any type, with its parking number where code that does not know the type
    /// reads it.
    struct Entry {
        parked: Rc<Cell<u64>>,
        slot: Rc<dyn Any>,
    }

    thread_local! {
        static SLOTS: RefCell<(u64, HashMap<u64, Entry>)> = RefCell::new((0, HashMap::new()));
    }

    /// Opens a slot on this shard; its number.
    pub(super) fn open<T: 'static>() -> u64 {
        let parked = Rc::new(Cell::new(0));
        let slot =
            Rc::new(Slot::<T> { queue: RefCell::new(VecDeque::new()), parked: Rc::clone(&parked) });
        SLOTS.with(|s| {
            let mut s = s.borrow_mut();
            s.0 += 1;
            let id = s.0;
            s.1.insert(id, Entry { parked, slot });
            id
        })
    }

    /// Slot `id` of this shard, if it is still open.
    pub(super) fn get<T: 'static>(id: u64) -> Option<Rc<Slot<T>>> {
        let slot = SLOTS.with(|s| s.borrow().1.get(&id).map(|e| Rc::clone(&e.slot)))?;
        slot.downcast::<Slot<T>>().ok()
    }

    /// Closes slot `id`: the answers left in it go with it.
    pub(super) fn close(id: u64) {
        let gone = SLOTS.with(|s| s.borrow_mut().1.remove(&id));
        drop(gone);
    }

    /// Puts `value` in slot `id` and wakes its receiver; dropped when the slot is closed.
    pub(super) fn deliver<T: 'static>(id: u64, value: T) {
        if let Some(slot) = get::<T>(id) {
            slot.queue.borrow_mut().push_back(value);
            wake(id);
        }
    }

    /// Wakes the receiver of slot `id` if it is parked.
    pub(super) fn wake(id: u64) {
        let parked = SLOTS.with(|s| s.borrow().1.get(&id).map_or(0, |e| e.parked.get()));
        if parked != 0 {
            crate::engine::wait::wake_parked(parked);
        }
    }
}

/// A new answer channel: kept on this shard when made on one, the standard library's otherwise.
pub fn channel<T: Send + 'static>() -> (Sender<T>, Receiver<T>) {
    let Some(shard) = wait::this_shard() else {
        let (tx, rx) = mpsc::channel();
        return (Sender { route: Route::Thread(tx) }, Receiver { inner: Inner::Thread(rx) });
    };
    let id = slots::open::<T>();
    let shared = Arc::new(Shared {
        senders: AtomicUsize::new(1),
        in_flight: AtomicUsize::new(0),
        receiver: AtomicBool::new(true),
        waiting: AtomicBool::new(false),
    });
    let tx = Route::Shard { shard, id, shared: Arc::clone(&shared), _of: PhantomData };
    let rx = Inner::Shard { shard, id, shared, _of: PhantomData };
    (Sender { route: tx }, Receiver { inner: rx })
}

impl<T: Send + 'static> Sender<T> {
    /// Hands `value` to the receiver.
    ///
    /// # Errors
    ///
    /// Gives `value` back when the receiver is gone.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let (shard, id, shared) = match &self.route {
            Route::Thread(tx) => return tx.send(value),
            Route::Shard { shard, id, shared, .. } => (*shard, *id, shared),
        };
        if !shared.receiver.load(Ordering::Acquire) {
            return Err(SendError(value));
        }
        if wait::this_shard() == Some(shard) {
            slots::deliver(id, value);
            return Ok(());
        }
        // Counted before it leaves: the receiver does not see the channel closed while an answer
        // is on its way.
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        let arrived = Arc::clone(shared);
        let posted = wait::post(
            shard,
            Box::new(move || {
                slots::deliver(id, value);
                arrived.in_flight.fetch_sub(1, Ordering::SeqCst);
            }),
        );
        if !posted {
            // No engine runs any more: the receiver's shard went with it, and its receiver.
            shared.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        let route = match &self.route {
            Route::Thread(tx) => Route::Thread(tx.clone()),
            Route::Shard { shard, id, shared, .. } => {
                shared.senders.fetch_add(1, Ordering::SeqCst);
                Route::Shard {
                    shard: *shard,
                    id: *id,
                    shared: Arc::clone(shared),
                    _of: PhantomData,
                }
            }
        };
        Sender { route }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let Route::Shard { shard, id, shared, .. } = &self.route else { return };
        if shared.senders.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        // The last one: a receiver parked with nothing on its way learns that nothing more comes.
        // An answer on its way wakes it when it arrives.
        if !shared.waiting.load(Ordering::SeqCst) || shared.in_flight.load(Ordering::SeqCst) != 0 {
            return;
        }
        let (shard, id) = (*shard, *id);
        let wake = move || slots::wake(id);
        if wait::this_shard() == Some(shard) {
            wake();
        } else {
            let _ = wait::post(shard, Box::new(wake));
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let Inner::Shard { shard, id, shared, .. } = &self.inner else { return };
        shared.receiver.store(false, Ordering::Release);
        let (shard, id) = (*shard, *id);
        if wait::this_shard() == Some(shard) {
            slots::close(id);
        } else {
            let _ = wait::post(shard, Box::new(move || slots::close(id)));
        }
    }
}

impl<T: Send + 'static> Receiver<T> {
    /// This receiver's slot, on its shard.
    ///
    /// # Panics
    ///
    /// Off its shard: its answers are kept there.
    fn slot(shard: u32, id: u64) -> Option<Rc<slots::Slot<T>>> {
        let here = wait::this_shard();
        assert_eq!(here, Some(shard), "an answer kept on shard {shard} is received on {here:?}");
        slots::get::<T>(id)
    }

    /// The next answer, if one is there.
    ///
    /// # Errors
    ///
    /// [`TryRecvError::Empty`] when none is there yet, [`TryRecvError::Disconnected`] when none
    /// will come.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let (shard, id, shared) = match &self.inner {
            Inner::Thread(rx) => return rx.try_recv(),
            Inner::Shard { shard, id, shared, .. } => (*shard, *id, shared),
        };
        let Some(slot) = Self::slot(shard, id) else { return Err(TryRecvError::Disconnected) };
        if let Some(v) = slot.queue.borrow_mut().pop_front() {
            return Ok(v);
        }
        // The senders first: an answer counted on its way before the last one went is seen.
        if shared.senders.load(Ordering::SeqCst) == 0
            && shared.in_flight.load(Ordering::SeqCst) == 0
        {
            // An answer that arrived meanwhile on this shard is taken first.
            return slot.queue.borrow_mut().pop_front().ok_or(TryRecvError::Disconnected);
        }
        Err(TryRecvError::Empty)
    }

    /// Waits for the next answer until `until` (`None`: no limit).
    fn recv_until(&self, until: Option<Instant>) -> Result<T, RecvTimeoutError> {
        let (shard, id, shared) = match &self.inner {
            Inner::Thread(rx) => return thread_recv(rx, until),
            Inner::Shard { shard, id, shared, .. } => (*shard, *id, shared),
        };
        loop {
            match self.try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => {}
            }
            if until.is_some_and(|u| Instant::now() >= u) {
                return Err(RecvTimeoutError::Timeout);
            }
            let here = wait::engine_thread_shard();
            assert_eq!(
                here,
                Some(shard),
                "an answer is awaited on shard {shard} outside an engine thread: it would block \
                 the shard that delivers it"
            );
            let Some(slot) = slots::get::<T>(id) else {
                return Err(RecvTimeoutError::Disconnected);
            };
            let p = wait::begin_park();
            slot.parked.set(wait::parking_id(&p));
            shared.waiting.store(true, Ordering::SeqCst);
            // The last sender may have gone between the look above and `waiting`: it saw nobody
            // waiting, so this side looks again instead of parking.
            if shared.senders.load(Ordering::SeqCst) == 0 {
                wait::cancel_park(p);
            } else {
                wait::finish_park(p, until);
            }
            shared.waiting.store(false, Ordering::SeqCst);
            slot.parked.set(0);
        }
    }

    /// Waits up to `timeout` for the next answer.
    ///
    /// # Errors
    ///
    /// [`RecvTimeoutError::Timeout`] when none came in time, [`RecvTimeoutError::Disconnected`]
    /// when none will come.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_until(Instant::now().checked_add(timeout))
    }

    /// Waits until `deadline` for the next answer.
    ///
    /// # Errors
    ///
    /// As [`Receiver::recv_timeout`].
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.recv_until(Some(deadline))
    }

    /// Waits for the next answer.
    ///
    /// # Errors
    ///
    /// [`RecvError`] when none will come.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.recv_until(None).map_err(|_| RecvError)
    }

    /// The answers there now, without waiting.
    pub fn try_iter(&self) -> impl Iterator<Item = T> + '_ {
        std::iter::from_fn(move || self.try_recv().ok())
    }

    /// The answers until the channel closes, waiting for each.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        std::iter::from_fn(move || self.recv().ok())
    }
}

/// A wait of a thread that is not a shard on the standard library's channel.
fn thread_recv<T>(rx: &mpsc::Receiver<T>, until: Option<Instant>) -> Result<T, RecvTimeoutError> {
    match until {
        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        Some(u) => rx.recv_timeout(u.saturating_duration_since(Instant::now())),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn answers_come_in_order_and_the_channel_closes_with_its_last_sender() {
        let (tx, rx) = channel::<u32>();
        let tx2 = tx.clone();
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        tx.send(1).unwrap();
        tx2.send(2).unwrap();
        assert_eq!(rx.recv(), Ok(1));
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![2]);
        drop(tx);
        tx2.send(3).unwrap();
        drop(tx2);
        // What was sent before the last sender went is still taken.
        assert_eq!(rx.recv_timeout(Duration::from_millis(1)), Ok(3));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
        assert_eq!(rx.recv(), Err(RecvError));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected));
    }

    #[test]
    fn a_send_to_a_receiver_that_is_gone_gives_the_value_back() {
        let (tx, rx) = channel::<String>();
        drop(rx);
        assert_eq!(tx.send("x".to_owned()), Err(SendError("x".to_owned())));
    }

    #[test]
    fn a_wait_times_out_and_an_answer_from_another_thread_wakes_it() {
        let (tx, rx) = channel::<u8>();
        let start = Instant::now();
        assert_eq!(rx.recv_timeout(Duration::from_millis(10)), Err(RecvTimeoutError::Timeout));
        assert!(start.elapsed() >= Duration::from_millis(10));
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            tx.send(7).unwrap();
        });
        assert_eq!(rx.recv_deadline(Instant::now() + Duration::from_secs(10)), Ok(7));
        t.join().unwrap();
        assert_eq!(rx.iter().count(), 0);
    }

    #[test]
    fn a_deadline_that_passed_still_takes_an_answer_that_is_there() {
        let (tx, rx) = channel::<u8>();
        tx.send(4).unwrap();
        assert_eq!(rx.recv_deadline(Instant::now()), Ok(4));
        assert_eq!(rx.recv_deadline(Instant::now()), Err(RecvTimeoutError::Timeout));
    }

    #[test]
    fn many_senders_on_many_threads_deliver_everything() {
        let (tx, rx) = channel::<u32>();
        let threads: Vec<_> = (0..4)
            .map(|k| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for i in 0..250 {
                        tx.send(k * 1000 + i).unwrap();
                    }
                })
            })
            .collect();
        drop(tx);
        let mut got: Vec<u32> = rx.iter().collect();
        for t in threads {
            t.join().unwrap();
        }
        got.sort_unstable();
        assert_eq!(got.len(), 1000);
        assert_eq!(got[0], 0);
        assert_eq!(got[999], 3249);
    }

    #[test]
    fn a_slot_keeps_its_answers_until_it_closes() {
        // The slots of a shard, used here from the test's thread as a shard uses them.
        let id = slots::open::<u16>();
        slots::deliver(id, 5u16);
        let slot = slots::get::<u16>(id).expect("open");
        assert_eq!(slot.queue.borrow().len(), 1);
        assert!(slots::get::<u32>(id).is_none(), "another type is not this slot");
        slots::close(id);
        assert!(slots::get::<u16>(id).is_none());
        // An answer that arrives after the close is dropped.
        slots::deliver(id, 6u16);
        assert!(slots::get::<u16>(id).is_none());
    }
}
