//! `teseladb`: see [`teseladb_server::program`]. Linux only (D-177): on other systems the binary
//! says so and points to the Docker image.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = teseladb_server::program(
        &args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &teseladb_sys::process::block_stop_signals,
        &teseladb_io::engine::signals::stop_requested,
    );
    std::process::exit(code);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("teseladb runs on Linux only; on this system use the Docker image teseladb/teseladb");
    std::process::exit(1);
}
