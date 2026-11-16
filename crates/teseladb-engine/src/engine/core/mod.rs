//! The runtime: reactor, tasks and futures, memory, shards, I/O, timers, synchronisation,
//! containers and metrics.
//!
//! Each shard runs one [`reactor::Reactor`] on its own OS thread. The reactor polls its I/O back
//! end, runs ready tasks from the run queues of its scheduling groups ([`scheduling`]), fires
//! timers ([`timer`]) and drains the cross-shard message queues ([`smp`]). Futures
//! ([`future`]) are the unit of asynchronous work; they are not `Send` and complete on the
//! shard that created them. Memory comes from a per-core allocator ([`memory`]) whose frees from
//! another shard go back to the owner.
//!
//! The rest of the module builds on that: sharded services ([`sharded`]), files and streams
//! ([`file`](mod@file), [`fstream`], [`iostream`]) with a per-shard I/O scheduler ([`io_queue`],
//! [`fair_queue`]), threads with their own stacks ([`thread`]), synchronisation primitives
//! ([`semaphore`], [`gate`], [`condition_variable`], [`shared_mutex`]), containers and smart
//! pointers tuned for single-shard use ([`circular_buffer`], [`chunked_fifo`], [`shared_ptr`],
//! [`sstring`]), and the metrics layer ([`metrics`], [`prometheus`], [`scollectd`]).

pub mod abort_on_ebadf;
pub mod abort_on_expiry;
pub mod abort_source;
pub mod abortable_fifo;
pub mod alien;
pub mod align;
pub mod aligned_buffer;
pub mod api;
pub mod app_template;
pub mod array_map;
pub mod bitops;
pub mod bitset_iter;
pub mod byteorder;
pub mod cacheline;
pub mod cgroup;
pub mod checked_ptr;
pub mod chunked_fifo;
pub mod circular_buffer;
pub mod circular_buffer_fixed_capacity;
pub mod condition_variable;
pub mod coroutine;
pub mod deleter;
pub mod distributed;
pub mod do_with;
pub mod dpdk_rte;
pub mod enum_;
pub mod exception_hacks;
pub mod execution_stage;
pub mod expiring_fifo;
pub mod fair_queue;
pub mod file;
pub mod file_impl;
pub mod file_types;
pub mod format;
pub mod fsnotify;
pub mod fsqual;
pub mod fstream;
pub mod function_traits;
pub mod future;
pub mod future_util;
pub mod gate;
pub mod idle_cpu_handler;
pub mod internal;
pub mod io_intent;
pub mod io_priority_class;
pub mod io_queue;
pub mod iostream;
pub mod iostream_impl;
pub mod layered_file;
pub mod linux_aio;
pub mod loop_;
pub mod lowres_clock;
pub mod make_task;
pub mod manual_clock;
pub mod map_reduce;
pub mod memory;
pub mod metrics;
pub mod metrics_api;
pub mod metrics_registration;
pub mod metrics_types;
pub mod on_internal_error;
pub mod parser_support;
pub mod pipe;
pub mod polymorphic_temporary_buffer;
pub mod posix;
pub mod preempt;
pub mod prefault;
pub mod prefetch;
pub mod print;
pub mod program_options;
pub mod prometheus;
pub mod queue;
pub mod reactor;
pub mod reactor_backend;
pub mod reactor_config;
pub mod relabel_config;
pub mod report_exception;
pub mod resource;
pub mod rwlock;
pub mod scattered_message;
pub mod scheduling;
pub mod scheduling_specific;
pub mod scollectd;
pub mod scollectd_api;
pub mod scollectd_impl;
pub mod semaphore;
pub mod shard_id;
pub mod sharded;
pub mod shared_future;
pub mod shared_mutex;
pub mod shared_ptr;
pub mod shared_ptr_debug_helper;
pub mod shared_ptr_incomplete;
pub mod signal;
pub mod simple_stream;
pub mod slab;
pub mod sleep;
pub mod smp;
pub mod smp_options;
pub mod sstring;
pub mod stall_sampler;
pub mod stream;
pub mod syscall_result;
pub mod syscall_work_queue;
pub mod systemwide_memory_barrier;
pub mod task;
pub mod temporary_buffer;
pub mod thread;
pub mod thread_cputime_clock;
pub mod thread_impl;
pub mod thread_pool;
pub mod timed_out_error;
pub mod timer;
pub mod timer_set;
pub mod transfer;
pub mod unaligned;
pub mod uname;
pub mod units;
pub mod vector_data_sink;
pub mod vla;
pub mod weak_ptr;
pub mod when_all;
pub mod when_any;
pub mod with_scheduling_group;
pub mod with_timeout;
