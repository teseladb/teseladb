//! What a core does with its copy of a range during a core change (007 FR-011,
//! `spec/tla/CoreRelocate.tla`). A range has the same core on every replica; `Relocate` in its log
//! moves it. On each node:
//!
//! 1. The replica that applies `Relocate` to another core stops applying on its own core and hands
//!    the range over to the target core of its node: its Raft state (term, vote, configuration),
//!    its state at the index it applied, the entries of its log past that index and its data as
//!    SST files.
//! 2. The target core ingests the data and writes the group whole to its log. Once that is
//!    durable it tells the old core, which removes its group, its log and its data.
//! 3. Once that removal is durable the target core starts the group. Two copies never run.
//!
//! After a restart, the old copy (its state says the range moved) hands the range over again; a
//! copy imported before the restart and not started is dropped while another core of the node
//! still holds the range, and started once none does.

/// What a core holds of a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Copy {
    /// A group of this core that runs, or ran before a restart.
    Running,
    /// A group written whole to this core's log by a hand-over and not started yet; `this_run`
    /// if the hand-over arrived since this core started.
    Imported {
        /// The hand-over arrived since this core started.
        this_run: bool,
    },
}

/// What the core does next with its copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Run it.
    Run,
    /// Stop it and hand the range over to the target core.
    Hand,
    /// Remove it: another copy is the range's.
    Drop,
    /// Start it: no other core of the node holds the range any more.
    Start,
    /// Wait for the other core.
    Wait,
}

/// The next step of core `core` with its copy `copy` of a range whose state says it moved to
/// `target` (`None`: never moved). `others_hold` says whether another core of the node holds the
/// range (`None`: not known yet).
pub fn step(core: u16, target: Option<u16>, copy: Copy, others_hold: Option<bool>) -> Step {
    match copy {
        Copy::Running if target.is_none_or(|t| t == core) => Step::Run,
        // The target drops an imported copy of an earlier hand-over when this one arrives.
        Copy::Running => Step::Hand,
        Copy::Imported { .. } if others_hold == Some(false) => Step::Start,
        Copy::Imported { this_run: false } if others_hold == Some(true) => Step::Drop,
        Copy::Imported { .. } => Step::Wait,
    }
}
