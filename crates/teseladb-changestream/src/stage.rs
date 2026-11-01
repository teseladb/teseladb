//! The `$changeStream` stage (014 FR-001): its options and the stages that may follow it, with
//! the errors of a `refrouter` 9.0.4 (measured on 01-11).

use teseladb_bson::Value;
use teseladb_hlc::{Hlc, from_timestamp};

use crate::token::{Position, TokenError};

/// The stage's name.
pub const CHANGE_STREAM: &str = "$changeStream";
/// The stage that splits large events (FR-011).
pub const SPLIT_LARGE_EVENT: &str = "$changeStreamSplitLargeEvent";

const FULL_DOCUMENT: &str = "fullDocument";
const BEFORE_CHANGE: &str = "fullDocumentBeforeChange";
const RESUME_AFTER: &str = "resumeAfter";
const START_AFTER: &str = "startAfter";
const START_AT: &str = "startAtOperationTime";
const ALL_FOR_CLUSTER: &str = "allChangesForCluster";
const EXPANDED: &str = "showExpandedEvents";
const DATA: &str = "_data";

/// Codes of the errors (`code`, as a `refrouter` 9.0.4 answers).
pub mod codes {
    /// `BadValue`.
    pub const BAD_VALUE: i32 = 2;
    /// `FailedToParse`.
    pub const FAILED_TO_PARSE: i32 = 9;
    /// `TypeMismatch`.
    pub const TYPE_MISMATCH: i32 = 14;
    /// `IllegalOperation`.
    pub const ILLEGAL_OPERATION: i32 = 20;
    /// `InvalidOptions`.
    pub const INVALID_OPTIONS: i32 = 72;
    /// `IDLUnknownField`.
    pub const UNKNOWN_FIELD: i32 = 40415;
    /// `$changeStream` not first.
    pub const NOT_FIRST: i32 = 40602;
    /// Both `resumeAfter` and `startAfter`.
    pub const BOTH_STARTS: i32 = 50865;
    /// A resume token without a string `_data`.
    pub const BAD_TOKEN: i32 = 40647;
    /// A token and `startAtOperationTime` together.
    pub const SEVERAL_STARTS: i32 = 40674;
    /// The stage's argument is no document.
    pub const NOT_AN_OBJECT: i32 = 6188500;
    /// `$changeStreamSplitLargeEvent` not last.
    pub const SPLIT_NOT_LAST: i32 = 7182802;
}

/// A refused `$changeStream` pipeline: its code and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageError {
    /// Code.
    pub code: i32,
    /// Message.
    pub message: String,
}

fn error(code: i32, message: impl Into<String>) -> StageError {
    StageError { code, message: message.into() }
}

/// `fullDocument`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FullDocument {
    /// `default`: only inserts and replacements carry it.
    #[default]
    Default,
    /// `updateLookup`: an update carries the document as it is when the event is read.
    UpdateLookup,
    /// `whenAvailable`: the post-image, or `null`.
    WhenAvailable,
    /// `required`: the post-image, or an error.
    Required,
}

/// `fullDocumentBeforeChange`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BeforeChange {
    /// `off`.
    #[default]
    Off,
    /// `whenAvailable`.
    WhenAvailable,
    /// `required`.
    Required,
}

/// Where the stream starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Start {
    /// Now.
    #[default]
    Now,
    /// After the event of a token.
    ResumeAfter(Position),
    /// After the event of a token, also past an `invalidate`.
    StartAfter(Position),
    /// At a mark (`startAtOperationTime`).
    At(Hlc),
}

/// Where the stream is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A collection.
    Collection,
    /// A database (`aggregate: 1` on it).
    Database,
    /// The whole cluster (`aggregate: 1` on `admin` with `allChangesForCluster`).
    Cluster,
}

/// The options of a `$changeStream`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeStreamSpec {
    /// `fullDocument`.
    pub full_document: FullDocument,
    /// `fullDocumentBeforeChange`.
    pub before: BeforeChange,
    /// Where it starts.
    pub start: Start,
    /// `showExpandedEvents`.
    pub expanded: bool,
    /// `allChangesForCluster`.
    pub all_for_cluster: bool,
}

const FULL_DOCUMENT_VALUES: [(&str, FullDocument); 4] = [
    ("default", FullDocument::Default),
    ("updateLookup", FullDocument::UpdateLookup),
    ("whenAvailable", FullDocument::WhenAvailable),
    ("required", FullDocument::Required),
];
const BEFORE_VALUES: [(&str, BeforeChange); 3] = [
    ("off", BeforeChange::Off),
    ("whenAvailable", BeforeChange::WhenAvailable),
    ("required", BeforeChange::Required),
];

fn enumeration<T: Copy>(v: &Value, field: &str, values: &[(&str, T)]) -> Result<T, StageError> {
    let shown = match v {
        Value::String(s) => s.as_str(),
        other => {
            let msg = format!(
                "BSON field '$changeStream.{field}' is the wrong type '{}', expected type 'string'",
                type_name(other)
            );
            return Err(error(codes::TYPE_MISMATCH, msg));
        }
    };
    values.iter().find(|(name, _)| *name == shown).map(|(_, t)| *t).ok_or_else(|| {
        let msg = format!(
            "Enumeration value '{shown}' for field '$changeStream.{field}' is not a valid value."
        );
        error(codes::BAD_VALUE, msg)
    })
}

fn token(v: &Value, field: &str) -> Result<Position, StageError> {
    let Value::Document(d) = v else {
        let msg = format!(
            "BSON field '$changeStream.{field}' is the wrong type '{}', expected type 'object'",
            type_name(v)
        );
        return Err(error(codes::TYPE_MISMATCH, msg));
    };
    let Some(Value::String(data)) = d.get(DATA) else {
        let msg = format!(
            "Bad resume token: _data of missing or of wrong type. Expected string, got {}",
            compact(d)
        );
        return Err(error(codes::BAD_TOKEN, msg));
    };
    Position::decode(data).map_err(|e| match e {
        TokenError::NotHex => {
            error(codes::FAILED_TO_PARSE, "resume token string was not a valid hex string")
        }
        _ => error(codes::FAILED_TO_PARSE, "resume token was not a valid resume token"),
    })
}

/// `d` as the message of a bad token writes it: `{_data: 1}`.
fn compact(d: &teseladb_bson::Document) -> String {
    let bytes = d.to_bytes().unwrap_or_default();
    let shown = teseladb_bson::RawDoc::from_bytes(&bytes)
        .map_or_else(|_| String::from("{}"), teseladb_bson::shell::shell_doc);
    shown.replace("{ ", "{").replace(" }", "}")
}

fn boolean(v: &Value, field: &str) -> Result<bool, StageError> {
    match v {
        Value::Boolean(b) => Ok(*b),
        other => {
            let msg = format!(
                "Field '{field}' should be a boolean value, but found: {}",
                type_name(other)
            );
            Err(error(codes::TYPE_MISMATCH, msg))
        }
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Document(_) => "object",
        Value::Array(_) => "array",
        Value::Int32(_) => "int",
        Value::Int64(_) => "long",
        Value::Double(_) => "double",
        Value::Null => "null",
        _ => "other",
    }
}

impl ChangeStreamSpec {
    /// Reads the stage's argument `v`; `scope` is where it was opened and `db` its database.
    pub fn parse(v: &Value, scope: Scope, ns: &str) -> Result<ChangeStreamSpec, StageError> {
        let Value::Document(d) = v else {
            let msg = format!(
                "$changeStream must take a nested object but found: $changeStream: {}",
                shown(v)
            );
            return Err(error(codes::NOT_AN_OBJECT, msg));
        };
        let mut spec = ChangeStreamSpec::default();
        let mut resume = None;
        let mut start_after = None;
        for (k, v) in d.iter() {
            match k {
                FULL_DOCUMENT => spec.full_document = enumeration(v, k, &FULL_DOCUMENT_VALUES)?,
                BEFORE_CHANGE => spec.before = enumeration(v, k, &BEFORE_VALUES)?,
                RESUME_AFTER => resume = Some(v),
                START_AFTER => start_after = Some(v),
                START_AT => match v {
                    Value::Timestamp { time, increment } => {
                        spec.start = Start::At(from_timestamp((*time, *increment)));
                    }
                    other => {
                        let msg = format!(
                            "BSON field '$changeStream.{k}' is the wrong type '{}', expected type 'timestamp'",
                            type_name(other)
                        );
                        return Err(error(codes::TYPE_MISMATCH, msg));
                    }
                },
                ALL_FOR_CLUSTER => spec.all_for_cluster = boolean(v, k)?,
                EXPANDED => spec.expanded = boolean(v, k)?,
                other => {
                    let msg = format!("BSON field '$changeStream.{other}' is an unknown field.");
                    return Err(error(codes::UNKNOWN_FIELD, msg));
                }
            }
        }
        match (resume, start_after) {
            (Some(_), Some(_)) => {
                let msg =
                    "Do not specify both 'resumeAfter' and 'startAfter' in a $changeStream stage";
                return Err(error(codes::BOTH_STARTS, msg));
            }
            (Some(v), None) => {
                spec.start = several(&spec, Start::ResumeAfter(token(v, RESUME_AFTER)?))?
            }
            (None, Some(v)) => {
                spec.start = several(&spec, Start::StartAfter(token(v, START_AFTER)?))?
            }
            (None, None) => {}
        }
        if spec.all_for_cluster != (scope == Scope::Cluster) {
            let msg = format!(
                "A $changeStream with 'allChangesForCluster:true' may only be opened on the 'admin' database, and with no collection name; found {ns}"
            );
            if spec.all_for_cluster {
                return Err(error(codes::INVALID_OPTIONS, msg));
            }
        }
        Ok(spec)
    }
}

/// `start`, unless `spec` already starts at a mark.
fn several(spec: &ChangeStreamSpec, start: Start) -> Result<Start, StageError> {
    match spec.start {
        Start::At(_) => Err(error(
            codes::SEVERAL_STARTS,
            "Only one type of resume option is allowed, but multiple were found",
        )),
        _ => Ok(start),
    }
}

fn shown(v: &Value) -> String {
    match v {
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Double(n) => n.to_string(),
        Value::String(s) => format!("\"{s}\""),
        Value::Boolean(b) => b.to_string(),
        _ => type_name(v).to_owned(),
    }
}

/// The stages that may follow `$changeStream`.
const ALLOWED: [&str; 9] = [
    "$match",
    "$project",
    "$addFields",
    "$set",
    "$unset",
    "$replaceRoot",
    "$replaceWith",
    "$redact",
    SPLIT_LARGE_EVENT,
];

/// Checks the stages after `$changeStream` (`names`, in order).
pub fn check_rest(names: &[&str]) -> Result<(), StageError> {
    for (i, name) in names.iter().enumerate() {
        if *name == CHANGE_STREAM {
            let msg =
                "$_internalChangeStreamOplogMatch is only valid as the first stage in a pipeline";
            return Err(error(codes::NOT_FIRST, msg));
        }
        if !ALLOWED.contains(name) {
            let msg = format!("{name} is not permitted in a $changeStream pipeline");
            return Err(error(codes::ILLEGAL_OPERATION, msg));
        }
        if *name == SPLIT_LARGE_EVENT && i + 1 != names.len() {
            let msg = "$changeStreamSplitLargeEvent must be the last stage in the pipeline";
            return Err(error(codes::SPLIT_NOT_LAST, msg));
        }
    }
    Ok(())
}
