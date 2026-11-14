//! The privilege actions of the reference server 9.0.4 (013 FR-010), in the order the reference
//! server lists them. Written by `scripts/gen-builtin-roles.py` from the recorded
//! answers of the reference server; do not edit.

/// A privilege action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionType {
    /// `addShard`.
    AddShard,
    /// `analyze`.
    Analyze,
    /// `analyzeShardKey`.
    AnalyzeShardKey,
    /// `anyAction`.
    AnyAction,
    /// `auditLogRotate`.
    AuditLogRotate,
    /// `bypassDocumentValidation`.
    BypassDocumentValidation,
    /// `changeCustomData`.
    ChangeCustomData,
    /// `changeOwnCustomData`.
    ChangeOwnCustomData,
    /// `changeOwnPassword`.
    ChangeOwnPassword,
    /// `changePassword`.
    ChangePassword,
    /// `changeStream`.
    ChangeStream,
    /// `checkFreeMonitoringStatus`.
    CheckFreeMonitoringStatus,
    /// `clearJumboFlag`.
    ClearJumboFlag,
    /// `cleanupStructuredEncryptionData`.
    CleanupStructuredEncryptionData,
    /// `closeAllDatabases`.
    CloseAllDatabases,
    /// `collMod`.
    CollMod,
    /// `collStats`.
    CollStats,
    /// `cpuProfiler`.
    CpuProfiler,
    /// `dbCheck`.
    DbCheck,
    /// `dropAllRolesFromDatabase`.
    DropAllRolesFromDatabase,
    /// `exportCollection`.
    ExportCollection,
    /// `impersonate`.
    Impersonate,
    /// `importCollection`.
    ImportCollection,
    /// `internal`.
    Internal,
    /// `listClusterCatalog`.
    ListClusterCatalog,
    /// `allCollectionStats`.
    AllCollectionStats,
    /// `appendOplogNote`.
    AppendOplogNote,
    /// `applicationMessage`.
    ApplicationMessage,
    /// `auditConfigure`.
    AuditConfigure,
    /// `authSchemaUpgrade`.
    AuthSchemaUpgrade,
    /// `bypassDefaultMaxTimeMS`.
    BypassDefaultMaxTimeMS,
    /// `bypassWriteBlockingMode`.
    BypassWriteBlockingMode,
    /// `bypassReplicaSetWritesBlocking`.
    BypassReplicaSetWritesBlocking,
    /// `checkMetadataConsistency`.
    CheckMetadataConsistency,
    /// `cleanupOrphaned`.
    CleanupOrphaned,
    /// `compact`.
    Compact,
    /// `compactStructuredEncryptionData`.
    CompactStructuredEncryptionData,
    /// `configureQueryAnalyzer`.
    ConfigureQueryAnalyzer,
    /// `connPoolStats`.
    ConnPoolStats,
    /// `connPoolSync`.
    ConnPoolSync,
    /// `convertToCapped`.
    ConvertToCapped,
    /// `createCollection`.
    CreateCollection,
    /// `createIndex`.
    CreateIndex,
    /// `createRole`.
    CreateRole,
    /// `createSearchIndexes`.
    CreateSearchIndexes,
    /// `createUser`.
    CreateUser,
    /// `dbHash`.
    DbHash,
    /// `dbStats`.
    DbStats,
    /// `dropCollection`.
    DropCollection,
    /// `dropConnections`.
    DropConnections,
    /// `dropDatabase`.
    DropDatabase,
    /// `dropIndex`.
    DropIndex,
    /// `dropRole`.
    DropRole,
    /// `dropSearchIndex`.
    DropSearchIndex,
    /// `dropUser`.
    DropUser,
    /// `enableProfiler`.
    EnableProfiler,
    /// `enableSharding`.
    EnableSharding,
    /// `find`.
    Find,
    /// `flushRouterConfig`.
    FlushRouterConfig,
    /// `forceUUID`.
    ForceUUID,
    /// `fsync`.
    Fsync,
    /// `getClusterParameter`.
    GetClusterParameter,
    /// `getCmdLineOpts`.
    GetCmdLineOpts,
    /// `getDatabaseVersion`.
    GetDatabaseVersion,
    /// `getDefaultRWConcern`.
    GetDefaultRWConcern,
    /// `getLog`.
    GetLog,
    /// `getParameter`.
    GetParameter,
    /// `getShardMap`.
    GetShardMap,
    /// `getShardVersion`.
    GetShardVersion,
    /// `getTransitionToDedicatedConfigServerStatus`.
    GetTransitionToDedicatedConfigServerStatus,
    /// `grantRole`.
    GrantRole,
    /// `hostInfo`.
    HostInfo,
    /// `indexStats`.
    IndexStats,
    /// `inprog`.
    Inprog,
    /// `insert`.
    Insert,
    /// `invalidateUserCache`.
    InvalidateUserCache,
    /// `issueDirectShardOperations`.
    IssueDirectShardOperations,
    /// `killAnyCursor`.
    KillAnyCursor,
    /// `killAnySession`.
    KillAnySession,
    /// `killCursors`.
    KillCursors,
    /// `killop`.
    Killop,
    /// `listCachedAndActiveUsers`.
    ListCachedAndActiveUsers,
    /// `listCollections`.
    ListCollections,
    /// `listDatabases`.
    ListDatabases,
    /// `listExtensions`.
    ListExtensions,
    /// `listIndexes`.
    ListIndexes,
    /// `listSampledQueries`.
    ListSampledQueries,
    /// `listSearchIndexes`.
    ListSearchIndexes,
    /// `listSessions`.
    ListSessions,
    /// `listShards`.
    ListShards,
    /// `logRotate`.
    LogRotate,
    /// `netstat`.
    Netstat,
    /// `oidcListKeys`.
    OidcListKeys,
    /// `oidcRefreshKeys`.
    OidcRefreshKeys,
    /// `oidReset`.
    OidReset,
    /// `querySettings`.
    QuerySettings,
    /// `queryStatsRead`.
    QueryStatsRead,
    /// `queryStatsReadTransformed`.
    QueryStatsReadTransformed,
    /// `blockReplicaSetWrites`.
    BlockReplicaSetWrites,
    /// `readBackupFile`.
    ReadBackupFile,
    /// `removeShard`.
    RemoveShard,
    /// `replSetConfigure`.
    ReplSetConfigure,
    /// `replSetGetConfig`.
    ReplSetGetConfig,
    /// `replSetGetStatus`.
    ReplSetGetStatus,
    /// `replSetHeartbeat`.
    ReplSetHeartbeat,
    /// `replSetResizeOplog`.
    ReplSetResizeOplog,
    /// `replSetStateChange`.
    ReplSetStateChange,
    /// `resync`.
    Resync,
    /// `rotateCertificates`.
    RotateCertificates,
    /// `runAsLessPrivilegedUser`.
    RunAsLessPrivilegedUser,
    /// `serverStatus`.
    ServerStatus,
    /// `setClusterParameter`.
    SetClusterParameter,
    /// `setDefaultRWConcern`.
    SetDefaultRWConcern,
    /// `setFeatureCompatibilityVersion`.
    SetFeatureCompatibilityVersion,
    /// `setFreeMonitoring`.
    SetFreeMonitoring,
    /// `setParameter`.
    SetParameter,
    /// `setUserWriteBlockMode`.
    SetUserWriteBlockMode,
    /// `shardCollection`.
    ShardCollection,
    /// `shardDrainingStatus`.
    ShardDrainingStatus,
    /// `shardedDataDistribution`.
    ShardedDataDistribution,
    /// `shardingState`.
    ShardingState,
    /// `shutdown`.
    Shutdown,
    /// `top`.
    Top,
    /// `touch`.
    Touch,
    /// `trafficRecord`.
    TrafficRecord,
    /// `transitionFromDedicatedConfigServer`.
    TransitionFromDedicatedConfigServer,
    /// `transitionToDedicatedConfigServer`.
    TransitionToDedicatedConfigServer,
    /// `unlock`.
    Unlock,
    /// `updateSearchIndex`.
    UpdateSearchIndex,
    /// `moveChunk`.
    MoveChunk,
    /// `moveCollection`.
    MoveCollection,
    /// `planCacheIndexFilter`.
    PlanCacheIndexFilter,
    /// `planCacheRead`.
    PlanCacheRead,
    /// `planCacheWrite`.
    PlanCacheWrite,
    /// `performRawDataOperations`.
    PerformRawDataOperations,
    /// `refineCollectionShardKey`.
    RefineCollectionShardKey,
    /// `reIndex`.
    ReIndex,
    /// `remove`.
    Remove,
    /// `renameCollectionSameDB`.
    RenameCollectionSameDB,
    /// `reshardCollection`.
    ReshardCollection,
    /// `revokeRole`.
    RevokeRole,
    /// `rewriteCollection`.
    RewriteCollection,
    /// `setAuthenticationRestriction`.
    SetAuthenticationRestriction,
    /// `splitChunk`.
    SplitChunk,
    /// `splitVector`.
    SplitVector,
    /// `storageDetails`.
    StorageDetails,
    /// `unshardCollection`.
    UnshardCollection,
    /// `update`.
    Update,
    /// `useTenant`.
    UseTenant,
    /// `useUUID`.
    UseUUID,
    /// `validate`.
    Validate,
    /// `viewRole`.
    ViewRole,
    /// `viewUser`.
    ViewUser,
    /// `applyOps`.
    ApplyOps,
}

impl ActionType {
    /// Every action, in the reference server's order.
    pub const ALL: [ActionType; 154] = [
        ActionType::AddShard,
        ActionType::Analyze,
        ActionType::AnalyzeShardKey,
        ActionType::AnyAction,
        ActionType::AuditLogRotate,
        ActionType::BypassDocumentValidation,
        ActionType::ChangeCustomData,
        ActionType::ChangeOwnCustomData,
        ActionType::ChangeOwnPassword,
        ActionType::ChangePassword,
        ActionType::ChangeStream,
        ActionType::CheckFreeMonitoringStatus,
        ActionType::ClearJumboFlag,
        ActionType::CleanupStructuredEncryptionData,
        ActionType::CloseAllDatabases,
        ActionType::CollMod,
        ActionType::CollStats,
        ActionType::CpuProfiler,
        ActionType::DbCheck,
        ActionType::DropAllRolesFromDatabase,
        ActionType::ExportCollection,
        ActionType::Impersonate,
        ActionType::ImportCollection,
        ActionType::Internal,
        ActionType::ListClusterCatalog,
        ActionType::AllCollectionStats,
        ActionType::AppendOplogNote,
        ActionType::ApplicationMessage,
        ActionType::AuditConfigure,
        ActionType::AuthSchemaUpgrade,
        ActionType::BypassDefaultMaxTimeMS,
        ActionType::BypassWriteBlockingMode,
        ActionType::BypassReplicaSetWritesBlocking,
        ActionType::CheckMetadataConsistency,
        ActionType::CleanupOrphaned,
        ActionType::Compact,
        ActionType::CompactStructuredEncryptionData,
        ActionType::ConfigureQueryAnalyzer,
        ActionType::ConnPoolStats,
        ActionType::ConnPoolSync,
        ActionType::ConvertToCapped,
        ActionType::CreateCollection,
        ActionType::CreateIndex,
        ActionType::CreateRole,
        ActionType::CreateSearchIndexes,
        ActionType::CreateUser,
        ActionType::DbHash,
        ActionType::DbStats,
        ActionType::DropCollection,
        ActionType::DropConnections,
        ActionType::DropDatabase,
        ActionType::DropIndex,
        ActionType::DropRole,
        ActionType::DropSearchIndex,
        ActionType::DropUser,
        ActionType::EnableProfiler,
        ActionType::EnableSharding,
        ActionType::Find,
        ActionType::FlushRouterConfig,
        ActionType::ForceUUID,
        ActionType::Fsync,
        ActionType::GetClusterParameter,
        ActionType::GetCmdLineOpts,
        ActionType::GetDatabaseVersion,
        ActionType::GetDefaultRWConcern,
        ActionType::GetLog,
        ActionType::GetParameter,
        ActionType::GetShardMap,
        ActionType::GetShardVersion,
        ActionType::GetTransitionToDedicatedConfigServerStatus,
        ActionType::GrantRole,
        ActionType::HostInfo,
        ActionType::IndexStats,
        ActionType::Inprog,
        ActionType::Insert,
        ActionType::InvalidateUserCache,
        ActionType::IssueDirectShardOperations,
        ActionType::KillAnyCursor,
        ActionType::KillAnySession,
        ActionType::KillCursors,
        ActionType::Killop,
        ActionType::ListCachedAndActiveUsers,
        ActionType::ListCollections,
        ActionType::ListDatabases,
        ActionType::ListExtensions,
        ActionType::ListIndexes,
        ActionType::ListSampledQueries,
        ActionType::ListSearchIndexes,
        ActionType::ListSessions,
        ActionType::ListShards,
        ActionType::LogRotate,
        ActionType::Netstat,
        ActionType::OidcListKeys,
        ActionType::OidcRefreshKeys,
        ActionType::OidReset,
        ActionType::QuerySettings,
        ActionType::QueryStatsRead,
        ActionType::QueryStatsReadTransformed,
        ActionType::BlockReplicaSetWrites,
        ActionType::ReadBackupFile,
        ActionType::RemoveShard,
        ActionType::ReplSetConfigure,
        ActionType::ReplSetGetConfig,
        ActionType::ReplSetGetStatus,
        ActionType::ReplSetHeartbeat,
        ActionType::ReplSetResizeOplog,
        ActionType::ReplSetStateChange,
        ActionType::Resync,
        ActionType::RotateCertificates,
        ActionType::RunAsLessPrivilegedUser,
        ActionType::ServerStatus,
        ActionType::SetClusterParameter,
        ActionType::SetDefaultRWConcern,
        ActionType::SetFeatureCompatibilityVersion,
        ActionType::SetFreeMonitoring,
        ActionType::SetParameter,
        ActionType::SetUserWriteBlockMode,
        ActionType::ShardCollection,
        ActionType::ShardDrainingStatus,
        ActionType::ShardedDataDistribution,
        ActionType::ShardingState,
        ActionType::Shutdown,
        ActionType::Top,
        ActionType::Touch,
        ActionType::TrafficRecord,
        ActionType::TransitionFromDedicatedConfigServer,
        ActionType::TransitionToDedicatedConfigServer,
        ActionType::Unlock,
        ActionType::UpdateSearchIndex,
        ActionType::MoveChunk,
        ActionType::MoveCollection,
        ActionType::PlanCacheIndexFilter,
        ActionType::PlanCacheRead,
        ActionType::PlanCacheWrite,
        ActionType::PerformRawDataOperations,
        ActionType::RefineCollectionShardKey,
        ActionType::ReIndex,
        ActionType::Remove,
        ActionType::RenameCollectionSameDB,
        ActionType::ReshardCollection,
        ActionType::RevokeRole,
        ActionType::RewriteCollection,
        ActionType::SetAuthenticationRestriction,
        ActionType::SplitChunk,
        ActionType::SplitVector,
        ActionType::StorageDetails,
        ActionType::UnshardCollection,
        ActionType::Update,
        ActionType::UseTenant,
        ActionType::UseUUID,
        ActionType::Validate,
        ActionType::ViewRole,
        ActionType::ViewUser,
        ActionType::ApplyOps,
    ];

    /// Its name.
    pub fn name(self) -> &'static str {
        match self {
            ActionType::AddShard => "addShard",
            ActionType::Analyze => "analyze",
            ActionType::AnalyzeShardKey => "analyzeShardKey",
            ActionType::AnyAction => "anyAction",
            ActionType::AuditLogRotate => "auditLogRotate",
            ActionType::BypassDocumentValidation => "bypassDocumentValidation",
            ActionType::ChangeCustomData => "changeCustomData",
            ActionType::ChangeOwnCustomData => "changeOwnCustomData",
            ActionType::ChangeOwnPassword => "changeOwnPassword",
            ActionType::ChangePassword => "changePassword",
            ActionType::ChangeStream => "changeStream",
            ActionType::CheckFreeMonitoringStatus => "checkFreeMonitoringStatus",
            ActionType::ClearJumboFlag => "clearJumboFlag",
            ActionType::CleanupStructuredEncryptionData => "cleanupStructuredEncryptionData",
            ActionType::CloseAllDatabases => "closeAllDatabases",
            ActionType::CollMod => "collMod",
            ActionType::CollStats => "collStats",
            ActionType::CpuProfiler => "cpuProfiler",
            ActionType::DbCheck => "dbCheck",
            ActionType::DropAllRolesFromDatabase => "dropAllRolesFromDatabase",
            ActionType::ExportCollection => "exportCollection",
            ActionType::Impersonate => "impersonate",
            ActionType::ImportCollection => "importCollection",
            ActionType::Internal => "internal",
            ActionType::ListClusterCatalog => "listClusterCatalog",
            ActionType::AllCollectionStats => "allCollectionStats",
            ActionType::AppendOplogNote => "appendOplogNote",
            ActionType::ApplicationMessage => "applicationMessage",
            ActionType::AuditConfigure => "auditConfigure",
            ActionType::AuthSchemaUpgrade => "authSchemaUpgrade",
            ActionType::BypassDefaultMaxTimeMS => "bypassDefaultMaxTimeMS",
            ActionType::BypassWriteBlockingMode => "bypassWriteBlockingMode",
            ActionType::BypassReplicaSetWritesBlocking => "bypassReplicaSetWritesBlocking",
            ActionType::CheckMetadataConsistency => "checkMetadataConsistency",
            ActionType::CleanupOrphaned => "cleanupOrphaned",
            ActionType::Compact => "compact",
            ActionType::CompactStructuredEncryptionData => "compactStructuredEncryptionData",
            ActionType::ConfigureQueryAnalyzer => "configureQueryAnalyzer",
            ActionType::ConnPoolStats => "connPoolStats",
            ActionType::ConnPoolSync => "connPoolSync",
            ActionType::ConvertToCapped => "convertToCapped",
            ActionType::CreateCollection => "createCollection",
            ActionType::CreateIndex => "createIndex",
            ActionType::CreateRole => "createRole",
            ActionType::CreateSearchIndexes => "createSearchIndexes",
            ActionType::CreateUser => "createUser",
            ActionType::DbHash => "dbHash",
            ActionType::DbStats => "dbStats",
            ActionType::DropCollection => "dropCollection",
            ActionType::DropConnections => "dropConnections",
            ActionType::DropDatabase => "dropDatabase",
            ActionType::DropIndex => "dropIndex",
            ActionType::DropRole => "dropRole",
            ActionType::DropSearchIndex => "dropSearchIndex",
            ActionType::DropUser => "dropUser",
            ActionType::EnableProfiler => "enableProfiler",
            ActionType::EnableSharding => "enableSharding",
            ActionType::Find => "find",
            ActionType::FlushRouterConfig => "flushRouterConfig",
            ActionType::ForceUUID => "forceUUID",
            ActionType::Fsync => "fsync",
            ActionType::GetClusterParameter => "getClusterParameter",
            ActionType::GetCmdLineOpts => "getCmdLineOpts",
            ActionType::GetDatabaseVersion => "getDatabaseVersion",
            ActionType::GetDefaultRWConcern => "getDefaultRWConcern",
            ActionType::GetLog => "getLog",
            ActionType::GetParameter => "getParameter",
            ActionType::GetShardMap => "getShardMap",
            ActionType::GetShardVersion => "getShardVersion",
            ActionType::GetTransitionToDedicatedConfigServerStatus => {
                "getTransitionToDedicatedConfigServerStatus"
            }
            ActionType::GrantRole => "grantRole",
            ActionType::HostInfo => "hostInfo",
            ActionType::IndexStats => "indexStats",
            ActionType::Inprog => "inprog",
            ActionType::Insert => "insert",
            ActionType::InvalidateUserCache => "invalidateUserCache",
            ActionType::IssueDirectShardOperations => "issueDirectShardOperations",
            ActionType::KillAnyCursor => "killAnyCursor",
            ActionType::KillAnySession => "killAnySession",
            ActionType::KillCursors => "killCursors",
            ActionType::Killop => "killop",
            ActionType::ListCachedAndActiveUsers => "listCachedAndActiveUsers",
            ActionType::ListCollections => "listCollections",
            ActionType::ListDatabases => "listDatabases",
            ActionType::ListExtensions => "listExtensions",
            ActionType::ListIndexes => "listIndexes",
            ActionType::ListSampledQueries => "listSampledQueries",
            ActionType::ListSearchIndexes => "listSearchIndexes",
            ActionType::ListSessions => "listSessions",
            ActionType::ListShards => "listShards",
            ActionType::LogRotate => "logRotate",
            ActionType::Netstat => "netstat",
            ActionType::OidcListKeys => "oidcListKeys",
            ActionType::OidcRefreshKeys => "oidcRefreshKeys",
            ActionType::OidReset => "oidReset",
            ActionType::QuerySettings => "querySettings",
            ActionType::QueryStatsRead => "queryStatsRead",
            ActionType::QueryStatsReadTransformed => "queryStatsReadTransformed",
            ActionType::BlockReplicaSetWrites => "blockReplicaSetWrites",
            ActionType::ReadBackupFile => "readBackupFile",
            ActionType::RemoveShard => "removeShard",
            ActionType::ReplSetConfigure => "replSetConfigure",
            ActionType::ReplSetGetConfig => "replSetGetConfig",
            ActionType::ReplSetGetStatus => "replSetGetStatus",
            ActionType::ReplSetHeartbeat => "replSetHeartbeat",
            ActionType::ReplSetResizeOplog => "replSetResizeOplog",
            ActionType::ReplSetStateChange => "replSetStateChange",
            ActionType::Resync => "resync",
            ActionType::RotateCertificates => "rotateCertificates",
            ActionType::RunAsLessPrivilegedUser => "runAsLessPrivilegedUser",
            ActionType::ServerStatus => "serverStatus",
            ActionType::SetClusterParameter => "setClusterParameter",
            ActionType::SetDefaultRWConcern => "setDefaultRWConcern",
            ActionType::SetFeatureCompatibilityVersion => "setFeatureCompatibilityVersion",
            ActionType::SetFreeMonitoring => "setFreeMonitoring",
            ActionType::SetParameter => "setParameter",
            ActionType::SetUserWriteBlockMode => "setUserWriteBlockMode",
            ActionType::ShardCollection => "shardCollection",
            ActionType::ShardDrainingStatus => "shardDrainingStatus",
            ActionType::ShardedDataDistribution => "shardedDataDistribution",
            ActionType::ShardingState => "shardingState",
            ActionType::Shutdown => "shutdown",
            ActionType::Top => "top",
            ActionType::Touch => "touch",
            ActionType::TrafficRecord => "trafficRecord",
            ActionType::TransitionFromDedicatedConfigServer => {
                "transitionFromDedicatedConfigServer"
            }
            ActionType::TransitionToDedicatedConfigServer => "transitionToDedicatedConfigServer",
            ActionType::Unlock => "unlock",
            ActionType::UpdateSearchIndex => "updateSearchIndex",
            ActionType::MoveChunk => "moveChunk",
            ActionType::MoveCollection => "moveCollection",
            ActionType::PlanCacheIndexFilter => "planCacheIndexFilter",
            ActionType::PlanCacheRead => "planCacheRead",
            ActionType::PlanCacheWrite => "planCacheWrite",
            ActionType::PerformRawDataOperations => "performRawDataOperations",
            ActionType::RefineCollectionShardKey => "refineCollectionShardKey",
            ActionType::ReIndex => "reIndex",
            ActionType::Remove => "remove",
            ActionType::RenameCollectionSameDB => "renameCollectionSameDB",
            ActionType::ReshardCollection => "reshardCollection",
            ActionType::RevokeRole => "revokeRole",
            ActionType::RewriteCollection => "rewriteCollection",
            ActionType::SetAuthenticationRestriction => "setAuthenticationRestriction",
            ActionType::SplitChunk => "splitChunk",
            ActionType::SplitVector => "splitVector",
            ActionType::StorageDetails => "storageDetails",
            ActionType::UnshardCollection => "unshardCollection",
            ActionType::Update => "update",
            ActionType::UseTenant => "useTenant",
            ActionType::UseUUID => "useUUID",
            ActionType::Validate => "validate",
            ActionType::ViewRole => "viewRole",
            ActionType::ViewUser => "viewUser",
            ActionType::ApplyOps => "applyOps",
        }
    }
}
