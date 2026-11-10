//! TeselaDB's hybrid logical clock (specification 005, FR-003, FR-024, FR-025; D-003).
//!
//! ```
//! use teseladb_hlc::{HlcClock, Next, succ, to_timestamp, from_timestamp};
//!
//! let clock = HlcClock::new(3).unwrap();
//! let Next::Mark(a) = clock.now(1_000) else { unreachable!() };
//! let Next::Mark(b) = clock.now(1_000) else { unreachable!() };
//! assert!(b > a && succ(a, 3) == b);
//! assert_eq!(from_timestamp(to_timestamp(b)), b);
//! ```
#![forbid(unsafe_code)]

mod clock;
pub mod guard;
mod persist;
mod timestamp;

pub use clock::{
    BadNode, HlcClock, LOGICAL_BITS, MAX_NODE, MAX_PHYSICAL, MAX_SEQ, NODE_BITS, Next, SEQ_BITS,
    node_of, seq_of, succ,
};
pub use guard::{ClockGuard, MAX_CLOCK_OFFSET_MS, PEER_WINDOW, PeerClocks, may_lead};
pub use persist::{AHEAD_MS, MAX_RECEIVED_AHEAD_MS, RENEW_MS, wait_on_start};
pub use teseladb_keys::Hlc;
pub use timestamp::{from_timestamp, to_timestamp};
