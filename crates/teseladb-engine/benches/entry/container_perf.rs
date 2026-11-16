//! Entry point of the `container_perf` program in `benches/container_perf.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../container_perf.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
