//! Entry point of the `defect_smp_message_waits_for_quota` program in
//! `tests/defect_smp_message_waits_for_quota.rs`. The engine runs on Linux only; on other
//! targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../defect_smp_message_waits_for_quota.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
