//! Fire-and-forget execution of futures on the current shard.
//!
//! A future passed here is kept alive by the reactor until it resolves; nobody waits for its
//! result (a failure is logged as a warning), and the reactor's background gate waits for all
//! such futures when the reactor closes.

use crate::engine::core::future::{Future, Futurize};

/// Runs a future to completion without anyone waiting for it.
pub use crate::engine::core::reactor::internal_misc::run_in_background;

/// Calls `func` and runs the future it returns in the background (see [`run_in_background`]).
///
/// If `func` returns a plain value or fails, the result is turned into a ready future first.
#[inline]
pub fn run_in_background_func<F, R>(func: F)
where
    F: FnOnce() -> R,
    R: Futurize<Output = ()>,
{
    let f: Future<()> = crate::engine::core::future::futurize_invoke(func);
    run_in_background(f);
}
