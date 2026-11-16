//! Entry point of the `slab_test` program in `tests/slab_test.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../slab_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
