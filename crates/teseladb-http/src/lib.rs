//! HTTP without I/O. The HTTP/1.1 client ([`client`]) of services such as AWS STS or the JWKS of an
//! `OpenID` provider, and the server side of an HTTP port: HTTP/1.1 ([`server1`]) and HTTP/2
//! ([`http2`], with HPACK in [`hpack`]) behind one connection ([`server`]), Server-Sent Events
//! ([`sse`]), CORS ([`cors`]), the limits ([`limits`]), the metrics ([`metrics`]) and MIME
//! multipart bodies ([`multipart`]). The sockets and TLS are the consumer's.
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
pub mod multipart;
pub mod server;
pub mod server1;
pub mod sse;
