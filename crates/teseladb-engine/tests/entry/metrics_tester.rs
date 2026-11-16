//! Entry point of the `metrics_tester` program in `tests/metrics_tester.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../metrics_tester.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
