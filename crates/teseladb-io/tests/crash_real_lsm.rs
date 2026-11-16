//! Real crashes through the engine's disk executor (Linux only): see `crash_real_lsm/linux.rs`.

#[cfg(target_os = "linux")]
#[path = "crash_real_lsm/linux.rs"]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
}
