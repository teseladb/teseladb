//! Entry point of the `perf_aio_allocating_write` program in
//! `tests/perf_aio_allocating_write.rs`. The engine runs on Linux only; on other targets this
//! is an empty program.

#[cfg(target_os = "linux")]
#[path = "../perf_aio_allocating_write.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
