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
const DB_FORBIDDEN: [char; 5] = ['/', '\\', ' ', '"', '\0'];
/// Prefix of the collections RefDb reserves.
const SYSTEM_PREFIX: &str = "system.";

/// Faults of `db.coll` that fail the whole command.
pub fn check(ns: &Namespace) -> Result<(), CommandError> {
    if ns.db.contains('\0') || ns.coll.contains('\0') {
        let msg = "namespaces cannot have embedded null characters";
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    let full = format!("{}.{}", ns.db, ns.coll);
    let shown = if ns.coll.is_empty() { ns.db.clone() } else { full.clone() };
    let invalid =
        || CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified: {shown}"));
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

/// Faults of `db.coll` that fail the writes to it (and the commands that create it).
pub fn check_writable(ns: &Namespace) -> Result<(), CommandError> {
    let full = format!("{}.{}", ns.db, ns.coll);
    if ns.coll.contains('$') {
        let msg = format!("Invalid collection name: {}", ns.coll);
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    if ns.coll.starts_with(SYSTEM_PREFIX) {
        return Err(CommandError::new(
            INVALID_NAMESPACE,
            format!("Invalid system namespace: {full}"),
        ));
    }
    if full.len() > MAX_NAMESPACE {
        let msg = format!(
            "Fully qualified namespace is too long. Namespace: {full} Max: {MAX_NAMESPACE}"
        );
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    if ns.coll.contains(".system.") {
        return Err(CommandError::new(BAD_VALUE, format!("Invalid namespace: {full}")));
    }
    Ok(())
}

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

/// `IllegalOperation` for any command on the `local` database, as through a `refrouter`.
pub fn check_db_reachable(db: &str) -> Result<(), CommandError> {
    match db == LOCAL_DB {
        true => {
            Err(CommandError::new(ILLEGAL_OPERATION, "Can't use 'local' database through refrouter"))
        }
        false => Ok(()),
    }
}

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
