//! Entry point of the `cov4_rest_app_test` program in `tests/cov4_rest_app_test.rs`. The engine
//! runs on Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../cov4_rest_app_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
