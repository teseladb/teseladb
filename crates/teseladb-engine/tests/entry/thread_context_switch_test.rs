//! Entry point of the `thread_context_switch_test` program in `tests/thread_context_switch_test.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../thread_context_switch_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
