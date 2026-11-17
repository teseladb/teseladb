#![allow(unsafe_code)]
#![cfg(target_os = "linux")]
//! The I/O class of a scheduling group is destroyed with the group. Before, it stayed in the
//! I/O queue, and a group created later under the same index reused it: its requests were
//! accounted (and its metrics labelled) under the destroyed group's name, with its shares.
//!
//! Observed through the I/O queue's metrics: the class label of the requests of the new
//! group must be its own name.

use std::collections::BTreeSet;

use teseladb::engine::core::aligned_buffer::allocate_aligned_buffer;
use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::metrics::impl_fns::get_value_map;
use teseladb::engine::core::reactor::{
    create_scheduling_group_with_shares, destroy_scheduling_group,
};
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::scheduling::internal::scheduling_group_index;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb::engine::util::tmp_file::TmpDir;
use teseladb::engine_test_case;

/// The class labels of the I/O queue's operation counters.
fn io_classes() -> BTreeSet<String> {
    let registry = get_value_map();
    // SAFETY: this shard's registry, with no other reference alive across the lookup.
    let values = unsafe { registry.get_mut_unchecked() }.get_value_map();
    let mut classes = BTreeSet::new();
    if let Some(fam) = values.get(&Sstring::from("io_queue_total_operations")) {
        for (labels, _) in fam.iter() {
            if let Some(c) = labels.labels().get(&Sstring::from("class")) {
                classes.insert(c.as_str().to_string());
            }
        }
    }
    classes
}

/// Writes one block of `path` from a task of `sg`.
fn write_in(sg: SchedulingGroup, path: String) -> Result<(), ExceptionPtr> {
    with_scheduling_group(sg, move || {
        teseladb::engine::core::thread::async_(move || -> Result<(), ExceptionPtr> {
            let mut f = open_file_dma(&path, OpenFlags::RW | OpenFlags::CREATE).get()?;
            let buf = allocate_aligned_buffer(4096, 4096)?;
            // SAFETY: the buffer holds 4096 bytes and lives until the write resolves.
            let n = unsafe { f.dma_write(0, buf.get(), 4096, std::ptr::null_mut()) }.get()?;
            assert_eq!(n, 4096);
            f.close().get()?;
            Ok(())
        })
    })
    .get()
}

engine_test_case!(test_io_class_is_destroyed_with_its_group, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let path = t.get_path().join("io-class.tmp").to_string_lossy().into_owned();

        let a = create_scheduling_group_with_shares("upstream_io_a".into(), 100.0).get()?;
        write_in(a, path.clone())?;
        assert!(io_classes().contains("upstream_io_a"), "{:?}", io_classes());
        let index = scheduling_group_index(a);
        destroy_scheduling_group(a).get()?;

        let b = create_scheduling_group_with_shares("upstream_io_b".into(), 200.0).get()?;
        assert_eq!(scheduling_group_index(b), index, "the new group reuses the index");
        write_in(b, path)?;
        let classes = io_classes();
        destroy_scheduling_group(b).get()?;
        assert!(
            classes.contains("upstream_io_b"),
            "the new group's requests are not its own: {classes:?}"
        );
        assert!(
            !classes.contains("upstream_io_a"),
            "the destroyed group's class is still there: {classes:?}"
        );
        Ok(())
    })
});
