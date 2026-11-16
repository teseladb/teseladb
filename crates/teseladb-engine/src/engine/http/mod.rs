//! HTTP/1.1 server and client on the per-core reactor.
//!
//! The server ([`httpd`]) runs one instance per shard: each accepts its own connections,
//! parses requests ([`request_parser`]), dispatches them through its routing table
//! ([`routes`], [`matcher`], [`matchrules`]) to handlers ([`handlers`],
//! [`function_handlers`], [`file_handler`], [`api_docs`]) and writes [`reply`]s, all
//! without cross-shard synchronisation. The client ([`client`]) keeps a pool of
//! connections to one server and parses responses ([`response_parser`]). Bodies may be
//! framed with a content length or with chunked transfer coding ([`chunk_parsers`],
//! [`common`], [`internal`]).

pub mod api_docs;
pub mod chunk_parsers;
pub mod client;
pub mod common;
pub mod connection_factory;
pub mod exception;
pub mod file_handler;
pub mod function_handlers;
pub mod handlers;
pub mod httpd;
pub mod internal;
pub mod json_path;
pub mod matcher;
pub mod matchrules;
pub mod mime_types;
pub mod reply;
pub mod request;
pub mod request_parser;
pub mod response_parser;
pub mod routes;
pub mod short_streams;
pub mod transformers;
pub mod url;
