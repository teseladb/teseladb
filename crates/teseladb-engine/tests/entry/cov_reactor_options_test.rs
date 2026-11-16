//! Entry point of the `cov_reactor_options_test` program in `tests/cov_reactor_options_test.rs`.
//! The engine runs on Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../cov_reactor_options_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
