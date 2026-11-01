//! `readConcern` and `writeConcern` as a command receives them (FR-019): their fields and errors
//! as RefDb's IDL (`read_concern.idl`, `write_concern.idl`) reads them, the commands that take
//! each one, and the write concern as its errors describe it. Every rule here was measured in a
//! `refrouter` 9.0.4.

use teseladb_bson::{Document, Value};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::read::ReadConcern;

use crate::error::{
    BAD_VALUE, CommandError, FAILED_TO_PARSE, IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD,
    INVALID_OPTIONS, NO_SUCH_KEY, UNKNOWN_REPL_WRITE_CONCERN,
};
use crate::types::{number, type_name, wrong_type, wrong_types};

/// `readConcern`.
pub const READ_CONCERN: &str = "readConcern";
/// `writeConcern`.
pub const WRITE_CONCERN: &str = "writeConcern";
/// The provenance a client may give (`provenance`).
pub const CLIENT_SUPPLIED: &str = "clientSupplied";
/// `w: "majority"`.
pub const MAJORITY: &str = "majority";
/// The provenances RefDb knows; only [`CLIENT_SUPPLIED`] may come from a client.
const PROVENANCES: [&str; 5] = [
    CLIENT_SUPPLIED,
    "implicitDefault",
    "customDefault",
    "getLastErrorDefaults",
    "internalWriteDefault",
];
/// The provenance of a write concern without `w`.
const IMPLICIT_DEFAULT: &str = "implicitDefault";
/// The provenance of the cluster's default write concern.
const CUSTOM_DEFAULT: &str = "customDefault";
/// The types of the `safeBool` fields of a write concern (`j`, `fsync`), in a `refrouter` 9.0's
/// order (measured).
const WC_SAFE_BOOL_TYPES: [&str; 5] = ["decimal", "int", "bool", "long", "double"];
/// Largest `w` a write concern may name.
const MAX_W: i64 = 50;
/// The write concern fields RefDb reads and ignores.
const IGNORED_WC_FIELDS: [&str; 3] = ["wElectionId", "wOpTime", "getLastError"];
/// The internal read concern flags (booleans).
const INTERNAL_RC_FLAGS: [&str; 2] =
    ["$_allowTransactionTableSnapshot", "$_waitLastStableRecoveryTimestamp"];

/// A `readConcern.level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// `local`.
    Local,
    /// `available`.
    Available,
    /// `majority`.
    Majority,
    /// `linearizable`.
    Linearizable,
    /// `snapshot`.
    Snapshot,
}

impl Level {
    /// Every level with its name.
    pub const ALL: [(Level, &'static str); 5] = [
        (Level::Local, "local"),
        (Level::Available, "available"),
        (Level::Majority, "majority"),
        (Level::Linearizable, "linearizable"),
        (Level::Snapshot, "snapshot"),
    ];

    /// The level named `name`.
    pub fn from_name(name: &str) -> Option<Level> {
        Level::ALL.iter().find(|(_, n)| *n == name).map(|(l, _)| *l)
    }

    /// The level's name.
    pub fn name(self) -> &'static str {
        Level::ALL.iter().find(|(l, _)| *l == self).map_or("", |(_, n)| n)
    }

    /// The range's read for this level (DV-010: `local` and `available` read what the majority
    /// confirmed; a snapshot outside a transaction reads at a mark the range confirms).
    pub fn to_range(self) -> ReadConcern {
        match self {
            Level::Local => ReadConcern::Local,
            Level::Available => ReadConcern::Available,
            Level::Majority | Level::Snapshot => ReadConcern::Majority,
            Level::Linearizable => ReadConcern::Linearizable,
        }
    }
}

/// A BSON timestamp: seconds and increment.
pub type Ts = (u32, u32);

/// A command's `readConcern`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReadConcernArgs {
    /// `level` (`None` reads as `local`).
    pub level: Option<Level>,
    /// `afterOpTime`.
    pub after_op_time: Option<Document>,
    /// `afterClusterTime`.
    pub after_cluster_time: Option<Ts>,
    /// `atClusterTime`.
    pub at_cluster_time: Option<Ts>,
}

fn unknown(owner: &str, k: &str) -> CommandError {
    CommandError::new(IDL_UNKNOWN_FIELD, format!("BSON field '{owner}.{k}' is an unknown field."))
}

fn invalid(msg: impl Into<String>) -> CommandError {
    CommandError::new(INVALID_OPTIONS, msg)
}

/// A `provenance` field of `owner`; `Ok(true)` when it is not [`CLIENT_SUPPLIED`].
fn provenance(owner: &str, v: &Value) -> Result<bool, CommandError> {
    match v {
        Value::String(s) if PROVENANCES.contains(&s.as_str()) => Ok(s != CLIENT_SUPPLIED),
        Value::String(s) => Err(CommandError::new(
            BAD_VALUE,
            format!("Enumeration value '{s}' for field '{owner}.provenance' is not a valid value."),
        )),
        other => Err(wrong_type(&format!("{owner}.provenance"), other, "string")),
    }
}

fn logical_time(field: &str, v: &Value) -> Result<Ts, CommandError> {
    match v {
        Value::Timestamp { time, increment } => Ok((*time, *increment)),
        other => Err(CommandError::new(
            crate::error::TYPE_MISMATCH,
            format!("\"{field}\" had the wrong type. Expected 17, found {}", type_name(other)),
        )),
    }
}

impl ReadConcernArgs {
    /// Reads `readConcern` of command `name`; `None` without one (or `null`).
    pub fn parse(cmd: &Document, name: &str) -> Result<Option<ReadConcernArgs>, CommandError> {
        match cmd.get(READ_CONCERN) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Document(d)) => ReadConcernArgs::from_doc(d).map(Some),
            Some(v) => Err(wrong_type(&format!("{name}.{READ_CONCERN}"), v, "object")),
        }
    }

    fn from_doc(d: &Document) -> Result<ReadConcernArgs, CommandError> {
        let mut rc = ReadConcernArgs::default();
        let mut foreign_provenance = false;
        for (k, v) in d.iter() {
            match (k, v) {
                (_, Value::Null) if k != "afterClusterTime" && k != "atClusterTime" => {}
                ("level", Value::String(s)) => {
                    let level = Level::ALL.iter().find(|(_, n)| n == s).map(|(l, _)| *l);
                    rc.level = Some(level.ok_or_else(|| {
                        CommandError::new(
                            BAD_VALUE,
                            format!(
                                "Enumeration value '{s}' for field 'readConcern.level' is not a valid value."
                            ),
                        )
                    })?);
                }
                ("level", other) => return Err(wrong_type("readConcern.level", other, "string")),
                ("afterOpTime", Value::Document(op)) => {
                    if !matches!(op.get("ts"), Some(Value::Timestamp { .. })) {
                        return Err(CommandError::new(
                            IDL_FAILED_TO_PARSE,
                            "BSON field 'OpTimeBase.ts' is missing but a required field",
                        ));
                    }
                    rc.after_op_time = Some(op.clone());
                }
                ("afterOpTime", _) => {
                    return Err(CommandError::new(
                        crate::error::EXPECTED_OBJECT,
                        "invalid parameter: expected an object (afterOpTime)",
                    ));
                }
                ("afterClusterTime", v) => rc.after_cluster_time = Some(logical_time(k, v)?),
                ("atClusterTime", v) => rc.at_cluster_time = Some(logical_time(k, v)?),
                ("provenance", v) => foreign_provenance = provenance(READ_CONCERN, v)?,
                (k, Value::Boolean(_)) if INTERNAL_RC_FLAGS.contains(&k) => {}
                (k, v) if INTERNAL_RC_FLAGS.contains(&k) => {
                    return Err(wrong_type(&format!("readConcern.{k}"), v, "bool"));
                }
                (k, _) => return Err(unknown(READ_CONCERN, k)),
            }
        }
        rc.check(foreign_provenance)?;
        Ok(rc)
    }

    /// The rules between the fields.
    fn check(&self, foreign_provenance: bool) -> Result<(), CommandError> {
        if self.after_cluster_time.is_some() && self.after_op_time.is_some() {
            return Err(invalid("Can not specify both afterClusterTime and afterOpTime"));
        }
        if self.after_cluster_time.is_some() && self.at_cluster_time.is_some() {
            return Err(invalid(
                "Specifying a timestamp for readConcern snapshot in a causally consistent session is not allowed. See https://docs.refdb.com/manual/core/read-isolation-consistency-recency/#causal-consistency",
            ));
        }
        let level = self.level.unwrap_or(Level::Local);
        if let Some(t) = self.after_cluster_time {
            if !matches!(level, Level::Majority | Level::Local | Level::Snapshot) {
                return Err(invalid(
                    "afterClusterTime field can be set only if level is equal to majority, local, or snapshot",
                ));
            }
            if t == (0, 0) {
                return Err(invalid("afterClusterTime cannot be a null timestamp"));
            }
        }
        if let Some(t) = self.at_cluster_time {
            if level != Level::Snapshot {
                return Err(invalid(
                    "atClusterTime field can be set only if level is equal to snapshot",
                ));
            }
            if t == (0, 0) {
                return Err(invalid("atClusterTime cannot be a null timestamp"));
            }
        }
        if foreign_provenance {
            return Err(invalid("readConcern provenance must be unset or \"clientSupplied\""));
        }
        Ok(())
    }

    /// The range's read.
    pub fn to_range(&self) -> ReadConcern {
        self.level.unwrap_or(Level::Local).to_range()
    }

    /// The read concern as RefDb's messages print it.
    fn describe(&self) -> String {
        let mut fields = Vec::new();
        // Only a level other than `local` is refused, so there is always one to print.
        fields.push(format!("level: \"{}\"", self.level.unwrap_or(Level::Local).name()));
        if let Some(op) = &self.after_op_time {
            fields.push(format!(
                "afterOpTime: {}",
                crate::commands::display(&Value::Document(op.clone()))
            ));
        }
        for (name, t) in
            [("afterClusterTime", self.after_cluster_time), ("atClusterTime", self.at_cluster_time)]
        {
            if let Some((time, inc)) = t {
                fields.push(format!("{name}: Timestamp({time}, {inc})"));
            }
        }
        fields.push(format!("provenance: \"{CLIENT_SUPPLIED}\""));
        format!("{{ readConcern: {{ {} }} }}", fields.join(", "))
    }
}

/// The write concern's `w`.
#[derive(Clone, Debug, PartialEq)]
pub enum WArg {
    /// A number of replicas (0 to 50).
    Nodes(u32),
    /// `"majority"`.
    Majority,
    /// A named mode of the replica set configuration; TeselaDB has none.
    Mode(String),
    /// Replica set tags (`{dc: 1}`), with their counts; TeselaDB has none.
    Tags(Vec<(String, i64)>),
}

/// What a write concern asks of the journal (`j`, `fsync`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    /// Neither `j` nor `fsync: true`.
    Unset,
    /// `j: false`.
    None,
    /// `j: true`.
    Journal,
    /// `fsync: true`.
    Fsync,
}

/// A command's `writeConcern`.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteConcernArgs {
    /// `w` (`None` is the default, `"majority"`).
    pub w: Option<WArg>,
    /// `j` and `fsync`.
    pub sync: Sync,
    /// `wtimeout` in milliseconds (0 = no limit).
    pub wtimeout: i32,
    /// It is the cluster's default (`setDefaultRWConcern`, 007 FR-018), not the command's.
    pub custom: bool,
}

/// Truncation of a number to an integer, saturating, as RefDb reads `w` and tag counts.
fn truncated(x: f64) -> i64 {
    if x >= i64::MAX as f64 {
        i64::MAX
    } else if x <= i64::MIN as f64 {
        i64::MIN
    } else {
        x.trunc() as i64
    }
}

fn parse_failure(msg: impl Into<String>) -> CommandError {
    CommandError::new(FAILED_TO_PARSE, msg)
}

fn parse_w(v: &Value) -> Result<WArg, CommandError> {
    match v {
        Value::Null => Ok(WArg::Mode(String::new())),
        Value::String(s) if s == MAJORITY => Ok(WArg::Majority),
        Value::String(s) => Ok(WArg::Mode(s.clone())),
        Value::Document(tags) => {
            if tags.is_empty() {
                return Err(parse_failure("tagged write concern requires tags"));
            }
            let mut out = Vec::new();
            for (k, n) in tags.iter() {
                let Some(x) = number(n) else {
                    let found = crate::commands::display(n);
                    return Err(parse_failure(format!(
                        "tags must be a single level document with only number values; found: {k}: {found}"
                    )));
                };
                out.push((k.to_owned(), truncated(x)));
            }
            Ok(WArg::Tags(out))
        }
        other => {
            let Some(x) = number(other) else {
                return Err(parse_failure(format!(
                    "w has to be a number, string, or object; found: {}",
                    type_name(other)
                )));
            };
            if x.is_nan() {
                return Err(parse_failure("w cannot be NaN"));
            }
            let n = truncated(x);
            match u32::try_from(n) {
                Ok(n) if i64::from(n) <= MAX_W => Ok(WArg::Nodes(n)),
                _ => Err(parse_failure(format!(
                    "w has to be a non-negative number and not greater than 50; found: {n}"
                ))),
            }
        }
    }
}

fn safe_bool(field: &str, v: &Value) -> Result<bool, CommandError> {
    match v {
        Value::Boolean(b) => Ok(*b),
        other => number(other).map(|x| x != 0.0).ok_or_else(|| {
            wrong_types(&format!("WriteConcernOptions.{field}"), other, &WC_SAFE_BOOL_TYPES)
        }),
    }
}

/// `wtimeout`: a number that fits 32 bits; any other type reads as 0.
fn parse_wtimeout(v: &Value) -> Result<i32, CommandError> {
    let Some(x) = number(v) else { return Ok(0) };
    if x.is_nan() {
        return Ok(0);
    }
    i32::try_from(truncated(x)).map_err(|_| parse_failure("wtimeout must be a 32-bit integer"))
}

impl WriteConcernArgs {
    /// Reads `writeConcern` of command `name`; `None` without one (or `null`).
    pub fn parse(cmd: &Document, name: &str) -> Result<Option<WriteConcernArgs>, CommandError> {
        match cmd.get(WRITE_CONCERN) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Document(d)) => WriteConcernArgs::from_doc(d).map(Some),
            Some(v) => Err(wrong_type(&format!("{name}.{WRITE_CONCERN}"), v, "object")),
        }
    }

    fn from_doc(d: &Document) -> Result<WriteConcernArgs, CommandError> {
        const OWNER: &str = "WriteConcernOptions";
        let mut wc = WriteConcernArgs { w: None, sync: Sync::Unset, wtimeout: 0, custom: false };
        let (mut j, mut fsync, mut foreign_provenance) = (None, false, false);
        for (k, v) in d.iter() {
            match k {
                "w" => wc.w = Some(parse_w(v)?),
                _ if matches!(v, Value::Null) => {}
                "j" => j = Some(safe_bool(k, v)?),
                "fsync" => fsync = safe_bool(k, v)?,
                "wtimeout" => wc.wtimeout = parse_wtimeout(v)?,
                "provenance" => foreign_provenance = provenance(OWNER, v)?,
                _ if IGNORED_WC_FIELDS.contains(&k) => {}
                _ => return Err(unknown(OWNER, k)),
            }
        }
        if fsync && j == Some(true) {
            return Err(parse_failure("fsync and j options cannot be used together"));
        }
        if foreign_provenance {
            return Err(invalid("writeConcern provenance must be unset or \"clientSupplied\""));
        }
        wc.sync = match (fsync, j) {
            (true, _) => Sync::Fsync,
            (false, Some(true)) => Sync::Journal,
            (false, Some(false)) => Sync::None,
            (false, None) => Sync::Unset,
        };
        Ok(wc)
    }

    /// The range's write concern: an unknown mode or tag set writes as `"majority"`, and
    /// `w: 0` keeps [`W::Zero`] (D-034; `j: false` does not lower `"majority"` nor `w: <n>`).
    pub fn to_range(&self) -> WriteConcern {
        let w = match &self.w {
            Some(WArg::Nodes(0)) => W::Zero,
            Some(WArg::Nodes(n)) => W::Nodes(*n),
            None | Some(WArg::Majority | WArg::Mode(_) | WArg::Tags(_)) => W::Majority,
        };
        let j = match self.sync {
            Sync::Journal | Sync::Fsync => true,
            Sync::None => false,
            Sync::Unset => w != W::Zero,
        };
        WriteConcern { w, j }
    }

    /// The error the reply carries after the write: a mode or tag set the configuration does not
    /// have. A negative `wtimeout` gives none: the write answers once the replicas it asked for
    /// have it, so its concern is met when the check runs (a `refnode` checks before its
    /// majority commit point moves and times out on some runs, measured).
    pub fn error_after_write(&self) -> Option<CommandError> {
        match &self.w {
            Some(WArg::Mode(m)) => Some(CommandError::new(
                UNKNOWN_REPL_WRITE_CONCERN,
                format!("No write concern mode named '{m}' found in replica set configuration"),
            )),
            Some(WArg::Tags(tags)) => {
                let key = tags.first().map_or("", |(k, _)| k.as_str());
                Some(CommandError::new(
                    NO_SUCH_KEY,
                    format!("No replica set tag key {key} in config"),
                ))
            }
            None | Some(WArg::Majority | WArg::Nodes(_)) => None,
        }
    }

    /// The write concern as `writeConcernError` messages print it
    /// (`{ w: 5, j: true, wtimeout: 0, provenance: "clientSupplied" }`).
    pub fn describe(&self) -> String {
        let mut fields = Vec::new();
        let majority = matches!(self.w, None | Some(WArg::Majority));
        fields.push(match &self.w {
            None | Some(WArg::Majority) => format!("w: \"{MAJORITY}\""),
            Some(WArg::Nodes(n)) => format!("w: {n}"),
            Some(WArg::Mode(m)) => format!("w: \"{m}\""),
            Some(WArg::Tags(tags)) => {
                let t: Vec<String> = tags.iter().map(|(k, n)| format!("{k}: {n}")).collect();
                format!("w: {{ {} }}", t.join(", "))
            }
        });
        match self.sync {
            Sync::Journal => fields.push(String::from("j: true")),
            Sync::None if majority => fields.push(String::from("j: true")),
            Sync::None => fields.push(String::from("j: false")),
            Sync::Unset | Sync::Fsync => {}
        }
        fields.push(format!("wtimeout: {}", self.wtimeout));
        if self.sync == Sync::Fsync {
            fields.push(String::from("fsync: true"));
        }
        let source = match (self.custom, self.w.is_none()) {
            (true, _) => CUSTOM_DEFAULT,
            (false, true) => IMPLICIT_DEFAULT,
            (false, false) => CLIENT_SUPPLIED,
        };
        fields.push(format!("provenance: \"{source}\""));
        format!("{{ {} }}", fields.join(", "))
    }
}

/// The write concern of a command without `writeConcern`, as its errors print it: the cluster's
/// default while one applies to the running command ([`with_defaults`]), else the implicit one.
pub fn default_write_concern() -> WriteConcernArgs {
    let custom = DEFAULTS.with(|d| d.borrow().1.clone());
    custom
        .map_or(WriteConcernArgs { w: None, sync: Sync::Unset, wtimeout: 0, custom: false }, |wc| {
            WriteConcernArgs { custom: true, ..wc }
        })
}

/// The read concern level of a command without `readConcern`: the cluster's default while one
/// applies to the running command, else `local`.
pub fn default_read_level() -> Level {
    DEFAULTS.with(|d| d.borrow().0).unwrap_or(Level::Local)
}

std::thread_local! {
    /// The cluster's default concerns for the command running on this thread (007 FR-018).
    static DEFAULTS: std::cell::RefCell<(Option<Level>, Option<WriteConcernArgs>)> =
        const { std::cell::RefCell::new((None, None)) };
}

/// Runs `f`, a command, with the cluster's default read level and write concern; the previous
/// ones come back after it, also when it unwinds.
pub fn with_defaults<T>(
    read: Option<Level>,
    write: Option<WriteConcernArgs>,
    f: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<(Option<Level>, Option<WriteConcernArgs>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.0.take() {
                DEFAULTS.with(|d| *d.borrow_mut() = prev);
            }
        }
    }
    let prev = DEFAULTS.with(|d| std::mem::replace(&mut *d.borrow_mut(), (read, write)));
    let _restore = Restore(Some(prev));
    f()
}

/// The `writeConcernError` of `e` for write concern `wc`, as a `refrouter` relays its shard's: the
/// code's name first and `relayed` after (` at node<N>`, D-102; measured on 9.0.4).
pub fn write_concern_error(e: &CommandError, wc: &WriteConcernArgs, relayed: &str) -> Document {
    let details = format!("{{ writeConcern: {} }}", wc.describe());
    Document::new()
        .with("code", e.code.code)
        .with("codeName", e.code.name)
        .with("errmsg", format!("{}: {}; Error details: {details}{relayed}", e.code.name, e.errmsg))
        .with("errInfo", Document::new())
}

/// The commands that take `writeConcern` (measured).
const WRITE_CONCERN_COMMANDS: [&str; 31] = [
    "insert",
    "update",
    "delete",
    "findAndModify",
    "findandmodify",
    "aggregate",
    "create",
    "collMod",
    "drop",
    "createIndexes",
    "dropIndexes",
    "dropDatabase",
    "renameCollection",
    "setDefaultRWConcern",
    "bulkWrite",
    "commitTransaction",
    "createUser",
    "updateUser",
    "dropUser",
    "dropAllUsersFromDatabase",
    "grantRolesToUser",
    "revokeRolesFromUser",
    "createRole",
    "updateRole",
    "dropRole",
    "dropAllRolesFromDatabase",
    "grantPrivilegesToRole",
    "revokePrivilegesFromRole",
    "grantRolesToRole",
    "revokeRolesFromRole",
    "_mergeAuthzCollections",
];

/// The read levels a command takes besides `local`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadSupport {
    /// Every level.
    All,
    /// Every level but `snapshot`.
    NoSnapshot,
    /// Only `local`.
    LocalOnly,
}

fn read_support(name: &str) -> ReadSupport {
    match name {
        "find" | "aggregate" | "distinct" | "getMore" | "killCursors" => ReadSupport::All,
        "count" => ReadSupport::NoSnapshot,
        _ => ReadSupport::LocalOnly,
    }
}

/// The name of command `name` in RefDb's messages: the other names a `refrouter` 9.0.4 takes for
/// a command (measured: `buildinfo`, `collstats`, `datasize`, `dbstats`, `findandmodify`,
/// `ismaster`) give its own.
pub fn canonical(name: &str) -> &str {
    match name {
        "findandmodify" => "findAndModify",
        "ismaster" => "isMaster",
        "buildinfo" => "buildInfo",
        "collstats" => "collStats",
        "datasize" => "dataSize",
        "dbstats" => "dbStats",
        other => other,
    }
}

/// Checks that command `name` takes the concerns it was given: first `writeConcern`, then the
/// `readConcern` level.
pub fn check_support(
    name: &str,
    read: Option<&ReadConcernArgs>,
    write: Option<&WriteConcernArgs>,
) -> Result<(), CommandError> {
    if write.is_some() && !WRITE_CONCERN_COMMANDS.contains(&name) {
        return Err(invalid("Command does not support writeConcern"));
    }
    let Some(rc) = read else { return Ok(()) };
    let level = rc.level.unwrap_or(Level::Local);
    let why = match (read_support(name), level) {
        (_, Level::Local) | (ReadSupport::All, _) => return Ok(()),
        (ReadSupport::NoSnapshot, Level::Snapshot) => "read concern snapshot not supported",
        (ReadSupport::NoSnapshot, _) => return Ok(()),
        (ReadSupport::LocalOnly, _) => "read concern not supported",
    };
    Err(invalid(format!(
        "Command {} does not support {} :: caused by :: {why}",
        canonical(name),
        rc.describe()
    )))
}
