//! Opening the node's TCP ports: a port that cannot be opened fails the start with an error that
//! names the port and its address, so whoever runs the node knows which one another process
//! holds (as `refnode` names the address it could not listen on).

use std::io;
use std::net::{SocketAddr, TcpListener};

/// The clients' port (006 FR-005).
pub const CLIENTS: &str = "clients' port";
/// The port the nodes talk to each other on (005).
pub const PEERS: &str = "peer port";
/// The administration port (007).
pub const ADMIN: &str = "administration port";
/// The HTTP port of the REST API (019).
pub const HTTP: &str = "HTTP port";

/// Listens on `addr`, the node's `port`; an error keeps its kind and says which port it was.
pub fn bind(port: &str, addr: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(addr).map_err(|e| io::Error::new(e.kind(), format!("{port} {addr}: {e}")))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_port_in_use_is_named_in_the_error() {
        let held = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = held.local_addr().expect("its address");
        let e = bind(HTTP, addr).expect_err("the port is held");
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
        assert!(e.to_string().starts_with(&format!("HTTP port {addr}: ")), "{e}");
        let free = bind(PEERS, "127.0.0.1:0".parse().expect("an address")).expect("a free port");
        assert_ne!(free.local_addr().expect("its address").port(), 0);
    }
}
