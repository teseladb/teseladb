//! The state a running command keeps on its thread, taken and put back as a whole (023 FR-018):
//! on the engine, a command that waits for the cores parks its engine thread and the other
//! commands of the shard run on the same OS thread, so each one's thread-local state goes with
//! it. [`take`] leaves the thread as a new one finds it; [`put`] brings a command's state back.
//!
//! What it carries: the cluster's default concerns of the command, what its JavaScript calls see
//! (enabled, cluster time, deadline, operation limit, stored globals, `killOp` flag), the bytes
//! its aggregation holds, the indexes query settings allow it, whether it is a sub-command of the
//! encryption step, and the runs its stages wrote to disk. The caches of a thread (built indexes,
//! compiled programs) are shared by the commands of the thread, as they are by successive ones.

use teseladb_bson::Value;

use crate::concerns::{Level, WriteConcernArgs};

/// The thread-local state of one command.
#[derive(Debug)]
pub struct CommandContext {
    defaults: (Option<Level>, Option<WriteConcernArgs>),
    js: super::js::Context,
    agg_bytes: u64,
    allowed: Option<(u32, Vec<Value>)>,
    in_sub_command: bool,
    runs_written: u64,
}

/// Takes the state of the command running on this thread, leaving a new thread's.
pub fn take() -> CommandContext {
    CommandContext {
        defaults: crate::concerns::take_defaults(),
        js: super::js::take_context(),
        agg_bytes: super::agg_memory::take_mine(),
        allowed: super::plan_exec::take_allowed(),
        in_sub_command: super::fle::take_in_sub_command(),
        runs_written: teseladb_exec::pipeline::spill::swap_runs_written(0),
    }
}

/// Puts back the state [`take`] took.
pub fn put(c: CommandContext) {
    crate::concerns::put_defaults(c.defaults);
    super::js::put_context(c.js);
    super::agg_memory::put_mine(c.agg_bytes);
    super::plan_exec::put_allowed(c.allowed);
    super::fle::put_in_sub_command(c.in_sub_command);
    teseladb_exec::pipeline::spill::swap_runs_written(c.runs_written);
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_command_taken_off_the_thread_leaves_it_clean_and_comes_back_whole() {
        // A command that runs: its defaults, its aggregation, its sub-command flag.
        crate::concerns::put_defaults((Some(Level::Majority), None));
        super::super::agg_memory::put_mine(512);
        super::super::fle::put_in_sub_command(true);
        teseladb_exec::pipeline::spill::swap_runs_written(3);
        super::super::plan_exec::put_allowed(Some((7, vec![Value::String("a_1".into())])));
        let first = take();
        // The next command on the thread starts as on a new thread.
        assert_eq!(crate::concerns::default_read_level(), Level::Local);
        assert_eq!(super::super::agg_memory::take_mine(), 0);
        assert!(!super::super::fle::take_in_sub_command());
        assert_eq!(teseladb_exec::pipeline::spill::runs_written(), 0);
        assert!(super::super::plan_exec::take_allowed().is_none());
        // It writes runs of its own, then the first comes back with its own state.
        teseladb_exec::pipeline::spill::swap_runs_written(10);
        let second = take();
        put(first);
        assert_eq!(crate::concerns::default_read_level(), Level::Majority);
        assert_eq!(super::super::agg_memory::take_mine(), 512);
        assert!(super::super::fle::take_in_sub_command());
        assert_eq!(teseladb_exec::pipeline::spill::runs_written(), 3);
        assert_eq!(super::super::plan_exec::take_allowed().map(|a| a.0), Some(7));
        put(second);
        assert_eq!(teseladb_exec::pipeline::spill::runs_written(), 10);
    }
}
