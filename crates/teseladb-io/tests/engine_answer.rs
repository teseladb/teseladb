//! The answers and waits of the engine without a lock shared between threads (023 FR-018,
//! FR-026): a channel made on a shard keeps its answers there, whoever sends them (the shard
//! itself, another shard, a thread that is not a shard), closes once its last sender goes with
//! nothing on its way, and wakes a parked receiver whose last sender went; a [`Waiter`] wakes an
//! engine thread parked on it from another shard and keeps a notification that came first.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::Smp;
use teseladb_io::engine::answer::{self, RecvTimeoutError, SendError, TryRecvError};
use teseladb_io::engine::stacks;
use teseladb_io::engine::wait::{self, Waiter};

const T: Duration = Duration::from_secs(10);

/// Another shard than this one, when there is one.
fn other_shard() -> u32 {
    (this_shard_id() + 1) % Smp::count().max(1)
}

// Answers from this shard, from another shard and from a thread that is not a shard all reach the
// engine thread that waits; the channel closes once the last sender is gone.
teseladb::engine_test_case!(answers_from_anywhere_reach_the_shard_that_asked, {
    make_coroutine(async move {
        stacks::run(Box::new(move || {
            let (tx, rx) = answer::channel::<u32>();
            tx.send(1).unwrap();
            let remote = tx.clone();
            assert!(wait::post(
                other_shard(),
                Box::new(move || {
                    for i in 0..100 {
                        remote.send(100 + i).unwrap();
                    }
                })
            ));
            let outside = tx.clone();
            let t = std::thread::spawn(move || {
                for i in 0..100 {
                    outside.send(1000 + i).unwrap();
                }
            });
            drop(tx);
            let mut got: Vec<u32> = rx.iter().collect();
            t.join().unwrap();
            got.sort_unstable();
            assert_eq!(got.len(), 201, "every answer, and then the channel closed");
            assert_eq!(got[0], 1);
            assert_eq!(got[1], 100);
            assert_eq!(got[200], 1099);
            assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
        }))
        .await
    })
});

// A receiver parked with no limit wakes when its last sender goes on another shard without
// sending: it learns at once that nothing comes.
teseladb::engine_test_case!(a_parked_receiver_wakes_when_its_last_sender_goes, {
    make_coroutine(async move {
        stacks::run(Box::new(move || {
            let (tx, rx) = answer::channel::<u8>();
            assert!(wait::post(
                other_shard(),
                Box::new(move || {
                    // Dropped later than the receiver parks.
                    drop(make_coroutine(async move {
                        sleep_steady(Duration::from_millis(20)).await?;
                        drop(tx);
                        Ok::<(), ExceptionPtr>(())
                    }));
                })
            ));
            let start = Instant::now();
            assert_eq!(rx.recv_timeout(T), Err(RecvTimeoutError::Disconnected));
            assert!(start.elapsed() < T / 2, "woken by the sender, not by the deadline");
        }))
        .await
    })
});

// A send to a receiver that is gone gives the value back, on the shard and from another one; a
// deadline that passed still takes what is there.
teseladb::engine_test_case!(a_send_after_the_receiver_went_gives_the_value_back, {
    make_coroutine(async move {
        stacks::run(Box::new(move || {
            let (tx, rx) = answer::channel::<String>();
            tx.send("kept".to_owned()).unwrap();
            assert_eq!(rx.recv_deadline(Instant::now()), Ok("kept".to_owned()));
            assert_eq!(rx.recv_deadline(Instant::now()), Err(RecvTimeoutError::Timeout));
            drop(rx);
            assert_eq!(tx.send("x".to_owned()), Err(SendError("x".to_owned())));
            let (back_tx, back) = answer::channel::<bool>();
            assert!(wait::post(
                other_shard(),
                Box::new(move || {
                    let refused = tx.send("y".to_owned()).is_err();
                    let _ = back_tx.send(refused);
                })
            ));
            assert_eq!(back.recv_timeout(T), Ok(true));
        }))
        .await
    })
});

// A waiter parks an engine thread until another shard notifies it, keeps a notification that came
// before the wait, and ends at its deadline without one.
teseladb::engine_test_case!(a_waiter_parks_an_engine_thread_until_another_shard_notifies, {
    make_coroutine(async move {
        stacks::run(Box::new(move || {
            assert!(wait::in_engine_thread());
            let w = Arc::new(Waiter::default());
            w.notify();
            assert!(w.wait(Some(Instant::now())), "kept for the next wait");
            let start = Instant::now();
            assert!(!w.wait(Some(start + Duration::from_millis(20))));
            assert!(start.elapsed() >= Duration::from_millis(20));
            let other = Arc::clone(&w);
            assert!(wait::post(
                other_shard(),
                Box::new(move || {
                    drop(make_coroutine(async move {
                        sleep_steady(Duration::from_millis(20)).await?;
                        other.notify();
                        Ok::<(), ExceptionPtr>(())
                    }));
                })
            ));
            let start = Instant::now();
            assert!(w.wait(Some(start + T)));
            assert!(start.elapsed() < T / 2, "woken by the notification");
            // From a thread that is not a shard as well.
            let other = Arc::clone(&w);
            let t = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                other.notify();
            });
            assert!(w.wait(Some(Instant::now() + T)));
            t.join().unwrap();
        }))
        .await
    })
});
