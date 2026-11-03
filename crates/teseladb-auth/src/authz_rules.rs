//! What each command needs (013 FR-011): the table of `contracts/privileges.md`, measured on
//! RefDb 9.0.4. A command missing from it needs `anyAction` on any resource, so that a command
//! added without its row is refused rather than open; a test of `teseladb-wire` checks that every
//! command the node runs has its row.

use teseladb_bson::{Document, Value};

use crate::actions::ActionType as A;
use crate::authz::{Check, Need, Requirement, Target, needs};
use crate::authz_users;
use crate::roles::ADMIN_DB;
use crate::users::{RoleName, UserName};

/// Who runs the command: the user and the roles granted to it directly.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    /// The user.
    pub user: &'a UserName,
    /// Its own roles (not those they inherit).
    pub roles: &'a [RoleName],
}

/// Where a row's actions apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum On {
    /// No privilege.
    Open,
    /// The cluster.
    Cluster,
    /// The database of `$db`.
    Db,
    /// The database the first field names.
    NamedDb,
    /// Collection `<$db>.<first field>`.
    Coll,
    /// The collection of the full namespace of the first field.
    FullNs,
    /// Every normal database.
    AnyNormal,
    /// Database `config`.
    Config,
    /// Database `admin`.
    Admin,
}

/// A command, where its actions apply and the actions.
type Row = (&'static str, On, &'static [A]);

/// The commands with a fixed requirement; the others are in [`SPECIAL`].
const ROWS: &[Row] = &[
    ("hello", On::Open, &[]),
    ("isMaster", On::Open, &[]),
    ("ping", On::Open, &[]),
    ("buildInfo", On::Open, &[]),
    ("whatsmyuri", On::Open, &[]),
    // The API keys check the user's own keys or the privilege on its database themselves
    // (019 FR-016): which user a `revokeApiKey` touches is only known once its key is read.
    ("createApiKey", On::Open, &[]),
    ("listApiKeys", On::Open, &[]),
    ("revokeApiKey", On::Open, &[]),
    ("isdbgrid", On::Open, &[]),
    ("listCommands", On::Open, &[]),
    ("connectionStatus", On::Open, &[]),
    ("features", On::Open, &[]),
    ("startSession", On::Open, &[]),
    ("endSessions", On::Open, &[]),
    ("refreshSessions", On::Open, &[]),
    ("killSessions", On::Open, &[]),
    ("commitTransaction", On::Open, &[]),
    ("abortTransaction", On::Open, &[]),
    ("saslStart", On::Open, &[]),
    ("saslContinue", On::Open, &[]),
    ("authenticate", On::Open, &[]),
    ("configureFailPoint", On::Open, &[]),
    ("find", On::Coll, &[A::Find]),
    ("count", On::Coll, &[A::Find]),
    ("distinct", On::Coll, &[A::Find]),
    ("delete", On::Coll, &[A::Remove]),
    ("drop", On::Coll, &[A::DropCollection]),
    ("collMod", On::Coll, &[A::CollMod]),
    ("listIndexes", On::Coll, &[A::ListIndexes]),
    ("collStats", On::Coll, &[A::CollStats]),
    ("createIndexes", On::Coll, &[A::CreateIndex]),
    ("setIndexCommitQuorum", On::Coll, &[A::CreateIndex]),
    ("dropIndexes", On::Coll, &[A::DropIndex]),
    ("reIndex", On::Coll, &[A::ReIndex]),
    ("convertToCapped", On::Coll, &[A::ConvertToCapped]),
    ("compact", On::Coll, &[A::Compact]),
    ("validate", On::Coll, &[A::Validate]),
    ("planCacheClear", On::Coll, &[A::PlanCacheWrite]),
    ("createSearchIndexes", On::Coll, &[A::CreateSearchIndexes]),
    ("updateSearchIndex", On::Coll, &[A::UpdateSearchIndex]),
    ("dropSearchIndex", On::Coll, &[A::DropSearchIndex]),
    ("listSearchIndexes", On::Coll, &[A::ListSearchIndexes]),
    ("recreateRangeDeletionTasks", On::Coll, &[A::CleanupOrphaned]),
    ("dataSize", On::FullNs, &[A::Find]),
    ("splitVector", On::FullNs, &[A::SplitVector]),
    ("shardCollection", On::FullNs, &[A::EnableSharding]),
    ("balancerCollectionStatus", On::FullNs, &[A::EnableSharding]),
    ("moveChunk", On::FullNs, &[A::MoveChunk]),
    ("moveRange", On::FullNs, &[A::MoveChunk]),
    ("setAllowMigrations", On::FullNs, &[A::MoveChunk]),
    ("split", On::FullNs, &[A::SplitChunk]),
    ("mergeChunks", On::FullNs, &[A::SplitChunk]),
    ("mergeAllChunksOnShard", On::FullNs, &[A::SplitChunk]),
    ("configureCollectionBalancing", On::FullNs, &[A::SplitChunk]),
    ("clearJumboFlag", On::FullNs, &[A::ClearJumboFlag]),
    ("moveCollection", On::FullNs, &[A::MoveCollection]),
    ("abortMoveCollection", On::FullNs, &[A::MoveCollection]),
    ("unshardCollection", On::FullNs, &[A::UnshardCollection]),
    ("abortUnshardCollection", On::FullNs, &[A::UnshardCollection]),
    ("rewriteCollection", On::FullNs, &[A::RewriteCollection]),
    ("abortRewriteCollection", On::FullNs, &[A::RewriteCollection]),
    ("reshardCollection", On::FullNs, &[A::ReshardCollection]),
    ("abortReshardCollection", On::FullNs, &[A::ReshardCollection]),
    ("commitReshardCollection", On::FullNs, &[A::ReshardCollection]),
    ("refineCollectionShardKey", On::FullNs, &[A::RefineCollectionShardKey]),
    ("analyzeShardKey", On::FullNs, &[A::AnalyzeShardKey]),
    ("configureQueryAnalyzer", On::FullNs, &[A::ConfigureQueryAnalyzer]),
    ("getShardVersion", On::FullNs, &[A::GetShardVersion]),
    ("dbStats", On::Db, &[A::DbStats]),
    ("dropDatabase", On::Db, &[A::DropDatabase]),
    ("dbHash", On::Db, &[A::DbHash]),
    ("compactStructuredEncryptionData", On::Db, &[A::CompactStructuredEncryptionData]),
    ("cleanupStructuredEncryptionData", On::Db, &[A::CleanupStructuredEncryptionData]),
    ("dropUser", On::Db, &[A::DropUser]),
    ("dropAllUsersFromDatabase", On::Db, &[A::DropUser]),
    ("dropRole", On::Db, &[A::DropRole]),
    ("dropAllRolesFromDatabase", On::Db, &[A::DropRole]),
    ("enableSharding", On::NamedDb, &[A::EnableSharding]),
    ("getDatabaseVersion", On::NamedDb, &[A::GetDatabaseVersion]),
    ("movePrimary", On::NamedDb, &[A::MoveChunk]),
    ("validateDBMetadata", On::AnyNormal, &[A::Validate]),
    ("setProfilingFilterGlobally", On::AnyNormal, &[A::EnableProfiler]),
    ("balancerStatus", On::Config, &[A::Find]),
    ("balancerStart", On::Config, &[A::Update]),
    ("balancerStop", On::Config, &[A::Update]),
    ("resetPlacementHistory", On::Admin, &[A::MoveChunk]),
    ("hostInfo", On::Cluster, &[A::HostInfo]),
    ("serverStatus", On::Cluster, &[A::ServerStatus]),
    ("netstat", On::Cluster, &[A::Netstat]),
    ("getParameter", On::Cluster, &[A::GetParameter]),
    ("setParameter", On::Cluster, &[A::SetParameter]),
    ("getCmdLineOpts", On::Cluster, &[A::GetCmdLineOpts]),
    ("getLog", On::Cluster, &[A::GetLog]),
    ("logRotate", On::Cluster, &[A::LogRotate]),
    ("connPoolStats", On::Cluster, &[A::ConnPoolStats]),
    ("connPoolSync", On::Cluster, &[A::ConnPoolSync]),
    ("dropConnections", On::Cluster, &[A::DropConnections]),
    ("fsync", On::Cluster, &[A::Fsync]),
    ("fsyncUnlock", On::Cluster, &[A::Unlock]),
    ("setUserWriteBlockMode", On::Cluster, &[A::SetUserWriteBlockMode]),
    ("invalidateUserCache", On::Cluster, &[A::InvalidateUserCache]),
    ("getDefaultRWConcern", On::Cluster, &[A::GetDefaultRWConcern]),
    ("setDefaultRWConcern", On::Cluster, &[A::SetDefaultRWConcern]),
    ("getClusterParameter", On::Cluster, &[A::GetClusterParameter]),
    ("logApplicationMessage", On::Cluster, &[A::ApplicationMessage]),
    ("setClusterParameter", On::Cluster, &[A::SetClusterParameter]),
    ("setFeatureCompatibilityVersion", On::Cluster, &[A::SetFeatureCompatibilityVersion]),
    ("rotateCertificates", On::Cluster, &[A::RotateCertificates]),
    ("top", On::Cluster, &[A::Top]),
    ("shardingState", On::Cluster, &[A::ShardingState]),
    ("replSetGetStatus", On::Cluster, &[A::ReplSetGetStatus]),
    ("replSetGetConfig", On::Cluster, &[A::ReplSetGetConfig]),
    ("replSetResizeOplog", On::Cluster, &[A::ReplSetResizeOplog]),
    ("replSetFreeze", On::Cluster, &[A::ReplSetStateChange]),
    ("replSetMaintenance", On::Cluster, &[A::ReplSetStateChange]),
    ("replSetSyncFrom", On::Cluster, &[A::ReplSetStateChange]),
    ("replSetAbortPrimaryCatchUp", On::Cluster, &[A::ReplSetStateChange]),
    ("replSetStepDown", On::Cluster, &[A::ReplSetStateChange]),
    ("replSetReconfig", On::Cluster, &[A::ReplSetConfigure]),
    ("replSetInitiate", On::Cluster, &[A::ReplSetConfigure]),
    ("listShards", On::Cluster, &[A::ListShards]),
    ("flushRouterConfig", On::Cluster, &[A::FlushRouterConfig]),
    ("appendOplogNote", On::Cluster, &[A::AppendOplogNote]),
    ("addShard", On::Cluster, &[A::AddShard]),
    ("removeShard", On::Cluster, &[A::RemoveShard]),
    ("startShardDraining", On::Cluster, &[A::RemoveShard]),
    ("stopShardDraining", On::Cluster, &[A::RemoveShard]),
    ("commitShardRemoval", On::Cluster, &[A::RemoveShard]),
    ("shardDrainingStatus", On::Cluster, &[A::ShardDrainingStatus]),
    ("getShardMap", On::Cluster, &[A::GetShardMap]),
    ("transitionToDedicatedConfigServer", On::Cluster, &[A::TransitionToDedicatedConfigServer]),
    (
        "startTransitionToDedicatedConfigServer",
        On::Cluster,
        &[A::TransitionToDedicatedConfigServer],
    ),
    ("stopTransitionToDedicatedConfigServer", On::Cluster, &[A::TransitionToDedicatedConfigServer]),
    (
        "commitTransitionToDedicatedConfigServer",
        On::Cluster,
        &[A::TransitionToDedicatedConfigServer],
    ),
    ("transitionFromDedicatedConfigServer", On::Cluster, &[A::TransitionFromDedicatedConfigServer]),
    (
        "getTransitionToDedicatedConfigServerStatus",
        On::Cluster,
        &[A::GetTransitionToDedicatedConfigServerStatus],
    ),
    ("shutdown", On::Cluster, &[A::Shutdown]),
    ("killOp", On::Cluster, &[A::Killop]),
    ("currentOp", On::Cluster, &[A::Inprog]),
    ("killAllSessions", On::Cluster, &[A::KillAnySession]),
    ("killAllSessionsByPattern", On::Cluster, &[A::KillAnySession]),
    ("lockInfo", On::Cluster, &[A::ServerStatus]),
    ("setQuerySettings", On::Cluster, &[A::QuerySettings]),
    ("removeQuerySettings", On::Cluster, &[A::QuerySettings]),
    ("getTrafficRecordingStatus", On::Cluster, &[A::TrafficRecord]),
    ("startTrafficRecording", On::Cluster, &[A::TrafficRecord]),
    ("stopTrafficRecording", On::Cluster, &[A::TrafficRecord]),
    ("autoCompact", On::Cluster, &[A::Compact]),
    ("getQueryableEncryptionCountInfo", On::Cluster, &[A::Internal]),
    ("untrackUnshardedCollection", On::Cluster, &[A::Internal]),
    ("coordinateCommitTransaction", On::Cluster, &[A::Internal]),
    ("autoSplitVector", On::Cluster, &[A::SplitVector]),
];

/// How a command whose needs depend on its fields is checked: `$db`, the command, who runs it.
type Rule = fn(&str, &Document, &Subject<'_>) -> Check;

/// The commands decided from their fields.
const SPECIAL: &[(&str, Rule)] = &[
    ("insert", |db, cmd, _| insert(db, cmd)),
    ("update", |db, cmd, _| update(db, cmd)),
    ("findAndModify", |db, cmd, _| find_and_modify(db, cmd)),
    ("bulkWrite", |db, cmd, _| bulk_write(db, cmd)),
    ("getMore", |db, cmd, _| get_more(db, cmd)),
    ("killCursors", |db, cmd, _| kill_cursors(db, cmd)),
    ("aggregate", |db, cmd, _| aggregate(db, cmd)),
    ("explain", explain),
    ("applyOps", apply_ops),
    ("mapReduce", |db, cmd, _| map_reduce(db, cmd)),
    ("create", |db, cmd, _| create(db, cmd)),
    ("renameCollection", |db, cmd, _| rename(db, cmd)),
    ("listDatabases", |_, cmd, _| list_databases(cmd)),
    ("listCollections", |db, cmd, _| list_collections(db, cmd)),
    ("profile", |db, cmd, _| profile(db, cmd)),
    ("cloneCollectionAsCapped", |db, cmd, _| clone_capped(db, cmd)),
    ("checkMetadataConsistency", |db, cmd, _| check_metadata(db, cmd)),
    ("getDiagnosticData", |_, _, _| {
        diagnostics(&[A::ServerStatus, A::ReplSetGetStatus, A::ConnPoolStats])
    }),
    ("rotateFTDC", |_, _, _| diagnostics(&[A::ServerStatus, A::ReplSetGetStatus])),
    ("addShardToZone", |_, _, _| {
        zones(needs(&Target::database(crate::authz::CONFIG_DB), &[A::Update]))
    }),
    ("removeShardFromZone", |_, _, _| zones(needs(&Target::Any, &[A::Find, A::Update]))),
    ("updateZoneKeyRange", |_, _, _| zones(needs(&Target::Any, &[A::Find, A::Update, A::Remove]))),
    ("createUser", |db, cmd, _| authz_users::create(db, cmd, A::CreateUser)),
    ("createRole", |db, cmd, _| authz_users::create(db, cmd, A::CreateRole)),
    ("updateRole", |db, cmd, _| authz_users::update_role(db, cmd)),
    ("updateUser", authz_users::update_user),
    ("usersInfo", authz_users::users_info),
    ("_mergeAuthzCollections", |_, cmd, _| authz_users::merge_authz_collections(cmd)),
    ("rolesInfo", authz_users::roles_info),
    ("grantRolesToUser", |db, cmd, _| authz_users::on_roles(db, cmd, A::GrantRole)),
    ("grantRolesToRole", |db, cmd, _| authz_users::on_roles(db, cmd, A::GrantRole)),
    ("revokeRolesFromUser", |db, cmd, _| authz_users::on_roles(db, cmd, A::RevokeRole)),
    ("revokeRolesFromRole", |db, cmd, _| authz_users::on_roles(db, cmd, A::RevokeRole)),
    ("grantPrivilegesToRole", |_, cmd, _| authz_users::on_privileges(cmd, A::GrantRole)),
    ("revokePrivilegesFromRole", |_, cmd, _| authz_users::on_privileges(cmd, A::RevokeRole)),
];

/// Whether the table has a row for command `name`.
pub fn has_rule(name: &str) -> bool {
    SPECIAL.iter().any(|(n, _)| *n == name) || ROWS.iter().any(|r| r.0 == name)
}

/// What command `cmd` (named `name`, run on `$db` `db`) needs from `who`.
pub fn check(name: &str, db: &str, cmd: &Document, who: &Subject<'_>) -> Check {
    if let Some((_, rule)) = SPECIAL.iter().find(|(n, _)| *n == name) {
        return rule(db, cmd, who);
    }
    let Some(&(_, on, actions)) = ROWS.iter().find(|r| r.0 == name) else {
        return all(&Target::Any, &[A::AnyAction]);
    };
    let target = match on {
        On::Open => return Check::Open,
        On::Cluster => Target::Cluster,
        On::Db => Target::database(db),
        On::NamedDb => match first(cmd) {
            Value::String(d) => Target::database(d),
            _ => Target::database(db),
        },
        On::Coll => coll(db, cmd),
        On::FullNs => full_ns(db, first(cmd)),
        On::AnyNormal => Target::AnyNormal,
        On::Config => Target::database(crate::authz::CONFIG_DB),
        On::Admin => Target::database(ADMIN_DB),
    };
    all(&target, actions)
}

fn all(target: &Target, actions: &[A]) -> Check {
    Check::Needs(vec![Requirement::all(target, actions)])
}

fn first(cmd: &Document) -> &Value {
    cmd.iter().next().map_or(&Value::Null, |(_, v)| v)
}

/// Whether field `f` is set to a true value, as RefDb reads `upsert` and the like.
fn truthy(cmd: &Document, f: &str) -> bool {
    match cmd.get(f) {
        Some(Value::Boolean(b)) => *b,
        Some(Value::Int32(n)) => *n != 0,
        Some(Value::Int64(n)) => *n != 0,
        Some(Value::Double(x)) => *x != 0.0,
        _ => false,
    }
}

fn text<'a>(cmd: &'a Document, f: &str) -> Option<&'a str> {
    match cmd.get(f) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

fn docs<'a>(cmd: &'a Document, f: &str) -> Vec<&'a Document> {
    match cmd.get(f) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| match v {
                Value::Document(d) => Some(d),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Collection `<db>.<first field>`; with a first field that is not a name, the database (the
/// command fails on its own fields later).
fn coll(db: &str, cmd: &Document) -> Target {
    match first(cmd) {
        Value::String(c) => Target::collection(db, c),
        _ => Target::database(db),
    }
}

fn full_ns(db: &str, v: &Value) -> Target {
    match v {
        Value::String(s) => Target::namespace(s).unwrap_or_else(|| Target::database(db)),
        _ => Target::database(db),
    }
}

fn get_more(db: &str, cmd: &Document) -> Check {
    let id = match first(cmd) {
        Value::Int64(n) => *n,
        Value::Int32(n) => i64::from(*n),
        _ => 0,
    };
    let target = match text(cmd, "collection") {
        Some(c) => Target::collection(db, c),
        None => Target::database(db),
    };
    Check::GetMore { target, id }
}

fn kill_cursors(db: &str, cmd: &Document) -> Check {
    let ids = match cmd.get("cursors") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| match v {
                Value::Int64(n) => Some(*n),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Check::KillCursors { target: coll(db, cmd), ids }
}

/// What the explained command needs.
fn explain(db: &str, cmd: &Document, who: &Subject<'_>) -> Check {
    match cmd.get("explain") {
        Some(Value::Document(inner)) => match inner.iter().next() {
            Some((n, _)) => check(n, db, inner, who),
            None => Check::Open,
        },
        _ => Check::Open,
    }
}

/// `applyOps` (018 FR-011), as measured on RefDb 9.0.4: a command whose `applyOps` is not an
/// array of documents with `op` needs nothing (it fails on its own); an empty array needs
/// everything; otherwise `applyOps` on the cluster and, entry by entry until the first one that
/// does not parse (the command fails there), what the entry does.
fn apply_ops(db: &str, cmd: &Document, who: &Subject<'_>) -> Check {
    let Some(Value::Array(entries)) = cmd.get("applyOps") else { return Check::Open };
    let docs: Option<Vec<&Document>> = entries
        .iter()
        .map(|e| match e {
            Value::Document(d) if d.get("op").is_some() => Some(d),
            _ => None,
        })
        .collect();
    let Some(docs) = docs else { return Check::Open };
    if docs.is_empty() {
        return all(&Target::Any, &[A::AnyAction]);
    }
    let mut out = vec![Requirement::all(&Target::Cluster, &[A::ApplyOps])];
    for entry in docs {
        let Some(needs) = entry_needs(db, entry, who) else { break };
        out.extend(needs);
    }
    Check::Needs(out)
}

/// What one `applyOps` entry needs; `None` if it does not parse.
fn entry_needs(db: &str, entry: &Document, who: &Subject<'_>) -> Option<Vec<Requirement>> {
    let (Some(op), Some(ns)) = (text(entry, "op"), text(entry, "ns")) else { return None };
    let Some(Value::Document(o)) = entry.get("o") else { return None };
    // The namespace of the entry; without a dot, the database's unnamed collection.
    let (ns_db, ns_coll) = ns.split_once('.').unwrap_or((ns, ""));
    let target = Target::collection(ns_db, ns_coll);
    let write = |action: A| {
        let mut needs = vec![Requirement::all(&target, &[action])];
        if entry.get("ui").is_some() {
            needs.push(Requirement::all(&Target::Cluster, &[A::UseUUID]));
        }
        Some(needs)
    };
    match op {
        "i" => write(A::Insert),
        "u" => write(A::Update),
        "d" => write(A::Remove),
        "n" => Some(vec![Requirement::all(&Target::Cluster, &[A::AppendOplogNote])]),
        "c" => {
            let (name, _) = o.iter().next()?;
            let inner_db = if ns_db.is_empty() { db } else { ns_db };
            Some(match check(name, inner_db, o, who) {
                Check::Open => Vec::new(),
                Check::Needs(needs) => needs,
                // A cursor command does not apply.
                Check::InDatabase(_) | Check::GetMore { .. } | Check::KillCursors { .. } => {
                    vec![Requirement::all(&Target::Any, &[A::AnyAction])]
                }
            })
        }
        _ => None,
    }
}

fn list_databases(cmd: &Document) -> Check {
    match cmd.get("authorizedDatabases") {
        Some(Value::Boolean(false)) => all(&Target::Cluster, &[A::ListDatabases]),
        _ => Check::Open,
    }
}

/// `listCollections`: with `nameOnly` and `authorizedCollections`, any privilege in the database.
fn list_collections(db: &str, cmd: &Document) -> Check {
    if truthy(cmd, "nameOnly") && truthy(cmd, "authorizedCollections") {
        return Check::InDatabase(db.to_owned());
    }
    all(&Target::database(db), &[A::ListCollections])
}

fn clone_capped(db: &str, cmd: &Document) -> Check {
    let to = Target::collection(db, text(cmd, "toCollection").unwrap_or_default());
    Check::Needs(vec![
        Requirement::all(&coll(db, cmd), &[A::Find]),
        Requirement::all(&to, &[A::Insert, A::CreateIndex, A::ConvertToCapped]),
    ])
}

fn check_metadata(db: &str, cmd: &Document) -> Check {
    match first(cmd) {
        Value::String(c) => all(&Target::collection(db, c), &[A::CheckMetadataConsistency]),
        _ if db == ADMIN_DB => all(&Target::Cluster, &[A::CheckMetadataConsistency]),
        _ => Check::Needs(vec![Requirement::either(
            needs(&Target::Cluster, &[A::CheckMetadataConsistency]),
            needs(&Target::database(db), &[A::CheckMetadataConsistency]),
        )]),
    }
}

/// `getDiagnosticData` and `rotateFTDC`: `cluster` on the cluster and `collStats` on `local`.
fn diagnostics(cluster: &[A]) -> Check {
    Check::Needs(vec![
        Requirement::all(&Target::Cluster, cluster),
        Requirement::all(&Target::database(crate::authz::LOCAL_DB), &[A::CollStats]),
    ])
}

/// The zone commands: `enableSharding` on the cluster, or the writes to `config` of `other`.
fn zones(other: Vec<Need>) -> Check {
    Check::Needs(vec![Requirement::either(needs(&Target::Cluster, &[A::EnableSharding]), other)])
}

/// The needs of a write on `targets`, with `bypassDocumentValidation` on each when asked.
fn written(cmd: &Document, targets: Vec<(Target, Vec<A>)>) -> Check {
    let bypass = truthy(cmd, "bypassDocumentValidation");
    let mut out: Vec<Need> = Vec::new();
    for (target, actions) in targets {
        out.extend(needs(&target, &actions));
        if bypass {
            out.push(Need::new(target, A::BypassDocumentValidation));
        }
    }
    out.sort();
    out.dedup();
    Check::Needs(vec![Requirement { options: vec![out] }])
}

fn insert(db: &str, cmd: &Document) -> Check {
    written(cmd, vec![(coll(db, cmd), vec![A::Insert])])
}

fn update(db: &str, cmd: &Document) -> Check {
    let upsert = docs(cmd, "updates").iter().any(|u| truthy(u, "upsert"));
    let acts = if upsert { vec![A::Update, A::Insert] } else { vec![A::Update] };
    written(cmd, vec![(coll(db, cmd), acts)])
}

fn find_and_modify(db: &str, cmd: &Document) -> Check {
    let change = if truthy(cmd, "remove") { A::Remove } else { A::Update };
    let mut acts = vec![A::Find, change];
    if truthy(cmd, "upsert") {
        acts.push(A::Insert);
    }
    written(cmd, vec![(coll(db, cmd), acts)])
}

/// `bulkWrite`: each operation on the namespace of `nsInfo` it points to.
fn bulk_write(db: &str, cmd: &Document) -> Check {
    let spaces: Vec<Target> = docs(cmd, "nsInfo")
        .iter()
        .map(|d| full_ns(db, d.get("ns").unwrap_or(&Value::Null)))
        .collect();
    let mut targets = Vec::new();
    for op in docs(cmd, "ops") {
        let Some((kind, index)) = op.iter().next() else { continue };
        let Some(target) = index_of(index).and_then(|i| spaces.get(i)) else { continue };
        let acts = match kind {
            "insert" => vec![A::Insert],
            "delete" => vec![A::Remove],
            _ if truthy(op, "upsert") => vec![A::Update, A::Insert],
            _ => vec![A::Update],
        };
        targets.push((target.clone(), acts));
    }
    written(cmd, targets)
}

fn index_of(v: &Value) -> Option<usize> {
    match v {
        Value::Int32(n) => usize::try_from(*n).ok(),
        Value::Int64(n) => usize::try_from(*n).ok(),
        _ => None,
    }
}

/// The first stages that replace the `find` of an `aggregate`, with what they need.
fn first_stage(db: &str, cmd: &Document, stage: &str, spec: &Value) -> Option<Vec<Need>> {
    let on_coll = matches!(first(cmd), Value::String(_));
    let source = coll(db, cmd);
    let all_users = matches!(spec, Value::Document(d) if truthy(d, "allUsers"));
    Some(match stage {
        "$collStats" => needs(&source, &[A::CollStats]),
        "$indexStats" => needs(&source, &[A::IndexStats]),
        "$planCacheStats" => needs(&source, &[A::PlanCacheRead]),
        "$listSearchIndexes" => needs(&Target::database(db), &[A::ListSearchIndexes]),
        "$changeStream" => {
            let cluster = matches!(spec, Value::Document(d) if truthy(d, "allChangesForCluster"));
            let target = match (cluster, on_coll) {
                (true, _) => Target::AnyNormal,
                (false, true) => source,
                (false, false) => Target::database(db),
            };
            needs(&target, &[A::ChangeStream, A::Find])
        }
        "$currentOp" => needs(&Target::Cluster, &[A::Inprog]),
        "$listSessions" | "$listLocalSessions" if all_users => {
            needs(&Target::Cluster, &[A::ListSessions])
        }
        "$listSessions" | "$listLocalSessions" | "$documents" => Vec::new(),
        "$shardedDataDistribution" => needs(&Target::Cluster, &[A::ShardedDataDistribution]),
        "$listClusterCatalog" => needs(&Target::Cluster, &[A::ListClusterCatalog]),
        "$querySettings" => needs(&Target::Cluster, &[A::QuerySettings]),
        "$listCatalog" => {
            needs(&Target::Any, &[A::ListDatabases, A::ListCollections, A::ListIndexes])
        }
        _ => return None,
    })
}

fn stages(v: Option<&Value>) -> Vec<(&str, &Value)> {
    let Some(Value::Array(a)) = v else { return Vec::new() };
    a.iter()
        .filter_map(|s| match s {
            Value::Document(d) => d.iter().next(),
            _ => None,
        })
        .collect()
}

fn aggregate(db: &str, cmd: &Document) -> Check {
    let pipeline = stages(cmd.get("pipeline"));
    let bypass = truthy(cmd, "bypassDocumentValidation");
    let mut out = match pipeline.first().and_then(|(s, spec)| first_stage(db, cmd, s, spec)) {
        Some(n) => n,
        None if matches!(first(cmd), Value::String(_)) => needs(&coll(db, cmd), &[A::Find]),
        None => Vec::new(),
    };
    walk(db, &pipeline, bypass, &mut out);
    out.sort();
    out.dedup();
    Check::Needs(vec![Requirement { options: vec![out] }])
}

/// The collection a stage names: a string, or `{db, coll}`.
fn named(db: &str, v: Option<&Value>) -> Option<Target> {
    match v? {
        Value::String(c) => Some(Target::collection(db, c)),
        Value::Document(d) => {
            let c = text(d, "coll")?;
            Some(Target::collection(text(d, "db").unwrap_or(db), c))
        }
        _ => None,
    }
}

/// What the stages of `pipeline` read and write, nested pipelines included.
fn walk(db: &str, pipeline: &[(&str, &Value)], bypass: bool, out: &mut Vec<Need>) {
    for (stage, spec) in pipeline {
        let d = match spec {
            Value::Document(d) => Some(d),
            _ => None,
        };
        match *stage {
            "$lookup" | "$graphLookup" | "$unionWith" => {
                let from = match (*stage, spec) {
                    ("$unionWith", Value::String(_)) => named(db, Some(spec)),
                    ("$unionWith", _) => named(db, d.and_then(|d| d.get("coll"))),
                    _ => named(db, d.and_then(|d| d.get("from"))),
                };
                if let Some(t) = from {
                    out.extend(needs(&t, &[A::Find]));
                }
                walk(db, &stages(d.and_then(|d| d.get("pipeline"))), bypass, out);
            }
            "$facet" => {
                for (_, sub) in d.map(|d| d.iter().collect::<Vec<_>>()).unwrap_or_default() {
                    walk(db, &stages(Some(sub)), bypass, out);
                }
            }
            "$out" | "$merge" => {
                let target = match (*stage, spec) {
                    (_, Value::String(_)) => named(db, Some(spec)),
                    ("$out", _) => named(db, Some(spec)),
                    _ => named(db, d.and_then(|d| d.get("into"))),
                };
                let Some(t) = target else { continue };
                let write = if *stage == "$out" { A::Remove } else { A::Update };
                out.extend(needs(&t, &[A::Insert, write]));
                if bypass {
                    out.push(Need::new(t, A::BypassDocumentValidation));
                }
            }
            _ => {}
        }
    }
}

fn map_reduce(db: &str, cmd: &Document) -> Check {
    let mut out = needs(&coll(db, cmd), &[A::Find]);
    let target = match cmd.get("out") {
        Some(Value::String(c)) => Some(Target::collection(db, c)),
        Some(Value::Document(d)) => ["replace", "merge", "reduce"]
            .iter()
            .find_map(|k| text(d, k))
            .map(|c| Target::collection(text(d, "db").unwrap_or(db), c)),
        _ => None,
    };
    if let Some(t) = target {
        out.extend(needs(&t, &[A::Insert, A::Remove]));
    }
    Check::Needs(vec![Requirement { options: vec![out] }])
}

fn create(db: &str, cmd: &Document) -> Check {
    let target = coll(db, cmd);
    if cmd.get("viewOn").is_some() {
        return all(&target, &[A::CreateCollection]);
    }
    if truthy(cmd, "capped") {
        return Check::Needs(vec![Requirement::either(
            needs(&target, &[A::CreateCollection, A::ConvertToCapped]),
            needs(&target, &[A::Insert, A::ConvertToCapped]),
        )]);
    }
    Check::Needs(vec![Requirement::either(
        needs(&target, &[A::CreateCollection]),
        needs(&target, &[A::Insert]),
    )])
}

fn rename(db: &str, cmd: &Document) -> Check {
    let from = full_ns(db, first(cmd));
    let to = full_ns(db, cmd.get("to").unwrap_or(&Value::Null));
    let mut copy = needs(&from, &[A::Find, A::DropCollection]);
    copy.extend(needs(&to, &[A::Insert, A::CreateIndex]));
    let mut reqs = match (&from, &to) {
        (Target::Collection(a, _), Target::Collection(b, _)) if a == b => {
            vec![Requirement::either(
                needs(&Target::database(a), &[A::RenameCollectionSameDB]),
                copy,
            )]
        }
        _ => vec![Requirement { options: vec![copy] }],
    };
    if truthy(cmd, "dropTarget") {
        reqs.push(Requirement::all(&to, &[A::DropCollection]));
    }
    Check::Needs(reqs)
}

fn profile(db: &str, cmd: &Document) -> Check {
    let read_only = match first(cmd) {
        Value::Int32(n) => *n == -1,
        Value::Int64(n) => *n == -1,
        Value::Double(x) => *x == -1.0,
        _ => false,
    };
    let enable = needs(&Target::database(db), &[A::EnableProfiler]);
    if read_only {
        let read = needs(&Target::collection(db, "system.profile"), &[A::Find]);
        return Check::Needs(vec![Requirement::either(enable, read)]);
    }
    Check::Needs(vec![Requirement { options: vec![enable] }])
}
