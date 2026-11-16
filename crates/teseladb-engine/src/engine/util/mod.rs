//! General-purpose utilities used across the engine.
//!
//! These modules are independent of the reactor's internals and can be used by
//! applications too. The main groups are:
//!
//! - logging: [`log`], [`log_impl`], [`log_cli`], [`lazy`];
//! - process and file helpers: [`process`], [`file`](mod@file), [`tmp_file`], [`read_first_line`],
//!   [`short_streams`], [`iostream`];
//! - scheduling helpers built on futures: [`later`], [`closeable`],
//!   [`shared_token_bucket`];
//! - memory and allocation: [`alloc_failure_injector`], [`critical_alloc_section`],
//!   [`memory_diagnostics`], [`used_size`];
//! - small value types and adaptors: [`bool_class`], [`optimized_optional`],
//!   [`noncopyable_function`], [`reference_wrapper`], [`indirect`],
//!   [`function_input_iterator`], [`transform_iterator`], [`tuple_utils`],
//!   [`variant_utils`], [`defer`];
//! - diagnostics: [`backtrace`], [`print_safe`], [`assert`](mod@assert), [`sampler`];
//! - configuration: [`program_options`], [`conversions`], [`string_utils`].
//!
//! Most state here is per thread (one reactor per thread), matching the shared-nothing
//! design: nothing in this module synchronizes across shards unless it says so.

pub mod alloc_failure_injector;
pub mod assert;
pub mod backtrace;
pub mod bool_class;
pub mod closeable;
pub mod concepts;
pub mod conversions;
pub mod critical_alloc_section;
pub mod defer;
pub mod eclipse;
pub mod exceptions;
pub mod file;
pub mod function_input_iterator;
pub mod indirect;
pub(crate) mod initial_exec_tls;
pub mod internal;
pub mod iostream;
pub mod is_smart_ptr;
pub mod later;
pub mod lazy;
pub mod log;
pub mod log_cli;
pub mod log_impl;
pub mod memory_diagnostics;
pub mod modules;
pub mod noncopyable_function;
pub mod optimized_optional;
pub mod print_safe;
pub mod process;
pub mod program_options;
pub mod read_first_line;
pub mod reference_wrapper;
pub mod sampler;
pub mod shared_token_bucket;
pub mod short_streams;
pub mod source_location_compat;
pub mod spinlock;
pub mod std_compat;
pub mod string_utils;
pub mod tmp_file;
pub mod transform_iterator;
pub mod tuple_utils;
pub mod used_size;
pub mod variant_utils;
