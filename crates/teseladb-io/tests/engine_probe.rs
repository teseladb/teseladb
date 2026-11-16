//! The engine's probe ([`EngineProbe`], 023 FR-028, FR-030) from every place that calls it: an
//! engine thread of a shard (where the administration port answers `/metrics`), a task of a
//! shard (its future), and a thread that is not a shard. A probe in an engine thread waits by
//! parking it, so its shard keeps running meanwhile: a wait that blocked the shard's thread never
//! sent the shard's messages to the others, and the probe never ended. The calls that block (a
//! `statvfs`, a file written and synced) go to threads that are not shards the same way
//! ([`wait::off_shard`], [`wait::spawn_off_shard`]).
//!
//! One engine per process (D4): a program with its own `main` that runs the cases one after
//! another in one engine of two shards; a case that does not end within its limit fails the
//! program without stopping the engine (a stuck shard would not stop).

#[cfg(target_os = "linux")]
mod cases {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use teseladb::engine::core::coroutine::make_coroutine;
    use teseladb::engine::core::future::ExceptionPtr;
    use teseladb::engine::core::reactor::internal_misc::run_in_background;
    use teseladb::engine::core::thread::async_ as in_thread;
    use teseladb_io::engine::{EngineConfig, EngineProbe, NodeEngine, wait};

    /// How long a case may take before it is taken for stuck.
    const LIMIT: Duration = Duration::from_secs(10);
    /// How long the slow shard takes to answer the probe.
    const SLOW: Duration = Duration::from_millis(300);

    type Case = fn(&NodeEngine, &EngineProbe);

    pub fn run() {
        let engine =
            NodeEngine::start(&EngineConfig { shards: 2, ..EngineConfig::default() }).unwrap();
        let probe = engine.probe();
        let cases: &[(&str, Case)] = &[
            ("a_probe_in_an_engine_thread_leaves_its_shard_running", in_an_engine_thread),
            ("the_metrics_in_an_engine_thread_have_every_shard", metrics_in_an_engine_thread),
            ("a_shards_task_gathers_every_shard_in_order", from_a_task),
            ("a_thread_that_is_not_a_shard_gathers_every_shard", from_a_thread),
            ("a_blocking_call_in_an_engine_thread_leaves_its_shard_running", blocking_call),
            ("a_blocking_call_that_panics_fails_and_the_shard_goes_on", blocking_panic),
            ("a_shards_task_hands_a_blocking_call_off_without_waiting", spawned_off),
        ];
        for (name, case) in cases {
            let start = Instant::now();
            case(&engine, &probe);
            println!("test {name} ... ok ({:?})", start.elapsed());
        }
        engine.stop();
        println!("test result: ok. {} passed", cases.len());
    }

    /// The answer `rx` gets within [`LIMIT`]; a case that does not answer ends the program.
    fn within<T>(rx: &mpsc::Receiver<T>, what: &str) -> T {
        match rx.recv_timeout(LIMIT) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "FAILED: {what} did not answer within {LIMIT:?} ({e}): the shard is stuck"
                );
                // A shard that is stuck does not stop, so the engine is left as it is.
                std::process::exit(1);
            }
        }
    }

    /// Runs `f` in an engine thread of shard 0 and gives its result to the test's thread.
    fn on_engine_thread<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> mpsc::Receiver<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        on_shard0(move || {
            run_in_background(in_thread(move || {
                assert!(wait::in_engine_thread());
                drop(tx.send(f()));
            }));
        });
        rx
    }

    /// Queues `f` on shard 0 from the test's thread.
    fn on_shard0(f: impl FnOnce() + Send + 'static) {
        assert!(wait::post(0, Box::new(f)), "the engine runs");
    }

    /// The defect of the load measurement: shard 0 serves `/metrics` in an engine thread while
    /// shard 1 is slow to answer. Shard 0 runs other work while it waits (a closure posted to it
    /// meanwhile runs before the probe ends), and the probe gets both shards.
    fn in_an_engine_thread(_engine: &NodeEngine, probe: &EngineProbe) {
        let p = probe.clone();
        let answer = on_engine_thread(move || {
            let started = Instant::now();
            let shards = p.on_each(|shard| {
                if shard == 1 {
                    // A slow shard, so that shard 0 has to wait for it.
                    std::thread::sleep(SLOW);
                }
                shard
            });
            (shards, started.elapsed())
        });
        // While shard 0's engine thread waits for shard 1, shard 0 still runs what it is given.
        std::thread::sleep(SLOW / 3);
        let (tx, rx) = mpsc::sync_channel(1);
        let asked = Instant::now();
        on_shard0(move || {
            let _ = tx.send(Instant::now());
        });
        let ran = within(&rx, "shard 0 while its engine thread waited for the probe");
        let (shards, waited) = within(&answer, "the probe in an engine thread");
        assert_eq!(shards, vec![0, 1]);
        assert!(waited >= SLOW, "the probe waited for the slow shard: {waited:?}");
        assert!(
            ran.duration_since(asked) < SLOW / 2,
            "shard 0 ran other work while its engine thread waited: {:?}",
            ran.duration_since(asked)
        );
    }

    /// `/metrics` as the administration port's handler gets it: every shard's families.
    fn metrics_in_an_engine_thread(_engine: &NodeEngine, probe: &EngineProbe) {
        let p = probe.clone();
        let answer = on_engine_thread(move || (p.metrics(), p.memory().len()));
        let (text, memories) = within(&answer, "the metrics in an engine thread");
        assert_eq!(memories, 2);
        assert!(text.contains("# TYPE teseladb_engine_memory_foreign_frees counter"), "{text}");
        for shard in ["shard=\"0\"", "shard=\"1\""] {
            assert!(text.contains(&format!("teseladb_engine_memory_foreign_frees{{{shard}}}")));
        }
    }

    /// A task of a shard awaits the future: every shard, in shard order.
    fn from_a_task(_engine: &NodeEngine, probe: &EngineProbe) {
        let (tx, rx) = mpsc::sync_channel(1);
        let p = probe.clone();
        on_shard0(move || {
            assert!(!wait::in_engine_thread());
            run_in_background(make_coroutine(async move {
                let shards = p.on_each_future(|shard| shard * 10).await?;
                drop(tx.send(shards));
                Ok::<(), ExceptionPtr>(())
            }));
        });
        assert_eq!(within(&rx, "the future on a shard's task"), vec![0, 10]);
    }

    /// The name of the calling thread.
    fn thread_name() -> String {
        std::thread::current().name().unwrap_or_default().to_owned()
    }

    /// A call that blocks (a `statvfs` of `/metrics`, a certificate written and synced), made from
    /// an engine thread of shard 0: it runs on a thread that is not a shard while shard 0 runs
    /// what it is given, and its result comes back.
    fn blocking_call(_engine: &NodeEngine, _probe: &EngineProbe) {
        let answer = on_engine_thread(|| {
            let started = Instant::now();
            let ran_on = wait::off_shard(|| {
                std::thread::sleep(SLOW);
                thread_name()
            });
            (ran_on.map_err(|e| e.to_string()), started.elapsed())
        });
        std::thread::sleep(SLOW / 3);
        let (tx, rx) = mpsc::sync_channel(1);
        let asked = Instant::now();
        on_shard0(move || {
            let _ = tx.send(Instant::now());
        });
        let ran = within(&rx, "shard 0 while its engine thread waited for the blocking call");
        let (ran_on, waited) = within(&answer, "the blocking call");
        let ran_on = ran_on.expect("the call's result");
        assert!(ran_on.starts_with("offload-"), "it ran on {ran_on:?}");
        assert!(waited >= SLOW, "the engine thread waited for it: {waited:?}");
        assert!(
            ran.duration_since(asked) < SLOW / 2,
            "shard 0 ran other work meanwhile: {:?}",
            ran.duration_since(asked)
        );
    }

    /// A blocking call that panics fails the wait with an error; the engine thread goes on.
    fn blocking_panic(_engine: &NodeEngine, _probe: &EngineProbe) {
        let answer = on_engine_thread(|| {
            let failed = wait::off_shard(|| -> u8 { panic!("the call failed") });
            let after = wait::off_shard(|| 7u8);
            (failed.is_err(), after.ok())
        });
        assert_eq!(within(&answer, "a blocking call that panics"), (true, Some(7)));
    }

    /// A task of a shard cannot wait: `off_shard` says so, and `spawn_off_shard` hands the call
    /// to a thread that is not a shard without waiting for it.
    fn spawned_off(_engine: &NodeEngine, _probe: &EngineProbe) {
        let (tx, rx) = mpsc::sync_channel(1);
        on_shard0(move || {
            assert!(!wait::in_engine_thread());
            let refused = wait::off_shard(|| ()).map_err(|e| e.kind());
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            let started = Instant::now();
            wait::spawn_off_shard(move || {
                std::thread::sleep(SLOW);
                let _ = done_tx.send(thread_name());
            });
            let _ = tx.send((refused, started.elapsed(), done_rx));
        });
        let (refused, took, done) = within(&rx, "a shard's task");
        assert_eq!(refused, Err(std::io::ErrorKind::WouldBlock));
        assert!(took < SLOW / 2, "the task did not wait for the call: {took:?}");
        let ran_on = within(&done, "the call handed off");
        assert!(ran_on.starts_with("offload-"), "it ran on {ran_on:?}");
    }

    /// The tests of the hot path count allocations from their own thread.
    fn from_a_thread(_engine: &NodeEngine, probe: &EngineProbe) {
        assert_eq!(wait::this_shard(), None);
        assert_eq!(probe.on_each(|shard| shard), vec![0, 1]);
        assert_eq!(probe.memory().len(), 2);
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    cases::run();
}
