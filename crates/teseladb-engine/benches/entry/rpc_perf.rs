//! Entry point of the `rpc_perf` program in `benches/rpc_perf.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../rpc_perf.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
