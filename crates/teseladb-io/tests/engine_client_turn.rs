//! The client engine of a process takes its turn with the node engines (D4), Linux only: see
//! [`body`]. Elsewhere the binary does nothing.

#[cfg(target_os = "linux")]
#[path = "engine_client_turn/body.rs"]
mod body;

fn main() {
    #[cfg(target_os = "linux")]
    body::main();
}
