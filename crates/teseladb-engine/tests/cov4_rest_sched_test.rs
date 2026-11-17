//! Scheduling groups and their keys: a key made from the default configuration (no storage,
//! no callbacks), a key with storage but no constructor, rename hook or destructor (values
//! stay zeroed, renames and destroys leave them alone), groups created after the keys, the
//! group's hash and `is_main`, the default rename hook, and the shard's specific data.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::any::TypeId;
use std::collections::HashSet;

use teseladb::engine::core::reactor::{
    create_scheduling_group, destroy_scheduling_group, rename_scheduling_group,
    scheduling_group_key_create,
};
use teseladb::engine::core::scheduling::{
    SchedulingGroupKeyConfig, SchedulingGroupSpecificRename, current_scheduling_group,
    default_scheduling_group,
};
use teseladb::engine::core::scheduling_specific::internal::{
    SpecificVal, get_scheduling_group_specific_thread_local_data,
};
use teseladb::engine::core::scheduling_specific::scheduling_group_get_specific;
use teseladb::engine_thread_test_case;

engine_thread_test_case!(keys_without_callbacks_and_group_identity, {
    // The default configuration is for `()`, with no storage.
    let d = SchedulingGroupKeyConfig::default();
    assert_eq!(d.type_index, TypeId::of::<()>());
    assert_eq!((d.allocation_size, d.alignment), (0, 0));
    assert!(d.constructor.is_none() && d.rename.is_none() && d.destructor.is_none());
    let empty_key = scheduling_group_key_create(d).get().unwrap();

    // Storage for a u64 and nothing else: the values are zero-initialized storage.
    let mut cfg = SchedulingGroupKeyConfig::with_type(TypeId::of::<u64>());
    cfg.allocation_size = 8;
    cfg.alignment = 8;
    let key = scheduling_group_key_create(cfg).get().unwrap();

    let sg = create_scheduling_group("cov4-sg".into(), "c4s".into(), 200.0).get().unwrap();
    assert!(!sg.is_main());
    assert!(default_scheduling_group().is_main());
    assert!(current_scheduling_group().is_main());
    // SAFETY: `key` holds u64 values; the references are used one at a time.
    unsafe {
        let v = scheduling_group_get_specific::<u64>(sg, key).unwrap();
        *v = 42;
    }
    rename_scheduling_group(sg, "cov4-sg-renamed".into()).get().unwrap();
    assert_eq!(sg.name().to_string(), "cov4-sg-renamed");
    // SAFETY: as above.
    unsafe {
        assert_eq!(*scheduling_group_get_specific::<u64>(sg, key).unwrap(), 42);
        assert!(scheduling_group_get_specific::<()>(sg, empty_key).is_ok());
    }

    // The hash follows the group's index: equal groups land in one slot, distinct in two.
    let mut set = HashSet::new();
    set.insert(sg);
    set.insert(sg);
    set.insert(default_scheduling_group());
    assert_eq!(set.len(), 2);
    assert!(set.contains(&sg));

    // The shard's data knows both keys and the group's values.
    // SAFETY: on a reactor thread, which installed the data; no other reference is live.
    let data = unsafe { get_scheduling_group_specific_thread_local_data() };
    assert!(data.scheduling_group_key_configs.len() >= 2);

    destroy_scheduling_group(sg).get().unwrap();
    // SAFETY: as above.
    assert!(unsafe { scheduling_group_get_specific::<u64>(sg, key) }.is_err());
});

engine_thread_test_case!(default_rename_hook_and_empty_value, {
    // A type without a rename hook: calling the default hook changes nothing.
    let mut x = 5i32;
    SchedulingGroupSpecificRename::rename(&mut x);
    assert_eq!(x, 5);
    assert!(!<i32 as SchedulingGroupSpecificRename>::HAS_RENAME);
    // An empty value holds no storage; renaming and dropping it do nothing.
    let mut v = SpecificVal::default();
    assert!(v.get().is_null());
    v.rename();
    drop(v);
});
