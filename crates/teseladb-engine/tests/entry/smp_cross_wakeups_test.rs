//! Entry point of the `smp_cross_wakeups_test` program in `tests/smp_cross_wakeups_test.rs`.
//! The engine runs on Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../smp_cross_wakeups_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
