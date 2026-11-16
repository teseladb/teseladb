//! Tests of the CPU-set parser and the cgroup readers, cross-shard calls to the other shards,
//! named service groups that bound the concurrency of cross-shard calls, and sharded services
//! with a single instance.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::io::Write;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::loop_::parallel_for_each;
use teseladb::engine::core::resource::{Cpuset, cgroup, parse_cpuset};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sharded::{NoShardedInstanceException, Sharded, ShardedService};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::{
    Smp, SmpServiceGroupConfig, create_smp_service_group, destroy_smp_service_group,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

fn set(v: &[u32]) -> Cpuset {
    v.iter().copied().collect()
}

#[test]
fn cpusets_parse_lists_and_ranges() {
    assert_eq!(parse_cpuset("3".into()).unwrap(), Some(set(&[3])));
    assert_eq!(parse_cpuset("0-3".into()).unwrap(), Some(set(&[0, 1, 2, 3])));
    assert_eq!(parse_cpuset("1,4-5,9".into()).unwrap(), Some(set(&[1, 4, 5, 9])));
    assert_eq!(parse_cpuset("2-2".into()).unwrap(), Some(set(&[2])));
    // A backwards range and malformed lists are not cpusets.
    assert_eq!(parse_cpuset("5-1".into()).unwrap(), None);
    for bad in ["", "a", "1,", ",1", "1-", "-1", "1;2", "1 2", "1-2-3"] {
        assert_eq!(parse_cpuset(bad.into()).unwrap(), None, "{bad:?}");
    }
    // Digits that do not fit the type are an error.
    assert!(parse_cpuset("99999999999".into()).is_err());
}

#[test]
fn cgroup_readers_report_this_container() {
    let cpus = cgroup::cpu_set().expect("the cgroup lists the CPUs");
    assert!(!cpus.is_empty());
    let online = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
    assert!(cpus.len() as u32 >= online.min(1));
    assert!(cgroup::memory_limit() > 0);
    // A missing setting reads as none.
    assert_eq!(
        cgroup::read_setting_v1v2_as::<u64>("nosuch/nosuch".into(), "nosuch.file".into()),
        None
    );

    let dir = std::env::temp_dir().join(format!("cov_resource_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("value");
    writeln!(std::fs::File::create(&file).unwrap(), "42").unwrap();
    assert_eq!(cgroup::read_setting_as::<u32>(file.display().to_string()), Some(42));
    assert_eq!(cgroup::read_setting_as::<bool>(file.display().to_string()), None);
    assert_eq!(cgroup::read_setting_as::<u32>(dir.join("missing").display().to_string()), None);
    std::fs::remove_dir_all(&dir).unwrap();
}

thread_case!(invoke_on_others_skips_the_caller, {
    let calls = Arc::new(AtomicU32::new(0));
    let me = this_shard_id();
    let c = calls.clone();
    Smp::invoke_on_others(move || {
        assert_ne!(this_shard_id(), me);
        c.fetch_add(1, Ordering::Relaxed);
    })
    .get()
    .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), Smp::count() - 1);

    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();
    let last = Smp::count() - 1;
    Smp::invoke_on_others_of(last, move || {
        assert_ne!(this_shard_id(), last);
        c.fetch_add(1, Ordering::Relaxed);
    })
    .get()
    .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), Smp::count() - 1);
});

thread_local! {
    static RUNNING: Cell<u32> = const { Cell::new(0) };
    static MAX_RUNNING: Cell<u32> = const { Cell::new(0) };
}

thread_case!(service_groups_bound_cross_shard_concurrency, {
    let cfg = SmpServiceGroupConfig {
        max_nonlocal_requests: 1,
        group_name: Some(Sstring::from("cov_group")),
    };
    let group = create_smp_service_group(cfg).get().unwrap();
    let target = Smp::count() - 1;
    let calls = parallel_for_each(0..6, move |_: i32| {
        Smp::submit_to_with(target, group, || {
            RUNNING.with(|r| r.set(r.get() + 1));
            MAX_RUNNING.with(|m| m.set(m.get().max(RUNNING.with(Cell::get))));
            sleep(Duration::from_millis(5)).then(|()| RUNNING.with(|r| r.set(r.get() - 1)))
        })
    });
    calls.get().unwrap();
    let max = Smp::submit_to(target, || Ok::<u32, ExceptionPtr>(MAX_RUNNING.with(Cell::get)))
        .get()
        .unwrap();
    if Smp::count() > 1 {
        // One request from this shard at a time on the target.
        assert_eq!(max, 1);
    }
    destroy_smp_service_group(group).get().unwrap();

    // Groups are reused after destruction.
    let a = create_smp_service_group(SmpServiceGroupConfig::default()).get().unwrap();
    let b = create_smp_service_group(SmpServiceGroupConfig::default()).get().unwrap();
    destroy_smp_service_group(a).get().unwrap();
    let c = create_smp_service_group(SmpServiceGroupConfig::default()).get().unwrap();
    destroy_smp_service_group(b).get().unwrap();
    destroy_smp_service_group(c).get().unwrap();
});

struct Single {
    value: u32,
    shard: u32,
}

impl ShardedService for Single {}

thread_case!(single_instance_services_live_on_shard_zero, {
    let mut s = pin!(Sharded::<Single>::new());
    s.as_mut().start_single(|| Single { value: 7, shard: this_shard_id() }).get().unwrap();
    assert!(s.local_is_initialized());
    // SAFETY: the instance is used only on this shard while the service runs.
    let local = unsafe { s.local_shared() };
    assert_eq!(local.value, 7);
    assert_eq!(local.shard, 0);
    // SAFETY: as above.
    assert_eq!(unsafe { s.get_local_service() }.unwrap().value, 7);
    drop(local);

    if Smp::count() > 1 {
        let ptr = &*s as *const Sharded<Single> as usize;
        let missing = Smp::submit_to(1, move || {
            // SAFETY: the container outlives this call, and is only read.
            let s = unsafe { &*(ptr as *const Sharded<Single>) };
            // SAFETY: nothing is borrowed from the (missing) instance.
            let e = unsafe { s.get_local_service() }.err().expect("no instance on shard 1");
            Ok::<(bool, bool), ExceptionPtr>((
                s.local_is_initialized(),
                e.downcast_ref::<NoShardedInstanceException>().is_some(),
            ))
        })
        .get()
        .unwrap();
        assert_eq!(missing, (false, true));
    }
    s.as_mut().stop().get().unwrap();
    assert!(!s.local_is_initialized());

    // A failing constructor fails the start and leaves the service stopped.
    let mut f = pin!(Sharded::<Single>::new());
    let err = f
        .as_mut()
        .start_single(|| -> Result<Single, ExceptionPtr> {
            Err(ExceptionPtr::new(std::io::Error::other("ctor failed")))
        })
        .get()
        .expect_err("the constructor failed");
    assert!(err.to_string().contains("ctor failed"), "{err}");
    assert!(!f.local_is_initialized());
});
