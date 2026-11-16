//! Entry point of the `perf_tests_perf` program in `benches/perf_tests_perf.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../perf_tests_perf.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
