//! Entry point of the `lz4_compare` program in `examples/lz4_compare.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../lz4_compare.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
