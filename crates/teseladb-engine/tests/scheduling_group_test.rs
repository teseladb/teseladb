//! Tests of scheduling groups: group-specific values set before and after groups exist,
//! inheritance of the current group by threads and `async_`, the group limit, renames
//! (with their callbacks), key constructors that fail, and per-group construction.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeSet;

use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::reactor::internal::{at_destroy, scheduling_group_count};
use teseladb::engine::core::reactor::{
    create_scheduling_group_with_shares as create_scheduling_group, destroy_scheduling_group,
    rename_scheduling_group, scheduling_group_key_create, yield_,
};
use teseladb::engine::core::scheduling::internal::scheduling_group_index;
use teseladb::engine::core::scheduling::{
    SchedulingGroup, SchedulingGroupKey, SchedulingGroupKeyConfig, SchedulingGroupSpecificRename,
    current_scheduling_group, make_scheduling_group_key_config, max_scheduling_groups,
};
use teseladb::engine::core::scheduling_specific::{
    map_reduce_scheduling_group_specific, reduce_scheduling_group_specific,
};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::thread::{ThreadAttributes, async_with_attributes};
use teseladb::engine::core::when_all::when_all_succeed;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb::engine::util::defer::defer;
use teseladb::engine_test_case;

type Ivec = Vec<i32>;

const NUM_SCHEDULING_GROUPS: i32 = 4;

/// The two `smp::invoke_on_all` steps the three `sg_specific_values_*` tests share: set
/// the values on every shard, then reduce them.
fn check_sg_specific_values(
    sgs: &[SchedulingGroup],
    key1: SchedulingGroupKey,
    key2: SchedulingGroupKey,
) -> Result<(), ExceptionPtr> {
    let sgs = sgs.to_vec();
    Smp::invoke_on_all(move || {
        let factor = this_shard_id() as i32 + 1;
        for i in 0..NUM_SCHEDULING_GROUPS {
            // SAFETY: each key was created for the type it is read
            // as, and each reference lives for its statement only, on this shard.
            *unsafe { sgs[i as usize].get_specific::<i32>(key1) } = (i + 1) * factor;
            // SAFETY: `key2` was created for `Ivec`, and the reference lives for this statement only, on this shard.
            unsafe { sgs[i as usize].get_specific::<Ivec>(key2) }.push((i + 1) * factor);
        }

        for i in 0..NUM_SCHEDULING_GROUPS {
            assert_eq!(
                {
                    // SAFETY: `key1` was created for `i32`, and the reference lives for this statement only, on this shard.
                    *unsafe { sgs[i as usize].get_specific::<i32>(key1) } = (i + 1) * factor;
                    // SAFETY: `key1` was created for `i32`, and the reference lives for this statement only, on this shard.
                    *unsafe { sgs[i as usize].get_specific::<i32>(key1) }
                },
                (i + 1) * factor
            );
            // SAFETY: `key2` was created for `Ivec`, and the reference lives for this statement only, on this shard.
            assert_eq!(unsafe { sgs[i as usize].get_specific::<Ivec>(key2) }[0], (i + 1) * factor);
        }
    })
    .get()?;

    Smp::invoke_on_all(move || {
        // SAFETY: on a reactor thread, `key1` holds `i32`s, and no reference to the values is
        // in use until the reduction resolves.
        unsafe { reduce_scheduling_group_specific::<i32, _, _>(|a: i32, b: i32| a + b, 0i32, key1) }
            .then(|sum: i32| {
                let factor = this_shard_id() as i32 + 1;
                let expected_sum =
                    ((1 + NUM_SCHEDULING_GROUPS) * NUM_SCHEDULING_GROUPS) * factor / 2;
                assert_eq!(expected_sum, sum);
            })
            .then(move |()| {
                let ivec_to_int = |v: &mut Ivec| if !v.is_empty() { v[0] } else { 0 };

                // SAFETY: as above, with `key2` holding `Ivec`s.
                unsafe {
                    map_reduce_scheduling_group_specific::<Ivec, _, _, _, _>(
                        ivec_to_int,
                        |a: i32, b: i32| a + b,
                        0i32,
                        key2,
                    )
                }
                .then(|sum: i32| {
                    let factor = this_shard_id() as i32 + 1;
                    let expected_sum =
                        ((1 + NUM_SCHEDULING_GROUPS) * NUM_SCHEDULING_GROUPS) * factor / 2;
                    assert_eq!(expected_sum, sum);
                })
            })
    })
    .get()?;
    Ok(())
}

/// The deferred action that destroys every group of `sgs`.
fn destroy_all(sgs: *const Vec<SchedulingGroup>) -> impl FnOnce() {
    move || {
        // SAFETY: the vector outlives the deferred action declared after it.
        for &sg in unsafe { &*sgs } {
            destroy_scheduling_group(sg).get().expect("destroy_scheduling_group");
        }
    }
}

// Test setting primitive and object as a value after all groups are created
engine_test_case!(sg_specific_values_define_after_sg_create, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sgs: Vec<SchedulingGroup> = Vec::new();
        for i in 0..NUM_SCHEDULING_GROUPS {
            sgs.push(create_scheduling_group(Sstring::from(format!("sg{}", i)), 100.0).get()?);
        }

        let _destroy_scheduling_groups = defer(destroy_all(&sgs));
        let key1_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<i32, _>(i32::default);
        let key1 = scheduling_group_key_create(key1_conf).get()?;
        let key2_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<Ivec, _>(Ivec::new);
        let key2 = scheduling_group_key_create(key2_conf).get()?;
        check_sg_specific_values(&sgs, key1, key2)
    })
});

// Test setting primitive and object as a value before all groups are created
engine_test_case!(sg_specific_values_define_before_sg_create, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sgs: Vec<SchedulingGroup> = Vec::new();
        let _destroy_scheduling_groups = defer(destroy_all(&sgs));
        let key1_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<i32, _>(i32::default);
        let key1 = scheduling_group_key_create(key1_conf).get()?;
        let key2_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<Ivec, _>(Ivec::new);
        let key2 = scheduling_group_key_create(key2_conf).get()?;

        for i in 0..NUM_SCHEDULING_GROUPS {
            sgs.push(create_scheduling_group(Sstring::from(format!("sg{}", i)), 100.0).get()?);
        }

        check_sg_specific_values(&sgs, key1, key2)
    })
});

// Test setting primitive and an object as a value before some groups are created
// and after some of the groups are created.
engine_test_case!(sg_specific_values_define_before_and_after_sg_create, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sgs: Vec<SchedulingGroup> = Vec::new();
        let _destroy_scheduling_groups = defer(destroy_all(&sgs));

        for i in 0..NUM_SCHEDULING_GROUPS / 2 {
            sgs.push(create_scheduling_group(Sstring::from(format!("sg{}", i)), 100.0).get()?);
        }
        let key1_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<i32, _>(i32::default);
        let key1 = scheduling_group_key_create(key1_conf).get()?;
        let key2_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<Ivec, _>(Ivec::new);
        let key2 = scheduling_group_key_create(key2_conf).get()?;

        for i in NUM_SCHEDULING_GROUPS / 2..NUM_SCHEDULING_GROUPS {
            sgs.push(create_scheduling_group(Sstring::from(format!("sg{}", i)), 100.0).get()?);
        }

        check_sg_specific_values(&sgs, key1, key2)
    })
});

// Test that the current scheduling group is inherited by `async_`.
engine_test_case!(sg_scheduling_group_inheritance_in_engine_async_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let sg = create_scheduling_group(Sstring::from("sg0"), 100.0).get()?;
        let _cleanup =
            defer(move || destroy_scheduling_group(sg).get().expect("destroy_scheduling_group"));
        let attr = ThreadAttributes { sched_group: Some(sg), ..ThreadAttributes::default() };
        let group_of = |attr: &ThreadAttributes| attr.sched_group.expect("sched_group");
        let attr2 = attr;
        async_with_attributes(attr, move || -> Result<(), ExceptionPtr> {
            assert_eq!(
                scheduling_group_index(current_scheduling_group()),
                scheduling_group_index(group_of(&attr2))
            );

            let attr3 = attr2;
            thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
                assert_eq!(
                    scheduling_group_index(current_scheduling_group()),
                    scheduling_group_index(group_of(&attr3))
                );

                let sched_group_idx = scheduling_group_index(group_of(&attr3));
                Smp::invoke_on_all(move || {
                    assert_eq!(scheduling_group_index(current_scheduling_group()), sched_group_idx);
                })
                .get()
            })
            .get()
        })
        .get()?;
        Ok(())
    })
});

engine_test_case!(yield_preserves_sg, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let sg = create_scheduling_group(Sstring::from("sg"), 100.0).get()?;
        let _cleanup =
            defer(move || destroy_scheduling_group(sg).get().expect("destroy_scheduling_group"));
        with_scheduling_group(sg, move || {
            yield_().then(move |()| {
                assert_eq!(
                    scheduling_group_index(current_scheduling_group()),
                    scheduling_group_index(sg)
                );
            })
        })
        .get()?;
        Ok(())
    })
});

engine_test_case!(sg_count, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        /// Destroys its scheduling group when dropped.
        struct SchedulingGroupDestroyer {
            sg: SchedulingGroup,
        }
        impl SchedulingGroupDestroyer {
            fn new(sg: SchedulingGroup) -> Self {
                SchedulingGroupDestroyer { sg }
            }
        }
        impl Drop for SchedulingGroupDestroyer {
            fn drop(&mut self) {
                destroy_scheduling_group(self.sg).get().expect("destroy_scheduling_group");
            }
        }

        // The reserve is necessary in order to never move a destroyer.
        let mut scheduling_groups_deferred_cleanup: Vec<SchedulingGroupDestroyer> =
            Vec::with_capacity(max_scheduling_groups() as usize);

        // try to create 3 groups too many.
        for i in scheduling_group_count()..max_scheduling_groups() as usize + 3 {
            assert!(scheduling_group_count() <= max_scheduling_groups() as usize);
            match create_scheduling_group(Sstring::from(format!("sg_{}", i)), 10.0).get() {
                Ok(sg) => {
                    scheduling_groups_deferred_cleanup.push(SchedulingGroupDestroyer::new(sg))
                }
                Err(e) if e.is::<RuntimeError>() => {
                    // make sure it is the right exception.
                    assert_eq!(
                        e.get().map(|x| x.what()).unwrap_or_default(),
                        format!("Scheduling group limit exceeded while creating sg_{}", i)
                    );
                    // make sure that the scheduling group count makes sense
                    assert_eq!(scheduling_group_count(), max_scheduling_groups() as usize);
                    // make sure that we expect this exception at this point
                    assert!(i >= max_scheduling_groups() as usize);
                }
                Err(e) => return Err(e),
            }
        }
        assert_eq!(scheduling_group_count(), max_scheduling_groups() as usize);
        Ok(())
    })
});

/// A group-local value that records the group name on construction and on each rename.
struct Value {
    _id: u64, // Used to check that the value only receives a rename(), not a reconstruction.
    _sg_name: String,
}

impl Value {
    /// A value with the current group's name.
    fn new() -> Self {
        let mut v = Value { _id: 0, _sg_name: String::new() };
        v.rename_value();
        v
    }

    fn rename_value(&mut self) {
        self._sg_name = current_scheduling_group().name().to_string();
    }
}

impl SchedulingGroupSpecificRename for Value {
    const HAS_RENAME: bool = true;
    fn rename(&mut self) {
        self.rename_value();
    }
}

engine_test_case!(sg_rename_callback, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // Tests that all sg-local values receive the rename() callback when scheduling groups
        // are renamed.
        let key_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<Value, _>(Value::new);
        let mut keys: Vec<SchedulingGroupKey> = Vec::new();
        for _ in 0..3 {
            keys.push(scheduling_group_key_create(key_conf.clone()).get()?);
        }

        let mut sgs: Vec<SchedulingGroup> = Vec::new();
        let _destroy_sgs = defer(destroy_all(&sgs));
        for s in 0..3 {
            sgs.push(
                create_scheduling_group(Sstring::from(format!("sg-old-{}", s)), 1000.0).get()?,
            );
        }

        let (sgs1, keys1) = (sgs.clone(), keys.clone());
        Smp::invoke_on_all(move || {
            // SAFETY: the keys hold `Value`s, and each reference
            // lives for its statement only, on this shard.
            // Index loops: `s` also names the group.
            #[allow(clippy::needless_range_loop)]
            for s in 0..sgs1.len() {
                for k in 0..keys1.len() {
                    assert_eq!(
                        // SAFETY: the keys hold `Value`s, and the reference lives for this statement only, on this shard.
                        unsafe { sgs1[s].get_specific::<Value>(keys1[k]) }._sg_name,
                        format!("sg-old-{}", s)
                    );
                    // SAFETY: the keys hold `Value`s, and the reference lives for this statement only, on this shard.
                    unsafe { sgs1[s].get_specific::<Value>(keys1[k]) }._id = 1;
                }
            }
        })
        .get()?;

        // Index loop: `s` also names the group.
        #[allow(clippy::needless_range_loop)]
        for s in 0..sgs.len() {
            rename_scheduling_group(sgs[s], Sstring::from(format!("sg-new-{}", s))).get()?;
        }

        let (sgs2, keys2) = (sgs.clone(), keys.clone());
        Smp::invoke_on_all(move || {
            // SAFETY: as above.
            // Index loops: `s` also names the group.
            #[allow(clippy::needless_range_loop)]
            for s in 0..sgs2.len() {
                for k in 0..keys2.len() {
                    assert_eq!(
                        // SAFETY: the keys hold `Value`s, and the reference lives for this statement only, on this shard.
                        unsafe { sgs2[s].get_specific::<Value>(keys2[k]) }._sg_name,
                        format!("sg-new-{}", s)
                    );
                    // Checks that the value only saw a rename(), not a reconstruction.
                    // SAFETY: the keys hold `Value`s, and the reference lives for this statement only, on this shard.
                    assert_eq!(unsafe { sgs2[s].get_specific::<Value>(keys2[k]) }._id, 1);
                }
            }
        })
        .get()?;
        Ok(())
    })
});

engine_test_case!(sg_create_and_key_create_in_parallel, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sgs: Vec<SchedulingGroup> = Vec::new();
        let key1_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<i32, _>(i32::default);
        let sgsp: *mut Vec<SchedulingGroup> = &mut sgs;
        when_all_succeed((
            create_scheduling_group(Sstring::from("sg1"), 100.0),
            scheduling_group_key_create(key1_conf.clone()),
            create_scheduling_group(Sstring::from("sg2"), 100.0),
            scheduling_group_key_create(key1_conf.clone()),
            create_scheduling_group(Sstring::from("sg3"), 100.0),
            scheduling_group_key_create(key1_conf.clone()),
            create_scheduling_group(Sstring::from("sg4"), 100.0),
            scheduling_group_key_create(key1_conf),
        ))
        .then_unpack(
            move |sg1: SchedulingGroup,
                  _k1: SchedulingGroupKey,
                  sg2: SchedulingGroup,
                  _k2: SchedulingGroupKey,
                  sg3: SchedulingGroup,
                  _k3: SchedulingGroupKey,
                  sg4: SchedulingGroup,
                  _k4: SchedulingGroupKey| {
                // SAFETY: `sgs` outlives the future, waited for right here.
                let sgs = unsafe { &mut *sgsp };
                sgs.push(sg1);
                sgs.push(sg2);
                sgs.push(sg3);
                sgs.push(sg4);
            },
        )
        .get()?;
        for &sg in &sgs {
            destroy_scheduling_group(sg).get()?;
        }
        Ok(())
    })
});

engine_test_case!(sg_key_constructor_exception_when_creating_new_key, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let key_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<i32, _>(i32::default);
        scheduling_group_key_create(key_conf).get()?;

        /// A value whose constructor fails with the runtime error `constructor failed`,
        /// which the value constructor of the key configuration raises as a panic carrying
        /// the exception (`ExceptionPtr::from_panic` turns it back into the exception).
        struct Thrower;
        impl Thrower {
            fn new() -> Self {
                std::panic::panic_any(RuntimeError::new("constructor failed"))
            }
        }
        impl Drop for Thrower {
            fn drop(&mut self) {
                // Shouldn't get here because the constructor shouldn't succeed
                panic!("ENGINE_ASSERT(false)");
            }
        }
        impl SchedulingGroupSpecificRename for Thrower {}

        let thrower_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<Thrower, _>(Thrower::new);
        match scheduling_group_key_create(thrower_conf).get() {
            Ok(_) => panic!("expected a RuntimeError"),
            Err(e) => assert!(e.is::<RuntimeError>()),
        }
        Ok(())
    })
});

engine_test_case!(sg_create_with_destroy_tasks, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        struct Nada;
        impl SchedulingGroupSpecificRename for Nada {}

        // at_destroy() functionality is deprecated, but test until removed.
        at_destroy(|| {}); // nothing really

        let sg_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<Nada, _>(|| Nada);
        scheduling_group_key_create(sg_conf).get()?;
        Ok(())
    })
});

thread_local! {
    static GROUPS: RefCell<BTreeSet<u32>> = const { RefCell::new(BTreeSet::new()) };
}

engine_test_case!(sg_create_check_unique_constructor_invocation, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // check we don't run constructor in same sched group more than once.
        struct Check;
        impl Check {
            fn new() -> Self {
                let sg = current_scheduling_group();
                let id = scheduling_group_index(sg);
                GROUPS.with(|g| {
                    assert!(!g.borrow().contains(&id));
                    g.borrow_mut().insert(id);
                });
                Check
            }
        }
        impl SchedulingGroupSpecificRename for Check {}

        Smp::invoke_on_all(|| {
            GROUPS.with(|g| g.borrow_mut().clear());
        })
        .get()?;
        let key1_conf: SchedulingGroupKeyConfig =
            make_scheduling_group_key_config::<Check, _>(Check::new);
        scheduling_group_key_create(key1_conf).get()?;

        // clean out data for ASAN.
        Smp::invoke_on_all(|| {
            GROUPS.with(|g| *g.borrow_mut() = BTreeSet::new());
        })
        .get()?;
        Ok(())
    })
});

/// The test runner: it runs the
/// registered test cases in declaration order, which these tests rely on: after
/// `sg_key_constructor_exception_when_creating_new_key` the key whose constructor throws stays
/// registered, and every scheduling group created later fails to
/// construct its value; so that test must run after every test that creates groups,
/// while the default test harness would run it before them (by name).
pub(crate) fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
