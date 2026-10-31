//! Inserts into a time series on the range's leader (015 FR-006, T012): the write's one key is
//! the bucket its measurements go to (`teseladb_mql::timeseries_write`), which the leader reads
//! and writes again with them. A full bucket refuses the write with [`is_refusal`], and the
//! command sends it to the next bucket of the window.

use teseladb_bson::{Document, RawDoc};
use teseladb_hlc::Hlc;
use teseladb_mql::timeseries_write::{BucketSpec, Full, TsError};

use super::{
    Checker, Evaluated, Lookup, MutationResult, Stored, WriteError, fill_timestamps, put,
    stored_bytes,
};
use crate::error::{BAD_VALUE, CommandError, INTERNAL_ERROR, WRITE_CONFLICT};

/// The messages of a write refused because its bucket is full, by count and by size.
pub const FULL_COUNT: &str = "time series bucket is full: count";
/// See [`FULL_COUNT`].
pub const FULL_SIZE: &str = "time series bucket is full: size";

/// The flags of a result's `n_modified` for a time series insert, which the command counts for
/// `collStats` (015 FR-010): the bucket was there, and the bucket stopped taking measurements by
/// its count or its size.
pub const BUCKET_EXISTED: i64 = 1;
/// See [`BUCKET_EXISTED`].
pub const STOPPED_BY_COUNT: i64 = 2;
/// See [`BUCKET_EXISTED`].
pub const STOPPED_BY_SIZE: i64 = 4;
/// See [`BUCKET_EXISTED`]: it stayed open past its size for a large measurement.
pub const KEPT_OPEN_LARGE: i64 = 8;

/// Why `e` refuses a write because its bucket is full, if it does.
pub fn refusal(e: &CommandError) -> Option<Full> {
    match (e.code.code == WRITE_CONFLICT.code, e.errmsg.as_str()) {
        (true, FULL_COUNT) => Some(Full::Count),
        (true, FULL_SIZE) => Some(Full::Size),
        _ => None,
    }
}

/// Whether `e` refuses a write because its bucket is full.
pub fn is_refusal(e: &CommandError) -> bool {
    refusal(e).is_some()
}

fn ts_error(e: TsError) -> CommandError {
    match e {
        TsError::NoTime(msg) => CommandError::new(BAD_VALUE, msg),
        TsError::Full(Full::Count) => CommandError::new(WRITE_CONFLICT, FULL_COUNT),
        TsError::Full(Full::Size) => CommandError::new(WRITE_CONFLICT, FULL_SIZE),
        TsError::Malformed(msg) => CommandError::new(INTERNAL_ERROR, msg),
    }
}

/// Evaluates the insert of measurements `docs` into the bucket at `keys[0]`, whose `_id` is
/// `id`, in order: until the bucket is full (the result's `n` says how many it took; none at all
/// is [`is_refusal`]), or until one fails, whose error the result carries at its place.
pub(super) fn insert(
    keys: &[Vec<u8>],
    docs: &[Document],
    (spec, id): (&BucketSpec, [u8; 12]),
    ts: Hlc,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<Evaluated, CommandError> {
    let [key] = keys else {
        return Err(CommandError::new(INTERNAL_ERROR, "a time series write without its bucket"));
    };
    let old = lookup(key).map(|(_, v)| RawDoc::from_bytes(v)).transpose()?;
    // A measurement the bucket's indexes refuse (a geometry, 511) fails alone: its own bucket's
    // entries are checked before it goes in.
    let alone = |doc: &Document| -> Result<(), CommandError> {
        let one = spec.append(id, None, doc).map_err(ts_error)?.to_bytes()?;
        checker.entries_at(Some(key), None, Some(&one), lookup).map(|_| ())
    };
    let mut r = MutationResult::default();
    let mut valid = Vec::with_capacity(docs.len());
    for (index, doc) in docs.iter().enumerate() {
        let filled = fill_timestamps(doc, ts).unwrap_or_else(|| doc.clone());
        let checked = stored_bytes(&filled, Stored::Insert)
            .and_then(|b| checker.check(&b, None))
            .and_then(|()| alone(&filled));
        match checked {
            Ok(()) => valid.push(filled),
            Err(error) => {
                r.errors.push(WriteError { index, error });
                break;
            }
        }
    }
    if valid.is_empty() {
        return Ok((Vec::new(), Vec::new(), r));
    }
    let (bucket, taken, stopped) = spec.append_all(id, old, &valid).map_err(ts_error)?;
    if taken < valid.len() {
        // The bucket filled up: what it did not take goes to the next one, not an error.
        r.errors.clear();
    }
    r.n = i64::try_from(taken).unwrap_or(i64::MAX);
    r.n_modified = if old.is_some() { BUCKET_EXISTED } else { 0 }
        | match stopped.full {
            Some(Full::Count) => STOPPED_BY_COUNT,
            Some(Full::Size) => STOPPED_BY_SIZE,
            None => 0,
        }
        | if stopped.kept_open { KEPT_OPEN_LARGE } else { 0 };
    let new = bucket.to_bytes()?;
    // The bucket's own indexes (on the meta value and the time, 015 FR-008) follow it.
    let (mut puts, deletes) = checker.entries_at(Some(key), old, Some(&new), lookup)?;
    puts.push(put(key, new));
    Ok((puts, deletes, r))
}
