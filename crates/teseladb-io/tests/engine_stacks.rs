//! The engine threads of a shard under many commands at once (023 FR-018), Linux only: see
//! [`body`]. Elsewhere the binary does nothing.

#[cfg(target_os = "linux")]
#[path = "engine_stacks/body.rs"]
mod body;

fn main() {
    #[cfg(target_os = "linux")]
    body::main();
}
