//! WebSocket server (RFC 6455).
//!
//! [`server`] accepts connections, performs the opening handshake and hands each
//! connection's data to the handler registered for its subprotocol; [`parser`] decodes
//! incoming frames; [`common`] holds the connection type, frame encoding and the
//! handshake key computation. Connections live on the shard that accepted them.

pub mod common;
pub mod parser;
pub mod server;
