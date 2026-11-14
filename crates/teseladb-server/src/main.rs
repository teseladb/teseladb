//! `teseladb`: see [`teseladb_server::program`].

#![forbid(unsafe_code)]

/// The memory of each client request comes from its thread's arena (012 FR-027); the rest, from
/// the system's allocator.
#[global_allocator]
static ALLOC: teseladb_sys::arena::ArenaAlloc = teseladb_sys::arena::ArenaAlloc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = teseladb_server::program(
        &args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &teseladb_sys::signal::install_stop_handler,
        &teseladb_sys::signal::stop_requested,
    );
    std::process::exit(code);
}
