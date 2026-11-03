//! The arguments every command takes besides its own (RefDb 9.0.4's `generic_argument.idl`),
//! the unknown-field check of the commands whose IDL is strict, and the order a `refrouter` 9.0.4
//! checks them in (measured): `maxTimeMS`, `readConcern`, `writeConcern`, `$readPreference` and
//! the command's unknown fields in the order they come (a few commands, [`PARSED_LATE`], check
//! their unknown fields at the end), then whether the command takes the write concern and the
//! read level it was given.

use teseladb_bson::{Document, Value};
use teseladb_router::read_pref::{ReadPrefError, ReadPreference, TagSet};

use crate::concerns::{
    READ_CONCERN, ReadConcernArgs, WRITE_CONCERN, WriteConcernArgs, canonical, check_support,
};
use crate::error::{
    BAD_VALUE, CommandError, IDL_DUPLICATE_FIELD, IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD,
    INVALID_OPTIONS, MAX_STALENESS_OUT_OF_RANGE,
};
use crate::ops::{MAX_TIME_MS, max_time_ms};
use crate::types::{number, wrong_type, wrong_types};

/// The generic arguments of RefDb 9.0.4, which any command takes (`$db` included).
pub const GENERIC_ARGUMENTS: [&str; 47] = [
    "$db",
    "apiVersion",
    "apiStrict",
    "apiDeprecationErrors",
    "maxTimeMS",
    "readConcern",
    "writeConcern",
    "lsid",
    "clientOperationKey",
    "txnNumber",
    "autocommit",
    "startTransaction",
    "stmtId",
    "comment",
    "$readPreference",
    "$clusterTime",
    "$audit",
    "$client",
    "$configServerState",
    "$oplogQueryData",
    "$queryOptions",
    "$replData",
    "databaseVersion",
    "help",
    "shardVersion",
    "tracking_info",
    "coordinator",
    "maxTimeMSOpOnly",
    "usesDefaultMaxTimeMS",
    "$configTime",
    "$topologyTime",
    "txnRetryCounter",
    "versionContext",
    "mayBypassWriteBlocking",
    "mayBypassReplicaSetWritesBlocking",
    "expectPrefix",
    "requestGossipRoutingCache",
    "startOrContinueTransaction",
    "$traceCtx",
    "rawData",
    "isDirectSystemBucketsAccess",
    "isExternalClientOnRouter",
    "staleConfigRetryAttempt",
    "transactionRuntimeContext",
    "ifrFlags",
    "ifrSenderVersion",
    "executionAdmissionContextType",
];

/// Whether `field` is a generic argument.
pub fn is_generic(field: &str) -> bool {
    GENERIC_ARGUMENTS.contains(&field)
}

/// The fields `insert`, `update` and `delete` share (`WriteCommandRequestBase`).
const WRITE_BASE: [&str; 11] = [
    "bypassDocumentValidation",
    "ordered",
    "stmtId",
    "stmtIds",
    "isTimeseriesNamespace",
    "collectionUUID",
    "encryptionInformation",
    "bypassEmptyTsReplacement",
    "includeQueryStatsMetrics",
    "$_originalQuery",
    "$_originalCollation",
];

/// The fields of the strict commands besides the generic ones and the command itself (their
/// IDL in 9.0.4, and the diagnostics commands measured); `None` for the commands that take any
/// field.
fn own_fields(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "explain" => &["explain", "verbosity"],
        "getDefaultRWConcern" => &["getDefaultRWConcern", "inMemory"],
        "setQuerySettings" => &["setQuerySettings", "settings"],
        "removeQuerySettings" => &["removeQuerySettings"],
        "reshardCollection" => &[
            "reshardCollection",
            "key",
            "unique",
            "numInitialChunks",
            "collation",
            "zones",
            "shardDistribution",
            "forceRedistribution",
            "reshardingUUID",
            "demoMode",
            "performVerification",
        ],
        "abortReshardCollection" => &["abortReshardCollection"],
        "moveCollection" => {
            &["moveCollection", "toShard", "oplogBatchApplierTaskCount", "performVerification"]
        }
        "unshardCollection" => {
            &["unshardCollection", "toShard", "oplogBatchApplierTaskCount", "performVerification"]
        }
        "rewriteCollection" => {
            &["rewriteCollection", "numInitialChunks", "zones", "performVerification", "demoMode"]
        }
        "addShardToZone" => &["addShardToZone", "zone"],
        "updateZoneKeyRange" => &["updateZoneKeyRange", "min", "max", "zone"],
        "configureCollectionBalancing" => &[
            "configureCollectionBalancing",
            "chunkSize",
            "defragmentCollection",
            "enableAutoMerger",
            "balanceShardsByDataSize",
        ],
        "balancerCollectionStatus" => &["balancerCollectionStatus"],
        "transitionToDedicatedConfigServer" => &["transitionToDedicatedConfigServer"],
        "setDefaultRWConcern" => {
            &["setDefaultRWConcern", "defaultReadConcern", "defaultWriteConcern"]
        }
        "aggregate" => &[
            "pipeline",
            "explain",
            "allowDiskUse",
            "cursor",
            "maxTimeMS",
            "bypassDocumentValidation",
            "collation",
            "hint",
            "let",
            "exchange",
            "fromRefRouter",
            "fromRouter",
            "needsMerge",
            "needsSortedMerge",
            "isMapReduceCommand",
            "collectionUUID",
            "runtimeConstants",
            "encryptionInformation",
            "passthroughToShard",
            "querySettings",
            "includeQueryStatsMetrics",
            "sampleId",
            "isClusterQueryWithoutShardKeyCmd",
            "isHybridSearch",
            "$_requestResumeToken",
            "$_requestReshardingResumeToken",
            "$_isMapReduceCommand",
            "$_generateV2ResumeTokens",
            "forcedPlanSolutionHash",
        ],
        "find" => &[
            "filter",
            "projection",
            "sort",
            "hint",
            "querySettings",
            "collation",
            "skip",
            "limit",
            "batchSize",
            "singleBatch",
            "allowDiskUse",
            "min",
            "max",
            "returnKey",
            "showRecordId",
            "tailable",
            "oplogReplay",
            "noCursorTimeout",
            "awaitData",
            "allowPartialResults",
            "let",
            "term",
            "readOnce",
            "$_requestResumeToken",
            "$_resumeAfter",
            "$_startAt",
            "runtimeConstants",
            "collectionUUID",
            "encryptionInformation",
            "mirrored",
            "sampleId",
            "includeQueryStatsMetrics",
            "forcedPlanSolutionHash",
            "originalQueryShapeHash",
        ],
        "getMore" => &[
            "collection",
            "batchSize",
            "term",
            "lastKnownCommittedOpTime",
            "includeQueryStatsMetrics",
            "includeMetrics",
        ],
        "killCursors" => &["cursors"],
        "count" => &[
            "query",
            "limit",
            "skip",
            "hint",
            "collation",
            "fields",
            "encryptionInformation",
            "mirrored",
            "sampleId",
            "includeQueryStatsMetrics",
            "originalQueryShapeHash",
        ],
        "distinct" => &[
            "key",
            "query",
            "collation",
            "mirrored",
            "sampleId",
            "hint",
            "querySettings",
            "includeQueryStatsMetrics",
            "originalQueryShapeHash",
        ],
        "insert" => &["documents"],
        "update" => &["updates", "let", "runtimeConstants"],
        "delete" => &["deletes", "let", "runtimeConstants"],
        "findAndModify" | "findandmodify" => &[
            "query",
            "fields",
            "sort",
            "batchSize",
            "singleBatch",
            "hint",
            "collation",
            "arrayFilters",
            "remove",
            "update",
            "upsert",
            "new",
            "stmtId",
            "bypassDocumentValidation",
            "let",
            "runtimeConstants",
            "encryptionInformation",
            "bypassEmptyTsReplacement",
            "sampleId",
            "$_originalQuery",
            "$_originalCollation",
            "$_allowShardKeyUpdatesWithoutFullShardKeyInQuery",
            "isTimeseriesNamespace",
        ],
        "bulkWrite" => &[
            "ops",
            "nsInfo",
            "cursor",
            "bypassDocumentValidation",
            "ordered",
            "stmtId",
            "stmtIds",
            "let",
            "errorsOnly",
            "bypassEmptyTsReplacement",
            "$_originalQuery",
            "$_originalCollation",
        ],
        "create" => &[
            "capped",
            "idIndex",
            "size",
            "max",
            "storageEngine",
            "validator",
            "validationLevel",
            "validationAction",
            "indexOptionDefaults",
            "viewOn",
            "pipeline",
            "collation",
            "changeStreamPreAndPostImages",
            "timeseries",
            "clusteredIndex",
            "expireAfterSeconds",
            "encryptedFields",
            "storageTier",
            "temp",
            "flags",
        ],
        "drop" => &["collectionUUID"],
        "renameCollection" => &["to", "dropTarget", "stayTemp", "collectionUUID"],
        "collMod" => &[
            "index",
            "validator",
            "validationLevel",
            "validationAction",
            "viewOn",
            "pipeline",
            "changeStreamPreAndPostImages",
            "expireAfterSeconds",
            "timeseries",
            "timeseriesBucketsMayHaveMixedSchemaData",
            "_removeLegacyTimeseriesBucketingParametersHaveChanged",
            "prepareConstraintValidationLevel",
            "dryRun",
            "collectionUUID",
            "cappedSize",
            "cappedMax",
            "recordIdsReplicated",
        ],
        "listDatabases" => &["nameOnly", "authorizedDatabases", "filter"],
        "listCollections" => {
            &["cursor", "filter", "nameOnly", "authorizedCollections", "includePendingDrops"]
        }
        "listIndexes" => {
            &["cursor", "includeBuildUUIDs", "includeIndexBuildInfo", "isTimeseriesNamespace"]
        }
        "dataSize" => &["keyPattern", "min", "max", "estimate", "maxSize", "maxObjects"],
        "dropConnections" => &["hostAndPort"],
        "connectionStatus" => &["showPrivileges"],
        "saslStart" => &["mechanism", "payload", "autoAuthorize", "options"],
        "authenticate" => &["mechanism", "user"],
        "createUser" | "updateUser" => &[
            "pwd",
            "customData",
            "roles",
            "digestPassword",
            "authenticationRestrictions",
            "mechanisms",
        ],
        "dropUser" | "dropAllUsersFromDatabase" | "invalidateUserCache" => &[],
        "grantRolesToUser" | "revokeRolesFromUser" | "grantRolesToRole" | "revokeRolesFromRole" => {
            &["roles"]
        }
        "createRole" | "updateRole" => &["privileges", "roles", "authenticationRestrictions"],
        "dropRole" | "dropAllRolesFromDatabase" => &[],
        "grantPrivilegesToRole" | "revokePrivilegesFromRole" => &["privileges"],
        "rolesInfo" => &["showPrivileges", "showAuthenticationRestrictions", "showBuiltinRoles"],
        "usersInfo" => &[
            "showCredentials",
            "showCustomData",
            "showPrivileges",
            "showAuthenticationRestrictions",
            "authenticatedMechanism",
            "filter",
        ],
        "dbStats" => &["scale", "freeStorage"],
        "_mergeAuthzCollections" => &["tempUsersCollection", "tempRolesCollection", "drop", "db"],
        "collStats" => &["scale", "verbose", "waitForLock", "numericOnly"],
        "planCacheClear" => &["query", "sort", "projection", "collation"],
        "dropDatabase" | "endSessions" | "refreshSessions" | "ping" | "buildInfo" | "hostInfo"
        | "getCmdLineOpts" | "getLog" | "logRotate" | "lockInfo" => &[],
        _ => return None,
    })
}

/// Whether command `name`, whose IDL is strict, does not take field `k`.
fn is_unknown(name: &str, k: &str) -> bool {
    let Some(own) = own_fields(canonical(name)) else { return false };
    let write_base = matches!(name, "insert" | "update" | "delete");
    !is_generic(k) && !own.contains(&k) && !(write_base && WRITE_BASE.contains(&k))
}

/// `IDLUnknownField` for the first field of `cmd` that command `name` does not take.
pub fn check_unknown_fields(cmd: &Document, name: &str) -> Result<(), CommandError> {
    let unknown = cmd.iter().skip(1).map(|(k, _)| k).find(|k| is_unknown(name, k));
    match unknown {
        None => Ok(()),
        Some(k) => Err(unknown_field(name, k)),
    }
}

/// `IDLUnknownField` for field `k` of command `name`.
fn unknown_field(name: &str, k: &str) -> CommandError {
    let msg = format!("BSON field '{}.{k}' is an unknown field.", canonical(name));
    CommandError::new(IDL_UNKNOWN_FIELD, msg)
}

/// What the generic arguments of a command ask.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Generic {
    /// `maxTimeMS` (`Some(0)` = an explicit "no limit").
    pub max_time_ms: Option<u64>,
    /// `readConcern`.
    pub read: Option<ReadConcernArgs>,
    /// `writeConcern`.
    pub write: Option<WriteConcernArgs>,
    /// `$readPreference` (007 FR-013).
    pub read_pref: Option<ReadPreference>,
}

/// The read preference argument.
const READ_PREFERENCE: &str = "$readPreference";

/// The numeric types of `maxStalenessSeconds`, in the order a `refrouter` 9.0.4 names them.
const STALENESS_TYPES: [&str; 4] = ["int", "long", "double", "decimal"];

/// `$readPreference` of command `name`, with the codes and messages of a `refrouter` 9.0.4
/// (measured): its fields' types first, then the rules of [`ReadPreference::new`]. Other fields
/// are taken and ignored, as there.
pub fn read_preference(cmd: &Document, name: &str) -> Result<Option<ReadPreference>, CommandError> {
    let Some(v) = cmd.get(READ_PREFERENCE) else { return Ok(None) };
    let Value::Document(rp) = v else {
        return Err(wrong_type(&format!("{name}.{READ_PREFERENCE}"), v, "object"));
    };
    let mut mode = None;
    let mut staleness = None;
    let mut tag_sets = Vec::new();
    let mut hedge = false;
    for (k, v) in rp.iter() {
        match k {
            "mode" => match v {
                Value::String(m) => mode = Some(m.clone()),
                other => return Err(wrong_type("readPreference.mode", other, "string")),
            },
            "maxStalenessSeconds" => {
                staleness = Some(number(v).ok_or_else(|| {
                    wrong_types("readPreference.maxStalenessSeconds", v, &STALENESS_TYPES)
                })?);
            }
            "tags" => match v {
                Value::Array(sets) => tag_sets = sets.iter().map(tag_set).collect(),
                other => return Err(wrong_type("readPreference.tags", other, "array")),
            },
            "hedge" => match v {
                // Hedging is on unless `enabled` says otherwise.
                Value::Document(h) => {
                    hedge = !matches!(h.get("enabled"), Some(Value::Boolean(false)))
                }
                other => return Err(wrong_type("readPreference.hedge", other, "object")),
            },
            _ => {}
        }
    }
    let Some(mode) = mode else {
        let msg = "BSON field 'readPreference.mode' is missing but a required field";
        return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg));
    };
    ReadPreference::new(&mode, staleness, tag_sets, hedge).map(Some).map_err(|e| {
        let code = match e {
            ReadPrefError::StalenessTooSmall => MAX_STALENESS_OUT_OF_RANGE,
            ReadPrefError::PrimaryWithHedge => INVALID_OPTIONS,
            _ => BAD_VALUE,
        };
        CommandError::new(code, e.to_string())
    })
}

/// A tag set: a document of string values; anything else is a set no node's tags have.
fn tag_set(v: &Value) -> TagSet {
    let Value::Document(d) = v else { return None };
    d.iter()
        .map(|(k, v)| match v {
            Value::String(s) => Some((k.to_owned(), s.clone())),
            _ => None,
        })
        .collect()
}

/// The field of the first statement of a transaction.
const START_TRANSACTION: &str = "startTransaction";
/// The session of a command.
const LSID: &str = "lsid";

/// The commands a `refrouter` 9.0.4 parses after reading their generic arguments and checking that
/// they take the concerns they were given: an unknown field of theirs comes after those errors
/// (measured). The others refuse an unknown field first.
const PARSED_LATE: [&str; 6] =
    ["listCollections", "collMod", "dbStats", "collStats", "getLog", "lockInfo"];

/// Reads the generic arguments of command `name` in the order they come, checks its fields and
/// whether it takes the concerns it was given, in the order of a `refrouter` 9.0.4.
pub fn parse(cmd: &Document, name: &str) -> Result<Generic, CommandError> {
    let late = PARSED_LATE.contains(&canonical(name));
    let mut g = Generic::default();
    let mut lsid_seen = false;
    // One walk in the order of the fields: each argument, and each unknown field of a command
    // parsed early, is checked where it comes (measured with `find`).
    for (k, _) in cmd.iter().skip(1) {
        match k {
            // A second session (measured on a `refrouter` 9.0.4 on 03-11-2026 with `find`,
            // `commitTransaction` and `abortTransaction`).
            LSID if lsid_seen => {
                let msg = format!("BSON field '{name}.{LSID}' is a duplicate field");
                return Err(CommandError::new(IDL_DUPLICATE_FIELD, msg).parsed());
            }
            LSID => lsid_seen = true,
            MAX_TIME_MS => g.max_time_ms = max_time_ms(cmd, name)?,
            READ_CONCERN => g.read = ReadConcernArgs::parse(cmd, canonical(name))?,
            WRITE_CONCERN => g.write = WriteConcernArgs::parse(cmd, canonical(name))?,
            READ_PREFERENCE => g.read_pref = read_preference(cmd, canonical(name))?,
            k if !late && is_unknown(name, k) => return Err(unknown_field(name, k)),
            _ => {}
        }
    }
    // The first statement of a transaction carries the transaction's read concern, which
    // RefDb checks against the transaction and not against the command (008 FR-009).
    let read = if cmd.get(START_TRANSACTION).is_some() { None } else { g.read.as_ref() };
    check_support(name, read, g.write.as_ref())?;
    if late {
        check_unknown_fields(cmd, name)?;
    }
    Ok(g)
}
