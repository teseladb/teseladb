//! A name lookup whose completion comes after its engine stopped, Linux only: see [`body`].
//! Elsewhere the binary does nothing.

#[cfg(target_os = "linux")]
#[path = "engine_late_lookup/body.rs"]
mod body;

fn main() {
    #[cfg(target_os = "linux")]
    body::main();
}
