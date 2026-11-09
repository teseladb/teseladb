//! Database and collection names (FR-026), with the messages a `refrouter` 9.0.4 gives (measured).
//! Some faults fail the whole command and others only the writes to that namespace, which get
//! them as `writeErrors`.

use teseladb_meta::catalog::Namespace;

use crate::error::{BAD_VALUE, CommandError, ILLEGAL_OPERATION, INVALID_NAMESPACE};

/// Longest database name, in bytes.
pub const MAX_DB_NAME: usize = 63;
/// Longest `db.collection`, in bytes.
pub const MAX_NAMESPACE: usize = 255;
/// Characters a database name cannot have (Unix rules, as RefDb on Linux).
pub const DB_FORBIDDEN: [char; 5] = ['/', '\\', ' ', '"', '\0'];
/// Prefix of the collections RefDb reserves.
const SYSTEM_PREFIX: &str = "system.";

/// Faults of `db.coll` that fail the whole command.
pub fn check(ns: &Namespace) -> Result<(), CommandError> {
    if ns.db.contains('\0') || ns.coll.contains('\0') {
        let msg = "namespaces cannot have embedded null characters";
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    // The name is written only for an error: every command checks its namespace.
    let invalid = || {
        let shown = match ns.coll.is_empty() {
            true => ns.db.clone(),
            false => format!("{}.{}", ns.db, ns.coll),
        };
        CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified: {shown}"))
    };
    if ns.db.contains('.') {
        let msg = format!("'.' is an invalid character in a db name: {}", ns.db);
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    if ns.db.len() > MAX_DB_NAME {
        let msg =
            format!("db name must be at most {MAX_DB_NAME} characters, found: {}", ns.db.len());
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    if ns.db.is_empty() || ns.coll.is_empty() || ns.db.contains(DB_FORBIDDEN) {
        return Err(invalid());
    }
    if ns.db.contains('$') {
        let msg = format!(
            "Database {} could not be created :: caused by :: Invalid db name specified: {}",
            ns.db, ns.db
        );
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    Ok(())
}

/// Faults of `db.coll` that fail `create`: those of the writes, but a client may create
/// `system.profile` to size the profiler's collection (measured on a `refrouter` 9.0.4).
pub fn check_creatable(ns: &Namespace) -> Result<(), CommandError> {
    check_name(ns, ns.coll != SYSTEM_PROFILE)
}

/// Faults of `db.coll` that fail the writes to it (and the commands that create it).
pub fn check_writable(ns: &Namespace) -> Result<(), CommandError> {
    check_name(ns, true)
}

/// The collection of a database's stored JavaScript functions, which clients write (016,
/// measured on a `refrouter` 9.0.4 on 02-11).
pub const SYSTEM_JS: &str = "system.js";

/// Faults of `db.coll`'s name; a `system.` collection other than `system.js` and the
/// `system.resharding.` ones is one when `system` says so.
fn check_name(ns: &Namespace, system: bool) -> Result<(), CommandError> {
    // The full name is written only for an error: every write checks its namespace.
    let full = || format!("{}.{}", ns.db, ns.coll);
    if ns.coll.contains('$') {
        let msg = format!("Invalid collection name: {}", ns.coll);
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    // A client may name a collection as a resharding's copy (measured on a `refrouter` 9.0.4,
    // 03-11-2026): it is no copy of any resharding, and its documents cannot be written.
    // `config.system.sessions` holds the cluster's record of sessions (006 FR-016), so it is no
    // longer refused (DV-042): its TTL index is created as in RefDb (FR-038).
    let sessions =
        ns.db == super::registry::SESSIONS_DB && ns.coll == super::registry::SESSIONS_COLL;
    let allowed =
        ns.coll == SYSTEM_JS || ns.coll.starts_with(super::reshard::TEMP_PREFIX) || sessions;
    if system && ns.coll.starts_with(SYSTEM_PREFIX) && !allowed {
        return Err(CommandError::new(
            INVALID_NAMESPACE,
            format!("Invalid system namespace: {}", full()),
        ));
    }
    if ns.db.len() + 1 + ns.coll.len() > MAX_NAMESPACE {
        let msg = format!(
            "Fully qualified namespace is too long. Namespace: {} Max: {MAX_NAMESPACE}",
            full()
        );
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    if ns.coll.contains(".system.") {
        return Err(CommandError::new(BAD_VALUE, format!("Invalid namespace: {}", full())));
    }
    if super::config_db::generated(ns) {
        return Err(super::config_db::not_writable(ns));
    }
    if super::oplog::is_oplog(ns) {
        return Err(super::oplog::write_refused());
    }
    if super::local_db::generated(ns) {
        return Err(super::local_db::not_writable(ns));
    }
    Ok(())
}

/// The collections no command writes (`system.views`, `system.profile`): RefDb's refusal, as a
/// `refrouter` relays it from the shard at `host` (measured on 9.0.4).
pub fn cannot_write(ns: &Namespace, host: &str) -> Option<CommandError> {
    [super::kinds::SYSTEM_VIEWS, SYSTEM_PROFILE].contains(&ns.coll.as_str()).then(|| {
        CommandError::new(
            INVALID_NAMESPACE,
            format!(
                "cluster write results unavailable from {host} :: caused by :: cannot write to {}.{}",
                ns.db, ns.coll
            ),
        )
    })
}

/// The profiler's collection.
const SYSTEM_PROFILE: &str = "system.profile";

/// `db.coll` from a command's database and collection name.
pub fn ns(db: &str, coll: &str) -> Namespace {
    Namespace { db: db.to_owned(), coll: coll.to_owned() }
}

/// `db.coll` read from a full name (`renameCollection`'s fields); `None` without a dot.
pub fn parse_full(full: &str) -> Option<Namespace> {
    let (db, coll) = full.split_once('.')?;
    Some(ns(db, coll))
}

/// The text `db.coll`.
pub fn full(ns: &Namespace) -> String {
    format!("{}.{}", ns.db, ns.coll)
}

/// The database a `refrouter` does not let clients use.
pub const LOCAL_DB: &str = "local";
/// The databases whose collections a `refrouter` does not let clients drop.
const UNDROPPABLE_DBS: [&str; 2] = ["admin", "config"];

/// `IllegalOperation` for dropping a collection of `admin` or `config`.
pub fn check_droppable(ns: &Namespace) -> Result<(), CommandError> {
    match UNDROPPABLE_DBS.contains(&ns.db.as_str()) {
        true => Err(CommandError::new(
            ILLEGAL_OPERATION,
            format!("Cannot drop collection in '{}' database in sharded cluster", ns.db),
        )),
        false => Ok(()),
    }
}

/// `IllegalOperation` for dropping the `admin` or `config` database.
pub fn check_db_droppable(db: &str) -> Result<(), CommandError> {
    match UNDROPPABLE_DBS.contains(&db) {
        true => Err(CommandError::new(ILLEGAL_OPERATION, format!("Cannot drop the {db} database"))),
        false => Ok(()),
    }
}
