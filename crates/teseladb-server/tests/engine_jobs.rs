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

#[test]
fn a_background_task_runs_in_the_background_group_until_it_is_stopped() {
    use teseladb::engine::core::scheduling::current_scheduling_group;
    use teseladb_io::IoClass;
    use teseladb_io::engine::disk::IoGroups;
    use teseladb_server::client_tasks::spawn_background;

    // The groups exist once the engine made them on a shard.
    let groups = block_on(IoGroups::get_or_create).expect("the groups");
    let (tx, rx) = answer::channel::<bool>();
    let task = spawn_background(
        0,
        Box::new(move |stop| {
            let _ = tx.send(current_scheduling_group() == groups.of(IoClass::Background));
            while !stop.stopped() {
                stop.pause(Duration::from_secs(10));
            }
        }),
    )
    .expect("an engine runs");
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
    let start = Instant::now();
    assert!(task.stop(), "it ended at the stop");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_background_task_runs_on_a_deep_stack_of_its_own() {
    use teseladb_server::client_tasks::spawn_background;

    // Its engine thread's own stack comes from the shard's memory, with no guard page in a
    // release build: the task runs on a stack with a guard below instead.
    // The engine runs (and has made its groups) once a shard answered.
    block_on(teseladb_io::engine::disk::IoGroups::get_or_create).expect("the groups");
    let (tx, rx) = answer::channel::<Option<usize>>();
    let task = spawn_background(
        0,
        Box::new(move |_| {
            let _ = tx.send(teseladb_sys::deep_stack::room_here());
        }),
    )
    .expect("an engine runs");
    let room = rx.recv_timeout(Duration::from_secs(10)).expect("the task ran");
    assert!(room.is_some_and(|r| r >= 1 << 20), "room below the task: {room:?}");
    assert!(task.stop());
}

/// Recurses `depth` times with a frame of about 1 KiB each.
#[inline(never)]
fn deep(depth: usize) -> usize {
    let frame = std::hint::black_box([u8::try_from(depth % 256).unwrap_or(0); 1024]);
    if depth == 0 {
        return usize::from(frame[0]);
    }
    1 + deep(std::hint::black_box(depth - 1))
}

#[test]
fn a_job_runs_deeper_than_its_engine_thread_on_a_stack_of_its_own_and_parks_there() {
    use teseladb_io::engine::stacks;
    use teseladb_server::client_listener::ENGINE_COMMAND_STACK;
    use teseladb_wire::commands::js::COMMAND_STACK;

    stacks::set_stack_size(ENGINE_COMMAND_STACK);
    let got = block_on(|| {
        engine_jobs::run(|| {
            let mut got = None;
            teseladb_sys::deep_stack::run(COMMAND_STACK, &mut |size| {
                // A wait on the deep stack parks the engine thread and comes back to it.
                teseladb_io::engine::wait::sleep(Duration::from_millis(5));
                // 64 MiB of frames: four times the engine thread's stack.
                got = Some((size, deep(64 << 10)));
            });
            got
        })
    })
    .expect("the engine ran the job");
    let (size, reached) = got.expect("the call ran");
    assert!(size.expect("a stack of its own") >= COMMAND_STACK);
    assert_eq!(reached, 64 << 10);
}

#[test]
fn an_outbound_call_from_an_engine_thread_waits_on_its_own_shard() {
    use teseladb::engine::core::sleep::sleep_steady;
    use teseladb_server::outbound;

    let got = block_on(|| {
        engine_jobs::run(|| {
            // A handler or a command calls an outbound client: no runner is needed, the work
            // runs on this shard while the engine thread parks.
            outbound::call(|| {
                make_coroutine(async {
                    sleep_steady(Duration::from_millis(5)).await?;
                    Ok::<u32, ExceptionPtr>(5)
                })
            })
        })
    })
    .expect("the engine ran the job");
    assert_eq!(got, Ok(5));
}

#[test]
fn block_on_from_an_engine_thread_runs_on_its_own_shard() {
    let got = block_on(|| {
        engine_jobs::run(|| {
            block_on(|| teseladb::engine::core::future::make_ready_future(9_u8))
                .map_err(|e| e.to_string())
        })
    })
    .expect("the engine ran the job");
    assert_eq!(got, Ok(9));
}
