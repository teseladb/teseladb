//! Entry point of the `allocator_test` program in `tests/allocator_test.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../allocator_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
