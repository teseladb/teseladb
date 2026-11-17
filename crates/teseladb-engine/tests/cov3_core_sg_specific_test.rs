//! Values kept per scheduling group (the original's scheduling_specific.hh): reading a key's
//! value of a named group and of the current group, and the error for a group that does not
//! exist (any more).
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use teseladb::engine::core::reactor::{
    create_scheduling_group, destroy_scheduling_group, scheduling_group_key_create,
};
use teseladb::engine::core::scheduling::{
    SchedulingGroupKeyConfig, current_scheduling_group, make_scheduling_group_key_config,
};
use teseladb::engine::core::scheduling_specific::{
    scheduling_group_get_specific, scheduling_group_get_specific_current,
};
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb::engine_thread_test_case;

engine_thread_test_case!(values_of_named_current_and_missing_groups, {
    let conf: SchedulingGroupKeyConfig = make_scheduling_group_key_config::<u64, _>(|| 7u64);
    let key = scheduling_group_key_create(conf).get().unwrap();
    let sg = create_scheduling_group("cov3-sgs".into(), "c3s".into(), 100.0).get().unwrap();
    // SAFETY: `key` holds u64 values; the references are used before any group changes.
    unsafe {
        let v = scheduling_group_get_specific::<u64>(sg, key).unwrap();
        assert_eq!(*v, 7);
        *v = 11;
        assert_eq!(*scheduling_group_get_specific::<u64>(sg, key).unwrap(), 11);
        let cur = scheduling_group_get_specific_current::<u64>(key);
        assert_eq!(*cur, 7, "the current group has its own value");
    }
    let in_group = with_scheduling_group(sg, move || {
        assert!(current_scheduling_group() == sg);
        // SAFETY: as above.
        Ok::<u64, teseladb::engine::core::future::ExceptionPtr>(unsafe {
            *scheduling_group_get_specific_current::<u64>(key)
        })
    })
    .get()
    .unwrap();
    assert_eq!(in_group, 11);
    destroy_scheduling_group(sg).get().unwrap();
    // SAFETY: as above.
    let e = unsafe { scheduling_group_get_specific::<u64>(sg, key) }.expect_err("destroyed");
    assert!(e.get().unwrap().what().contains("does not exist"), "{e:?}");
});
