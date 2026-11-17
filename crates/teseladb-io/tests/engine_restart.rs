//! Engines started one after the other in one process, Linux only: see [`body`]. Elsewhere the
//! binary does nothing.

#[cfg(target_os = "linux")]
#[path = "engine_restart/body.rs"]
mod body;

fn main() {
    #[cfg(target_os = "linux")]
    body::main();
}
