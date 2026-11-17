//! Entry point of the `cov3_core_startup_options_test` program in
//! `tests/cov3_core_startup_options_test.rs`. The engine runs on Linux only; on other targets
//! this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../cov3_core_startup_options_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
