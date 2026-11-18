//! Entry point of the `other_work_waits_test` program in `tests/other_work_waits_test.rs`.
//! The engine runs on Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../other_work_waits_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
