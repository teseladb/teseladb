//! Work the leader of a range does over the rows of a scan for the node that asked for it, so
//! that the rows do not travel (010 FR-034): the `$match`es and the partial `$group` of an
//! aggregation, whose partial groups go back in bytes. The request carries the work in bytes
//! ([`Work`]); the leader scans its part of the range as for any scan, runs the work over the
//! rows on its core and answers [`Worked`]. A leader that cannot run it answers
//! [`Worked::Declined`], and the node that asked reads the part as before.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_exec::pipeline::Program;
use teseladb_types::{DecodeError, Decoder, Encoder};

/// The rows of a scan, key and stored document.
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// What the leader runs over the rows of the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Work {
    /// An aggregation's program ([`Program::encode`]): the answer is its partial groups.
    Fold(Vec<u8>),
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
    /// The leader did not run it: the program did not decode or declined the rows (a document
    /// that does not read, a stage that fails, keys past the limit, a state that does not
    /// merge). The node that asked reads the part itself, which gives the error a run in one
    /// core gives.
    Declined,
}

const FOLD: u8 = 0;
const FOLDED: u8 = 0;
const DECLINED: u8 = 1;

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
        }
    }

    /// Reads what [`Work::put`] wrote.
    pub(crate) fn get(d: &mut Decoder<'_>) -> Result<Work, DecodeError> {
        let at = d.position();
        match d.u8()? {
            FOLD => Ok(Work::Fold(d.bytes()?.to_vec())),
            _ => Err(DecodeError::InvalidValue { offset: at }),
        }
    }

    /// The answer for `rows`, the leader's part of at most `limit` documents.
    pub(crate) fn apply(&self, rows: &Rows, limit: usize) -> Worked {
        match self {
            Work::Fold(program) => {
                let groups = Program::decode(program)
                    .and_then(|p| p.run(rows))
                    .and_then(|groups| groups.encode());
                match groups {
                    Some(groups) => Worked::Folded {
                        groups,
                        examined: u32::try_from(rows.len()).unwrap_or(u32::MAX),
                        next: next_after(rows, limit),
                    },
                    None => Worked::Declined,
                }
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
        assert_eq!(Work::Fold(b"junk".to_vec()).apply(&rows, 10), Worked::Declined);
    }

    #[test]
    fn work_and_answers_read_back_and_nothing_else_does() {
        let works = [Work::Fold(vec![1, 2, 3]), Work::Fold(Vec::new())];
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
    fn a_full_part_goes_on_after_its_last_key() {
        let rows: Rows = vec![(vec![1], vec![]), (vec![2], vec![])];
        assert_eq!(next_after(&rows, 2), Some(vec![2, 0]));
        assert_eq!(next_after(&rows, 3), None);
        assert_eq!(next_after(&Vec::new(), 0), None);
    }
}
