//! The waits of a node's start for its engine end at their limit, Linux only: see [`body`].
//! Elsewhere the binary does nothing.

#[cfg(target_os = "linux")]
#[path = "engine_slow_start/body.rs"]
mod body;

fn main() {
    #[cfg(target_os = "linux")]
    body::main();
}
