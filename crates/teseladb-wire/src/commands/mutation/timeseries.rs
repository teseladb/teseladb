//! Inserts into a time series on the range's leader (015 FR-006, T012): the write's one key is
//! the bucket its measurements go to (`teseladb_mql::timeseries_write`), which the leader reads
//! and writes again with them. A full bucket refuses the write with [`is_refusal`], and the
//! command sends it to the next bucket of the window.

use teseladb_bson::Document;
use teseladb_hlc::Hlc;
use teseladb_keys::local_id::NO_LOCAL_ID;
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
/// See [`FULL_COUNT`]: the measurement has a field of another type than the bucket's.
pub const FULL_SCHEMA: &str = "time series bucket is closed: schema change";

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
/// See [`BUCKET_EXISTED`]: it stopped for a measurement with a field of another type.
pub const STOPPED_BY_SCHEMA: i64 = 16;

/// The message prefix of a measurement refused into a bucket that was there: RefDb writes it
/// as an update of the bucket (measured on a `refnode` 9.0.4 on 02-11-2026).
pub const UPDATE_FAILED: &str = "Plan executor error during update :: caused by :: ";

/// Why `e` refuses a write because its bucket is full, if it does.
pub fn refusal(e: &CommandError) -> Option<Full> {
    match (e.code.code == WRITE_CONFLICT.code, e.errmsg.as_str()) {
        (true, FULL_COUNT) => Some(Full::Count),
        (true, FULL_SIZE) => Some(Full::Size),
        (true, FULL_SCHEMA) => Some(Full::Schema),
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
        TsError::Full(Full::Schema) => CommandError::new(WRITE_CONFLICT, FULL_SCHEMA),
        TsError::Malformed(msg) => CommandError::new(INTERNAL_ERROR, msg),
    }
}

/// Evaluates the insert of measurements `docs` into the bucket at `keys[0]`, whose `_id` is
/// `id`, in order, until the bucket stops taking them (the result's `n` says how many it took;
/// none at all is [`is_refusal`]). The measurements it takes go in together: when one of them
/// fails, none does and the result carries that error at its place (measured on a `refnode`
/// 9.0.4 on 02-11-2026).
pub(super) fn insert(
    keys: &[Vec<u8>],
    docs: &[Document],
    (spec, id): (&BucketSpec, [u8; 12]),
    ts: Hlc,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<Evaluated, CommandError> {
    // A time series write is bound to its one bucket.
    let key = keys.first().map(Vec::as_slice).unwrap_or_default();
    let old = lookup(key).map(|(_, v)| teseladb_keys::stored::raw(v)).transpose()?;
    // A measurement the bucket's indexes refuse (a geometry, 511) is found alone: its own
    // bucket's entries are checked before it goes in.
    let alone = |doc: &Document| -> Result<(), CommandError> {
        let one = spec.append(id, None, doc).map_err(ts_error)?.to_bytes()?;
        checker.entries_at(Some(key), (None, NO_LOCAL_ID), Some(&one), lookup).map(|_| ())
    };
    let mut r = MutationResult::default();
    let mut filled = Vec::with_capacity(docs.len());
    let mut refused = None;
    for (index, doc) in docs.iter().enumerate() {
        let one = fill_timestamps(doc, Some(ts))?.unwrap_or_else(|| doc.clone());
        let checked = stored_bytes(&one, Stored::Insert)
            .and_then(|b| checker.check(&b, None))
            .and_then(|()| alone(&one));
        filled.push(one);
        if let Err(error) = checked {
            refused = Some(WriteError { index, error });
            break;
        }
    }
    let (bucket, taken, stopped) = spec.append_all(id, old, &filled).map_err(ts_error)?;
    if let Some(mut e) = refused.filter(|e| e.index < taken) {
        // The bucket takes the refused measurement: none of them goes in.
        if old.is_some() {
            e.error.errmsg = format!("{UPDATE_FAILED}{}", e.error.errmsg);
        }
        r.errors.push(e);
        return Ok((Vec::new(), Vec::new(), r));
    }
    r.n = i64::try_from(taken).unwrap_or(i64::MAX);
    r.n_modified = if old.is_some() { BUCKET_EXISTED } else { 0 }
        | match stopped.full {
            Some(Full::Count) => STOPPED_BY_COUNT,
            Some(Full::Size) => STOPPED_BY_SIZE,
            Some(Full::Schema) => STOPPED_BY_SCHEMA,
            None => 0,
        }
        | if stopped.kept_open { KEPT_OPEN_LARGE } else { 0 };
    let new = bucket.to_bytes()?;
    // The bucket's own indexes (on the meta value and the time, 015 FR-008) follow it.
    let (mut puts, deletes) =
        checker.entries_at(Some(key), (old, NO_LOCAL_ID), Some(&new), lookup)?;
    puts.push(put(key, (new, NO_LOCAL_ID), checker, lookup(key).map(|(_, v)| v)));
    Ok((puts, deletes, r))
}

/// The place of the first of measurements `docs` of a time series laid out by `spec` that the
/// collection's rules refuse on their own (its validator, or an index that cannot take it): an
/// ordered insert stops there before it writes any bucket.
pub fn first_refused(docs: &[Document], spec: &BucketSpec, rules: &super::Rules) -> Option<usize> {
    let checker = super::Checker::new(rules, true).ok()?;
    let none = |_: &[u8]| -> Option<(u8, &[u8])> { None };
    let ts = Hlc { physical: 0, logical: 0 };
    docs.iter().position(|doc| {
        // With a mark the filling never fails.
        let filled = fill_timestamps(doc, Some(ts)).ok().flatten();
        let one = filled.unwrap_or_else(|| doc.clone());
        let alone = || -> Result<(), CommandError> {
            let bucket = spec.append([0; 12], None, &one).map_err(ts_error)?.to_bytes()?;
            checker.entries_at(Some(&[]), (None, NO_LOCAL_ID), Some(&bucket), &none).map(|_| ())
        };
        stored_bytes(&one, Stored::Insert)
            .and_then(|b| checker.check(&b, None))
            .and_then(|()| alone())
            .is_err()
    })
}
