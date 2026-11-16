//! Real crashes through the engine's disk executor (Linux only): see `crash_real/linux.rs`.

#[cfg(target_os = "linux")]
#[path = "crash_real/linux.rs"]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
}
