//! The `$clusterTime` a client sends (013 FR-014): read with the errors a `refrouter` 9.0.4 gives
//! (measured 30-10-2026, with and without `--keyFile`) and, on a node with authentication,
//! checked against the node's signing keys. The fields are read in the order they come; a
//! subdocument says which of its required fields is missing when it is read; `null` counts as
//! missing; unknown fields are ignored. A signature's hash that is not 20 bytes is refused even
//! for a time that needs no signature. Without keys (no authentication) any signature passes, as
//! on a `refrouter` without `--keyFile`.

use teseladb_auth::clustertime::{self, ClusterTimeError, KEY_LEN, Time};
use teseladb_hlc::MAX_RECEIVED_AHEAD_MS;
use teseladb_mql::decimal_saturating;
use teseladb_bson::{Document, Value};

use crate::commands::Backend;
use crate::error::{
    CLUSTER_TIME_FAILS_RATE_LIMITER, CommandError, IDL_FAILED_TO_PARSE, KEY_NOT_FOUND,
    TIME_PROOF_MISMATCH, TYPE_MISMATCH, UNSUPPORTED_FORMAT,
};
use crate::hello::NodeInfo;
use crate::types::{wrong_type, wrong_types};

/// The generic argument.
pub const CLUSTER_TIME: &str = "$clusterTime";
/// The types `keyId` takes, in the order of RefDb's message.
const KEY_ID_TYPES: [&str; 4] = ["int", "long", "decimal", "double"];
/// The binary subtype of a hash.
const GENERAL: u8 = 0;

/// A `$clusterTime` read from a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GivenTime {
    /// The time.
    pub time: Time,
    /// Its signature: hash and key id.
    pub signature: Option<([u8; KEY_LEN], i64)>,
}

fn missing(path: &str) -> CommandError {
    let msg = format!("BSON field '{path}' is missing but a required field");
    CommandError::new(IDL_FAILED_TO_PARSE, msg)
}

/// RefDb's name of a binary subtype in its messages (measured).
pub(crate) fn subtype_name(subtype: u8) -> &'static str {
    match subtype {
        0 => "general",
        1 => "function",
        2 => "byte(deprecated)",
        3 => "UUID(deprecated)",
        4 => "UUID",
        5 => "MD5",
        6 => "encrypt",
        7 => "column",
        8 => "sensitive",
        9 => "vector",
        0x80 => "Custom",
        _ => "invalid",
    }
}

/// A number as a 64-bit integer the way RefDb reads `keyId`: a double truncated, a decimal
/// rounded half to even, both saturated and NaN as 0 (measured).
fn key_id(path: impl Fn() -> String, v: &Value) -> Result<i64, CommandError> {
    match v {
        Value::Int32(n) => Ok(i64::from(*n)),
        Value::Int64(n) => Ok(*n),
        // `as` truncates toward zero, saturates and gives 0 for NaN.
        Value::Double(d) => Ok(*d as i64),
        Value::Decimal128(d) => Ok(decimal_saturating(*d)),
        other => Err(wrong_types(&path(), other, &KEY_ID_TYPES)),
    }
}

/// The hash and the key id of a `signature` subdocument at `path()`. The paths of the error
/// messages are written only for an error: every command carries a `$clusterTime`.
fn signature(
    path: impl Fn() -> String,
    doc: &Document,
) -> Result<([u8; KEY_LEN], i64), CommandError> {
    let hash_path = || format!("{}.hash", path());
    let id_path = || format!("{}.keyId", path());
    let (mut hash, mut id) = (None, None);
    for (k, v) in doc.iter() {
        match (k, v) {
            ("hash", Value::Null) => hash = None,
            ("keyId", Value::Null) => id = None,
            ("hash", Value::Binary { subtype: GENERAL, bytes }) => hash = Some(bytes.as_slice()),
            ("hash", Value::Binary { subtype, .. }) => {
                let msg = format!(
                    "BSON field '{}' is the wrong binData type '{}', expected type 'general'",
                    hash_path(),
                    subtype_name(*subtype)
                );
                return Err(CommandError::new(TYPE_MISMATCH, msg));
            }
            ("hash", other) => return Err(wrong_type(&hash_path(), other, "binData")),
            ("keyId", other) => id = Some(key_id(id_path, other)?),
            _ => {}
        }
    }
    let hash = hash.ok_or_else(|| missing(&hash_path()))?;
    let id = id.ok_or_else(|| missing(&id_path()))?;
    let hash = clustertime::hash(hash).map_err(|e| error(&e))?;
    Ok((hash, id))
}

/// Reads the `$clusterTime` of command `name`; `None` without one.
pub fn parse(cmd: &Document, name: &str) -> Result<Option<GivenTime>, CommandError> {
    // The paths of the error messages are written only for an error.
    let path = || format!("{name}.{CLUSTER_TIME}");
    let doc = match cmd.get(CLUSTER_TIME) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Document(d)) => d,
        Some(other) => return Err(wrong_type(&path(), other, "object")),
    };
    let time_path = || format!("{}.clusterTime", path());
    let (mut time, mut sig) = (None, None);
    for (k, v) in doc.iter() {
        match (k, v) {
            ("clusterTime", Value::Null) => time = None,
            ("clusterTime", Value::Timestamp { time: secs, increment }) => {
                time = Some(Time { secs: *secs, inc: *increment });
            }
            ("clusterTime", other) => return Err(wrong_type(&time_path(), other, "timestamp")),
            ("signature", Value::Null) => sig = None,
            ("signature", Value::Document(d)) => {
                sig = Some(signature(|| format!("{}.signature", path()), d)?);
            }
            ("signature", other) => {
                return Err(wrong_type(&format!("{}.signature", path()), other, "object"));
            }
            _ => {}
        }
    }
    let time = time.ok_or_else(|| missing(&time_path()))?;
    Ok(Some(GivenTime { time, signature: sig }))
}

/// RefDb's error for a refused `$clusterTime`.
fn error(e: &ClusterTimeError) -> CommandError {
    let code = match e {
        ClusterTimeError::HashLength(_) => UNSUPPORTED_FORMAT,
        ClusterTimeError::KeyNotFound(..) => KEY_NOT_FOUND,
        ClusterTimeError::Mismatch => TIME_PROOF_MISMATCH,
    };
    CommandError::new(code, e.message())
}

/// Reads and checks the `$clusterTime` of command `name` on `node`: a time past the node's own
/// needs a valid signature on a node with keys.
pub fn check(cmd: &Document, name: &str, node: &NodeInfo) -> Result<(), CommandError> {
    let Some(given) = parse(cmd, name)? else { return Ok(()) };
    let Some(ring) = &node.keys else { return Ok(()) };
    let current = Time { secs: node.cluster_time.time, inc: node.cluster_time.increment };
    let signature = given.signature.as_ref().map(|(hash, id)| (hash, *id));
    ring.check(given.time, current, signature).map_err(|e| error(&e))
}

/// Takes the `$clusterTime` of command `name`, already checked, into the node's clock (008
/// FR-013), as a `refrouter` 9.0.4 advances its clock with it: a write that follows lands above what
/// the client saw, even on another node. One past the node's clock and more than
/// [`MAX_RECEIVED_AHEAD_MS`] ahead of its wall clock fails with 205 and the node's clock is left
/// alone (DV-101): a `refrouter` allows 365 days,
/// but the HLC's bound on disk would then keep the node from starting again until its wall clock
/// passed it.
pub fn take<B: Backend + ?Sized>(
    cmd: &Document,
    name: &str,
    backend: &mut B,
) -> Result<(), CommandError> {
    let Some(given) = parse(cmd, name)? else { return Ok(()) };
    let mark = teseladb_hlc::from_timestamp((given.time.secs, given.time.inc));
    // A time the node's clock already reached is one it gave out or heard: taking it moves
    // nothing, whatever the wall clock says (the clock may run ahead of it after a burst of
    // elections, 005 FR-005).
    if mark <= backend.cluster_mark() {
        return Ok(());
    }
    let wall = backend.wall_ms();
    if mark.physical > wall.saturating_add(MAX_RECEIVED_AHEAD_MS) {
        let msg = format!(
            "New $clusterTime, {}, is too far from this node's wall clock time, {}.",
            given.time.secs,
            wall / 1000
        );
        return Err(CommandError::new(CLUSTER_TIME_FAILS_RATE_LIMITER, msg));
    }
    backend.observe_mark(mark);
    Ok(())
}
