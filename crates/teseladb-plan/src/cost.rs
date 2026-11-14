//! How the planner chooses among the plans of a query (010 FR-001, T016): each runs for a trial on
//! the node's own data, as the reference server's classic engine does (measured on the reference
//! router 9.0.4, 01-11-2026), until one has given the documents a first batch takes or reached its
//! end, and the plan that gave the most documents per unit of work wins; a plan that needs no
//! fetch, no sort or no intersection gets a small bonus, and ties go to the plan made first.

/// What a plan did in its trial.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Trial {
    /// Units of work: a key or a document read, a document given, the end reached.
    pub works: u64,
    /// Documents given.
    pub advanced: u64,
    /// Whether it reads documents after the keys of an index.
    pub fetches: bool,
    /// Whether it sorts what it reads.
    pub sorts: bool,
    /// Whether it intersects index scans.
    pub intersects: bool,
    /// Whether it reached its end within the trial.
    pub eof: bool,
}

/// The documents a trial ends at (the reference server's `internalQueryPlanEvaluationMaxResults`).
pub const TRIAL_RESULTS: u64 = 101;

/// The units of work a trial ends at, at least (`internalQueryPlanEvaluationWorks`); with more
/// documents, a share of them (`internalQueryPlanEvaluationCollFraction`, 0.3).
pub const TRIAL_WORKS: u64 = 10_000;

/// The units of work a trial over `documents` documents ends at.
pub fn trial_works(documents: u64) -> u64 {
    TRIAL_WORKS.max(documents.saturating_mul(3) / 10)
}

/// The bonus of a plan that reached its end within the trial.
pub const EOF_BONUS: f64 = 1.0;

/// The reference server's score of a trial: one, plus the documents per unit of work, plus a bonus
/// smaller than any difference of those for each of no fetch, no sort and no intersection, plus a
/// whole point for a plan that reached its end (measured: of two plans with the same documents per
/// unit of work, the one at its end wins).
pub fn score(t: &Trial) -> f64 {
    let works = t.works.max(1) as f64;
    let productivity = t.advanced as f64 / works;
    let epsilon = (1.0 / (10.0 * works)).min(1e-4);
    let bonuses = [!t.fetches, !t.sorts, !t.intersects].iter().filter(|b| **b).count() as f64;
    let eof = if t.eof { EOF_BONUS } else { 0.0 };
    1.0 + productivity + epsilon * bonuses + eof
}

/// The plans by their trials, best first; equal scores keep the order the plans were made in.
pub fn ranked(trials: &[Trial]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..trials.len()).collect();
    order.sort_by(|a, b| score(&trials[*b]).total_cmp(&score(&trials[*a])));
    order
}
