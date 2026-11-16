//! Entry point of the `scheduling_group_test` program in `tests/scheduling_group_test.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../scheduling_group_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
