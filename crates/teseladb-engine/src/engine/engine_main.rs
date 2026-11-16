//! Crate-level entry module of the engine.
//!
//! It holds no code. Every subsystem of the engine is a public module of the crate, declared in
//! the crate root, so the crate itself is the single unit that applications import; nothing needs
//! to be re-exported here.
//!
//! The subsystems depend on each other in cycles (for example, futures use the backtrace
//! utilities, which use the string type, which is part of the core), so they are not split into
//! separately built units; they are compiled together as one crate.
