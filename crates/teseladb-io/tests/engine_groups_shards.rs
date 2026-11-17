//! The disk classes' scheduling groups made once per engine (023 FR-012) when every shard asks
//! for them at once: the node's cores, its log and its outlets open their disks together on start.
//! Its own test binary, so that its case is the first to ask for the groups (`engine_groups.rs`
//! covers two callers on one shard).
#![cfg(target_os = "linux")]

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::smp::Smp;
use teseladb::engine_test_case;
use teseladb_io::engine::disk::IoGroups;

// Every shard asks for the groups at the same time before any exist: each gets the same groups,
// and no group is made twice (two of one name make the engine refuse its metrics).
engine_test_case!(the_groups_asked_for_by_every_shard_at_once_are_made_once, {
    make_coroutine(async move {
        assert!(Smp::count() >= 2, "the case needs a second shard");
        let asked: Vec<Future<IoGroups>> =
            (0..Smp::count()).map(|s| Smp::submit_to(s, IoGroups::get_or_create)).collect();
        let mut got = Vec::new();
        for f in asked {
            got.push(f.await);
        }
        let first = got[0].as_ref().map(|g| *g).map_err(ToString::to_string);
        for g in &got {
            assert_eq!(g.as_ref().map(|g| *g).map_err(ToString::to_string), first);
        }
        assert!(first.is_ok(), "the groups were made: {first:?}");
        Ok::<(), ExceptionPtr>(())
    })
});
