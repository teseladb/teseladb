//! Entry point of the `alien_test` program in `tests/alien_test.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../alien_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
