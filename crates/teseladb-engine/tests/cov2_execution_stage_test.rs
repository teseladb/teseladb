//! Execution stages beyond execution_stage_test: the stage accessors (`name`, `poll`,
//! `flush`), the per-stage counters exported as `execution_stages_*` metrics, dropping a
//! stage whose flush task is pending (its queued calls break, its memory is freed by that
//! task), the registry lookups, the inheriting stage's per-group statistics and its failure
//! when the per-group stage name is taken (the original's `_stages_by_name` uniqueness).
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use teseladb::engine::core::execution_stage::internal::ExecutionStageManager;
use teseladb::engine::core::execution_stage::{
    ConcreteExecutionStage, ExecutionStage, InheritingConcreteExecutionStage,
};
use teseladb::engine::core::future::{BrokenPromise, ExceptionPtr, InvalidArgument};
use teseladb::engine::core::metrics::impl_fns::get_value_map;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine_test_case;

fn name(s: &str) -> Sstring {
    Sstring::from(s)
}

/// The value of the execution-stage counter `metric` of stage `stage`.
fn stage_metric(metric: &str, stage: &str) -> Option<f64> {
    let registry = get_value_map();
    // SAFETY: this shard's registry, with no other reference alive across the lookup.
    let values = unsafe { registry.get_mut_unchecked() }.get_value_map();
    let fam = values.get(&name(&format!("execution_stages_{metric}")))?;
    for (labels, slot) in fam.iter() {
        if labels
            .labels()
            .get(&name("execution_stage"))
            .is_some_and(|v| *v == stage)
            && let Some(m) = slot
        {
            return Some(m.call().d());
        }
    }
    None
}

engine_test_case!(accessors_flush_poll_and_exported_counters, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage = ConcreteExecutionStage::with_default_group(&name("cov2_acc"), |x: i32| {
            Ok::<i32, ExceptionPtr>(x * 2)
        })?;
        assert_eq!(stage.name(), "cov2_acc");
        assert!(!stage.poll(), "nothing queued");
        assert!(!stage.flush(), "an empty stage schedules nothing");
        let f = stage.call(21);
        assert!(stage.poll(), "one call queued");
        assert!(!stage.flush(), "the flush task is already scheduled");
        assert_eq!(f.get()?, 42);
        assert!(!stage.poll());
        let st = *stage.get_stats();
        assert_eq!(st.function_calls_enqueued, 1);
        assert_eq!(st.function_calls_executed, 1);
        assert_eq!(st.tasks_scheduled, 1);
        assert_eq!(stage_metric("tasks_scheduled", "cov2_acc"), Some(1.0));
        assert_eq!(
            stage_metric("function_calls_enqueued", "cov2_acc"),
            Some(1.0)
        );
        assert_eq!(
            stage_metric("function_calls_executed", "cov2_acc"),
            Some(1.0)
        );
        // Whether the one flush was preempted depends on timing; the metric reports the counter.
        assert_eq!(
            stage_metric("tasks_preempted", "cov2_acc"),
            Some(st.tasks_preempted as f64)
        );
        assert!(st.tasks_preempted <= 1);
        drop(stage);
        assert_eq!(
            stage_metric("tasks_scheduled", "cov2_acc"),
            None,
            "unregistered"
        );
        Ok(())
    })
});

engine_test_case!(dropping_a_stage_with_a_pending_flush_breaks_its_calls, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage =
            ConcreteExecutionStage::with_default_group(&name("cov2_drop"), |x: i32| {
                Ok::<i32, ExceptionPtr>(x)
            })?;
        let f1 = stage.call(1);
        let f2 = stage.call(2);
        drop(stage);
        for f in [f1, f2] {
            let e = f.get().err().expect("the call was dropped with the stage");
            assert!(e.downcast_ref::<BrokenPromise>().is_some(), "{e}");
        }
        // The pending flush task runs now and frees the orphaned stage.
        yield_().get()?;
        // The name is free again.
        let mut again =
            ConcreteExecutionStage::with_default_group(&name("cov2_drop"), |x: i32| {
                Ok::<i32, ExceptionPtr>(x + 1)
            })?;
        assert_eq!(again.call(1).get()?, 2);
        Ok(())
    })
});

engine_test_case!(the_registry_finds_stages_by_name, {
    async_(|| -> Result<(), ExceptionPtr> {
        let stage = ConcreteExecutionStage::new(
            &name("cov2_reg"),
            SchedulingGroup::default(),
            |x: i32| Ok::<i32, ExceptionPtr>(x),
        )?;
        let mgr = ExecutionStageManager::get();
        // SAFETY: this thread's manager; each reference ends with its call.
        unsafe {
            let s: *mut ExecutionStage = (*mgr).get_stage("cov2_reg");
            assert!(!s.is_null());
            assert_eq!((*s).name(), "cov2_reg");
            assert!((*mgr).get_stage("cov2_no_such_stage").is_null());
            // Re-pointing a registration that is not there changes nothing; re-pointing the
            // stage to itself keeps it registered.
            (*mgr).update_execution_stage_registration(std::ptr::dangling_mut(), s);
            (*mgr).update_execution_stage_registration(s, s);
            assert_eq!((*mgr).get_stage("cov2_reg"), s);
            // Unregistering a stage that is not registered is a no-op.
            (*mgr).unregister_execution_stage(std::ptr::dangling_mut());
            assert_eq!((*mgr).get_stage("cov2_reg"), s);
        }
        drop(stage);
        Ok(())
    })
});

engine_test_case!(inheriting_stage_reports_per_group_stats_and_name_clashes, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage = InheritingConcreteExecutionStage::new(&name("cov2_inh"), |x: i32| {
            Ok::<i32, ExceptionPtr>(x * 3)
        });
        assert!(stage.get_stats().is_empty(), "no group used yet");
        assert_eq!(stage.call(2).get()?, 6);
        assert_eq!(stage.call(3).get()?, 9);
        let stats = stage.get_stats();
        assert_eq!(stats.len(), 1);
        assert!(stats[0].sg == SchedulingGroup::default());
        assert_eq!(stats[0].stats.function_calls_executed, 2);

        // The per-group stage of "cov2_clash" in the default group is "cov2_clash.<group>";
        // a concrete stage of that name already exists, so the call fails.
        let group_name = SchedulingGroup::default().name().clone();
        let taken = format!("cov2_clash.{}", group_name.as_str());
        let _holder = ConcreteExecutionStage::with_default_group(&name(&taken), |x: i32| {
            Ok::<i32, ExceptionPtr>(x)
        })?;
        let mut clash = InheritingConcreteExecutionStage::new(&name("cov2_clash"), |x: i32| {
            Ok::<i32, ExceptionPtr>(x)
        });
        let e = clash.call(1).get().err().expect("name taken");
        assert!(e.downcast_ref::<InvalidArgument>().is_some(), "{e}");
        assert!(e.to_string().contains("already exists"), "{e}");
        Ok(())
    })
});
