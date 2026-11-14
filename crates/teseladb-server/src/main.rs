//! `teseladb`: see [`teseladb_server::program`].

#![forbid(unsafe_code)]

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
