//! Entry point of the `cov4_err_dns_test` program in `tests/cov4_err_dns_test.rs`.
//! The engine runs on Linux only (and the resolver with the `dns` feature); elsewhere this is
//! an empty program.

#[cfg(all(target_os = "linux", feature = "dns"))]
#[path = "../cov4_err_dns_test.rs"]
mod program;

fn main() {
    #[cfg(all(target_os = "linux", feature = "dns"))]
    program::main();
}
