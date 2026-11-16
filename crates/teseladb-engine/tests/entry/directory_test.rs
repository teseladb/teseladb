//! Entry point of the `directory_test` program in `tests/directory_test.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../directory_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
