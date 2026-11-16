//! Real crashes through the engine's disk executor (Linux only): see `crash_real_lsm/linux.rs`.

#[cfg(target_os = "linux")]
#[path = "crash_real_lsm/linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}
