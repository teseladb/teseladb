//! RefDb's error codes, the error of a command and its reply document (contract
//! `error-codes.md`, FR-021), and the translation of the range's errors (005) into them.

use teseladb_bson::{Document, Value};
use teseladb_range::host::{MutationError, RangeError};

/// `errmsg` of a write whose Raft entry would pass 64 MiB (005 FR-023): the node writes it again
/// as an internal transaction in parts (008 FR-006).
pub const ENTRY_TOO_LARGE: &str = "object to insert too large";

/// A RefDb error code with its `codeName`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code {
    /// Number.
    pub code: i32,
    /// Name; codes without a name of their own are `Location<code>`.
    pub name: &'static str,
}

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
    /// A document past RefDb's nesting limit (003 FR-003).
    OVERFLOW = 15, "Overflow";
    /// Authentication failed (013).
    AUTHENTICATION_FAILED = 18, "AuthenticationFailed";
    /// Invalid BSON.
    INVALID_BSON = 22, "InvalidBSON";
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
    /// A forwarded request timed out.
    NETWORK_TIMEOUT = 89, "NetworkTimeout";
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
    /// A change of the feature compatibility version RefDb does not allow from the current one.
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
    /// Required field missing in a command.
    IDL_FAILED_TO_PARSE = 40414, "IDLFailedToParse";
    /// A field given twice where it may appear once.
    IDL_DUPLICATE_FIELD = 40413, "IDLDuplicateField";
    /// Unknown field in a command.
    IDL_UNKNOWN_FIELD = 40415, "IDLUnknownField";
    /// `getMore` in a session on a cursor opened outside any.
    CURSOR_NOT_IN_SESSION = 50799, "Location50799";
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
    /// `getMore` without a session on a session's cursor.
    CURSOR_SESSION_REQUIRED = 50800, "Location50800";
    /// `getMore` in a session other than its cursor's.
    CURSOR_OTHER_SESSION = 50801, "Location50801";
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
    /// A value that is not the object RefDb reads there: a `readConcern.afterOpTime`, a field of
    /// a stored user or role (013 FR-030).
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
    /// A role change RefDb refuses: a cycle, a privilege outside the role's database (013).
    INVALID_ROLE_MODIFICATION = 49, "InvalidRoleModification";
    /// `createUser` of a user that exists (013).
    USER_ALREADY_EXISTS = 51_003, "Location51003";
    /// An empty password, refused by RefDb's SASLprep (013).
    PASSWORD_NOT_TERMINATED = 50_687, "Location50687";
    /// A command that needs a collection, run on a view (015 FR-002).
    COMMAND_NOT_SUPPORTED_ON_VIEW = 166, "CommandNotSupportedOnView";
    /// An option or stage a view does not take (015 FR-001).
    OPTION_NOT_SUPPORTED_ON_VIEW = 167, "OptionNotSupportedOnView";
    /// A pipeline operator or cursor option a pipeline does not take (a tailable cursor on a view
    /// or a time series, 015 FR-004).
    INVALID_PIPELINE_OPERATOR = 168, "InvalidPipelineOperator";
    /// A command on a `system.buckets.*` namespace, which RefDb 9.0 no longer serves (015 FR-006).
    LEGACY_TIMESERIES_BUCKETS = 491, "CommandNotSupportedOnLegacyTimeseriesBucketsNamespace";
    /// A partial filter of a time series index that has no form over the buckets (015 FR-008).
    TIMESERIES_PREDICATE = 5_916_301, "Location5916301";
    /// A view on too many views (015 FR-001).
    VIEW_DEPTH_LIMIT_EXCEEDED = 165, "ViewDepthLimitExceeded";
    /// A view that reaches itself (015 FR-001).
    GRAPH_CONTAINS_CYCLE = 5, "GraphContainsCycle";
    /// A pipeline stage with more or fewer than one field (010).
    STAGE_ONE_FIELD = 40_323, "Location40323";
    /// A pipeline stage RefDb does not know (010).
    UNRECOGNIZED_STAGE = 40_324, "Location40324";
    /// A stage RefDb keeps for its own pipelines (010).
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
    /// A clustered capped collection, a test feature of RefDb (015 FR-009).
    CLUSTERED_CAPPED_TEST = 6_127_800, "Location6127800";
    /// A `txnNumber` without `autocommit: false` on a command that is no retryable write (008).
    NOT_A_RETRYABLE_WRITE_COMMAND = 50768, "NotARetryableWriteCommand";
    /// A `txnNumber` on a command that takes none, such as `hello` (008).
    TXN_NUMBER_NOT_ALLOWED = 50889, "Location50889";
    /// A commit of a transaction the node has no record of, without its `recoveryToken` (008).
    RECOVERY_TOKEN_REQUIRED = 50940, "Location50940";
    /// A `count` on a view whose pipeline fails on the shard, as a `refrouter` relays it.
    COUNT_ON_VIEW_FAILED = 4_848_401, "Location4848401";
}

/// Label of a write the driver may retry.
pub const RETRYABLE_WRITE_ERROR: &str = "RetryableWriteError";
/// Label of a transaction the driver may retry whole.
pub const TRANSIENT_TRANSACTION_ERROR: &str = "TransientTransactionError";
/// Label of a commit whose result is unknown: the driver asks again (008).
pub const UNKNOWN_TRANSACTION_COMMIT_RESULT: &str = "UnknownTransactionCommitResult";
/// The `errmsg` of a write conflict in a transaction, as RefDb 9.0 writes it.
pub const WRITE_CONFLICT_MESSAGE: &str = "Caused by :: Write conflict during plan execution and \
     yielding is disabled. :: Please retry your operation or multi-document transaction.";

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
    /// Who raises it, as a `refrouter` would: what decides whether it ends a transaction (008).
    pub origin: ErrorOrigin,
}

/// Who raises an error of a command, as a `refrouter` 9.0.4 and its shard share the work (measured on
/// 01-11-2026): an error of a statement of a transaction ends the transaction only if the shard
/// raised it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorOrigin {
    /// The router, while it parses the command: the command's fields and their types, and the
    /// names of the pipeline's stages.
    Router,
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
        }
    }

    /// The error, raised while the router parses the command ([`ErrorOrigin::Router`]).
    pub fn parsed(self) -> CommandError {
        CommandError { origin: ErrorOrigin::Router, ..self }
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

    /// The reply document: `ok: 0`, `errmsg`, `code`, `codeName`, the extra fields and
    /// `errorLabels`, in RefDb's order.
    pub fn to_document(&self) -> Document {
        let mut d = Document::new()
            .with("ok", 0.0)
            .with("errmsg", self.errmsg.as_str())
            .with("code", self.code.code)
            .with("codeName", self.code.name);
        for (k, v) in self.extra.iter() {
            d.push(k, v.clone());
        }
        if !self.labels.is_empty() {
            let labels = self.labels.iter().map(|&l| Value::from(l)).collect::<Vec<_>>();
            d.push("errorLabels", labels);
        }
        d
    }

    /// The error carried as a range mutation's error, so that a mutation evaluated on the
    /// range's leader brings back its RefDb code (see [`CommandError::from_mutation`]).
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
    /// A BSON fault with RefDb's code for it (`BSONObjectTooLarge`, `Overflow`, …).
    fn from(e: teseladb_bson::BsonError) -> CommandError {
        let (n, name) = e.code();
        CommandError::new(Code { code: n, name }, e.to_string())
    }
}

impl From<teseladb_collation::SpecError> for CommandError {
    /// A collation document RefDb refuses, with its code.
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

/// The RefDb error for a range error (005), as a `refrouter` reports a shard's.
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
        RangeError::WriteConflict | RangeError::IntentConflict(_) => (
            CommandError::new(WRITE_CONFLICT, WRITE_CONFLICT_MESSAGE)
                .with_label(TRANSIENT_TRANSACTION_ERROR),
            false,
        ),
        RangeError::NoSuchTransaction => (
            CommandError::new(NO_SUCH_TRANSACTION, "the transaction has been aborted")
                .with_label(TRANSIENT_TRANSACTION_ERROR),
            false,
        ),
    };
    match retryable && ctx.retryable_write {
        true => err.with_label(RETRYABLE_WRITE_ERROR),
        false => err,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique() {
        for (i, a) in ALL.iter().enumerate() {
            assert!(ALL[i + 1..].iter().all(|b| b.code != a.code), "{a:?}");
        }
        assert_eq!(code(59), Some(COMMAND_NOT_FOUND));
        assert_eq!(code(424_242), None);
    }
}
