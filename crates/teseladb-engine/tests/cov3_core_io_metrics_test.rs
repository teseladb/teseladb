//! Reading every metric a shard registers after it has done disk I/O and talked to another
//! shard: the per-class I/O queue counters (bytes and operations by direction, splits, queue and
//! execution times, starvation, queue lengths, delay, shares), the fair queue's per-stream
//! consumption and activations, and the message queues between shards. Each value is checked
//! against the work done (as the original's io_queue.cc and smp.cc metric definitions count it).
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::collections::BTreeMap;

use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::metrics::impl_::MetricValue;
use teseladb::engine::core::metrics::impl_fns::get_values;
use teseladb::engine::core::metrics::set_relabel_configs;
use teseladb::engine::core::relabel_config::metrics::{
    RelabelAction, RelabelConfig, RelabelConfigRegex,
};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine_thread_test_case;

const BLOCK: usize = 4096;

type Series = Vec<(BTreeMap<Sstring, Sstring>, MetricValue)>;

/// Every series of this shard, by family name.
fn all_series() -> BTreeMap<String, Series> {
    let values = get_values();
    let v = values.get_local().expect("this shard's values");
    let md = v.metadata.as_ref().expect("metadata");
    let mut out: BTreeMap<String, Series> = BTreeMap::new();
    for (family, vals) in md.iter().zip(v.values.iter()) {
        let e = out.entry(family.mf.name.to_string()).or_default();
        for (m, val) in family.metrics.iter().zip(vals.iter()) {
            e.push((m.labels().clone(), val.clone()));
        }
    }
    out
}

fn sum(all: &BTreeMap<String, Series>, name: &str) -> f64 {
    all.get(name)
        .unwrap_or_else(|| panic!("no family {name}"))
        .iter()
        .map(|(_, v)| v.d())
        .sum()
}

fn write_and_read(path: &str) -> Result<(), ExceptionPtr> {
    let mut f = open_file_dma(
        path,
        OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE,
    )
    .get()?;
    let mut buf = TemporaryBuffer::<u8>::aligned(BLOCK, 2 * BLOCK);
    // SAFETY: the buffer holds 2 * BLOCK writable bytes.
    unsafe { std::ptr::write_bytes(buf.get_write(), b'm', 2 * BLOCK) };
    // SAFETY: the buffer lives until the write resolves; no I/O intent.
    let n = unsafe { f.dma_write::<u8>(0, buf.get(), 2 * BLOCK, std::ptr::null_mut()) }.get()?;
    assert_eq!(n, 2 * BLOCK);
    f.flush().get()?;
    // SAFETY: no I/O intent.
    let back = unsafe { f.dma_read_bulk::<u8>(0, 2 * BLOCK, std::ptr::null_mut()) }.get()?;
    assert_eq!(back.size(), 2 * BLOCK);
    f.close().get()
}

engine_thread_test_case!(every_registered_metric_reads_the_work_done, {
    let dir = std::env::temp_dir();
    let path = dir
        .join(format!("cov3_io_metrics_{}", std::process::id()))
        .display()
        .to_string();
    let before = all_series();
    let bytes_before = before
        .get("io_queue_total_bytes")
        .map_or(0.0, |s| s.iter().map(|(_, v)| v.d()).sum());
    write_and_read(&path).unwrap();
    if Smp::count() > 1 {
        let r = Smp::submit_to(1, || Ok::<u32, ExceptionPtr>(7))
            .get()
            .unwrap();
        assert_eq!(r, 7);
    }
    let all = all_series();

    // Bytes and operations, by direction and in total.
    let wb = sum(&all, "io_queue_total_write_bytes");
    let rb = sum(&all, "io_queue_total_read_bytes");
    assert!(wb >= (2 * BLOCK) as f64, "{wb}");
    assert!(rb >= (2 * BLOCK) as f64, "{rb}");
    assert!(sum(&all, "io_queue_total_write_ops") >= 1.0);
    assert!(sum(&all, "io_queue_total_read_ops") >= 1.0);
    let total = sum(&all, "io_queue_total_bytes");
    assert_eq!(total, wb + rb, "the total is the sum of both directions");
    assert!(total >= bytes_before + (4 * BLOCK) as f64);
    assert_eq!(
        sum(&all, "io_queue_total_operations"),
        sum(&all, "io_queue_total_write_ops") + sum(&all, "io_queue_total_read_ops")
    );
    // Nothing was split; times are non-negative; nothing is left queued or executing.
    assert!(sum(&all, "io_queue_total_split_ops") >= 0.0);
    assert!(sum(&all, "io_queue_total_split_bytes") >= 0.0);
    assert!(sum(&all, "io_queue_total_delay_sec") >= 0.0);
    assert!(
        sum(&all, "io_queue_total_exec_sec") > 0.0,
        "the disk took some time"
    );
    assert!(sum(&all, "io_queue_starvation_time_sec") >= 0.0);
    assert_eq!(sum(&all, "io_queue_queue_length"), 0.0);
    assert_eq!(sum(&all, "io_queue_disk_queue_length"), 0.0);
    assert!(sum(&all, "io_queue_delay") >= 0.0);
    assert!(sum(&all, "io_queue_shares") > 0.0, "every class has shares");
    // The fair queue's consumption, per stream.
    let consumption = all.get("io_queue_consumption").expect("consumption");
    assert!(
        consumption
            .iter()
            .all(|(l, _)| l.contains_key(&Sstring::from("stream")))
    );
    assert!(sum(&all, "io_queue_consumption") > 0.0);
    assert!(sum(&all, "io_queue_adjusted_consumption") >= 0.0);
    assert!(sum(&all, "io_queue_activations") >= 1.0);

    if Smp::count() > 1 {
        // The message queue metrics are registered disabled (as in reactor.cc,
        // `sm::metric_disabled`): a relabel rule keeps them, on both shards. The queues of a
        // pair of shards are read by the metrics of either side: count the messages on both.
        // As in the original, removing the rules restores the labels but not the enabled
        // flag: a drop rule disables them again, then the rules are cleared.
        fn keep_smp(keep: bool) -> Result<(), ExceptionPtr> {
            let rules = vec![RelabelConfig {
                source_labels: vec!["__name__".into()],
                expr: RelabelConfigRegex::new("smp_.*")?,
                action: if keep {
                    RelabelAction::Keep
                } else {
                    RelabelAction::Drop
                },
                ..RelabelConfig::default()
            }];
            set_relabel_configs(&rules).get()?;
            if !keep {
                set_relabel_configs(&[]).get()?;
            }
            Ok(())
        }
        Smp::invoke_on_all(|| keep_smp(true)).get().unwrap();
        let r = Smp::submit_to(1, || Ok::<u32, ExceptionPtr>(8))
            .get()
            .unwrap();
        assert_eq!(r, 8);
        let all = all_series();
        let messages = |all: &BTreeMap<String, Series>, suffix: &str| -> f64 {
            all.iter()
                .filter(|(k, _)| k.ends_with(suffix))
                .flat_map(|(_, s)| s.iter().map(|(_, v)| v.d()))
                .sum()
        };
        let there = Smp::submit_to(1, move || {
            let a = all_series();
            Ok::<(f64, f64, f64), ExceptionPtr>((
                messages(&a, "total_sent_messages"),
                messages(&a, "total_received_messages"),
                messages(&a, "total_completed_messages"),
            ))
        })
        .get()
        .unwrap();
        let sent = messages(&all, "total_sent_messages") + there.0;
        let received = messages(&all, "total_received_messages") + there.1;
        let completed = messages(&all, "total_completed_messages") + there.2;
        assert!(
            sent >= 1.0,
            "sent {sent} received {received} completed {completed}"
        );
        assert!(
            received >= 1.0,
            "sent {sent} received {received} completed {completed}"
        );
        assert!(
            completed >= 1.0,
            "sent {sent} received {received} completed {completed}"
        );
        for name in [
            "send_batch_queue_length",
            "receive_batch_queue_length",
            "complete_batch_queue_length",
            "send_queue_length",
            "total_received_messages",
            "total_completed_messages",
        ] {
            assert!(all.keys().any(|k| k.ends_with(name)), "{name}");
        }
        Smp::invoke_on_all(|| keep_smp(false)).get().unwrap();
        assert!(
            !all_series().keys().any(|k| k.starts_with("smp_")),
            "disabled again"
        );
    }
    std::fs::remove_file(&path).unwrap();
});
