//! The task that drives a core on the engine (023 FR-009, FR-011, FR-015; T021, T039), with a toy
//! logic: ping-pong between two nodes and their ticks, disk operations in order, redial after a
//! refused connection, the address a peer announced, a peer that goes away, rejected handshakes,
//! slow readers and oversized frames, the stop that waits for the logic's writes, the rest ticks,
//! admission and the background class. The cases of the loop this replaces, on the engine.
//!
//! One engine per process (D4): a program with its own `main` that runs the cases one after
//! another in one engine of two shards, a toy node on each.

#[cfg(target_os = "linux")]
#[path = "engine_core_loop/cases.rs"]
mod cases;
#[cfg(target_os = "linux")]
#[path = "common/toy.rs"]
mod toy;

fn main() {
    #[cfg(target_os = "linux")]
    cases::run();
}
