//! Reactor entry points not reached elsewhere: the scheduling-group operations that must fail
//! (destroying the default or the current group, renaming the default one, an unknown group),
//! renaming a group whose I/O class exists (its I/O queue metrics follow the new name, as
//! reactor.cc `rename_scheduling_group` renames the queues), UDP channels made by the reactor's
//! free functions, the reactor's process-spawning wrappers, the idle-time and wake-up
//! accessors, and the exception reporters.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::path::Path;

use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::internal::poll::Pollfn;
use teseladb::engine::core::metrics::impl_fns::get_values;
use teseladb::engine::core::reactor::experimental::{spawn_process, spawn_process_default};
use teseladb::engine::core::reactor::internal_misc::{
    ReactorStallSampler, log_timer_callback_exception,
};
use teseladb::engine::core::reactor::{
    create_scheduling_group, destroy_scheduling_group, engine, make_udp_channel,
    make_udp_channel_bound, no_such_scheduling_group, rename_scheduling_group, report_exception,
};
use teseladb::engine::core::scheduling::default_scheduling_group;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::util::process::experimental::{SpawnParameters, WaitStatus};
use teseladb::engine_thread_test_case;

fn what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

engine_thread_test_case!(group_operations_that_must_fail, {
    let e = destroy_scheduling_group(default_scheduling_group())
        .get()
        .unwrap_err();
    assert!(
        what(&e).contains("Attempt to destroy the default scheduling group"),
        "{e:?}"
    );
    let e = rename_scheduling_group(default_scheduling_group(), "x".into())
        .get()
        .unwrap_err();
    assert!(
        what(&e).contains("Attempt to rename the default scheduling group"),
        "{e:?}"
    );

    let sg = create_scheduling_group("cov3-cur".into(), "c3c".into(), 100.0)
        .get()
        .unwrap();
    // From inside the group, destroying it is refused.
    let r = with_scheduling_group(sg, move || destroy_scheduling_group(sg)).get();
    let e = r.unwrap_err();
    assert!(
        what(&e).contains("Attempt to destroy the current scheduling group"),
        "{e:?}"
    );
    destroy_scheduling_group(sg).get().unwrap();

    let e = no_such_scheduling_group(sg);
    assert!(
        what(&e).contains("The scheduling group does not exist"),
        "{e:?}"
    );
});

/// The `class` labels of the I/O queue's total-bytes series on this shard.
fn io_classes() -> Vec<String> {
    let values = get_values();
    let v = values.get_local().unwrap();
    v.metadata
        .as_ref()
        .unwrap()
        .iter()
        .filter(|f| f.mf.name == "io_queue_total_bytes")
        .flat_map(|f| f.metrics.iter())
        .filter_map(|m| {
            m.labels()
                .get(&Sstring::from("class"))
                .map(|c| c.to_string())
        })
        .collect()
}

engine_thread_test_case!(renaming_a_group_renames_its_io_class, {
    let sg = create_scheduling_group("cov3-io-a".into(), "c3a".into(), 100.0)
        .get()
        .unwrap();
    let path = std::env::temp_dir().join(format!("cov3_rename_{}", std::process::id()));
    let name = path.display().to_string();
    let n2 = name.clone();
    with_scheduling_group(sg, move || {
        thread_async(move || {
            let mut f = open_file_dma(&n2, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE)
                .get()?;
            let align = f.disk_write_dma_alignment() as usize;
            let mut buf = TemporaryBuffer::<u8>::aligned(align, align);
            // SAFETY: the buffer owns `align` writable bytes.
            unsafe { std::ptr::write_bytes(buf.get_write(), b'r', align) };
            // SAFETY: the buffer stays alive until the write completes; no intent.
            let n = unsafe { f.dma_write(0, buf.get(), align, std::ptr::null_mut()) }.get()?;
            assert_eq!(n, align);
            f.close().get()
        })
    })
    .get()
    .unwrap();
    assert!(
        io_classes().iter().any(|c| c == "cov3-io-a"),
        "{:?}",
        io_classes()
    );
    rename_scheduling_group(sg, "cov3-io-b".into())
        .get()
        .unwrap();
    let classes = io_classes();
    assert!(classes.iter().any(|c| c == "cov3-io-b"), "{classes:?}");
    assert!(!classes.iter().any(|c| c == "cov3-io-a"), "{classes:?}");
    assert_eq!(sg.name().as_str(), "cov3-io-b");
    destroy_scheduling_group(sg).get().unwrap();
    std::fs::remove_file(&path).unwrap();
});

engine_thread_test_case!(udp_channels_from_the_reactor, {
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    let mut a = make_udp_channel_bound(&SocketAddress::from_inet_address(&ip, 0)).unwrap();
    let dst = a.local_address();
    let mut b = make_udp_channel().unwrap();
    b.send_str(&dst, "cov3").get().unwrap();
    let mut d = a.receive().get().unwrap();
    let p = d.get_data();
    p.linearize_all();
    let frag = p.frag(0);
    // SAFETY: the fragment's bytes live in the packet, which outlives this read.
    let bytes = unsafe { std::slice::from_raw_parts(frag.base, frag.size) };
    assert_eq!(bytes, b"cov3");
});

engine_thread_test_case!(processes_from_the_reactor_wrappers, {
    let mut p = spawn_process_default(Path::new("/bin/true")).get().unwrap();
    match p.wait().get().unwrap() {
        WaitStatus::Exited(e) => assert_eq!(e.exit_code, 0),
        WaitStatus::Signaled(_) => panic!("signaled"),
    }
    let params = SpawnParameters {
        argv: vec!["sh".into(), "-c".into(), "exit 3".into()],
        env: vec![],
    };
    let mut p = spawn_process(Path::new("/bin/sh"), params).get().unwrap();
    match p.wait().get().unwrap() {
        WaitStatus::Exited(e) => assert_eq!(e.exit_code, 3),
        WaitStatus::Signaled(_) => panic!("signaled"),
    }
});

engine_thread_test_case!(idle_time_wakeup_and_reporters, {
    // SAFETY: on this shard's reactor thread; the reference lasts for these calls.
    let r = unsafe { &*engine() };
    let idle = r.total_idle_time();
    teseladb::engine::core::sleep::sleep_steady(std::time::Duration::from_millis(20))
        .get()
        .unwrap();
    assert!(r.total_idle_time() >= idle, "idle time only grows");
    // Waking a reactor that is running is harmless.
    r.wakeup();
    let e = ExceptionPtr::new(RuntimeError::new("cov3 reported"));
    report_exception("cov3", &e);
    log_timer_callback_exception(e);
});

#[test]
fn the_stall_sampler_never_has_work_and_ends_a_run_on_entering_interrupt_mode() {
    let mut s = ReactorStallSampler::default();
    assert!(!s.poll());
    assert!(!s.pure_poll());
    assert!(s.try_enter_interrupt_mode());
    s.exit_interrupt_mode();
    // Burn some CPU: the run's wall time covers it.
    let t = std::time::Instant::now();
    while t.elapsed() < std::time::Duration::from_millis(5) {
        std::hint::black_box(0u64);
    }
    let r = s.report();
    assert!(
        r.run_wall_time >= std::time::Duration::from_millis(4),
        "{r}"
    );
    assert!(r.to_string().contains("stalls"), "{r}");
}
