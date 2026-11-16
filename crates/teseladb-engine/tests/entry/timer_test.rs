//! Entry point of the `timer_test` program in `tests/timer_test.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../timer_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
