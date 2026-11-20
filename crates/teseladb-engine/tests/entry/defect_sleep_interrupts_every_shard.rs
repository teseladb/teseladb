//! Entry point of the `defect_sleep_interrupts_every_shard` program in
//! `tests/defect_sleep_interrupts_every_shard.rs`. The engine runs on Linux only; on other
//! targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../defect_sleep_interrupts_every_shard.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
