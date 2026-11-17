//! The disk configuration of the I/O queues (the original's resource.cc `disk_config_params`):
//! the number of I/O groups, the I/O properties given inline or in a file, the per-disk queue
//! limits, rates scaled by the rate factor and checked for zero, block devices identified by
//! their own device number, and the queue configuration generated from each disk (rates per
//! I/O group, write-to-read multipliers, saturation lengths, latency goal, stall threshold).
#![cfg(target_os = "linux")]

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::PathBuf;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::io_queue::IoQueue;
use teseladb::engine::core::reactor::{DiskConfigParams, ReactorOptions};
use teseladb::engine::core::smp_options::SmpOptions;

fn tmp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cov3-disk-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn disk(mp: &str, extra: &str) -> String {
    format!(
        "{{mountpoint: {mp}, read_iops: 20000, read_bandwidth: 200M, write_iops: 10000, \
         write_bandwidth: 100M{extra}}}"
    )
}

fn props(disks: &[String]) -> String {
    format!("{{disks: [{}]}}", disks.join(", "))
}

fn parse(
    max_queues: u32,
    smp: impl FnOnce(&mut SmpOptions),
    reactor: impl FnOnce(&mut ReactorOptions),
) -> Result<DiskConfigParams, ExceptionPtr> {
    let mut smp_opts = SmpOptions::new();
    smp(&mut smp_opts);
    let mut reactor_opts = ReactorOptions::default();
    reactor(&mut reactor_opts);
    let mut p = DiskConfigParams::new(max_queues);
    p.parse_config(&smp_opts, &reactor_opts)?;
    Ok(p)
}

fn message(r: Result<DiskConfigParams, ExceptionPtr>) -> String {
    match r {
        Ok(_) => panic!("the configuration was accepted"),
        Err(e) => e.get().unwrap().what(),
    }
}

#[test]
fn io_groups_must_be_positive() {
    let e = message(parse(8, |s| s.num_io_groups.set_value(0), |_| {}));
    assert_eq!(e, "num-io-groups must be greater than zero");
    let p = parse(8, |s| s.num_io_groups.set_value(3), |_| {}).unwrap();
    assert_eq!(p.num_io_groups(), 3);
    // Without the option the number is left for the caller to compute.
    assert_eq!(parse(8, |_| {}, |_| {}).unwrap().num_io_groups(), 0);
}

#[test]
fn the_latency_goal_defaults_to_one_and_a_half_task_quotas() {
    let p = DiskConfigParams::new(1);
    let mut o = ReactorOptions::default();
    o.task_quota_ms.set_value(2.0);
    assert_eq!(p.latency_goal_opt(&o), 3.0);
    o.io_latency_goal_ms.set_value(5.0);
    assert_eq!(p.latency_goal_opt(&o), 5.0);
    assert_eq!(p.per_io_group(10, 3), 3);
    assert_eq!(p.per_io_group(1, 3), 1, "never below one");
}

#[test]
fn a_disk_generates_its_queue_configuration() {
    let dir = tmp_dir("gen");
    let mp = dir.display().to_string();
    let extra = ", read_saturation_length: 64k, write_saturation_length: 128k, duplex: true, \
                 rate_factor: 0.5";
    let p = parse(
        8,
        |s| s.io_properties.set_value(props(&[disk(&mp, extra)])),
        |r| {
            r.io_latency_goal_ms.set_value(2.0);
            r.io_completion_notify_ms.set_value(7);
        },
    )
    .unwrap();
    let mut ids: Vec<u32> = p.queue_ids().collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1], "the disk is queue 1; queue 0 is the placeholder");
    let dev = std::fs::metadata(&dir).unwrap().dev();
    assert_eq!(p.queue_devices(1), &vec![dev]);
    assert_eq!(p.queue_devices(0), &vec![0]);
    assert_eq!(p.latency_goal(), Duration::from_millis(2));
    assert_eq!(p.stall_threshold(), Duration::from_millis(7));

    let base = u64::from(IoQueue::READ_REQUEST_BASE_COUNT);
    let cfg = p.generate_config(1, 2);
    // Rates halved by the factor, then divided between the two groups.
    let read_bw = 200u64 << 19; // 200M * 0.5
    let write_bw = 100u64 << 19;
    assert_eq!(cfg.id, 1);
    assert_eq!(cfg.blocks_count_rate, (base * (read_bw / 2)) >> IoQueue::BLOCK_SIZE_SHIFT);
    assert_eq!(u64::from(cfg.disk_blocks_write_to_read_multiplier), base * read_bw / write_bw);
    assert_eq!(cfg.req_count_rate, base * (10000 / 2));
    assert_eq!(u64::from(cfg.disk_req_write_to_read_multiplier), base * 10000 / 5000);
    assert_eq!(cfg.disk_read_saturation_length, 64 << 10);
    assert_eq!(cfg.disk_write_saturation_length, 128 << 10);
    assert_eq!(cfg.mountpoint, mp);
    assert!(cfg.duplex);
    assert_eq!(cfg.rate_limit_duration, Duration::from_millis(2));
    assert_eq!(cfg.stall_threshold, Duration::from_millis(7));
    assert_eq!(cfg.block_count_limit_min, (64 << 10) >> IoQueue::BLOCK_SIZE_SHIFT);

    // The placeholder queue has no limits: its rates stay at the defaults.
    let unlimited = p.generate_config(0, 2);
    let defaults = teseladb::engine::core::io_queue::IoQueueConfig::default();
    assert_eq!(unlimited.blocks_count_rate, defaults.blocks_count_rate);
    assert_eq!(unlimited.req_count_rate, defaults.req_count_rate);
    assert_eq!(unlimited.disk_read_saturation_length, defaults.disk_read_saturation_length);
    assert!(unlimited.mountpoint.is_empty());
    // Without the stall option the threshold is unbounded.
    let p = parse(8, |_| {}, |_| {}).unwrap();
    assert_eq!(p.stall_threshold(), Duration::MAX);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_disk_of_several_mount_points_from_a_file() {
    let a = tmp_dir("file");
    let file = a.join("io.yaml");
    std::fs::write(
        &file,
        "disks:\n  - mountpoints: [/proc, /sys]\n    read_iops: 100\n    read_bandwidth: 1M\n    \
         write_iops: 100\n    write_bandwidth: 1M\n",
    )
    .unwrap();
    let p =
        parse(8, |s| s.io_properties_file.set_value(file.display().to_string()), |_| {}).unwrap();
    assert_eq!(p.queue_devices(1).len(), 2);
    assert_eq!(p.generate_config(1, 1).mountpoint, "/proc:/sys");
    // A file that cannot be read.
    let missing = a.join("missing.yaml").display().to_string();
    let e = message(parse(8, |s| s.io_properties_file.set_value(missing.clone()), |_| {}));
    assert!(e.contains("bad file"), "{e}");
    std::fs::remove_dir_all(&a).unwrap();
}

#[test]
fn queues_beyond_the_maximum_are_rejected() {
    let p = props(&[disk("/proc", ""), disk("/sys", "")]);
    let e = message(parse(1, |s| s.io_properties.set_value(p.clone()), |_| {}));
    assert_eq!(e, "Configured number of queues 1 is larger than the maximum 1");
    assert!(parse(2, |s| s.io_properties.set_value(p), |_| {}).is_ok());
}

#[test]
fn every_rate_must_stay_positive_after_the_factor() {
    let zero = "R/W bytes and req rates must not be zero";
    for d in [
        "{mountpoint: /proc, read_iops: 1, read_bandwidth: 1M, write_iops: 1, write_bandwidth: 0}",
        "{mountpoint: /proc, read_iops: 1, read_bandwidth: 1M, write_iops: 0, write_bandwidth: 1M}",
        "{mountpoint: /proc, read_iops: 0, read_bandwidth: 1M, write_iops: 1, write_bandwidth: 1M}",
        "{mountpoint: /proc, read_iops: 1, read_bandwidth: 1M, write_iops: 1, write_bandwidth: 1M, \
         rate_factor: 0.1}",
    ] {
        let e = message(parse(8, |s| s.io_properties.set_value(props(&[d.to_string()])), |_| {}));
        assert_eq!(e, zero, "{d}");
    }
}

#[test]
fn a_block_device_is_identified_by_its_own_device_number() {
    // The first block device under /dev, if the system has one.
    let Some(dev) = std::fs::read_dir("/dev")
        .unwrap()
        .flatten()
        .find(|e| e.file_type().is_ok_and(|t| t.is_block_device()))
    else {
        eprintln!("no block device: nothing to check");
        return;
    };
    let path = dev.path().display().to_string();
    let rdev = std::fs::metadata(dev.path()).unwrap().rdev();
    let p = parse(8, |s| s.io_properties.set_value(props(&[disk(&path, "")])), |_| {}).unwrap();
    assert_eq!(p.queue_devices(1), &vec![rdev]);
}
