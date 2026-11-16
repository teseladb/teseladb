//! Entry point of the `engine_tls` measurement in `benches/engine_tls.rs`. The engine runs on
//! Linux only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../engine_tls.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
