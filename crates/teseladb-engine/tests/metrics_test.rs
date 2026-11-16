//! Tests of the metrics layer: groups, label values across shards, renaming scheduling
//! groups, relabeling (adding, dropping, enabling and disabling series), family aggregation
//! and the estimated histogram.

#![cfg(target_os = "linux")]

#[path = "common/minstd.rs"]
mod minstd;

use std::collections::BTreeSet;
use std::time::Duration;

use minstd::UniformIntDistribution;
use teseladb::engine::core::future::{make_exception_future, make_ready_future};
use teseladb::engine::core::internal::estimated_histogram::TimeEstimatedHistogram;
use teseladb::engine::core::loop_::do_for_each;
use teseladb::engine::core::metrics::impl_fns::get_values;
use teseladb::engine::core::metrics::{
    Description, Label, LabelInstance, SkipWhenEmpty, make_counter_fn, make_counter_labeled,
    make_gauge_fn, make_gauge_labeled, set_metric_family_configs, set_relabel_configs,
};
use teseladb::engine::core::metrics_api::impl_::MetricSeriesMetadata;
use teseladb::engine::core::metrics_api::impl_::ValuesCopy;
use teseladb::engine::core::metrics_registration::MetricDefinition;
use teseladb::engine::core::metrics_registration::MetricGroups;
use teseladb::engine::core::reactor::{
    create_scheduling_group_with_shares, rename_scheduling_group,
};
use teseladb::engine::core::relabel_config::metrics::{
    MetricFamilyConfig, RelabelAction, RelabelConfig, RelabelConfigRegex,
};
use teseladb::engine::core::sharded::ForeignPtr;
use teseladb::engine::core::shared_ptr::SharedPtr;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::testing::random::testing::local_random;
use teseladb::engine_test_case;

engine_test_case!(test_add_group, {
    // Just has to compile:
    let mut groups = MetricGroups::default();
    match groups
        .add_group("g1", Vec::<MetricDefinition>::new())
        .and_then(|g| g.add_group("g2", Vec::<MetricDefinition>::new()))
    {
        Ok(_) => make_ready_future(()),
        Err(e) => make_exception_future(e),
    }
});

/// The values `get_values()` gathered, read on this shard, which owns them.
fn local_values(p: &ForeignPtr<SharedPtr<ValuesCopy>>) -> &ValuesCopy {
    match p.get_local() {
        Some(v) => v,
        None => panic!("get_values() is read on the shard that called it"),
    }
}

fn get_label_values(metric_name: Sstring, label_name: Sstring) -> BTreeSet<Sstring> {
    let all_metrics = get_values();
    let all_metadata = local_values(&all_metrics).metadata.as_ref().expect("metadata");
    let qp_group = all_metadata.iter().find(|x| x.mf.name == metric_name.as_str());
    assert!(qp_group.is_some());
    let mut labels = BTreeSet::new();
    for metric in qp_group.expect("group").metrics.iter() {
        let found = metric.labels().get(&label_name);
        assert!(found.is_some());
        labels.insert(Sstring::from(found.expect("label").as_str()));
    }
    labels
}

engine_test_case!(test_renaming_scheuling_groups, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // this seams a little bit out of place but the
        // renaming functionality is primarily for statistics
        // otherwise those classes could have just been reused
        // without renaming them.
        const NAME1: &str = "A";
        const NAME2: &str = "B";
        let sg = create_scheduling_group_with_shares("hello".into(), 111.0).get()?;
        // repeatedly change the group name back and forth in
        // decresing time intervals to see if it generate double
        //registration statistics errors.
        for i in 0..1000u64 {
            let name = if i % 2 != 0 { NAME1 } else { NAME2 };
            let prev_name = if i % 2 != 0 { NAME2 } else { NAME1 };
            sleep(Duration::from_micros(100000 / (i + 1))).get()?;
            rename_scheduling_group(sg, name.into()).get()?;
            let label_vals =
                get_label_values(Sstring::from("scheduler_shares"), Sstring::from("group"));
            // validate that the name that we *renamed to* is in the stats
            assert!(label_vals.contains(&Sstring::from(name)));
            // validate that the name that we *renamed from* is *not* in the stats
            assert!(!label_vals.contains(&Sstring::from(prev_name)));
        }

        Smp::invoke_on_all(move || {
            // flip a fair coin and rename to one of two options and rename to that
            // scheduling group name, do it 1000 in parallel on all shards so there
            // is a chance of collision.
            let dist = UniformIntDistribution::new(0, i32::MAX as u64);
            do_for_each(0..1000, move |_i: i32| {
                let odd = !dist.sample_fn(&mut || u64::from(local_random())).is_multiple_of(2);
                rename_scheduling_group(sg, if odd { NAME1 } else { NAME2 }.into())
            })
        })
        .get()?;
        let label_vals =
            get_label_values(Sstring::from("scheduler_shares"), Sstring::from("group"));
        // validate that only one of the names is eventually in the metrics
        let name1_found = label_vals.contains(&Sstring::from(NAME1));
        let name2_found = label_vals.contains(&Sstring::from(NAME2));
        assert!((name1_found && !name2_found) || (name2_found && !name1_found));
        Ok(())
    })
});

fn count_by_label(label: &str) -> i32 {
    let values = get_values();
    let mut count = 0;
    for md in local_values(&values).metadata.as_ref().expect("metadata").iter() {
        for mi in md.metrics.iter() {
            if label.is_empty() || mi.labels().contains_key(&Sstring::from(label)) {
                count += 1;
            }
        }
    }
    count
}

fn count_by_fun(f: impl Fn(&MetricSeriesMetadata) -> bool) -> i32 {
    let values = get_values();
    let mut count = 0;
    for md in local_values(&values).metadata.as_ref().expect("metadata").iter() {
        for mi in md.metrics.iter() {
            if f(mi) {
                count += 1;
            }
        }
    }
    count
}

fn regex(s: &str) -> RelabelConfigRegex {
    RelabelConfigRegex::new(s).expect("regex")
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

engine_test_case!(test_relabel_add_labels, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut app_metrics = MetricGroups::default();
        app_metrics.add_group(
            "test",
            vec![
                make_gauge_fn("gauge_1", Description::new("gague 1"), || 0),
                make_counter_fn("counter_1", Description::new("counter 1"), || 1),
            ],
        )?;

        let mut rl = vec![RelabelConfig::default(); 1];
        rl[0].source_labels = strings(&["__name__"]);
        rl[0].target_label = "level".to_string();
        rl[0].replacement = "1".to_string();
        rl[0].expr = regex("test_counter_.*");

        let success = set_relabel_configs(&rl).get()?;
        assert_eq!(success.metrics_relabeled_due_to_collision, 0);
        assert_eq!(count_by_label("level"), 1);
        app_metrics.add_group(
            "test",
            vec![make_counter_fn("counter_2", Description::new("counter 2"), || 2)],
        )?;
        assert_eq!(count_by_label("level"), 2);
        set_relabel_configs(&[]).get()?;
        Ok(())
    })
});

engine_test_case!(test_metrics_family_aggregate, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut app_metrics = MetricGroups::default();
        let lb = Label::new("lb");
        app_metrics.add_group(
            "test",
            vec![
                make_gauge_fn("gauge_1", Description::new("gague 1"), || 1).label(&lb.call("1")),
                make_gauge_fn("gauge_1", Description::new("gague 1"), || 2).label(&lb.call("2")),
                make_counter_fn("counter_1", Description::new("counter 1"), || 3)
                    .label(&lb.call("1")),
                make_counter_fn("counter_1", Description::new("counter 1"), || 4)
                    .label(&lb.call("2")),
            ],
        )?;
        let mut rl = vec![RelabelConfig::default(); 2];
        rl[0].source_labels = strings(&["__name__"]);
        rl[0].action = RelabelAction::Drop;

        rl[1].source_labels = strings(&["lb"]);
        rl[1].action = RelabelAction::Keep;
        // Dropping the lev label would cause a conflict, but not crash the system
        set_relabel_configs(&rl).get()?;

        let mut fc = vec![MetricFamilyConfig::default(); 2];
        fc[0].name = "test_gauge_1".to_string();
        fc[0].aggregate_labels = strings(&["lb"]);
        fc[1].regex_name = regex("test_gauge1.*");
        fc[1].aggregate_labels = strings(&["ll", "aa"]);
        set_metric_family_configs(&fc);
        let mut values = get_values();
        let mut count = 0;
        for md in local_values(&values).metadata.as_ref().expect("metadata").iter() {
            if md.mf.name == "test_gauge_1" {
                assert_eq!(md.mf.aggregate_labels.len(), 1);
                assert_eq!(md.mf.aggregate_labels[0], "lb");
            } else {
                assert_eq!(md.mf.aggregate_labels.len(), 0);
            }
            count += 1;
        }
        assert_eq!(count, 2);
        app_metrics.add_group(
            "test",
            vec![
                make_gauge_fn("gauge1_1", Description::new("gague 1"), || 1).label(&lb.call("1")),
                make_gauge_fn("gauge1_1", Description::new("gague 1"), || 2).label(&lb.call("2")),
                make_counter_fn("counter1_1", Description::new("counter 1"), || 3)
                    .label(&lb.call("1")),
                make_counter_fn("counter1_1", Description::new("counter 1"), || 4)
                    .label(&lb.call("2")),
            ],
        )?;
        values = get_values();
        count = 0;
        for md in local_values(&values).metadata.as_ref().expect("metadata").iter() {
            if md.mf.name == "test_gauge_1" {
                assert_eq!(md.mf.aggregate_labels.len(), 1);
                assert_eq!(md.mf.aggregate_labels[0], "lb");
            } else if md.mf.name == "test_gauge1_1" {
                assert_eq!(md.mf.aggregate_labels.len(), 2);
                assert_eq!(md.mf.aggregate_labels[0], "ll");
            } else {
                assert_eq!(md.mf.aggregate_labels.len(), 0);
            }
            count += 1;
        }
        assert_eq!(count, 4);
        let rl1: Vec<RelabelConfig> = Vec::new();
        set_relabel_configs(&rl1).get()?;
        Ok(())
    })
});

engine_test_case!(test_relabel_drop_label_prevent_runtime_conflicts, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut app_metrics = MetricGroups::default();
        app_metrics.add_group(
            "test2",
            vec![
                make_gauge_labeled(
                    "gauge_1",
                    Description::new("gague 1"),
                    vec![LabelInstance::new("g", "1")],
                    || 0,
                ),
                make_counter_fn("counter_1", Description::new("counter 1"), || 0),
                make_counter_labeled(
                    "counter_1",
                    Description::new("counter 1"),
                    vec![LabelInstance::new("lev", "2")],
                    || 0,
                ),
            ],
        )?;
        assert_eq!(count_by_label("lev"), 1);

        let mut rl = vec![RelabelConfig::default(); 1];
        rl[0].source_labels = strings(&["lev"]);
        rl[0].expr = regex("2");
        rl[0].target_label = "lev".to_string();
        rl[0].action = RelabelAction::DropLabel;
        // Dropping the lev label would cause a conflict, but not crash the system
        let mut success = set_relabel_configs(&rl).get()?;
        assert_eq!(success.metrics_relabeled_due_to_collision, 1);
        assert_eq!(count_by_label("lev"), 0);
        assert_eq!(count_by_label("err"), 1);

        // resetting all the labels to their initial state
        success = set_relabel_configs(&[]).get()?;
        assert_eq!(success.metrics_relabeled_due_to_collision, 0);
        assert_eq!(count_by_label("lev"), 1);
        assert_eq!(count_by_label("err"), 0);
        set_relabel_configs(&[]).get()?;
        Ok(())
    })
});

fn skip_count() -> i32 {
    count_by_fun(|mi: &MetricSeriesMetadata| mi.should_skip_when_empty() == SkipWhenEmpty::YES)
}

engine_test_case!(test_relabel_enable_disable_skip_when_empty, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut app_metrics = MetricGroups::default();
        app_metrics.add_group(
            "test3",
            vec![
                make_gauge_labeled(
                    "gauge_1",
                    Description::new("gague 1"),
                    vec![LabelInstance::new("lev3", "3")],
                    || 0,
                ),
                make_counter_labeled(
                    "counter_1",
                    Description::new("counter 1"),
                    vec![LabelInstance::new("lev3", "3")],
                    || 0,
                ),
                make_counter_labeled(
                    "counter_2",
                    Description::new("counter 2"),
                    vec![LabelInstance::new("lev3", "3")],
                    || 0,
                ),
            ],
        )?;
        let mut rl = vec![RelabelConfig::default(); 2];
        rl[0].source_labels = strings(&["__name__"]);
        rl[0].action = RelabelAction::Drop;

        rl[1].source_labels = strings(&["lev3"]);
        rl[1].expr = regex("3");
        rl[1].action = RelabelAction::Keep;
        // We just disable all metrics besides those mark as lev3
        let mut success = set_relabel_configs(&rl).get()?;
        assert_eq!(success.metrics_relabeled_due_to_collision, 0);
        assert_eq!(count_by_label(""), 3);
        assert_eq!(skip_count(), 0);

        let mut rl2 = vec![RelabelConfig::default(); 3];
        rl2[0].source_labels = strings(&["__name__"]);
        rl2[0].action = RelabelAction::Drop;

        rl2[1].source_labels = strings(&["lev3"]);
        rl2[1].expr = regex("3");
        rl2[1].action = RelabelAction::Keep;

        rl2[2].source_labels = strings(&["__name__"]);
        rl2[2].expr = regex("test3.*");
        rl2[2].action = RelabelAction::SkipWhenEmpty;

        success = set_relabel_configs(&rl2).get()?;
        assert_eq!(success.metrics_relabeled_due_to_collision, 0);
        assert_eq!(count_by_label(""), 3);
        assert_eq!(skip_count(), 3);
        // clear the configuration
        success = set_relabel_configs(&[]).get()?;
        let _ = success;
        app_metrics.add_group(
            "test3",
            vec![
                make_counter_labeled(
                    "counter_3",
                    Description::new("counter 2"),
                    vec![LabelInstance::new("lev3", "3")],
                    || 0,
                )
                .skip(SkipWhenEmpty::YES),
            ],
        )?;
        let mut rl3 = vec![RelabelConfig::default(); 3];
        rl3[0].source_labels = strings(&["__name__"]);
        rl3[0].action = RelabelAction::Drop;

        rl3[1].source_labels = strings(&["lev3"]);
        rl3[1].expr = regex("3");
        rl3[1].action = RelabelAction::Keep;

        rl3[2].source_labels = strings(&["__name__"]);
        rl3[2].expr = regex("test3.*");
        rl3[2].action = RelabelAction::ReportWhenEmpty;

        success = set_relabel_configs(&rl3).get()?;
        assert_eq!(success.metrics_relabeled_due_to_collision, 0);
        assert_eq!(skip_count(), 0);
        set_relabel_configs(&[]).get()?;
        Ok(())
    })
});

engine_test_case!(test_estimated_histogram, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut histogram1 = TimeEstimatedHistogram::default();
        let mut histogram2 = TimeEstimatedHistogram::default();
        // The number of linearly-spaced buckets between consecutive powers of 2 in time_estimated_histogram.
        const PRECISION: u32 = 4;
        // The lower bound of time_estimated_histogram is 512 us.
        let mut min = Duration::from_micros(512);
        let mut next = min * 2;
        for _ in 0..16 {
            let delta = (next - min) / PRECISION;
            for j in 0..PRECISION {
                histogram1.add(min + delta * j);
            }
            min = next;
            next *= 2;
        }
        assert_eq!(histogram1.count(), 64);
        for i in 0..64 {
            assert_eq!(histogram1.get(i), 1);
        }

        min = Duration::from_micros(512);
        next = min * 2;
        for _ in 0..8 {
            let delta = (next - min) / PRECISION;
            for j in 0..PRECISION {
                histogram2.add(min + delta * j);
            }
            min = next;
            next *= 2;
        }
        assert_eq!(histogram2.count(), 32);
        for i in 0..32 {
            assert_eq!(histogram2.get(i), 1);
        }
        for i in 33..64 {
            assert_eq!(histogram2.get(i), 0);
        }
        histogram1.merge(&histogram2);
        assert_eq!(histogram1.count(), 96);
        for i in 0..32 {
            assert_eq!(histogram1.get(i), 2);
        }
        for i in 33..64 {
            assert_eq!(histogram1.get(i), 1);
        }
        let mh = histogram1.to_metrics_histogram();
        for i in 0..32u64 {
            assert_eq!(mh.buckets[i as usize].count, 2 + i * 2);
        }
        for i in 33..64u64 {
            assert_eq!(mh.buckets[i as usize].count, 33 + i);
        }
        Ok(())
    })
});
