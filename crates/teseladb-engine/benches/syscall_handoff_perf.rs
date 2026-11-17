#![allow(unsafe_code)]
//! Round trip of one operation through the shard's syscall thread.
//!
//! Runs `--total-ops` `stat` calls of `/` one after the other (each one handed to the syscall
//! thread and completed back on the reactor) and prints the operations per second and the
//! latency percentiles in microseconds. The device plays no part, so this isolates the cost of
//! the hand-off between the reactor and its syscall thread.

use std::time::Instant;

use teseladb::engine::core::api::FollowSymlink;
use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::reactor::file_type;

pub(crate) fn main() {
    let mut at = AppTemplate::new(Default::default());
    at.add_options().opt(
        "total-ops",
        bpo::value::<u32>().default_value(200000),
        "Operations to run, one at a time",
    );
    let atp: *mut AppTemplate = &mut *at;
    let args: Vec<String> = std::env::args().collect();
    let status = at.run(args, move || {
        // SAFETY: the app outlives its run().
        let cfg = unsafe { (*atp).configuration() };
        let total_ops = *cfg.get::<u32>("total-ops").expect("default");
        make_coroutine(async move {
            let mut lat_ns: Vec<u64> = Vec::with_capacity(total_ops as usize);
            let start = Instant::now();
            for _ in 0..total_ops {
                let t = Instant::now();
                let ty = file_type("/", FollowSymlink::YES).await?;
                lat_ns.push(t.elapsed().as_nanos() as u64);
                assert!(ty.is_some(), "/ exists");
            }
            let secs = start.elapsed().as_secs_f64();
            lat_ns.sort_unstable();
            let pct = |p: f64| {
                let i = ((lat_ns.len() as f64 - 1.0) * p).round() as usize;
                lat_ns[i] as f64 / 1000.0
            };
            println!(
                "{:<10} {:<12} {:<8} {:<8} {:<8} {:<8}",
                "ops", "ops/s", "p50_us", "p90_us", "p99_us", "max_us"
            );
            println!(
                "{:>10} {:>12.0} {:>8.1} {:>8.1} {:>8.1} {:>8.1}",
                total_ops,
                total_ops as f64 / secs,
                pct(0.5),
                pct(0.9),
                pct(0.99),
                pct(1.0)
            );
            Ok(())
        })
    });
    std::process::exit(status);
}
