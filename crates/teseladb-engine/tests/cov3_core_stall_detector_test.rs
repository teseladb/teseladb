//! The POSIX timer stall detector, the fallback the original (reactor.cc
//! `make_cpu_stall_detector`) uses when perf events are not available: created on a thread of
//! its own (outside a reactor, whose shard already registers the detector's metric), it keeps
//! its configuration, stops its timer for sleeps, reports through the test reporting function,
//! and deletes its timer when dropped.
#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use teseladb::engine::core::internal::stall_detector::{
    CpuStallDetector, CpuStallDetectorConfig, CpuStallDetectorPosixTimer,
};

#[test]
fn the_posix_timer_detector_reports_through_its_function() {
    let reports = Arc::new(AtomicUsize::new(0));
    let r = reports.clone();
    let cfg = CpuStallDetectorConfig {
        threshold: Duration::from_secs(60),
        stall_detector_reports_per_minute: 5,
        report: Some(Arc::new(move || {
            r.fetch_add(1, Ordering::Relaxed);
        })),
        ..CpuStallDetectorConfig::default()
    };
    let mut d = CpuStallDetectorPosixTimer::new(cfg).expect("a POSIX CPU-time timer");
    assert_eq!(d.get_config().threshold, Duration::from_secs(60));
    assert_eq!(d.get_config().stall_detector_reports_per_minute, 5);
    d.start_sleep().unwrap();
    d.end_sleep();
    d.generate_trace();
    d.generate_trace();
    assert_eq!(reports.load(Ordering::Relaxed), 2);
    let mut cfg = d.get_config();
    cfg.threshold = Duration::from_secs(30);
    d.update_config(cfg);
    assert_eq!(d.get_config().threshold, Duration::from_secs(30));
    assert!(CpuStallDetector::signal_number() > 0);
    drop(d);
}
