//! `teseladb`: see [`teseladb_server::run`].

#![forbid(unsafe_code)]

fn main() {
    if let Err(e) = teseladb_sys::signal::install_stop_handler() {
        eprintln!("teseladb: cannot handle stop signals: {e}");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = teseladb_server::run(
        &args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &teseladb_sys::signal::stop_requested,
    );
    std::process::exit(code);
}
