//! The answers of the cores to whoever asked (023 FR-018): a channel with the methods of the
//! standard library's `mpsc` channel, whose receiving side waits by parking its engine thread
//! ([`super::wait`]) when it runs in one and blocks its thread otherwise. A core answers from its
//! shard; the answer reaches a waiting shard as one `smp` message, and none when the receiver is
//! not parked (it takes the answer when it next looks).
//!
//! Any number of senders, one receiver. The receiver sees the channel closed once every sender
//! is gone and nothing is left in it; a send after the receiver is gone gives the value back.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvError, RecvTimeoutError, SendError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::wait::Waiter;

struct Chan<T> {
    queue: Mutex<VecDeque<T>>,
    senders: AtomicUsize,
    receiver: AtomicBool,
    waiter: Waiter,
}

impl<T> Chan<T> {
    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<T>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The sending side of an answer channel; cheap to clone, usable from any thread.
pub struct Sender<T> {
    chan: Arc<Chan<T>>,
}

/// The receiving side of an answer channel.
pub struct Receiver<T> {
    chan: Arc<Chan<T>>,
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

/// A new answer channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Chan {
        queue: Mutex::new(VecDeque::new()),
        senders: AtomicUsize::new(1),
        receiver: AtomicBool::new(true),
        waiter: Waiter::default(),
    });
    (Sender { chan: Arc::clone(&chan) }, Receiver { chan })
}

impl<T> Sender<T> {
    /// Hands `value` to the receiver.
    ///
    /// # Errors
    ///
    /// Gives `value` back when the receiver is gone.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        if !self.chan.receiver.load(Ordering::Acquire) {
            return Err(SendError(value));
        }
        self.chan.queue().push_back(value);
        self.chan.waiter.notify();
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        self.chan.senders.fetch_add(1, Ordering::AcqRel);
        Sender { chan: Arc::clone(&self.chan) }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.chan.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The receiver learns that nothing more comes.
            self.chan.waiter.notify();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.chan.receiver.store(false, Ordering::Release);
    }
}

impl<T> Receiver<T> {
    /// The next answer, if one is there.
    ///
    /// # Errors
    ///
    /// [`TryRecvError::Empty`] when none is there yet, [`TryRecvError::Disconnected`] when none
    /// will come.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        if let Some(v) = self.chan.queue().pop_front() {
            return Ok(v);
        }
        if self.chan.senders.load(Ordering::Acquire) == 0 {
            // A value sent just before the last sender went is taken first.
            return self.chan.queue().pop_front().ok_or(TryRecvError::Disconnected);
        }
        Err(TryRecvError::Empty)
    }

    /// Waits for the next answer until `until` (`None`: no limit).
    fn recv_until(&self, until: Option<Instant>) -> Result<T, RecvTimeoutError> {
        loop {
            match self.try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => {}
            }
            if !self.chan.waiter.wait(until) {
                // The deadline passed: an answer that came at the same time is still taken.
                return self.try_recv().map_err(|e| match e {
                    TryRecvError::Empty => RecvTimeoutError::Timeout,
                    TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                });
            }
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
}
