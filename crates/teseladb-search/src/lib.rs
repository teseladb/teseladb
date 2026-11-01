//! TeselaDB's full-text and vector search (specification 011).
//!
//! The crate gathers `$text`, RefDb Search (`$search`, `$searchMeta`) and `$vectorSearch`. Like
//! the storage it does no I/O of its own. So far it holds the scoring core of research R2 and R3:
//!
//! - [`query::bm25`]: BM25 with the `boost · idf · tf` formula of RefDb Search and BM25F for the
//!   field weights of `$text` (FR-016).
//! - [`query::wand`]: Block-Max WAND top-k retrieval over posting lists cut into blocks of 128
//!   postings, whose bounds come from per-block impacts and the global statistics at the read
//!   timestamp (FR-009, FR-015).
//! - [`segment::fst`]: the term dictionary of a field, a minimal acyclic transducer from terms to
//!   `u64` that also runs automata over its terms for fuzzy, prefix and regex queries (FR-009,
//!   FR-018).
//! - [`query::fuzzy`] and [`query::regex`]: Levenshtein, prefix, regex and wildcard automata run
//!   over that dictionary to expand `fuzzy`, `autocomplete`, `regex` and `wildcard` queries
//!   (FR-018).
//! - [`definition`]: search and vector index definitions, checked and filled with their
//!   defaults as `refsearch` does (FR-006).
//! - [`vector`] and [`segment::hnsw`]: the similarities and scores of `$vectorSearch` and the
//!   HNSW graph of each vector field of a segment (FR-005, FR-017).
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod analysis;
pub mod definition;
pub mod index;
pub mod indexer;
pub mod query;
pub mod segment;
pub mod vector;
pub mod vector_indexer;
