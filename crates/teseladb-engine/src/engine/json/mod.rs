//! JSON output: formatting of values and the JSON object model used by the HTTP API.
//!
//! [`formatter`] writes Rust values (numbers, strings, maps, sequences, dates) as JSON,
//! into a string or streamed to an output stream; [`json_elements`] provides named,
//! optional members and objects built from them, which render themselves as JSON.

pub mod formatter;
pub mod json_elements;
