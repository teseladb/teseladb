//! Authorization (013 FR-011): what a privilege's resource covers, what each command needs and
//! whether a set of privileges gives it. The rules are those measured on the reference server 9.0.4
//! (`contracts/privileges.md`, with the recorded answers of the reference server); the
//! table of commands is in [`crate::authz_rules`].

use crate::actions::ActionType;
use crate::privileges::{Privilege, Resource};

/// Prefix of the system collections, which a database-wide privilege does not cover.
pub const SYSTEM_PREFIX: &str = "system.";
/// Prefix of the bucket collections of time series.
pub const BUCKETS_PREFIX: &str = "system.buckets.";
/// `config`, outside "any database".
pub const CONFIG_DB: &str = "config";
/// `local`, outside "any database".
pub const LOCAL_DB: &str = "local";

/// What a command acts on.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    /// The cluster.
    Cluster,
    /// Any resource: only `{anyResource: true}` covers it.
    Any,
    /// Every normal database at once, the pattern `{db: "", collection: ""}`.
    AnyNormal,
    /// A database.
    Database(String),
    /// A collection: database and name.
    Collection(String, String),
}

impl Target {
    /// Collection `coll` of `db`.
    pub fn collection(db: &str, coll: &str) -> Target {
        Target::Collection(db.to_owned(), coll.to_owned())
    }

    /// Database `db`.
    pub fn database(db: &str) -> Target {
        Target::Database(db.to_owned())
    }

    /// The collection of a full namespace `db.coll`; `None` without a dot or with an empty part.
    pub fn namespace(full: &str) -> Option<Target> {
        let (db, coll) = full.split_once('.')?;
        (!db.is_empty() && !coll.is_empty()).then(|| Target::collection(db, coll))
    }
}

fn normal_db(db: &str) -> bool {
    db != CONFIG_DB && db != LOCAL_DB
}

fn system(coll: &str) -> bool {
    coll.starts_with(SYSTEM_PREFIX)
}

impl Resource {
    /// Whether a privilege on this resource applies to `t`.
    pub fn covers(&self, t: &Target) -> bool {
        match self {
            Resource::Any => true,
            Resource::Cluster => *t == Target::Cluster,
            Resource::Ns { db, collection } => ns_covers(db, collection, t),
            Resource::SystemBuckets { db, bucket } => match t {
                Target::Collection(d, c) => {
                    db.as_ref().is_none_or(|x| x == d)
                        && c.strip_prefix(BUCKETS_PREFIX) == Some(bucket.as_str())
                }
                _ => false,
            },
        }
    }

    /// Whether a privilege on this resource touches database `db` (a privilege on the database,
    /// on one of its collections or on every database): what `listDatabases` and
    /// `listCollections` with `authorizedCollections` look at.
    pub fn touches(&self, db: &str) -> bool {
        match self {
            Resource::Any => true,
            Resource::Cluster => false,
            Resource::Ns { db: d, collection } if d.is_empty() => {
                !collection.is_empty() || normal_db(db)
            }
            Resource::Ns { db: d, .. } => d == db,
            Resource::SystemBuckets { db: d, .. } => d.as_ref().is_none_or(|x| x == db),
        }
    }
}

fn ns_covers(db: &str, coll: &str, t: &Target) -> bool {
    match (db.is_empty(), coll.is_empty(), t) {
        (true, true, Target::AnyNormal) => true,
        (true, true, Target::Database(d)) => normal_db(d),
        (true, true, Target::Collection(d, c)) => normal_db(d) && !system(c),
        (true, false, Target::Collection(_, c)) => c == coll,
        (false, true, Target::Database(d)) => d == db,
        (false, true, Target::Collection(d, c)) => d == db && !system(c),
        (false, false, Target::Collection(d, c)) => d == db && c == coll,
        _ => false,
    }
}

/// What a privilege on `r` asks of another set of privileges: the target it names.
fn target_of(r: &Resource) -> Target {
    match r {
        Resource::Cluster => Target::Cluster,
        Resource::Any => Target::Any,
        Resource::Ns { db, collection } => match (db.is_empty(), collection.is_empty()) {
            (true, true) => Target::AnyNormal,
            (false, true) => Target::database(db),
            _ => Target::collection(db, collection),
        },
        Resource::SystemBuckets { db, bucket } => {
            Target::collection(db.as_deref().unwrap_or(""), &format!("{BUCKETS_PREFIX}{bucket}"))
        }
    }
}

/// Whether `privileges` give every action of `required` on its resource: what the
/// administration port asks of a user, with the privileges of `clusterMonitor` for `/metrics`
/// and those of `clusterAdmin` for the commands (013 FR-017).
pub fn holds_all(privileges: &[Privilege], required: &[Privilege]) -> bool {
    required.iter().all(|p| {
        let target = target_of(&p.resource);
        p.actions.iter().all(|a| allows(privileges, &target, *a))
    })
}

/// Whether `privileges` allow `action` on `target`; `anyAction` allows every action.
pub fn allows(privileges: &[Privilege], target: &Target, action: ActionType) -> bool {
    privileges.iter().any(|p| {
        p.resource.covers(target)
            && (p.actions.contains(&action) || p.actions.contains(&ActionType::AnyAction))
    })
}

/// Whether `privileges` touch database `db` (see [`Resource::touches`]).
pub fn touches(privileges: &[Privilege], db: &str) -> bool {
    privileges.iter().any(|p| p.resource.touches(db))
}

/// An action on a target.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Need {
    /// What it acts on.
    pub target: Target,
    /// The action.
    pub action: ActionType,
}

impl Need {
    /// `action` on `target`.
    pub fn new(target: Target, action: ActionType) -> Need {
        Need { target, action }
    }
}

/// One condition of a command: one of its options, each a set of needs that must all hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// The options; the requirement holds if one of them holds whole.
    pub options: Vec<Vec<Need>>,
}

impl Requirement {
    /// Every action of `actions` on `target`.
    pub fn all(target: &Target, actions: &[ActionType]) -> Requirement {
        Requirement { options: vec![needs(target, actions)] }
    }

    /// Either of two sets of needs.
    pub fn either(a: Vec<Need>, b: Vec<Need>) -> Requirement {
        Requirement { options: vec![a, b] }
    }

    /// Whether `privileges` meet it.
    pub fn holds(&self, privileges: &[Privilege]) -> bool {
        self.options.iter().any(|o| o.iter().all(|n| allows(privileges, &n.target, n.action)))
    }
}

/// Every action of `actions` on `target`.
pub fn needs(target: &Target, actions: &[ActionType]) -> Vec<Need> {
    actions.iter().map(|a| Need::new(target.clone(), *a)).collect()
}

/// What a command needs before it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Nothing.
    Open,
    /// Every requirement.
    Needs(Vec<Requirement>),
    /// Some privilege that touches the database (`listCollections` with `nameOnly` and
    /// `authorizedCollections`).
    InDatabase(String),
    /// `getMore` of cursor `id` on `target`: the cursor is the user's, and `find` on `target`.
    GetMore {
        /// The cursor's collection, as the command names it.
        target: Target,
        /// The cursor.
        id: i64,
    },
    /// `killCursors`: nothing for the user's own cursors, `killAnyCursor` on `target` or on the
    /// cluster if one of `ids` belongs to someone else.
    KillCursors {
        /// The collection.
        target: Target,
        /// The cursors.
        ids: Vec<i64>,
    },
}

impl Check {
    /// Whether `privileges` pass it; for the cursor checks, given whether every cursor named is
    /// the user's.
    pub fn passes(&self, privileges: &[Privilege], own_cursors: bool) -> bool {
        match self {
            Check::Open => true,
            Check::Needs(reqs) => reqs.iter().all(|r| r.holds(privileges)),
            Check::InDatabase(db) => touches(privileges, db),
            Check::GetMore { target, .. } => allows(privileges, target, ActionType::Find),
            Check::KillCursors { target, .. } => {
                own_cursors
                    || allows(privileges, target, ActionType::KillAnyCursor)
                    || allows(privileges, &Target::Cluster, ActionType::KillAnyCursor)
            }
        }
    }
}
