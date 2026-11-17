//! The engine's probe ([`EngineProbe`], 023 FR-028, FR-030) from every place that calls it: an
//! engine thread of a shard (where the administration port answers `/metrics`), a task of a
//! shard (its future), and a thread that is not a shard. A probe in an engine thread waits by
//! parking it, so its shard keeps running meanwhile: a wait that blocked the shard's thread never
//! sent the shard's messages to the others, and the probe never ended. The calls that block (a
//! `statvfs`, a file written and synced) go to threads that are not shards the same way
//! ([`wait::off_shard`], [`wait::spawn_off_shard`]), and a client worker's read of a block of an
//! SST file goes through the engine's files ([`block_files`]).
//!
//! One engine per process (D4): a program with its own `main` that runs the cases one after
//! another in one engine of two shards; a case that does not end within its limit fails the
//! program without stopping the engine (a stuck shard would not stop).

#[cfg(target_os = "linux")]
mod cases {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use teseladb::engine::core::coroutine::make_coroutine;
    use teseladb::engine::core::future::ExceptionPtr;
    use teseladb::engine::core::reactor::internal_misc::run_in_background;
    use teseladb::engine::core::thread::async_ as in_thread;
    use teseladb_io::engine::block_files::{self, BlockFiles};
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
            ("a_block_read_in_an_engine_thread_leaves_its_shard_running", block_read_parks),
            ("a_block_read_keeps_its_commands_state_while_it_waits", block_read_keeps_state),
            ("block_reads_in_an_engine_thread_share_the_shards_files", block_reads_share),
            ("a_shards_task_does_not_wait_for_a_block", block_read_from_a_task),
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

    /// A new empty directory for a case's files.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("engine-probe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The read of a block a client worker makes outside the cores (005 FR-028), from an engine
    /// thread of shard 0, of a file whose open waits (a named pipe without a writer, as a slow
    /// disk would): the open and the read go through the engine, so shard 0 runs what it is given
    /// while they wait. Made with the system's calls on the shard's thread, shard 0 waits too.
    fn block_read_parks(_engine: &NodeEngine, _probe: &EngineProbe) {
        let dir = scratch("fifo");
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(made.success());
        let at = dir.clone();
        let answer = on_engine_thread(move || {
            let started = Instant::now();
            let mut files = BlockFiles::default();
            // A pipe read with an offset may fail: only the wait matters here.
            drop(files.read(&at, "fifo", 0, 4));
            drop(files);
            (started.elapsed(), block_files::shard_open_files())
        });
        // The writer that lets the open end comes after SLOW.
        let writer = std::thread::spawn(move || {
            std::thread::sleep(SLOW);
            let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).expect("writer");
            w.write_all(b"data").expect("write");
        });
        std::thread::sleep(SLOW / 3);
        let (tx, rx) = mpsc::sync_channel(1);
        let asked = Instant::now();
        on_shard0(move || {
            let _ = tx.send(Instant::now());
        });
        let ran = within(&rx, "shard 0 while its engine thread waited for a block");
        let (waited, open) = within(&answer, "the block read");
        writer.join().expect("the writer");
        assert!(waited >= SLOW / 2, "the read waited for the writer: {waited:?}");
        assert!(
            ran.duration_since(asked) < SLOW / 2,
            "shard 0 ran other work meanwhile: {:?}",
            ran.duration_since(asked)
        );
        assert_eq!(open, 0, "the reader let go of the file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    thread_local! {
        /// The state of the command running on a shard's thread, as a command's context is.
        static COMMAND: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn save_command() -> Box<dyn std::any::Any> {
        Box::new(COMMAND.with(|c| c.replace(0)))
    }

    fn restore_command(state: Box<dyn std::any::Any>) {
        if let Ok(v) = state.downcast::<u32>() {
            COMMAND.with(|c| c.set(*v));
        }
    }

    /// A command's thread-local state survives a block read that parks its engine thread while
    /// another command runs on the shard (023 FR-018): the read saves it with the process's
    /// switch hooks, as every other wait of an engine thread does.
    fn block_read_keeps_state(_engine: &NodeEngine, _probe: &EngineProbe) {
        assert!(wait::set_switch_hooks(wait::SwitchHooks {
            save: save_command,
            restore: restore_command
        }));
        let dir = scratch("state");
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(made.success());
        let at = dir.clone();
        let first = on_engine_thread(move || {
            COMMAND.with(|c| c.set(1));
            let mut files = BlockFiles::default();
            drop(files.read(&at, "fifo", 0, 4));
            drop(files);
            COMMAND.with(std::cell::Cell::get)
        });
        std::thread::sleep(SLOW / 3);
        // Another command on the shard while the first waits.
        let second = on_engine_thread(|| {
            let before = COMMAND.with(std::cell::Cell::get);
            COMMAND.with(|c| c.set(2));
            before
        });
        let seen = within(&second, "a second command while the first waits");
        let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).expect("writer");
        w.write_all(b"data").expect("write");
        drop(w);
        assert_eq!(seen, 0, "the second command does not see the first one's state");
        assert_eq!(within(&first, "the block read"), 1, "the first command's state came back");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Blocks read in engine threads of shard 0 are the file's bytes at any offset and length,
    /// short at its end; two readers of one file share the shard's open file, and the last to
    /// let go closes it (H-052).
    fn block_reads_share(_engine: &NodeEngine, _probe: &EngineProbe) {
        let dir = scratch("blocks");
        let bytes: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("sst"), &bytes).unwrap();
        let at = dir.clone();
        let answer = on_engine_thread(move || {
            let (mut a, mut b) = (BlockFiles::default(), BlockFiles::default());
            let reads = [(0u64, 4096usize), (1, 100), (4095, 4098), (12_345, 3000), (19_990, 100)]
                .map(|(offset, len)| a.read(&at, "sst", offset, len).map_err(|e| e.to_string()));
            let other = b.read(&at, "sst", 7, 9).map_err(|e| e.to_string());
            let shared = block_files::shard_open_files();
            a.release();
            let after_one = block_files::shard_open_files();
            b.release();
            (reads, other, (shared, after_one, block_files::shard_open_files()))
        });
        let (reads, other, open) = within(&answer, "block reads in an engine thread");
        let expect = [(0, 4096), (1, 100), (4095, 4098), (12_345, 3000), (19_990, 10)];
        for (got, (offset, len)) in reads.into_iter().zip(expect) {
            assert_eq!(got.expect("a block"), bytes[offset..offset + len], "at {offset}");
        }
        assert_eq!(other.expect("a block"), bytes[7..16]);
        assert_eq!(open, (1, 1, 0), "one open file for both, closed by the last");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A shard's task cannot wait for a block: the read says so at once (it goes to the core).
    fn block_read_from_a_task(_engine: &NodeEngine, _probe: &EngineProbe) {
        let (tx, rx) = mpsc::sync_channel(1);
        let exe = std::env::current_exe().unwrap();
        on_shard0(move || {
            let (dir, name) = (exe.parent().unwrap(), exe.file_name().unwrap().to_str().unwrap());
            let mut files = BlockFiles::default();
            let _ = tx.send(files.read(dir, name, 0, 16).map(|_| ()).map_err(|e| e.kind()));
        });
        let got = within(&rx, "a block read from a shard's task");
        assert_eq!(got, Err(std::io::ErrorKind::WouldBlock));
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
