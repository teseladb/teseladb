//! The C library's allocator (012 FR-022): glibc's arenas capped at the node's cores, and one
//! thread of the process that hands the arenas' free memory back to the system every
//! [`RELEASE_EVERY`].
//!
//! By default glibc makes up to eight arenas per CPU, and what the threads other than the main one
//! free stays in their arenas: in the sustained load of 002 a node held 1.6 GiB of live
//! allocations and 7.3 GiB resident.

use std::sync::Once;
use std::thread;
use std::time::Duration;

/// How often the free memory of the arenas goes back to the system.
pub const RELEASE_EVERY: Duration = Duration::from_secs(10);

/// The fewest arenas: one for the threads of the cores and one for the rest.
const MIN_ARENAS: usize = 2;

static RELEASER: Once = Once::new();

/// Arenas for a node of `cores` cores.
pub fn arenas_for(cores: u16) -> usize {
    usize::from(cores).max(MIN_ARENAS)
}

/// Caps the arenas for `cores` cores and starts the thread that releases free memory, once per
/// process. Returns the cap, or 0 when the C library has no arenas to cap (musl, macOS).
pub fn bound(cores: u16) -> usize {
    let arenas = arenas_for(cores);
    let capped = teseladb_sys::host::limit_allocator_arenas(arenas) == Some(true);
    if teseladb_sys::host::release_free_memory().is_some() {
        RELEASER.call_once(|| {
            let run = || loop {
                thread::sleep(RELEASE_EVERY);
                let _ = teseladb_sys::host::release_free_memory();
            };
            // Without the thread the node runs as before: the memory only goes back later.
            let _ = thread::Builder::new().name("memory-release".into()).spawn(run);
        });
    }
    if capped { arenas } else { 0 }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_node_gets_one_arena_per_core_and_never_fewer_than_two() {
        assert_eq!(arenas_for(1), 2);
        assert_eq!(arenas_for(2), 2);
        assert_eq!(arenas_for(12), 12);
    }

    #[test]
    fn the_cap_holds_only_where_the_library_has_arenas() {
        let glibc = cfg!(all(target_os = "linux", target_env = "gnu"));
        assert_eq!(bound(3), if glibc { 3 } else { 0 });
        // A second node in the process starts no second thread and caps again.
        assert_eq!(bound(3), if glibc { 3 } else { 0 });
    }
}
