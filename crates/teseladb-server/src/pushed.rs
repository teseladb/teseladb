//! Work the leader of a range does over the rows of a scan for the node that asked for it, so
//! that the rows do not travel: the `$match`es and the partial `$group` of an aggregation, whose
//! partial groups go back in bytes (010 FR-034), and a scan's filter, after which only the
//! documents it accepts go back (FR-027). The request carries the work in bytes
//! ([`Work`]); the leader scans its part of the range as for any scan, runs the work over the
//! rows on its core and answers [`Worked`]. A leader that cannot run it answers
//! [`Worked::Declined`], and the node that asked reads the part as before.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_exec::pipeline::Program;
use teseladb_wire::commands::pushed_filter::SentFilter;
use teseladb_types::{DecodeError, Decoder, Encoder};

/// The rows of a scan, key and stored document.
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// What the leader runs over the rows of the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Work {
    /// An aggregation's program ([`Program::encode`]): the answer is its partial groups.
    Fold(Vec<u8>),
    /// A filter in BSON that decides from the document alone: the answer is the documents it
    /// accepts.
    Filter(Vec<u8>),
}

/// What the leader answers for a [`Work`] over a part of its range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Worked {
    /// The partial groups of the rows read ([`teseladb_exec::pipeline::PartialGroups::encode`]).
    Folded {
        /// The encoded groups.
        groups: Vec<u8>,
        /// Documents read.
        examined: u32,
        /// Where the scan of the range goes on, when it stopped at its number of documents.
        next: Option<Vec<u8>>,
    },
    /// The documents the filter accepted.
    Matched {
        /// Key and stored document of each, in key order.
        rows: Rows,
        /// For each of them, the documents examined up to it, itself included.
        seen: Vec<u32>,
        /// Documents read.
        examined: u32,
        /// Where the scan of the range goes on, when it stopped at its number of documents.
        next: Option<Vec<u8>>,
    },
    /// The leader did not run it: the program did not decode or declined the rows (a document
    /// that does not read, a stage or a filter that fails, keys past the limit, a state that does
    /// not merge). The node that asked reads the part itself, which gives the error a run in one
    /// core gives.
    Declined,
}

const FOLD: u8 = 0;
const FILTER: u8 = 1;
const FOLDED: u8 = 0;
const DECLINED: u8 = 1;
const MATCHED: u8 = 2;

/// Parts a leader ran for a request of another node since the process started.
static FOR_OTHERS: AtomicU64 = AtomicU64::new(0);

/// Parts of a scan whose work the leader of their range ran for a request that came from
/// another node, since the process started (010 FR-034).
pub fn ran_for_other_nodes() -> u64 {
    FOR_OTHERS.load(Ordering::Relaxed)
}

/// Counts a part the leader ran for another node.
pub(crate) fn note_ran_for_other_node() {
    FOR_OTHERS.fetch_add(1, Ordering::Relaxed);
}

/// The kinds of work sent to the leaders that can go back to the usual path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// A split aggregation ([`Work::Fold`]): the coordinator reads the span and runs the
    /// pipeline.
    Fold,
    /// A part of a filtered scan ([`Work::Filter`]): the node reads the part and filters it.
    Filter,
    /// A batch of an index scan's document keys (010 FR-027): each key is read on its own, or
    /// the documents come back for the filter to be evaluated here.
    Lookup,
}

impl Fallback {
    /// Every kind, in the order of [`fallbacks`].
    pub const ALL: [Fallback; 3] = [Fallback::Fold, Fallback::Filter, Fallback::Lookup];

    /// Its label in `/metrics`.
    pub fn label(self) -> &'static str {
        match self {
            Fallback::Fold => "fold",
            Fallback::Filter => "filter",
            Fallback::Lookup => "lookup",
        }
    }
}

/// Work that went back to the usual path, by [`Fallback`].
static FELL_BACK: [AtomicU64; Fallback::ALL.len()] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Times work sent to the leaders went back to the usual path since the process started, in the
/// order of [`Fallback::ALL`].
pub fn fallbacks() -> [u64; Fallback::ALL.len()] {
    FELL_BACK.each_ref().map(|n| n.load(Ordering::Relaxed))
}

/// Counts work of kind `f` that went back to the usual path.
pub(crate) fn note_fallback(f: Fallback) {
    FELL_BACK[f as usize].fetch_add(1, Ordering::Relaxed);
}

/// Writes the counters of the work sent to the leaders, and of the work this node ran for
/// others, at the end of `out` in the text format of `/metrics` (010 FR-034).
pub fn metrics(out: &mut String) {
    use std::fmt::Write as _;
    let counters = [
        (
            "teseladb_pushed_scattered_groups_total",
            "Aggregations whose partial groups the cores that own the ranges gave.",
            crate::backend::scattered_groups(),
        ),
        (
            "teseladb_pushed_scattered_parts_sent_away_total",
            "Parts of split aggregations whose program went to the leader on another node.",
            crate::backend::scattered_parts_sent_away(),
        ),
        (
            "teseladb_pushed_filtered_parts_sent_total",
            "Parts of filtered scans sent with their filter to the leader of their range.",
            crate::backend::filtered_parts_sent(),
        ),
        (
            "teseladb_pushed_parts_taken_ahead_total",
            "Parts of filtered scans asked for ahead that their scan took.",
            crate::backend::parts_taken_ahead(),
        ),
        (
            "teseladb_pushed_ran_for_other_nodes_total",
            "Parts this node ran as leader for a request of another node.",
            ran_for_other_nodes(),
        ),
        (
            "teseladb_pushed_lookup_batches_sent_total",
            "Batches of an index scan's document keys sent to the core of their range.",
            crate::backend::lookup_batches_sent(),
        ),
        (
            "teseladb_pushed_lookup_keys_sent_total",
            "Document keys those batches carried.",
            crate::backend::lookup_keys_sent(),
        ),
        (
            "teseladb_pushed_lookup_keys_filtered_total",
            "Document keys sent with the residual filter for the leader to evaluate.",
            crate::backend::lookup_keys_filtered(),
        ),
        (
            "teseladb_pushed_lookup_keys_here_total",
            "Document keys of index scans read on this node outside the cores.",
            crate::backend::lookup_keys_here(),
        ),
    ];
    for (name, help, n) in counters {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {n}");
    }
    let name = "teseladb_pushed_fallbacks_total";
    let _ =
        writeln!(out, "# HELP {name} Work sent to the leaders that went back to the usual path.");
    let _ = writeln!(out, "# TYPE {name} counter");
    for (f, n) in Fallback::ALL.iter().zip(fallbacks()) {
        let _ = writeln!(out, "{name}{{work=\"{}\"}} {n}", f.label());
    }
}

/// Where the scan of a range goes on after `rows`, a part of at most `limit` documents: after its
/// last key when it is full.
pub(crate) fn next_after(rows: &Rows, limit: usize) -> Option<Vec<u8>> {
    match (rows.len() >= limit, rows.last()) {
        (true, Some((k, _))) => Some([k.as_slice(), &[0]].concat()),
        _ => None,
    }
}

impl Work {
    /// Writes it at the end of `e`.
    pub(crate) fn put(&self, e: &mut Encoder) {
        match self {
            Work::Fold(program) => {
                e.u8(FOLD).bytes(program);
            }
            Work::Filter(filter) => {
                e.u8(FILTER).bytes(filter);
            }
        }
    }

    /// Reads what [`Work::put`] wrote.
    pub(crate) fn get(d: &mut Decoder<'_>) -> Result<Work, DecodeError> {
        let at = d.position();
        match d.u8()? {
            FOLD => Ok(Work::Fold(d.bytes()?.to_vec())),
            FILTER => Ok(Work::Filter(d.bytes()?.to_vec())),
            _ => Err(DecodeError::InvalidValue { offset: at }),
        }
    }

    /// The answer for `rows`, the leader's part of at most `limit` documents.
    pub(crate) fn apply(&self, rows: Rows, limit: usize) -> Worked {
        let examined = u32::try_from(rows.len()).unwrap_or(u32::MAX);
        let next = next_after(&rows, limit);
        match self {
            Work::Fold(program) => {
                let groups = Program::decode(program)
                    .and_then(|p| p.run(&rows))
                    .and_then(|groups| groups.encode());
                match groups {
                    Some(groups) => Worked::Folded { groups, examined, next },
                    None => Worked::Declined,
                }
            }
            Work::Filter(filter) => {
                let key = rows.first().map_or(&[][..], |(k, _)| k.as_slice());
                let Some(kept) = SentFilter::parse(filter, key).and_then(|f| f.keep(rows)) else {
                    return Worked::Declined;
                };
                let seen = kept.seen.into_iter().map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                Worked::Matched { rows: kept.rows, seen: seen.collect(), examined, next }
            }
        }
    }
}

impl Worked {
    /// Writes it at the end of `e`.
    pub(crate) fn put(&self, e: &mut Encoder) {
        match self {
            Worked::Folded { groups, examined, next } => {
                e.u8(FOLDED).bytes(groups).u32(*examined).bool(next.is_some());
                e.bytes(next.as_deref().unwrap_or_default());
            }
            Worked::Matched { rows, seen, examined, next } => {
                e.u8(MATCHED).var(rows.len() as u64);
                for ((k, v), n) in rows.iter().zip(seen) {
                    e.bytes(k).bytes(v).u32(*n);
                }
                e.u32(*examined).bool(next.is_some());
                e.bytes(next.as_deref().unwrap_or_default());
            }
            Worked::Declined => {
                e.u8(DECLINED);
            }
        }
    }

    /// Reads what [`Worked::put`] wrote.
    pub(crate) fn get(d: &mut Decoder<'_>) -> Result<Worked, DecodeError> {
        let at = d.position();
        match d.u8()? {
            FOLDED => {
                let (groups, examined) = (d.bytes()?.to_vec(), d.u32()?);
                let (some, next) = (d.bool()?, d.bytes()?.to_vec());
                Ok(Worked::Folded { groups, examined, next: some.then_some(next) })
            }
            DECLINED => Ok(Worked::Declined),
            MATCHED => {
                let n = d.var()?;
                if n > d.remaining() as u64 {
                    return Err(DecodeError::InvalidValue { offset: at });
                }
                let (mut rows, mut seen) = (Vec::new(), Vec::new());
                for _ in 0..n {
                    rows.push((d.bytes()?.to_vec(), d.bytes()?.to_vec()));
                    seen.push(d.u32()?);
                }
                let examined = d.u32()?;
                let (some, next) = (d.bool()?, d.bytes()?.to_vec());
                Ok(Worked::Matched { rows, seen, examined, next: some.then_some(next) })
            }
            _ => Err(DecodeError::InvalidValue { offset: at }),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_program_that_does_not_decode_is_declined() {
        let rows: Rows = vec![(vec![1], vec![5, 0, 0, 0, 0])];
        assert_eq!(Work::Fold(b"junk".to_vec()).apply(rows.clone(), 10), Worked::Declined);
        assert_eq!(Work::Filter(b"junk".to_vec()).apply(rows, 10), Worked::Declined);
    }

    #[test]
    fn work_and_answers_read_back_and_nothing_else_does() {
        let works = [Work::Fold(vec![1, 2, 3]), Work::Fold(Vec::new()), Work::Filter(vec![5, 0])];
        for w in works {
            let mut e = Encoder::new();
            w.put(&mut e);
            let b = e.finish();
            assert_eq!(Work::get(&mut Decoder::new(&b)).unwrap(), w);
        }
        let answers = [
            Worked::Folded { groups: vec![9; 4], examined: 3, next: Some(vec![1, 0]) },
            Worked::Folded { groups: Vec::new(), examined: 0, next: None },
            Worked::Declined,
            Worked::Matched {
                rows: vec![(vec![1], vec![2, 3]), (vec![4], vec![])],
                seen: vec![1, 7],
                examined: 9,
                next: Some(vec![4, 0]),
            },
            Worked::Matched { rows: Vec::new(), seen: Vec::new(), examined: 0, next: None },
        ];
        for a in answers {
            let mut e = Encoder::new();
            a.put(&mut e);
            let b = e.finish();
            assert_eq!(Worked::get(&mut Decoder::new(&b)).unwrap(), a);
        }
        assert!(Work::get(&mut Decoder::new(&[7])).is_err());
        assert!(Worked::get(&mut Decoder::new(&[7])).is_err());
    }

    #[test]
    fn a_fallback_counts_under_its_kind_in_the_metrics() {
        let before = fallbacks();
        note_fallback(Fallback::Filter);
        let after = fallbacks();
        assert!(after[1] > before[1]);
        let mut out = String::new();
        metrics(&mut out);
        let line = "teseladb_pushed_fallbacks_total{work=\"filter\"} ";
        let n: u64 = out.lines().find_map(|l| l.strip_prefix(line)?.parse().ok()).unwrap();
        assert!(n >= after[1]);
        assert!(out.contains("teseladb_pushed_fallbacks_total{work=\"fold\"} "), "{out}");
    }

    #[test]
    fn a_full_part_goes_on_after_its_last_key() {
        let rows: Rows = vec![(vec![1], vec![]), (vec![2], vec![])];
        assert_eq!(next_after(&rows, 2), Some(vec![2, 0]));
        assert_eq!(next_after(&rows, 3), None);
        assert_eq!(next_after(&Vec::new(), 0), None);
    }
}
