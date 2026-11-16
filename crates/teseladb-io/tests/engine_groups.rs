//! The disk classes' scheduling groups asked for by several callers before they exist (023
//! FR-012): a node's start asks for them from the clock's keeper and from the device log's writer
//! at once, both on shard 0. Each caller gets the same groups, made once; a second creation would
//! register the groups' metrics again under the same names, which the engine refuses. A process
//! of its own, so that no other test has made the groups before. Linux only: the engine runs on
//! Linux.
#![cfg(target_os = "linux")]

use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine_test_case;
use teseladb_io::IoClass;
use teseladb_io::engine::disk::{IoGroups, LOG_SHARES};

engine_test_case!(groups_asked_for_at_once_are_made_once, {
    // Both asked before either creation ends, as the keeper and the writer ask.
    let first: Future<IoGroups> = IoGroups::get_or_create();
    let second: Future<IoGroups> = IoGroups::get_or_create();
    first.then(move |a: IoGroups| {
        second.then(move |b: IoGroups| {
            assert_eq!(a, b, "one set of groups for every caller");
            assert_eq!(a.of(IoClass::Log).get_shares(), LOG_SHARES);
            // Once made, a later caller gets them at once.
            let third = IoGroups::get_or_create();
            assert!(third.available());
            assert_eq!(third.get().expect("the groups"), a);
            Ok::<_, ExceptionPtr>(())
        })
    })
});
