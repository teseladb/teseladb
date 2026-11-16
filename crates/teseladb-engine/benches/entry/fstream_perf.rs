//! Entry point of the `fstream_perf` program in `benches/fstream_perf.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../fstream_perf.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
