//! Entry point of the `defect_refused_sleep_no_syscalls` program in
//! `tests/defect_refused_sleep_no_syscalls.rs`. The engine runs on Linux only; on other
//! targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../defect_refused_sleep_no_syscalls.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
