//! The options of `teseladb start` translated into the engine's (023 FR-027, FR-033, T069;
//! `contracts/cli-engine.md`): each row of the contract, from the command line to the engine's
//! command line, through the node's configuration.
#![cfg(target_os = "linux")]

use teseladb_io::engine::resources::{allowed_cpus, engine_config};
use teseladb_io::engine::{DirectIo, EngineConfig, Placement};
use teseladb_server::cli::{Command, StartOpts, parse_with};
use teseladb_server::node_config;

fn start(s: &str) -> StartOpts {
    let args: Vec<String> = s.split_whitespace().map(String::from).collect();
    match parse_with(&args, &|_| Ok(Vec::new())) {
        Ok(Command::Start(o)) => *o,
        other => panic!("{s}: {other:?}"),
    }
}

/// The engine of `teseladb start` with `s`, as the node starts it.
fn engine_of(s: &str) -> EngineConfig {
    let cfg = node_config(&start(s));
    let cores = cfg.cores.unwrap_or(1);
    engine_config(cores, &cfg.placement, cfg.memory_limit, cfg.files).expect("it fits").0
}

/// The value after `flag` in the engine's command line.
fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

#[test]
fn cores_cpu_list_and_memory_become_smp_cpuset_and_memory() {
    let first = allowed_cpus()[0];
    let e = engine_of(&format!(
        "start --insecure --cores 1 --cpu-list {first} --memory-limit 256M --direct-io on"
    ));
    let a = e.args();
    assert_eq!(value(&a, "--smp").as_deref(), Some("1"));
    assert_eq!(value(&a, "--cpuset"), Some(first.to_string()));
    assert_eq!(value(&a, "--thread-affinity").as_deref(), Some("1"));
    assert_eq!(value(&a, "--mbind").as_deref(), Some("1"));
    assert_eq!(value(&a, "--memory"), Some((256u64 << 20).to_string()));
    assert_eq!(e.direct_io, DirectIo::On);
    assert!(!a.iter().any(|x| x == "--relaxed-dma" || x == "--kernel-page-cache"));
}

#[test]
fn without_a_cpu_list_the_shards_take_the_cpus_the_process_may_use() {
    let cfg = node_config(&start("start --insecure --cores 1"));
    assert_eq!(cfg.placement, Placement::Allowed);
    let e = engine_of("start --insecure --cores 1");
    assert_eq!(e.cpus, vec![allowed_cpus()[0]]);
    // Without --memory-limit, three quarters of the host within the engine's grant (FR-028).
    let m = e.memory.expect("always a memory");
    assert_eq!(m, teseladb_io::engine::resources::node_memory(None, 1));
    assert!(m > 0);
}

#[test]
fn direct_io_chooses_the_mode_of_the_engine_s_files() {
    let auto = engine_of("start --insecure --cores 1").args();
    assert!(auto.contains(&"--relaxed-dma".to_owned()), "{auto:?}");
    let off = engine_of("start --insecure --cores 1 --direct-io off").args();
    assert_eq!(value(&off, "--kernel-page-cache").as_deref(), Some("1"));
}

#[test]
fn the_fixed_rows_are_always_there() {
    let a = engine_of("start --insecure --cores 1").args();
    assert_eq!(value(&a, "--network-stack").as_deref(), Some("posix"));
    assert_eq!(value(&a, "--blocked-reactor-notify-ms").as_deref(), Some("25"));
    assert_eq!(value(&a, "--task-quota-ms").as_deref(), Some("0.5"));
    // The engine picks its backend (FR-033) and polls before sleeping: neither is set.
    assert!(!a.iter().any(|x| x == "--reactor-backend" || x == "--poll-mode"));
}

#[test]
fn a_cpu_list_with_fewer_cpus_than_cores_is_a_usage_error() {
    let args: Vec<String> =
        "start --insecure --cores 4 --cpu-list 0-1".split_whitespace().map(String::from).collect();
    let err = parse_with(&args, &|_| Ok(Vec::new())).expect_err("fewer CPUs").to_string();
    assert!(err.contains("--cpu-list names 2 CPUs for 4 cores"), "{err}");
}
