#![cfg(target_os = "linux")]
//! The stage an inheriting execution stage keeps for a scheduling group is named after the
//! group, and follows its renames. Before, it kept the old name: its metrics were named after
//! a group that no longer existed, and once a new group took the old name, the stage for it
//! could not be created ("Execution stage ... already exists"), so calls from it failed.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::execution_stage::InheritingConcreteExecutionStage;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{
    create_scheduling_group_with_shares, destroy_scheduling_group, rename_scheduling_group,
};
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::{Thread, ThreadAttributes, async_ as thread_async};
use teseladb::engine::util::defer::defer;
use teseladb::engine_test_case;

/// Calls the stage once from a thread of `sg`.
fn call_from(
    es: *mut InheritingConcreteExecutionStage<(), ()>,
    sg: SchedulingGroup,
) -> Result<(), ExceptionPtr> {
    let attr = ThreadAttributes { sched_group: Some(sg), ..ThreadAttributes::default() };
    let mut th = Thread::with_attributes(attr, move || -> Result<(), ExceptionPtr> {
        // SAFETY: the stage outlives the thread, joined right after.
        unsafe { (*es).call(()) }.get()
    })?;
    th.join().get()
}

engine_test_case!(test_execution_stage_follows_scheduling_group_rename, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut es = InheritingConcreteExecutionStage::<(), ()>::new(&Sstring::from("es"), |()| {});
        let esp: *mut InheritingConcreteExecutionStage<(), ()> = &mut es;

        let sg_a = create_scheduling_group_with_shares("s1".into(), 100.0).get()?;
        let _ka = defer(move || {
            let _ = destroy_scheduling_group(sg_a).get();
        });
        call_from(esp, sg_a)?; // creates the stage "es.s1"
        rename_scheduling_group(sg_a, "s2".into()).get()?; // the stage becomes "es.s2"

        let sg_b = create_scheduling_group_with_shares("s1".into(), 100.0).get()?;
        let _kb = defer(move || {
            let _ = destroy_scheduling_group(sg_b).get();
        });
        // A new "es.s1" stage for the new group, which needs the name to be free.
        call_from(esp, sg_b)?;
        // And the renamed group's stage still works.
        call_from(esp, sg_a)?;
        drop(es);
        Ok(())
    })
});
