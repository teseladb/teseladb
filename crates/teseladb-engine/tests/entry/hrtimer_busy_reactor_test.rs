//! Entry point of the `hrtimer_busy_reactor_test` program in
//! `tests/hrtimer_busy_reactor_test.rs`. The engine runs on Linux only; on other targets
//! this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../hrtimer_busy_reactor_test.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
