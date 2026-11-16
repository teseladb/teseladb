//! The jobs of the node's ports in engine threads (023 T049): a job that waits for an answer
//! parks its engine thread and the shard runs the others meanwhile; a result comes back, a panic
//! fails the job's future, and a drain waits for the jobs still running.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb_io::engine::answer;
use teseladb_server::engine_jobs;
use teseladb_server::shard_client::block_on;

#[test]
fn a_job_that_waits_lets_the_shard_run_another_and_gives_its_result() {
    let got = block_on(|| {
        make_coroutine(async move {
            let (tx, rx) = answer::channel::<u32>();
            // The first job parks on the channel; the second answers it on the same shard.
            let waits = engine_jobs::run(move || rx.recv_timeout(Duration::from_secs(10)).ok());
            let answers = engine_jobs::run(move || tx.send(7).is_ok());
            let sent = answers.await?;
            let got = waits.await?;
            Ok::<_, ExceptionPtr>((sent, got))
        })
    })
    .expect("the engine ran the jobs");
    assert_eq!(got, (true, Some(7)));
}

#[test]
fn a_job_that_panics_fails_its_future_and_a_drain_ends() {
    let failed = block_on(|| {
        make_coroutine(async move {
            let r = engine_jobs::run(|| -> u8 { panic!("a handler failed") }).await;
            Ok::<_, ExceptionPtr>(r.is_err())
        })
    })
    .expect("the engine ran the job");
    assert!(failed);
    let start = Instant::now();
    let drained = block_on(|| engine_jobs::drain(Duration::from_secs(10))).expect("drained");
    assert!(drained);
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(engine_jobs::running(), 0);
}

#[test]
fn a_spawned_job_runs_in_the_background() {
    let (tx, rx) = answer::channel::<&'static str>();
    block_on(move || {
        engine_jobs::spawn(move || {
            teseladb_io::engine::wait::sleep(Duration::from_millis(5));
            let _ = tx.send("done");
        });
        teseladb::engine::core::future::make_ready_future(())
    })
    .expect("spawned");
    // This thread is not a shard: the receiver blocks it until the job answers.
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok("done"));
}
