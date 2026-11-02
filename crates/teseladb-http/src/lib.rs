//! HTTP of TeselaDB without I/O. 013 FR-025: the HTTP/1.1 client of AWS STS (REFDB-AWS) and of
//! the JWKS of an OpenID provider (REFDB-OIDC). 019 FR-001 to FR-004 and FR-030: the server of
//! the HTTP port, HTTP/1.1 ([`server1`]) and HTTP/2 ([`http2`], with HPACK in [`hpack`]) behind
//! one connection ([`server`]), Server-Sent Events ([`sse`]), CORS ([`cors`]), the limits
//! ([`limits`]) and the metrics ([`metrics`]). The sockets and TLS are `teseladb-server`'s.
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod client;
pub mod cors;
pub mod hpack;
pub mod http1;
pub mod http2;
pub mod http2_frames;
pub mod huffman;
pub mod limits;
pub mod metrics;
pub mod server;
pub mod server1;
pub mod sse;
