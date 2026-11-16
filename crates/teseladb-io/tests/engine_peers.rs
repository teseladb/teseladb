//! Connections between nodes on the engine (023 FR-015, FR-016; T039, T040): TLS 1.3 between
//! nodes of one CA and the refusals (013 FR-006), enrollments (FR-005) and joins answered by the
//! acceptor, rotated settings, and a node of two cores: the engine hands each connection to the
//! shard its source port names, a connection that reaches the other shard is relayed, and a core
//! dials from a port of its own.
//!
//! One engine per process (D4): a program with its own `main` that runs the cases in order.

#[cfg(target_os = "linux")]
#[path = "engine_peers/cases.rs"]
mod cases;
#[cfg(target_os = "linux")]
#[path = "common/tls.rs"]
mod tls;
#[cfg(target_os = "linux")]
#[path = "common/toy.rs"]
mod toy;

fn main() {
    #[cfg(target_os = "linux")]
    cases::run();
}
