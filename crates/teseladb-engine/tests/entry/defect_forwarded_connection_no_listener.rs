//! Entry point of the `defect_forwarded_connection_no_listener` program in
//! `tests/defect_forwarded_connection_no_listener.rs`. The engine runs on Linux only; on other
//! targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../defect_forwarded_connection_no_listener.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
