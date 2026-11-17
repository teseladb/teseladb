//! Messages from foreign threads to the default instance (the one the application template
//! installs): `run_on_default` and `submit_to_default`, from several threads at once and in
//! bursts larger than one batch of the queue, with every message run exactly once and every
//! result delivered.

#![cfg(target_os = "linux")]
// The test covers the deprecated default-instance entry points on purpose.
#![allow(deprecated)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use teseladb::engine::core::alien;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError, make_ready_future};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine_test_case;

const THREADS: u64 = 4;
const MESSAGES: u64 = 3000;

engine_test_case!(default_instance_from_many_threads, {
    make_coroutine(async move {
        assert!(!alien::internal::default_instance().is_null());
        let ran = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for t in 0..THREADS {
            let ran = ran.clone();
            let failed = failed.clone();
            threads.push(std::thread::spawn(move || {
                let shard = (t as u32) % Smp::count();
                for _ in 0..MESSAGES {
                    let ran = ran.clone();
                    alien::run_on_default(shard, move || {
                        assert_eq!(this_shard_id(), shard);
                        ran.fetch_add(1, Ordering::Relaxed);
                    });
                }
                let rx: Vec<_> = (0..64u64)
                    .map(|i| alien::submit_to_default(shard, move || make_ready_future(i * t)))
                    .collect();
                let sum: u64 = rx.into_iter().map(|r| r.recv().expect("result").expect("ok")).sum();
                if sum != t * (0..64u64).sum::<u64>() {
                    failed.store(true, Ordering::Relaxed);
                }
                // A failed future comes back as the error.
                let e = alien::submit_to_default(shard, || {
                    teseladb::engine::core::future::make_exception_future::<u32>(ExceptionPtr::new(
                        RuntimeError::new("from the shard"),
                    ))
                })
                .recv()
                .expect("result");
                if !e.is_err_and(|e| e.to_string().contains("from the shard")) {
                    failed.store(true, Ordering::Relaxed);
                }
            }));
        }
        let mut waited = 0;
        while threads.iter().any(|t| !t.is_finished()) {
            sleep(Duration::from_millis(2)).await?;
            waited += 1;
            assert!(waited < 30_000, "the alien threads did not finish");
        }
        for t in threads {
            t.join().expect("thread");
        }
        // The run_on messages of every thread ran before its submit_to results came back (the
        // queue is FIFO per producer).
        assert_eq!(ran.load(Ordering::Relaxed), THREADS * MESSAGES);
        assert!(!failed.load(Ordering::Relaxed));
        Ok(())
    })
});
