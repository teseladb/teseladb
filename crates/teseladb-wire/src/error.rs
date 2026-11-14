//! The reference server's error codes, the error of a command and its reply document (contract
//! `error-codes.md`, FR-021), and the translation of the range's errors (005) into them.

use crate::commands::txn::{ABORT_TRANSACTION, COMMIT_TRANSACTION};
use teseladb_bson::{Document, Value};
use teseladb_range::host::{MutationError, RangeError};

/// `errmsg` of a write whose Raft entry would pass 64 MiB (005 FR-023): the node writes it again
/// as an internal transaction in parts (008 FR-006).
pub const ENTRY_TOO_LARGE: &str = "object to insert too large";

/// A reference server error code with its `codeName`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code {
    /// Number.
    pub code: i32,
    /// Name; codes without a name of their own are `Location<code>`.
    pub name: &'static str,
}

/// An error the reference router 9.0.4 answers with `errmsg` alone, without `code` nor `codeName`
/// (some of `split`'s, 018 FR-004).
pub const UNCODED: Code = Code { code: 0, name: "" };

macro_rules! codes {
    ($($(#[$doc:meta])* $id:ident = $n:literal, $name:literal;)*) => {
        $( $(#[$doc])* pub const $id: Code = Code { code: $n, name: $name }; )*
        /// Every code of this module, to find one by number.
        pub const ALL: &[Code] = &[$($id),*];
    };
}

codes! {
    /// Unexpected internal failure.
    INTERNAL_ERROR = 1, "InternalError";
    /// Invalid option or value.
    BAD_VALUE = 2, "BadValue";
    /// Malformed command.
    FAILED_TO_PARSE = 9, "FailedToParse";
    /// A write batch with no operations or too many.
    INVALID_LENGTH = 16, "InvalidLength";
    /// User not found (013).
    USER_NOT_FOUND = 11, "UserNotFound";
    /// Not authorized (013), and a security token section in `OP_MSG`.
    UNAUTHORIZED = 13, "Unauthorized";
    /// An operation the target does not allow (renaming a collection to itself).
    ILLEGAL_OPERATION = 20, "IllegalOperation";
    /// A field of the wrong BSON type.
    TYPE_MISMATCH = 14, "TypeMismatch";
    /// An error the reference search server gives without a code of its own (011: a search index
    /// definition).
    UNKNOWN_ERROR = 8, "UnknownError";
    /// A search index with the same name and another definition (011).
    INDEX_ALREADY_EXISTS = 68, "IndexAlreadyExists";
    /// A command that the reference search server refused (011: `updateSearchIndex` with a wrong
    /// definition).
    COMMAND_FAILED = 125, "CommandFailed";
    /// A document past the reference server's nesting limit (003 FR-003).
    OVERFLOW = 15, "Overflow";
    /// Authentication failed (013).
    AUTHENTICATION_FAILED = 18, "AuthenticationFailed";
    /// A tenant past its operations, operations at once or bytes written per second on a node
    /// (007 FR-032; labelled `SystemOverloadedError`, and retried as the codes of
    /// [`RETRYABLE_CODES`], D-148).
    INGRESS_REQUEST_RATE_LIMIT_EXCEEDED = 462, "IngressRequestRateLimitExceeded";
    /// A tenant past its storage, database or collection quota (007 FR-031).
    OUT_OF_DISK_SPACE = 14031, "OutOfDiskSpace";
    /// Invalid BSON.
    INVALID_BSON = 22, "InvalidBSON";
    /// Already set up, differently (`shardCollection` with another key, 018).
    ALREADY_INITIALIZED = 23, "AlreadyInitialized";
    /// Collection missing where one is required.
    NAMESPACE_NOT_FOUND = 26, "NamespaceNotFound";
    /// Cursor expired, killed or of another node.
    CURSOR_NOT_FOUND = 43, "CursorNotFound";
    /// A tailable cursor whose next document a capped collection evicted (015 FR-004).
    CAPPED_POSITION_LOST = 136, "CappedPositionLost";
    /// `create` of an existing collection.
    NAMESPACE_EXISTS = 48, "NamespaceExists";
    /// `maxTimeMS` expired.
    MAX_TIME_MS_EXPIRED = 50, "MaxTimeMSExpired";
    /// Command that does not exist.
    COMMAND_NOT_FOUND = 59, "CommandNotFound";
    /// Change of `_id`.
    IMMUTABLE_FIELD = 66, "ImmutableField";
    /// A write concern mode that does not exist.
    UNKNOWN_REPL_WRITE_CONCERN = 79, "UnknownReplWriteConcern";
    /// A field name starting with `$` where it is not allowed.
    DOLLAR_PREFIXED_FIELD_NAME = 52, "DollarPrefixedFieldName";
    /// An `_id` of a type a stored document cannot have.
    INVALID_ID_FIELD = 53, "InvalidIdField";
    /// An index the query needs does not exist (`$text` without a text index).
    INDEX_NOT_FOUND = 27, "IndexNotFound";
    /// A path an update cannot create in the document.
    PATH_NOT_VIABLE = 28, "PathNotViable";
    /// Two update operators on the same path, or on a path and its prefix.
    CONFLICTING_UPDATE_OPERATORS = 40, "ConflictingUpdateOperators";
    /// An upsert whose filter sets a path twice.
    NOT_SINGLE_VALUE_FIELD = 54, "NotSingleValueField";
    /// An empty field name in a path.
    EMPTY_FIELD_NAME = 56, "EmptyFieldName";
    /// `$near` or `$nearSphere` without a geospatial index.
    NO_QUERY_EXECUTION_PLANS = 291, "NoQueryExecutionPlans";
    /// A regular expression that does not compile.
    REGEX_INVALID = 51_091, "Location51091";
    /// An unknown regular expression option.
    REGEX_INVALID_OPTION = 51_108, "Location51108";
    /// A projection path that collides with another one.
    PROJECTION_PATH_COLLISION = 31_250, "Location31250";
    /// An inclusion in an exclusion projection.
    PROJECTION_INCLUSION_IN_EXCLUSION = 31_253, "Location31253";
    /// An exclusion in an inclusion projection.
    PROJECTION_EXCLUSION_IN_INCLUSION = 31_254, "Location31254";
    /// `$listClusterCatalog` on a collection.
    CLUSTER_CATALOG_ON_COLLECTION = 9_621_301, "Location9621301";
    /// `$planCacheStats` of a collection that does not exist.
    PLAN_CACHE_NO_COLLECTION = 50_933, "Location50933";
    /// Invalid combination of options.
    INVALID_OPTIONS = 72, "InvalidOptions";
    /// Invalid database or collection name.
    INVALID_NAMESPACE = 73, "InvalidNamespace";
    /// No replica satisfies a read preference (007 FR-013, D-093).
    FAILED_TO_SATISFY_READ_PREFERENCE = 133, "FailedToSatisfyReadPreference";
    /// A `maxStalenessSeconds` under 90 (007 FR-013).
    MAX_STALENESS_OUT_OF_RANGE = 160, "MaxStalenessOutOfRange";
    /// A sharding command on a collection without a shard key (018).
    NAMESPACE_NOT_SHARDED = 118, "NamespaceNotSharded";
    /// No resharding of that namespace is running (018 FR-002).
    NO_SUCH_RESHARD_COLLECTION = 339, "NoSuchReshardCollection";
    /// The resharding was aborted (018 FR-002).
    RESHARD_COLLECTION_ABORTED = 341, "ReshardCollectionAborted";
    /// A write that found the collection before a resharding's critical section and reached a
    /// range that the critical section fences (018 FR-002): what a shard answers the reference
    /// router, which refreshes its routing and sends the write again.
    STALE_CONFIG = 13_388, "StaleConfig";
    /// A new shard key without the cardinality for the chunks asked (018 FR-002).
    RESHARD_CARDINALITY = 4_952_606, "Location4952606";
    /// A sample size that is not a positive integer (`numInitialChunks` of a resharding).
    SAMPLE_SIZE = 28_747, "Location28747";
    /// A forwarded request timed out.
    NETWORK_TIMEOUT = 89, "NetworkTimeout";
    /// A request of the node that got no answer within its time limit: what the reference router
    /// answers when a shard does not answer in time.
    NETWORK_INTERFACE_EXCEEDED_TIME_LIMIT = 202, "NetworkInterfaceExceededTimeLimit";
    /// The codes `failGetMoreAfterCursorCheckout` may inject that the node has no other use for
    /// (006 FR-034).
    HOST_NOT_FOUND = 7, "HostNotFound";
    /// What the reference router 9.0.4 turns a shard's not-primary and shutdown errors of a
    /// `getMore` into (measured with `failGetMoreAfterCursorCheckout` on 05-11-2026).
    HOST_UNREACHABLE = 6, "HostUnreachable";
    /// See [`HOST_NOT_FOUND`].
    SOCKET_EXCEPTION = 9001, "SocketException";
    /// See [`HOST_NOT_FOUND`].
    NOT_PRIMARY_NO_SECONDARY_OK = 13_435, "NotPrimaryNoSecondaryOk";
    /// See [`HOST_NOT_FOUND`].
    STALE_EPOCH = 150, "StaleEpoch";
    /// See [`HOST_NOT_FOUND`].
    RETRY_CHANGE_STREAM = 234, "RetryChangeStream";
    /// See [`HOST_NOT_FOUND`].
    STALE_SHARD_VERSION = 63, "OBSOLETE_StaleShardVersion";
    /// The node is shutting down.
    SHUTDOWN_IN_PROGRESS = 91, "ShutdownInProgress";
    /// The node's TLS settings could not be loaded (`rotateCertificates`, 013 FR-006).
    INVALID_SSL_CONFIGURATION = 140, "InvalidSSLConfiguration";
    /// `w` larger than the range's replicas.
    UNSATISFIABLE_WRITE_CONCERN = 100, "UnsatisfiableWriteConcern";
    /// Transaction conflict (008).
    WRITE_CONFLICT = 112, "WriteConflict";
    /// Command that exists but is not taken this way.
    COMMAND_NOT_SUPPORTED = 115, "CommandNotSupported";
    /// Schema validation failed.
    DOCUMENT_VALIDATION_FAILURE = 121, "DocumentValidationFailure";
    /// Client metadata without `driver` or `os`.
    CLIENT_METADATA_MISSING_FIELD = 183, "ClientMetadataMissingField";
    /// `application.name` over 128 bytes.
    CLIENT_METADATA_APP_NAME_TOO_LARGE = 184, "ClientMetadataAppNameTooLarge";
    /// Client metadata over 512 bytes.
    CLIENT_METADATA_DOCUMENT_TOO_LARGE = 185, "ClientMetadataDocumentTooLarge";
    /// Client metadata after the first `hello`.
    CLIENT_METADATA_CANNOT_BE_MUTATED = 186, "ClientMetadataCannotBeMutated";
    /// Leader change during the operation.
    PRIMARY_STEPPED_DOWN = 189, "PrimarySteppedDown";
    /// Read below the range's GC threshold (005 FR-013).
    SNAPSHOT_TOO_OLD = 239, "SnapshotTooOld";
    /// A `snapshot` read before its database was added (008 FR-012).
    MIGRATION_CONFLICT = 272, "MigrationConflict";
    /// An index specification with an option the index cannot have.
    INVALID_INDEX_SPECIFICATION_OPTION = 197, "InvalidIndexSpecificationOption";
    /// A `txnNumber` below the session's highest.
    TRANSACTION_TOO_OLD = 225, "TransactionTooOld";
    /// Transaction not found (008).
    NO_SUCH_TRANSACTION = 251, "NoSuchTransaction";
    /// `startTransaction` with the running `txnNumber`, or a transaction another command has
    /// (008).
    CONFLICTING_OPERATION_IN_PROGRESS = 117, "ConflictingOperationInProgress";
    /// `abortTransaction` of a committed transaction (008).
    TRANSACTION_COMMITTED = 256, "TransactionCommitted";
    /// A command a transaction cannot run (008).
    OPERATION_NOT_SUPPORTED_IN_TRANSACTION = 263, "OperationNotSupportedInTransaction";
    /// A transaction's state passed the coordinator's memory budget (008 FR-010).
    TRANSACTION_TOO_LARGE = 257, "TransactionTooLarge";
    /// Overload.
    EXCEEDED_TIME_LIMIT = 262, "ExceededTimeLimit";
    /// A sort over 100 MiB without `allowDiskUse`.
    QUERY_EXCEEDED_MEMORY_LIMIT = 292, "QueryExceededMemoryLimitNoDiskUseAllowed";
    /// `OP_QUERY` command other than `hello` or `isMaster`.
    UNSUPPORTED_OP_QUERY_COMMAND = 352, "UnsupportedOpQueryCommand";
    /// A BSON column (binary subtype 7) that does not follow the column layout (003 T029).
    NON_CONFORMANT_BSON = 378, "NonConformantBSON";
    /// A write to a user database while `setUserWriteBlockMode` blocks them (007 FR-029).
    USER_WRITES_BLOCKED = 371, "UserWritesBlocked";
    /// A feature compatibility version that does not exist (007 FR-027).
    INVALID_FCV = 4_926_900, "Location4926900";
    /// A change of the feature compatibility version without `confirm: true`.
    FCV_NOT_CONFIRMED = 7_369_100, "Location7369100";
    /// A downgrade of the feature compatibility version that a collection's options prevent.
    CANNOT_DOWNGRADE = 332, "CannotDowngrade";
    /// A change of the feature compatibility version the reference server does not allow from the
    /// current one.
    FCV_BAD_TRANSITION = 5_147_403, "Location5147403";
    /// Two fields of a compound index go through different arrays (009).
    CANNOT_INDEX_PARALLEL_ARRAYS = 171, "CannotIndexParallelArrays";
    /// A transaction that would pass `maxConcurrentMultiDocumentTransactions` (008 FR-020).
    TOO_MANY_OPEN_TRANSACTIONS = 509, "TooManyOpenTransactions";
    /// A value of a `2dsphere` field that is not a valid geometry (009).
    GEO_KEY_EXTRACTION_FAILED = 510, "GeoKeyExtractionFailed";
    /// A geometry of a time series' measurement that its bucket's `2dsphere` index cannot take
    /// (015 FR-006).
    GEO_KEY_EXTRACTION_FAILED_TIMESERIES = 511, "GeoKeyExtractionFailedTimeseries";
    /// A `2d` value that is neither an array nor a document of two numbers (009).
    GEO_NOT_LEGACY_PAIR = 13_026, "Location13026";
    /// A `2d` point outside the index's grid (009).
    GEO_OUT_OF_GRID = 13_027, "Location13027";
    /// A `2d` value with one element (009).
    GEO_ONE_ELEMENT = 13_068, "Location13068";
    /// A hashed field with an array on its path (009).
    HASHED_ARRAY = 16_766, "Location16766";
    /// A unique hashed index (009).
    HASHED_UNIQUE = 16_764, "Location16764";
    /// More than one hashed field in an index key (009).
    HASHED_FIELDS = 31_303, "Location31303";
    /// A `2d` location in no known format (009).
    GEO_LOCATION_NOT_PAIR = 16_804, "Location16804";
    /// A regular field of a compound wildcard index holding an array (009).
    WILDCARD_COMPOUND_ARRAY = 7_246_301, "Location7246301";
    /// Range leader not reachable after internal retries.
    NOT_WRITABLE_PRIMARY = 10107, "NotWritablePrimary";
    /// Document over 16 MiB.
    BSON_OBJECT_TOO_LARGE = 10334, "BSONObjectTooLarge";
    /// Unique index violation.
    DUPLICATE_KEY = 11000, "DuplicateKey";
    /// The node is shutting down while the operation runs.
    INTERRUPTED_AT_SHUTDOWN = 11600, "InterruptedAtShutdown";
    /// `killOp` interrupted the operation.
    INTERRUPTED = 11601, "Interrupted";
    /// A node without an authorization lease (013 FR-012): its copy of the users and roles may be
    /// behind.
    NOT_PRIMARY_OR_SECONDARY = 13436, "NotPrimaryOrSecondary";
    /// A new database whose name differs only in case from an existing one (D-077).
    DATABASE_DIFFER_CASE = 13297, "DatabaseDifferCase";
    /// Primary change in replica set mode (018).
    INTERRUPTED_DUE_TO_REPL_STATE_CHANGE = 11602, "InterruptedDueToReplStateChange";
    /// `configureFailPoint` of a failpoint that does not exist.
    FAIL_POINT_SET_FAILED = 266, "FailPointSetFailed";
    /// A command about the audit log on a node without one (013 FR-031).
    AUDITING_NOT_ENABLED = 344, "AuditingNotEnabled";
    /// `auditConfig` changed on a node without `auditLog.runtimeConfiguration` (013 FR-031).
    RUNTIME_AUDIT_CONFIGURATION_NOT_ENABLED = 345, "RuntimeAuditConfigurationNotEnabled";
    /// A query its shape's settings reject (010 FR-023).
    QUERY_REJECTED_BY_SETTINGS = 411, "QueryRejectedBySettings";
    /// `setQuerySettings` with no setting it knows.
    QUERY_SETTINGS_EMPTY = 8_727_502, "Location8727502";
    /// `setQuerySettings` whose merged settings are only defaults.
    QUERY_SETTINGS_DEFAULTS = 7_746_604, "Location7746604";
    /// A query settings target that has no query shape.
    QUERY_SHAPE_UNSUPPORTED = 7_746_402, "Location7746402";
    /// A query knob that query settings cannot set.
    QUERY_KNOB_NOT_SETTABLE = 12_194_501, "Location12194501";
    /// An index hint of query settings without its namespace.
    INVALID_INDEX_HINT = 8_727_501, "Location8727501";
    /// An index hint of query settings without its database.
    INVALID_INDEX_HINT_DB = 8_727_500, "Location8727500";
    /// Required field missing in a command.
    IDL_FAILED_TO_PARSE = 40414, "IDLFailedToParse";
    /// A field given twice where it may appear once.
    IDL_DUPLICATE_FIELD = 40413, "IDLDuplicateField";
    /// Unknown field in a command.
    IDL_UNKNOWN_FIELD = 40415, "IDLUnknownField";
    /// A change stream lacks a pre- or post-image it requires (014 FR-008).
    NO_MATCHING_DOCUMENT = 47, "NoMatchingDocument";
    /// `resumeAfter` from an `invalidate` (014).
    INVALID_RESUME_TOKEN = 260, "InvalidResumeToken";
    /// A change stream event lost its `_id` in the pipeline, or the stream cannot go on (014).
    CHANGE_STREAM_FATAL = 280, "ChangeStreamFatalError";
    /// A change stream resumes from a point older than what the cluster keeps (014 FR-006).
    CHANGE_STREAM_HISTORY_LOST = 286, "ChangeStreamHistoryLost";
    /// A resume token without a string `_data` (014).
    CS_BAD_TOKEN = 40_647, "Location40647";
    /// A resume token and `startAtOperationTime` together (014).
    CS_SEVERAL_STARTS = 40_674, "Location40674";
    /// `$changeStream` not first in the pipeline (014).
    CS_NOT_FIRST = 40_602, "Location40602";
    /// Both `resumeAfter` and `startAfter` (014).
    CS_BOTH_STARTS = 50_865, "Location50865";
    /// `$changeStream` with an argument that is no document (014).
    CS_NOT_AN_OBJECT = 6_188_500, "Location6188500";
    /// `$changeStreamSplitLargeEvent` with an argument that is not an empty document (014).
    CS_SPLIT_SPEC = 7_182_800, "Location7182800";
    /// `$changeStreamSplitLargeEvent` not last (014).
    CS_SPLIT_NOT_LAST = 7_182_802, "Location7182802";
    /// `getMore` in a session on a cursor opened outside any, on a member of a replica set.
    CURSOR_NOT_IN_SESSION = 50736, "Location50736";
    /// `getMore` in a session on a cursor opened outside any, on the reference router
    /// (`validateLSID`).
    ROUTER_CURSOR_NOT_IN_SESSION = 50799, "Location50799";
    /// `getMore` without a session on a session's cursor, on the reference router.
    ROUTER_CURSOR_SESSION_REQUIRED = 50800, "Location50800";
    /// `getMore` in a session other than its cursor's, on the reference router.
    ROUTER_CURSOR_OTHER_SESSION = 50801, "Location50801";
    /// `killOp` without `op`.
    KILL_OP_MISSING_OP = 50759, "Location50759";
    /// `killOp` with an `op` of a type it does not take.
    KILL_OP_OP_TYPE = 50760, "Location50760";
    /// `killOp` with a text `op` that is not `shardid:opid`.
    KILL_OP_FORMAT = 28625, "Location28625";
    /// `killOp` with a number `op` that does not fit 32 bits.
    KILL_OP_RANGE = 26823, "Location26823";
    /// A cluster parameter or option that does not exist.
    NO_SUCH_KEY = 4, "NoSuchKey";
    /// A `clusterAuthMode` that is not the next step of the node's (013 FR-034).
    ILLEGAL_CLUSTER_AUTH_MODE_TRANSITION = 5_579_202, "Location5579202";
    /// `killOp` naming a shard; TeselaDB has none (DV-031).
    SHARD_NOT_FOUND = 70, "ShardNotFound";
    /// `getMore` without a session on a session's cursor, on a member of a replica set.
    CURSOR_SESSION_REQUIRED = 50737, "Location50737";
    /// `getMore` in a session other than its cursor's, on a member of a replica set.
    CURSOR_OTHER_SESSION = 50738, "Location50738";
    /// A command that could not do what it was asked (`getLog` of an unknown log).
    OPERATION_FAILED = 96, "OperationFailed";
    /// A UUID that is not 16 bytes of subtype 4.
    INVALID_UUID = 207, "InvalidUUID";
    /// A `$clusterTime` signature whose hash is not 20 bytes (013 FR-014).
    UNSUPPORTED_FORMAT = 12, "UnsupportedFormat";
    /// A `$clusterTime` whose signature does not match it.
    TIME_PROOF_MISMATCH = 204, "TimeProofMismatch";
    /// A `$clusterTime` signed with a key the node does not have for that time.
    KEY_NOT_FOUND = 211, "KeyNotFound";
    /// A `$clusterTime` too far ahead of the node's wall clock (008 FR-013, DV-101).
    CLUSTER_TIME_FAILS_RATE_LIMITER = 205, "ClusterTimeFailsRateLimiter";
    /// A document whose shard key holds an array (018 FR-001).
    SHARD_KEY_NOT_FOUND = 61, "ShardKeyNotFound";
    /// A zone no shard is in (018 FR-003).
    ZONE_NOT_FOUND = 177, "ZoneNotFound";
    /// A zone's interval that overlaps another one of the collection (018 FR-003).
    RANGE_OVERLAP_CONFLICT = 178, "RangeOverlapConflict";
    /// The last shard of a zone that still has intervals (018 FR-003).
    ZONE_STILL_IN_USE = 163, "ZoneStillInUse";
    /// Intervals of one namespace with other fields (018 FR-003).
    BOUND_FIELDS_MISMATCH = 16_634, "Location16634";
    /// A collection whose UUID is not the one the command names.
    COLLECTION_UUID_MISMATCH = 361, "CollectionUUIDMismatch";
    /// An update statement with constants (`c`) that is not a pipeline.
    CONSTANTS_ONLY_FOR_PIPELINE = 51198, "Location51198";
    /// `OP_MSG` with two body sections.
    MULTIPLE_BODIES = 40430, "Location40430";
    /// `OP_MSG` with two document sequences of the same name.
    DUPLICATE_SEQUENCE = 40431, "Location40431";
    /// `OP_MSG` with an unknown section kind.
    UNKNOWN_SECTION = 40432, "Location40432";
    /// A field both in the body and as a document sequence.
    DUPLICATE_FIELD_IN_SEQUENCE = 40433, "Location40433";
    /// `OP_MSG` without body.
    MISSING_BODY = 40587, "Location40587";
    /// Negative `topologyVersion.counter`.
    NEGATIVE_TOPOLOGY_COUNTER = 51758, "Location51758";
    /// `topologyVersion` without `maxAwaitTimeMS`, or the reverse.
    AWAIT_WITHOUT_TOPOLOGY = 51760, "Location51760";
    /// `topologyVersion.counter` greater than the node's.
    TOPOLOGY_COUNTER_AHEAD = 51761, "Location51761";
    /// `collMod` to `validationLevel: "constraint"` on a collection with invalid documents.
    CONSTRAINT_UPGRADE = 12_370_902, "Location12370902";
    /// An index specification with a version or an option that cannot be created.
    CANNOT_CREATE_INDEX = 67, "CannotCreateIndex";
    /// `collMod` to `unique: true` of an index whose entries have duplicates (009).
    CANNOT_CONVERT_INDEX_TO_UNIQUE = 359, "CannotConvertIndexToUnique";
    /// An operator a context does not take, such as `$expr` in a partial index's filter (009).
    QUERY_FEATURE_NOT_ALLOWED = 224, "QueryFeatureNotAllowed";
    /// A `clustered` index specification on a collection that is not clustered.
    CLUSTERED_INDEX_MISMATCH = 6_243_700, "Location6243700";
    /// `needsSortedMerge` without `needsMerge` in an `aggregate` (006 FR-021).
    CONTRADICTORY_MERGE = 10_372_401, "Location10372401";
    /// `isTimeseriesNamespace` sent to a router (006 FR-021).
    TIMESERIES_NS_ON_ROUTER = 5_916_401, "Location5916401";
    /// A `min` or `max` of `find` that is not the hinted index's key (006 FR-008).
    MIN_MAX_INDEX_MISMATCH = 51_174, "Location51174";
    /// A `min` of `find` that does not come before its `max` in the hinted index (006 FR-008).
    MIN_NOT_BEFORE_MAX = 51_175, "Location51175";
    /// An `apiVersion` other than `"1"` (006 FR-035).
    API_VERSION_ERROR = 322, "APIVersionError";
    /// What `apiStrict: true` refuses (006 FR-035).
    API_STRICT_ERROR = 323, "APIStrictError";
    /// What `apiDeprecationErrors: true` refuses (006 FR-035).
    API_DEPRECATION_ERROR = 324, "APIDeprecationError";
    /// `apiStrict` or `apiDeprecationErrors` without `apiVersion` (006 FR-035).
    API_WITHOUT_VERSION = 4_886_600, "Location4886600";
    /// `dropIndexes` of a clustered collection's index (015 FR-009).
    CLUSTERED_INDEX_DROP = 5_979_800, "Location5979800";
    /// A value that is not the object the reference server reads there: a
    /// `readConcern.afterOpTime`, a field of a stored user or role (013 FR-030).
    EXPECTED_OBJECT = 10_065, "Location10065";
    /// A stored SCRAM credential without a valid iteration count (measured, 013 FR-030).
    SCRAM_ITERATION_COUNT = 17_501, "Location17501";
    /// A stored SCRAM credential without its salt.
    SCRAM_MISSING_SALT = 17_502, "Location17502";
    /// A stored SCRAM credential without its server key.
    SCRAM_MISSING_SERVER_KEY = 17_503, "Location17503";
    /// A stored SCRAM credential without its stored key.
    SCRAM_MISSING_STORED_KEY = 17_504, "Location17504";
    /// A stored SCRAM credential that is not Base64 of the right length.
    SCRAM_CREDENTIALS_INVALID = 50_684, "Location50684";
    /// `OP_QUERY` on something other than a command namespace.
    OP_QUERY_REMOVED = 5_739_101, "Location5739101";
    /// A SASL conversation that does not exist (013).
    PROTOCOL_ERROR = 17, "ProtocolError";
    /// A SASL mechanism unknown, not enabled or without credentials (013).
    MECHANISM_UNAVAILABLE = 334, "MechanismUnavailable";
    /// A SASL payload string that is not Base64 (013).
    SASL_PAYLOAD_NOT_BASE64 = 10_270, "Location10270";
    /// `speculativeAuthenticate` with a command other than `saslStart` or `authenticate` (013).
    SPECULATIVE_UNKNOWN_COMMAND = 51_769, "Location51769";
    /// A role that does not exist (013).
    ROLE_NOT_FOUND = 31, "RoleNotFound";
    /// `createRole` of a role that exists (013).
    ROLE_ALREADY_EXISTS = 51_002, "Location51002";
    /// A role change the reference server refuses: a cycle, a privilege outside the role's database
    /// (013).
    INVALID_ROLE_MODIFICATION = 49, "InvalidRoleModification";
    /// `createUser` of a user that exists (013).
    USER_ALREADY_EXISTS = 51_003, "Location51003";
    /// An empty password, refused by the reference server's SASLprep (013).
    PASSWORD_NOT_TERMINATED = 50_687, "Location50687";
    /// A command that needs a collection, run on a view (015 FR-002).
    COMMAND_NOT_SUPPORTED_ON_VIEW = 166, "CommandNotSupportedOnView";
    /// An option or stage a view does not take (015 FR-001).
    OPTION_NOT_SUPPORTED_ON_VIEW = 167, "OptionNotSupportedOnView";
    /// A pipeline operator or cursor option a pipeline does not take (a tailable cursor on a view
    /// or a time series, 015 FR-004).
    INVALID_PIPELINE_OPERATOR = 168, "InvalidPipelineOperator";
    /// A command on a `system.buckets.*` namespace, which the reference server 9.0 no longer serves
    /// (015 FR-006).
    LEGACY_TIMESERIES_BUCKETS = 491, "CommandNotSupportedOnLegacyTimeseriesBucketsNamespace";
    /// A partial filter of a time series index that has no form over the buckets (015 FR-008).
    TIMESERIES_PREDICATE = 5_916_301, "Location5916301";
    /// A view on too many views (015 FR-001).
    VIEW_DEPTH_LIMIT_EXCEEDED = 165, "ViewDepthLimitExceeded";
    /// A view that reaches itself (015 FR-001).
    GRAPH_CONTAINS_CYCLE = 5, "GraphContainsCycle";
    /// A pipeline stage with more or fewer than one field (010).
    STAGE_ONE_FIELD = 40_323, "Location40323";
    /// A pipeline stage the reference server does not know (010).
    UNRECOGNIZED_STAGE = 40_324, "Location40324";
    /// A stage the reference server keeps for its own pipelines (010).
    INTERNAL_STAGE = 5_491_300, "Location5491300";
    /// A writing stage inside `$facet` (010).
    FACET_WRITES = 40_600, "Location40600";
    /// A writing stage inside the sub-pipeline of `$lookup` (010).
    LOOKUP_WRITES = 51_047, "Location51047";
    /// A writing stage inside the sub-pipeline of `$unionWith` (010).
    UNION_WITH_WRITES = 31_441, "Location31441";
    /// `clusteredIndex: <bool>` (015 FR-009).
    CLUSTERED_LEGACY = 5_979_703, "Location5979703";
    /// A clustered index without `unique: true` (015 FR-009).
    CLUSTERED_UNIQUE = 5_979_700, "Location5979700";
    /// A clustered index of a version other than 2 (015 FR-009).
    CLUSTERED_VERSION = 5_979_704, "Location5979704";
    /// `clusteredIndex` on a view (015 FR-009).
    CLUSTERED_ON_VIEW = 6_026_500, "Location6026500";
    /// A clustered collection with `capped` and `size` (015 FR-009).
    CLUSTERED_CAPPED_SIZE = 6_049_200, "Location6049200";
    /// A clustered capped collection, a test feature of the reference server (015 FR-009).
    CLUSTERED_CAPPED_TEST = 6_127_800, "Location6127800";
    /// A `txnNumber` without `autocommit: false` on a command that is no retryable write (008).
    NOT_A_RETRYABLE_WRITE_COMMAND = 50768, "NotARetryableWriteCommand";
    /// A `txnNumber` on a command that takes none, such as `hello` (008).
    TXN_NUMBER_NOT_ALLOWED = 50889, "Location50889";
    /// A commit of a transaction the node has no record of, without its `recoveryToken` (008).
    RECOVERY_TOKEN_REQUIRED = 50940, "Location50940";
    /// A `count` on a view whose pipeline fails on the shard, as the reference router relays it.
    COUNT_ON_VIEW_FAILED = 4_848_401, "Location4848401";
}

/// Label of a write the driver may retry.
pub const RETRYABLE_WRITE_ERROR: &str = "RetryableWriteError";
/// Label of a transaction the driver may retry whole.
pub const TRANSIENT_TRANSACTION_ERROR: &str = "TransientTransactionError";
/// Label of a commit whose result is unknown: the driver asks again (008).
pub const UNKNOWN_TRANSACTION_COMMIT_RESULT: &str = "UnknownTransactionCommitResult";
/// The `errmsg` of a write conflict in a transaction, as the reference server 9.0 writes it.
pub const WRITE_CONFLICT_MESSAGE: &str = "Caused by :: Write conflict during plan execution and \
     yielding is disabled. :: Please retry your operation or multi-document transaction.";
/// The `errmsg` of a write a resharding's critical section refused (018 FR-002).
pub const CRITICAL_SECTION_MESSAGE: &str =
    "the collection is in the critical section of a resharding: the write goes again once it ends";

/// The error of a command: what goes back in `ok: 0`.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandError {
    /// Code and name.
    pub code: Code,
    /// Message.
    pub errmsg: String,
    /// `errorLabels`.
    pub labels: Vec<&'static str>,
    /// Fields after `codeName` (such as `keyPattern` and `keyValue`, or `errInfo`).
    pub extra: Document,
    /// Who raises it, as the reference router would: what decides whether it ends a transaction
    /// (008).
    pub origin: ErrorOrigin,
    /// `errorLabels` go first, before `ok`: the order of the reference router 9.0.4's answer to the
    /// `commitTransaction` or `abortTransaction` it ends with `recoveryToken` (measured on
    /// 03-11-2026).
    pub labels_first: bool,
}

/// Who raises an error of a command, as the reference router 9.0.4 and its shard share the work
/// (measured on 01-11-2026): an error of a statement of a transaction ends the transaction only if
/// the shard raised it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorOrigin {
    /// The router, while it parses the command: the command's fields and their types, and the
    /// names of the pipeline's stages.
    Router,
    /// The shard, while it parses a query before the statement joins the transaction: a first
    /// statement's error leaves the transaction without participants, where the router commits
    /// it and answers its abort itself; a later statement's error ends it as any of the shard's
    /// (measured on 01-11-2026 and 03-11-2026 with an unknown operator in `find`).
    QueryParse,
    /// The shard: everything else.
    Shard,
}

/// The codes only the parse of a command gives: IDL's and an unknown pipeline stage.
const ROUTER_CODES: [Code; 4] =
    [IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, IDL_DUPLICATE_FIELD, UNRECOGNIZED_STAGE];

impl ErrorOrigin {
    /// The origin of an error of `code`, unless its site says otherwise
    /// ([`CommandError::parsed`]).
    pub fn of(code: Code) -> ErrorOrigin {
        match ROUTER_CODES.contains(&code) {
            true => ErrorOrigin::Router,
            false => ErrorOrigin::Shard,
        }
    }
}

impl CommandError {
    /// An error with `code` and `errmsg`, without labels.
    pub fn new(code: Code, errmsg: impl Into<String>) -> CommandError {
        CommandError {
            code,
            errmsg: errmsg.into(),
            labels: Vec::new(),
            extra: Document::new(),
            origin: ErrorOrigin::of(code),
            labels_first: false,
        }
    }

    /// The error, raised while the router parses the command ([`ErrorOrigin::Router`]).
    pub fn parsed(self) -> CommandError {
        CommandError { origin: ErrorOrigin::Router, ..self }
    }

    /// The error, raised while the shard parses the statement's query
    /// ([`ErrorOrigin::QueryParse`]).
    pub fn query_parse(self) -> CommandError {
        CommandError { origin: ErrorOrigin::QueryParse, ..self }
    }

    /// The error with the mark of a version in its read's uncertainty window (008 FR-008), which
    /// the layer that restarts the read takes back with [`CommandError::take_uncertain`].
    pub fn with_uncertain(mut self, ts: teseladb_hlc::Hlc) -> CommandError {
        let (time, increment) = teseladb_hlc::to_timestamp(ts);
        self.extra.insert(UNCERTAIN_AT, Value::Timestamp { time, increment });
        self
    }

    /// The mark [`CommandError::with_uncertain`] put, taken out of the error.
    pub fn take_uncertain(&mut self) -> Option<teseladb_hlc::Hlc> {
        match self.extra.remove(UNCERTAIN_AT) {
            Some(Value::Timestamp { time, increment }) => {
                Some(teseladb_hlc::from_timestamp((time, increment)))
            }
            _ => None,
        }
    }

    /// The error with `label` added.
    pub fn with_label(mut self, label: &'static str) -> CommandError {
        if !self.labels.contains(&label) {
            self.labels.push(label);
        }
        self
    }

    /// The error with its `errorLabels` before `ok` ([`CommandError::labels_first`]).
    pub fn with_labels_first(self) -> CommandError {
        CommandError { labels_first: true, ..self }
    }

    /// The reply document: `ok: 0`, `errmsg`, `code`, `codeName`, the extra fields and
    /// `errorLabels`, in the reference server's order (the labels first when
    /// [`CommandError::labels_first`]).
    pub fn to_document(&self) -> Document {
        let labels = self.labels.iter().map(|&l| Value::from(l)).collect::<Vec<_>>();
        let mut d = Document::new();
        if self.labels_first && !labels.is_empty() {
            d.push("errorLabels", labels.clone());
        }
        d.push("ok", 0.0);
        d.push("errmsg", self.errmsg.as_str());
        if self.code != UNCODED {
            d.push("code", self.code.code);
            d.push("codeName", self.code.name);
        }
        for (k, v) in self.extra.iter() {
            d.push(k, v.clone());
        }
        if !self.labels_first && !labels.is_empty() {
            d.push("errorLabels", labels);
        }
        d
    }

    /// The error carried as a range mutation's error, so that a mutation evaluated on the
    /// range's leader brings back its reference server code (see [`CommandError::from_mutation`]).
    pub fn to_mutation(&self) -> MutationError {
        MutationError(format!("{}\u{0}{}", self.code.code, self.errmsg))
    }

    /// The error a mutation failed with: the code and message of
    /// [`CommandError::to_mutation`], or `InternalError` for any other text.
    pub fn from_mutation(e: &MutationError) -> CommandError {
        let parsed = e.0.split_once('\u{0}').and_then(|(n, msg)| {
            let n: i32 = n.parse().ok()?;
            Some(CommandError::new(code(n)?, msg))
        });
        parsed.unwrap_or_else(|| CommandError::new(INTERNAL_ERROR, e.0.as_str()))
    }
}

impl From<teseladb_bson::BsonError> for CommandError {
    /// A BSON fault with the reference server's code for it (`BSONObjectTooLarge`, `Overflow`, …).
    fn from(e: teseladb_bson::BsonError) -> CommandError {
        let (n, name) = e.code();
        CommandError::new(Code { code: n, name }, e.to_string())
    }
}

impl From<teseladb_collation::SpecError> for CommandError {
    /// A collation document the reference server refuses, with its code.
    fn from(e: teseladb_collation::SpecError) -> CommandError {
        CommandError::new(Code { code: e.code, name: e.code_name }, e.message)
    }
}

/// The code numbered `n`, if this module knows it.
pub fn code(n: i32) -> Option<Code> {
    ALL.iter().copied().find(|c| c.code == n)
}

/// The field of an error that carries the mark of a version in its read's uncertainty window;
/// it never reaches a client (008 FR-008).
const UNCERTAIN_AT: &str = "$uncertainAt";
/// `errmsg` of a read that met a version in its uncertainty window in a later statement of a
/// transaction.
pub const UNCERTAIN_MESSAGE: &str =
    "the read met a version written in its uncertainty window and the transaction cannot restart";

/// Context of a range error: whether the command is a retryable write (it gets
/// `RetryableWriteError` on retryable codes) and whether a `maxTimeMS` of the command expired.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RangeContext {
    /// The command is a retryable write (`txnNumber` outside a transaction).
    pub retryable_write: bool,
    /// The deadline that passed was the command's `maxTimeMS`.
    pub max_time_expired: bool,
}

/// The reference server error for a range error (005), as the reference router reports a shard's.
pub fn from_range(e: &RangeError, ctx: RangeContext) -> CommandError {
    let (err, retryable) = match e {
        RangeError::NotLeader { .. } => {
            (CommandError::new(NOT_WRITABLE_PRIMARY, "the range's leader is not reachable"), true)
        }
        // A read still pushing the owner of an intent at its deadline timed out.
        RangeError::Timeout | RangeError::ReadIntent { .. } if ctx.max_time_expired => {
            (CommandError::new(MAX_TIME_MS_EXPIRED, "operation exceeded time limit"), false)
        }
        RangeError::Timeout | RangeError::ReadIntent { .. } => {
            (CommandError::new(NETWORK_TIMEOUT, "the range did not answer in time"), true)
        }
        RangeError::Overloaded => {
            (CommandError::new(EXCEEDED_TIME_LIMIT, "the range is overloaded"), true)
        }
        RangeError::OutOfRange => {
            (CommandError::new(INTERNAL_ERROR, "the key is outside the range"), false)
        }
        RangeError::Unsatisfiable => (
            CommandError::new(
                UNSATISFIABLE_WRITE_CONCERN,
                "Not enough data-bearing nodes to satisfy the write concern",
            ),
            false,
        ),
        RangeError::TooLarge => (CommandError::new(BSON_OBJECT_TOO_LARGE, ENTRY_TOO_LARGE), false),
        RangeError::Unavailable => {
            (CommandError::new(INTERRUPTED_AT_SHUTDOWN, "interrupted at shutdown"), true)
        }
        // A statement of a command of several is in doubt after a change of leader (005
        // FR-036): the driver repeats the command and the statements already written answer
        // from their records.
        RangeError::EntryLost => (
            CommandError::new(
                INTERRUPTED_DUE_TO_REPL_STATE_CHANGE,
                "operation was interrupted because the range's leader changed",
            ),
            true,
        ),
        // A later statement of a transaction answers this; the first one, and a read outside a
        // transaction, go again above the mark it carries (008 FR-008).
        RangeError::ReadUncertain { ts } => (
            CommandError::new(WRITE_CONFLICT, UNCERTAIN_MESSAGE)
                .with_label(TRANSIENT_TRANSACTION_ERROR)
                .with_uncertain(*ts),
            false,
        ),
        RangeError::SnapshotTooOld => (
            CommandError::new(SNAPSHOT_TOO_OLD, "the read timestamp is older than the history"),
            false,
        ),
        RangeError::Mutation(m) => (CommandError::from_mutation(m), false),
        // Another transaction's intent, or a newer version: the driver repeats the transaction.
        RangeError::WriteConflict | RangeError::IntentConflict(_) | RangeError::KeyLocked => (
            CommandError::new(WRITE_CONFLICT, WRITE_CONFLICT_MESSAGE)
                .with_label(TRANSIENT_TRANSACTION_ERROR),
            false,
        ),
        RangeError::NoSuchTransaction => (
            CommandError::new(NO_SUCH_TRANSACTION, "the transaction has been aborted")
                .with_label(TRANSIENT_TRANSACTION_ERROR),
            false,
        ),
        // A statement outside a transaction waits for the critical section and goes again
        // (`crud`); a transaction is repeated by the driver.
        RangeError::Fenced => (
            CommandError::new(STALE_CONFIG, CRITICAL_SECTION_MESSAGE)
                .with_label(TRANSIENT_TRANSACTION_ERROR),
            false,
        ),
    };
    match retryable && ctx.retryable_write {
        true => err.with_label(RETRYABLE_WRITE_ERROR),
        false => err,
    }
}

/// The codes labelled for a retry (D-148): a node that is not or no longer the primary, one
/// shutting down or cut off, and a tenant past its limits (462).
const RETRYABLE_CODES: [Code; 13] = [
    HOST_UNREACHABLE,
    HOST_NOT_FOUND,
    NETWORK_TIMEOUT,
    SHUTDOWN_IN_PROGRESS,
    PRIMARY_STEPPED_DOWN,
    EXCEEDED_TIME_LIMIT,
    INGRESS_REQUEST_RATE_LIMIT_EXCEEDED,
    SOCKET_EXCEPTION,
    NOT_WRITABLE_PRIMARY,
    INTERRUPTED_AT_SHUTDOWN,
    INTERRUPTED_DUE_TO_REPL_STATE_CHANGE,
    NOT_PRIMARY_NO_SECONDARY_OK,
    NOT_PRIMARY_OR_SECONDARY,
];

/// `err` with the label D-148 gives an error of [`RETRYABLE_CODES`] (`ShutdownInProgress` of a
/// drained node, `NotPrimaryOrSecondary` of a lost lease, a tenant's refusal, …) for command
/// `cmd`. With `txnNumber` and `autocommit`, a statement of a transaction other
/// than its commit or abort gets `TransientTransactionError` (the driver repeats the transaction)
/// and the commit and the abort get `RetryableWriteError`; with `txnNumber` alone, a retryable
/// write, it gets `RetryableWriteError` (the driver repeats the statement on another node). The
/// labels the error has are kept, and one that already says how to retry it gets no other. An
/// error answered before the command is dispatched takes its labels from here (D-148).
#[must_use]
pub fn with_retryable_labels(err: CommandError, cmd: &Document) -> CommandError {
    let labelled = |l: &str| err.labels.contains(&l);
    if !RETRYABLE_CODES.contains(&err.code)
        || labelled(TRANSIENT_TRANSACTION_ERROR)
        || labelled(RETRYABLE_WRITE_ERROR)
        || cmd.get("txnNumber").is_none()
    {
        return err;
    }
    let ends_transaction = cmd
        .iter()
        .next()
        .is_some_and(|(name, _)| [COMMIT_TRANSACTION, ABORT_TRANSACTION].contains(&name));
    match cmd.get("autocommit").is_some() && !ends_transaction {
        true => err.with_label(TRANSIENT_TRANSACTION_ERROR),
        false => err.with_label(RETRYABLE_WRITE_ERROR),
    }
}

/// An error of the node's own data that should not happen (an index definition or a bucket the
/// node wrote and cannot read back), as `InternalError` with the error's description.
pub fn internal(e: impl std::fmt::Debug) -> CommandError {
    CommandError::new(INTERNAL_ERROR, format!("{e:?}"))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_error_answered_before_dispatch_takes_the_labels_of_its_code_and_command() {
        let drained = || CommandError::new(SHUTDOWN_IN_PROGRESS, "the node is drained");
        let labels = |cmd: Document| with_retryable_labels(drained(), &cmd).labels;
        let txn = Value::Int64(3);
        // A retryable write (`txnNumber` without `autocommit`): the driver repeats the statement
        // on another node.
        for name in ["insert", "update", "delete", "findAndModify", "bulkWrite"] {
            let cmd = Document::new().with(name, "c").with("txnNumber", txn.clone());
            assert_eq!(labels(cmd), [RETRYABLE_WRITE_ERROR], "{name}");
        }
        // Without `txnNumber`: no label.
        assert!(labels(Document::new().with("insert", "c")).is_empty());
        assert!(labels(Document::new().with("find", "c")).is_empty());
        // Inside a transaction: the transaction is repeated, its commit and abort retried.
        let in_txn = |name: &str| {
            Document::new().with(name, 1).with("txnNumber", txn.clone()).with("autocommit", false)
        };
        assert_eq!(labels(in_txn("insert")), [TRANSIENT_TRANSACTION_ERROR]);
        assert_eq!(labels(in_txn("find")), [TRANSIENT_TRANSACTION_ERROR]);
        assert_eq!(labels(in_txn(COMMIT_TRANSACTION)), [RETRYABLE_WRITE_ERROR]);
        assert_eq!(labels(in_txn(ABORT_TRANSACTION)), [RETRYABLE_WRITE_ERROR]);
        // `autocommit` without `txnNumber` is no transaction for the labels.
        assert!(labels(Document::new().with("find", "c").with("autocommit", false)).is_empty());
        assert!(labels(Document::new()).is_empty(), "an empty command");
        // Another category gets nothing; an error that already says how to retry keeps it.
        let insert = Document::new().with("insert", "c").with("txnNumber", txn.clone());
        let bad = CommandError::new(BAD_VALUE, "x");
        assert!(with_retryable_labels(bad, &insert).labels.is_empty());
        let own =
            CommandError::new(NOT_WRITABLE_PRIMARY, "x").with_label(TRANSIENT_TRANSACTION_ERROR);
        assert_eq!(with_retryable_labels(own, &insert).labels, [TRANSIENT_TRANSACTION_ERROR]);
        let lost = CommandError::new(NOT_PRIMARY_OR_SECONDARY, "the lease is lost");
        assert_eq!(with_retryable_labels(lost, &insert).labels, [RETRYABLE_WRITE_ERROR]);
        // A tenant's refusal keeps the labels it has and, being of the category, takes the one
        // its statement is retried on.
        let refused = || {
            CommandError::new(INGRESS_REQUEST_RATE_LIMIT_EXCEEDED, "x")
                .with_label("SystemOverloadedError")
                .with_label("RetryableError")
        };
        assert_eq!(
            with_retryable_labels(refused(), &insert).labels,
            ["SystemOverloadedError", "RetryableError", RETRYABLE_WRITE_ERROR]
        );
        assert_eq!(
            with_retryable_labels(refused(), &in_txn("insert")).labels,
            ["SystemOverloadedError", "RetryableError", TRANSIENT_TRANSACTION_ERROR]
        );
    }

    #[test]
    fn an_internal_error_carries_its_description() {
        let e = internal("bad bucket");
        assert_eq!((e.code, e.errmsg.as_str()), (INTERNAL_ERROR, "\"bad bucket\""));
    }

    #[test]
    fn codes_are_unique() {
        for (i, a) in ALL.iter().enumerate() {
            assert!(ALL[i + 1..].iter().all(|b| b.code != a.code), "{a:?}");
        }
        assert_eq!(code(59), Some(COMMAND_NOT_FOUND));
        assert_eq!(code(424_242), None);
    }

    #[test]
    fn labels_go_last_unless_asked_first() {
        let e = CommandError::new(NO_SUCH_TRANSACTION, "x").with_label("L");
        let e = CommandError { extra: Document::new().with("recoveryToken", Document::new()), ..e };
        let keys = |d: Document| d.iter().map(|(k, _)| k.to_owned()).collect::<Vec<_>>();
        let last = ["ok", "errmsg", "code", "codeName", "recoveryToken", "errorLabels"];
        assert_eq!(keys(e.to_document()), last);
        let first = ["errorLabels", "ok", "errmsg", "code", "codeName", "recoveryToken"];
        assert_eq!(keys(e.clone().with_labels_first().to_document()), first);
        // Without labels there is nothing to put first.
        let bare = CommandError::new(NO_SUCH_TRANSACTION, "x").with_labels_first();
        assert_eq!(keys(bare.to_document()), ["ok", "errmsg", "code", "codeName"]);
        assert_eq!(bare.query_parse().origin, ErrorOrigin::QueryParse);
    }
}
