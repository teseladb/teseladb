//! Entry point of the `app_template_test` program in `tests/app_template_test.rs`. The engine
//! runs on Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../app_template_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
