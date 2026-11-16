//! Internal building blocks of the core runtime.
//!
//! These modules are shared by the reactor, its I/O backends and the I/O queues: kernel ABI
//! definitions, I/O request descriptors and sinks, cancellation intents, the pollable file
//! descriptor wrapper, poller traits, the stall detector and small utilities. They are public
//! so that sibling modules can use them, but they are not a stable user-facing API.

pub mod api_level;
pub mod buffer_allocator;
pub mod estimated_histogram;
pub mod io_desc;
pub mod io_intent;
pub mod io_request;
pub mod io_sink;
pub mod linux_abi;
pub mod poll;
pub mod pollable_fd;
pub mod read_state;
pub mod run_in_background;
pub mod stall_detector;
pub mod uname;
