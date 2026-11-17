//! Entry point of the `defect_epoll_error_spin` program in `tests/defect_epoll_error_spin.rs`.
//! The engine runs on Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../defect_epoll_error_spin.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
