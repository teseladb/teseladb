//! Entry point of the `hello_world` program in `examples/hello_world.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../hello_world.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
