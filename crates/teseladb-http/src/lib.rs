//! HTTP/1.1 of TeselaDB without I/O (013 FR-025, plan of 013; 019 FR-001 extends it): the
//! messages of RFC 9112 written and read as bytes, for the clients of AWS STS (REFDB-AWS) and of
//! the JWKS of an OpenID provider (REFDB-OIDC). The connection, with TLS, is `teseladb-io`'s.
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod client;
pub mod cors;
pub mod http1;
pub mod server1;
