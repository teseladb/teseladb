//! Entry points that connect the native TCP implementation to the generic network-stack
//! API: listening on a port and creating a client socket over a TCP-over-IPv4 instance.

/// [`tcpv4_listen`] returns a server socket listening on a port of a TCP/IPv4 instance;
/// [`tcpv4_socket`] returns a client socket that connects through it.
pub use crate::engine::net::tcp::{tcpv4_listen, tcpv4_socket};
