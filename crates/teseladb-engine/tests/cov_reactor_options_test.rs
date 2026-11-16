//! Starts the engine with each group of reactor and SMP options in a process of its own and
//! checks the exit code: polling modes, overprovisioning, memory limits and locking (with the
//! memory prefaulter), the tuning switches with a stalled reactor, disk properties given inline
//! and in a file, the linux-aio control block budget, the help screens, and the options that
//! must stop the start (malformed or contradictory disk properties, unavailable CPUs, bad
//! values, unknown options). A program with its own `main` (`harness = false`): one engine per
//! process, so each case runs this program again with `COV_REACTOR_CASE` naming it.
#![cfg(target_os = "linux")]
// The DMA reads and writes are unsafe engine interfaces; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;

const CASE_VAR: &str = "COV_REACTOR_CASE";
const DIR_VAR: &str = "COV_REACTOR_DIR";
const CASE_TIMEOUT: Duration = Duration::from_secs(120);
const BLOCK: usize = 4096;

/// One start of the engine: its options, the exit codes it may end with, and whether the
/// application stalls its reactor on purpose.
struct Case {
    name: &'static str,
    args: Vec<String>,
    exit_codes: &'static [i32],
    stall: bool,
}

const OK: &[i32] = &[0];
const INVALID: &[i32] = &[1];

fn case(name: &'static str, args: &[&str], exit_codes: &'static [i32]) -> Case {
    Case { name, args: args.iter().map(|s| s.to_string()).collect(), exit_codes, stall: false }
}

/// Disk properties of one disk at `mountpoint`, in the inline (flow) form.
fn disk(mountpoint: &Path, iops: u32) -> String {
    format!(
        "{{mountpoint: {}, read_iops: {iops}, read_bandwidth: 200M, write_iops: {iops}, \
         write_bandwidth: 100M, read_saturation_length: 64k, write_saturation_length: 128k, \
         duplex: true, rate_factor: 0.5}}",
        mountpoint.display()
    )
}

fn io_properties(disks: &[String]) -> String {
    format!("--io-properties={{disks: [{}]}}", disks.join(", "))
}

fn cases(dir: &Path) -> Vec<Case> {
    let props_file = dir.join("io_properties.yaml");
    let missing = dir.join("missing");
    let mut tuning = case(
        "tuning_switches_with_a_stall",
        &[
            "--task-quota-ms=0.2",
            "--io-latency-goal-ms=2",
            "--io-completion-notify-ms=1",
            "--max-task-backlog=100",
            "--blocked-reactor-notify-ms=5",
            "--blocked-reactor-reports-per-minute=100",
            "--blocked-reactor-report-format-oneline=0",
            "--relaxed-dma",
            "--linux-aio-nowait=0",
            "--unsafe-bypass-fsync=1",
            "--kernel-page-cache=1",
            "--abort-on-engine-bad-alloc",
            "--force-aio-syscalls=1",
            "--dump-memory-diagnostics-on-alloc-failure-kind=all",
            "--aio-fsync=0",
            "--no-handle-interrupt",
            "--mbind=0",
            "--enable-glibc-exception-scaling-workaround=0",
        ],
        OK,
    );
    tuning.stall = true;
    vec![
        case("poll_mode", &["--poll-mode", "--smp=1"], OK),
        case(
            "idle_polling_without_aio_polling",
            &["--idle-poll-time-us=50000", "--poll-aio=0"],
            OK,
        ),
        case("overprovisioned", &["--overprovisioned", "--thread-affinity=0"], OK),
        case("memory_locked_and_prefaulted", &["--memory=192M", "--lock-memory=1", "--smp=1"], OK),
        case("reserved_memory", &["--reserve-memory=512M"], OK),
        tuning,
        Case {
            name: "inline_disk_properties",
            args: vec![io_properties(&[disk(dir, 20000)])],
            exit_codes: OK,
            stall: false,
        },
        Case {
            name: "disk_properties_file",
            args: vec![format!("--io-properties-file={}", props_file.display())],
            exit_codes: OK,
            stall: false,
        },
        Case {
            name: "both_disk_property_sources",
            args: vec![
                io_properties(&[disk(dir, 20000)]),
                format!("--io-properties-file={}", props_file.display()),
            ],
            exit_codes: INVALID,
            stall: false,
        },
        case("disk_properties_not_a_map", &["--io-properties=[1, 2]"], INVALID),
        case("disk_properties_unknown_section", &["--io-properties={other: []}"], INVALID),
        Case {
            name: "disk_without_mountpoint_on_disk",
            args: vec![io_properties(&[disk(&missing, 20000)])],
            exit_codes: INVALID,
            stall: false,
        },
        Case {
            name: "two_disks_on_one_device",
            args: vec![io_properties(&[disk(dir, 20000), disk(dir, 10000)])],
            exit_codes: INVALID,
            stall: false,
        },
        Case {
            name: "disk_with_zero_rates",
            args: vec![io_properties(&[disk(dir, 0)])],
            exit_codes: INVALID,
            stall: false,
        },
        // One byte and one request per second. The properties parse and the start succeeds, as
        // in the reference (which only warns of the latency goal). Without hwloc, as in the
        // reference built without it
        // (resource.cc, allocate() without hwloc), only the default queue exists and the disk's
        // queue is never built, so the workload's I/O runs unthrottled and the case exits 0.
        // With hwloc the disk's queue is built and the workload's one-block write waits for
        // tokens the 1 B/s rate takes days to give, as the reference does: the case is left
        // out there.
        #[cfg(not(feature = "hwloc"))]
        Case {
            name: "disk_too_slow_for_any_request",
            args: vec![format!(
                "--io-properties={{disks: [{{mountpoint: {}, read_iops: 1, read_bandwidth: 1, \
                 write_iops: 1, write_bandwidth: 1}}]}}",
                dir.display()
            )],
            exit_codes: OK,
            stall: false,
        },
        case("unavailable_cpu", &["--cpuset=4095"], INVALID),
        // The lowered budget takes all the control blocks left; the kernel may still refuse a
        // context, which fails the start instead.
        case(
            "aio_budget_lowered",
            &["--reactor-backend=linux-aio", "--max-networking-io-control-blocks=500000"],
            &[0, 1],
        ),
        case(
            "aio_budget_exceeded",
            &["--reactor-backend=linux-aio", "--reserve-io-control-blocks=4000000"],
            INVALID,
        ),
        case("negative_task_quota", &["--task-quota-ms=-1"], INVALID),
        case("malformed_memory_size", &["--memory=lots"], INVALID),
        case("help", &["--help"], OK),
        case("help_engine", &["--help-engine"], OK),
        case("help_loggers", &["--help-loggers"], OK),
        case("unknown_option", &["--no-such-cov-option"], &[2]),
    ]
}

/// The application of every case that starts: some I/O, a sleep, a cross-shard call and,
/// when asked, a stall of the reactor longer than the stall threshold.
fn workload(dir: PathBuf, stall: bool) -> Result<(), ExceptionPtr> {
    let name = dir.join(format!("data_{}", std::process::id())).display().to_string();
    let mut f =
        open_file_dma(&name, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE).get()?;
    let mut buf = TemporaryBuffer::<u8>::aligned(BLOCK, BLOCK);
    // SAFETY: the buffer holds BLOCK writable bytes.
    unsafe { std::ptr::write_bytes(buf.get_write(), b'w', BLOCK) };
    // SAFETY: the buffer lives until the write resolves.
    let n = unsafe { f.dma_write::<u8>(0, buf.get(), BLOCK, std::ptr::null_mut()) }.get()?;
    assert_eq!(n, BLOCK);
    f.flush().get()?;
    // SAFETY: no I/O intent.
    let back = unsafe { f.dma_read_bulk::<u8>(0, BLOCK, std::ptr::null_mut()) }.get()?;
    assert!(back.as_slice()[..BLOCK].iter().all(|&c| c == b'w'));
    f.close().get()?;
    std::fs::remove_file(&name).map_err(ExceptionPtr::new)?;
    sleep(Duration::from_millis(30)).get()?;
    let last = Smp::count() - 1;
    assert_eq!(Smp::submit_to(last, || Ok::<u32, ExceptionPtr>(this_shard_id())).get()?, last);
    if stall {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(40) {
            std::hint::spin_loop();
        }
        sleep(Duration::from_millis(10)).get()?;
    }
    Ok(())
}

/// Runs case `name` in this process and returns the engine's exit code.
fn run_case(name: &str) -> i32 {
    let dir = PathBuf::from(std::env::var(DIR_VAR).expect("the directory of the run"));
    let Some(c) = cases(&dir).into_iter().find(|c| c.name == name) else {
        eprintln!("no case {name}");
        return 100;
    };
    let mut args = vec!["cov_reactor_options_test".to_string()];
    args.extend(c.args);
    let stall = c.stall;
    let mut app = AppTemplate::new(Default::default());
    app.run(args, move || {
        let dir = dir.clone();
        thread_async(move || workload(dir, stall))
    })
}

/// Runs every case in a child process and checks its exit code.
fn run_all() -> i32 {
    let dir = std::env::temp_dir().join(format!("cov_reactor_options_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the run's directory");
    std::fs::write(
        dir.join("io_properties.yaml"),
        format!(
            "disks:\n  - mountpoints: [{}]\n    read_iops: 1000\n    read_bandwidth: 50M\n    \
             write_iops: 1000\n    write_bandwidth: 50M\n",
            dir.display()
        ),
    )
    .expect("the properties file");
    let exe = std::env::current_exe().expect("this program");
    let mut failures = Vec::new();
    for c in cases(&dir) {
        let start = Instant::now();
        let mut child = Command::new(&exe)
            .env(CASE_VAR, c.name)
            .env(DIR_VAR, &dir)
            .spawn()
            .expect("the case's process");
        let status = loop {
            if let Some(status) = child.try_wait().expect("the case's status") {
                break Some(status);
            }
            if start.elapsed() > CASE_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let code = status.and_then(|s| s.code());
        println!("case {}: exit {:?} (expected one of {:?})", c.name, code, c.exit_codes);
        if !code.is_some_and(|code| c.exit_codes.contains(&code)) {
            failures.push(c.name);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if failures.is_empty() {
        0
    } else {
        eprintln!("cases with an unexpected exit: {failures:?}");
        1
    }
}

pub(crate) fn main() {
    let code = match std::env::var(CASE_VAR) {
        Ok(name) => run_case(&name),
        Err(_) => run_all(),
    };
    std::process::exit(code);
}
