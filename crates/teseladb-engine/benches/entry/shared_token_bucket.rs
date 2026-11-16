//! Entry point of the `shared_token_bucket` program in `benches/shared_token_bucket.rs`. The engine runs on Linux
//! only; on other targets this is an empty program.

#[cfg(target_os = "linux")]
#[path = "../shared_token_bucket.rs"]
mod program;

fn main() {
    #[cfg(target_os = "linux")]
    program::main();
}
