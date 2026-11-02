//! Query execution: scoring, top-k retrieval and term expansion.

pub mod bm25;
pub mod fuzzy;
pub(crate) mod paginate;
pub mod postings;
pub mod regex;
pub mod search;
pub(crate) mod sort;
pub mod vector_search;
pub mod wand;

#[cfg(test)]
mod explain_tests;
#[cfg(test)]
mod sort_tests;
