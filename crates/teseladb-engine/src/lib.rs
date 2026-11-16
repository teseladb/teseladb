//! The engine of the database: a shared-nothing, thread-per-core asynchronous runtime and the
//! services built on it.
//!
//! The engine runs on Linux only: it is built on epoll, io_uring, linux-aio, `accept4` and ELF
//! thread-local sections, and links gnutls and c-ares. On every other target the crate is empty.
//!
#![cfg_attr(
    target_os = "linux",
    doc = r"Everything lives under [`engine`]. The runtime starts one OS thread per CPU core (a *shard*),
each with its own reactor, task queues, timers, memory allocator and I/O queues. Shards share
no mutable state: work moves between them by message passing
([`engine::core::smp`]), and per-shard copies of a service are kept with
[`engine::core::sharded`]. Inside a shard, code is cooperative: futures and coroutines run on
the reactor until they yield, so they must not block the thread.

Every program that links this crate allocates through the engine's per-core
allocator ([`engine::core::memory`]) unless it is built with
`--cfg engine_default_allocator`."
)]
// The engine is a runtime over raw system interfaces (syscalls, io_uring rings, its own memory
// allocator, FFI to gnutls and c-ares); every `unsafe` block carries a `// SAFETY:` comment.
#![allow(unsafe_code)]
#![warn(missing_docs)]

#[cfg(target_os = "linux")]
pub mod engine;

/// Every program that links this crate allocates through the engine's allocator, which is
/// built for the per-core memory layout of the runtime, unless it is built with the
/// `engine_default_allocator` configuration.
#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
#[global_allocator]
static ENGINE_GLOBAL_ALLOCATOR: engine::core::memory::EngineAllocator =
    engine::core::memory::EngineAllocator;
