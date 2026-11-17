//! Entry point of the `syscall_handoff_perf` program in `benches/syscall_handoff_perf.rs`. The
//! engine runs on Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../syscall_handoff_perf.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
