//! The engine: a shared-nothing, per-core asynchronous runtime with its own reactor, memory
//! allocator, futures, networking stacks and protocol servers.
//!
//! The modules, from the bottom up:
//!
//! * [`core`]: the runtime itself. The per-shard reactor and its back ends (io_uring, linux-aio,
//!   epoll), the task scheduler and scheduling groups, futures and promises, timers, the
//!   per-core memory allocator, cross-shard messaging and sharded services, files and streams,
//!   the I/O scheduler, synchronisation primitives (semaphores, gates, condition variables),
//!   utility containers and smart pointers, and the metrics layer with its Prometheus and
//!   collectd exporters.
//! * [`coroutine`]: `async` helpers on top of the futures of `core` (parallel loops, generators,
//!   switching scheduling group, yielding).
//! * [`net`]: networking: the POSIX stack over the kernel's sockets and a native user-space
//!   TCP/IP stack (Ethernet, ARP, IPv4, TCP, UDP, DHCP) over DPDK or virtio devices, plus DNS
//!   resolution, TLS and socket addresses.
//! * [`http`], [`websocket`], [`rpc`] and [`json`]: protocol servers and clients built on the
//!   streams of `core` and the sockets of `net`.
//! * [`lz4`]: the LZ4 block and stream compressor used by the RPC layer.
//! * [`util`]: logging, backtraces, program options, closeable resources and other support
//!   code.
//! * [`testing`]: the test harness that runs test cases inside a running engine.
//! * [`engine_main`]: holds no items; the crate root already exposes every module.

pub mod core;
pub mod coroutine;
pub mod engine_main;
pub mod http;
pub mod json;
pub mod lz4;
pub mod net;
pub mod rpc;
pub mod testing;
pub mod util;
pub mod websocket;
