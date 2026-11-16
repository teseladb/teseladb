//! Entry point of the `rest_api_httpd` program in `tests/rest_api_httpd.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../rest_api_httpd.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
