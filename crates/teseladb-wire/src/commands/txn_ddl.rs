//! The collections a client transaction creates (008 FR-014, T019), as the reference router 9.0.4
//! lets a transaction create them (measured on 31-10-2026): `create`, an insert into a collection
//! that does not exist, and `createIndexes` on a collection the transaction created while it is
//! still empty. The collection is created in Group 0 under a name of its own, which no command of a
//! client sees ([`is_scratch`]), and the transaction's statements see it under its own name. The
//! commit renames it to that name before the transaction's record commits; a name that another
//! client took in the meantime aborts the transaction with `WriteConflict`, as the reference server
//! does. An abort drops it, and the node's keeper drops those left by transactions that are no
//! longer running on the node (a restart ends them all).
//!
//! The database of the collection exists from its creation, also if the transaction aborts, as on
//! the reference router 9.0.4 (measured: `listDatabases` lists it before the commit and after the
//! abort).

use teseladb_bson::{Document, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_range::intents::TxnId;
use teseladb_range::read::ReadConcern;

use super::names::full;
use super::query::documents_of;
use super::{Backend, Shared, lock};
use crate::error::{
    CommandError, NAMESPACE_EXISTS, OPERATION_NOT_SUPPORTED_IN_TRANSACTION,
    TRANSIENT_TRANSACTION_ERROR, WRITE_CONFLICT,
};

/// The start of the name a transaction's new collection has until the commit.
pub const SCRATCH_PREFIX: &str = "system.teseladb.txn.";
/// `create`.
const CREATE: &str = "create";
/// `createIndexes`.
pub const CREATE_INDEXES: &str = "createIndexes";
/// The field of `createIndexes`' reply that says whether it created the collection.
pub const CREATED_AUTOMATICALLY: &str = "createdCollectionAutomatically";
/// The field of a reply that holds each shard's own reply.
const RAW: &str = "raw";
/// `errmsg` of a view created in a transaction (measured on 9.0.4).
pub const VIEW_IN_TRANSACTION: &str = "Cannot create a view in a multi-document transaction.";
/// `errmsg` of a time series created in a transaction (measured on 9.0.4).
pub const TIMESERIES_IN_TRANSACTION: &str =
    "Cannot create a time-series collection in a multi-document transaction.";

/// A collection the transaction created.
#[derive(Clone, Debug, PartialEq)]
pub struct Created {
    /// The name the client gave it.
    pub real: Namespace,
    /// Its name in Group 0 until the commit.
    pub scratch: Namespace,
    /// Its entry.
    pub entry: CollectionEntry,
}

/// Whether `ns` is the name of a collection a transaction created and has not committed.
pub fn is_scratch(ns: &Namespace) -> bool {
    ns.coll.starts_with(SCRATCH_PREFIX)
}

/// The name collection `real` of transaction `txn` of node `node` has until the commit: the node
/// first, so that its keeper knows its own, and the client's name last, which the audit log shows
/// (013).
fn scratch_name(real: &Namespace, node: u64, txn: TxnId) -> Namespace {
    let coll = format!("{SCRATCH_PREFIX}{node}.{:032x}.{}", txn.0, real.coll);
    Namespace { db: real.db.clone(), coll }
}

/// The node that created a transaction's collection, from its name.
fn creator(ns: &Namespace) -> Option<u64> {
    ns.coll.strip_prefix(SCRATCH_PREFIX)?.split('.').next()?.parse().ok()
}

/// The collection `ns` as the transaction sees it, if it created it.
pub fn find<'a>(created: &'a [Created], ns: &Namespace) -> Option<&'a Created> {
    created.iter().find(|c| &c.real == ns)
}

/// A catalog command of a statement of transaction `txn`: a creation of a collection that does
/// not exist yet makes it the transaction's own; everything else goes to `inner`.
pub fn catalog_write(
    inner: &mut dyn Backend,
    shared: &Shared,
    (txn, created): (TxnId, &mut Vec<Created>),
    cmd: CatalogCommand,
) -> Result<Option<CollectionEntry>, CommandError> {
    let CatalogCommand::Create { ns, uuid, options, if_absent } = cmd else {
        return inner.catalog_write(cmd);
    };
    if let Some(c) = find(created, &ns) {
        return match if_absent {
            true => Ok(Some(c.entry.clone())),
            false => Err(exists(&ns)),
        };
    }
    if inner.collection(&ns)?.is_some() {
        return inner.catalog_write(CatalogCommand::Create { ns, uuid, options, if_absent });
    }
    let scratch = scratch_name(&ns, inner.node(), txn);
    // Known to the keeper before Group 0 has it, so that it never takes it for one left behind.
    lock(shared).txn_scratch.insert(scratch.clone());
    let made = inner.catalog_write(CatalogCommand::Create {
        ns: scratch.clone(),
        uuid,
        options,
        if_absent: false,
    });
    let entry = match made {
        Ok(Some(entry)) => entry,
        failed => {
            lock(shared).txn_scratch.remove(&scratch);
            return Err(match failed {
                Err(e) => e,
                Ok(_) => {
                    CommandError::new(crate::error::INTERNAL_ERROR, "Group 0 created no collection")
                }
            });
        }
    };
    created.push(Created { real: ns, scratch, entry: entry.clone() });
    Ok(Some(entry))
}

/// The reference server's error for a collection a transaction creates that exists (measured on
/// 9.0.4).
fn exists(ns: &Namespace) -> CommandError {
    CommandError::new(NAMESPACE_EXISTS, format!("Collection {} already exists.", full(ns)))
}

fn refused(msg: impl Into<String>) -> CommandError {
    CommandError::new(OPERATION_NOT_SUPPORTED_IN_TRANSACTION, msg)
}

/// The rules of the reference server 9.0.4 for `create` and `createIndexes` in a transaction
/// (measured): no view, no time series, no collection that exists, and indexes only on an empty
/// collection the transaction created. `backend` reads as the transaction reads.
pub fn precheck(
    backend: &mut dyn Backend,
    created: &[Created],
    (name, db, cmd): (&str, &str, &Document),
) -> Result<(), CommandError> {
    let target = |field: &str| match cmd.get(field) {
        Some(Value::String(c)) => Some(Namespace { db: db.to_owned(), coll: c.clone() }),
        _ => None,
    };
    match name {
        CREATE => {
            if cmd.get("viewOn").is_some() {
                return Err(refused(VIEW_IN_TRANSACTION));
            }
            if cmd.get("timeseries").is_some() {
                return Err(refused(TIMESERIES_IN_TRANSACTION));
            }
            match target(CREATE) {
                Some(ns) if find(created, &ns).is_some() || backend.collection(&ns)?.is_some() => {
                    Err(exists(&ns))
                }
                Some(_) | None => Ok(()),
            }
        }
        CREATE_INDEXES => {
            let Some(ns) = target(CREATE_INDEXES) else { return Ok(()) };
            match find(created, &ns) {
                Some(c) => {
                    let (start, end) = documents_of(&c.entry);
                    if backend.scan(&start, &end, 1, ReadConcern::Local)?.is_empty() {
                        Ok(())
                    } else {
                        Err(refused(format!(
                            "Cannot create new indexes on non-empty collection {} in a multi-document transaction.",
                            full(&ns)
                        )))
                    }
                }
                None if backend.collection(&ns)?.is_some() => Err(refused(format!(
                    "Cannot create new indexes on existing collection {} in a multi-document transaction.",
                    full(&ns)
                ))),
                None => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// `createIndexes`' reply on a collection the transaction created: the reference server says it
/// created the collection, also when an earlier statement did, at the top and in the shard's reply
/// under `raw` (measured on 9.0.4 on 31-10-2026 and 03-11-2026).
pub fn mark_created(created: &[Created], db: &str, cmd: &Document, reply: &mut Document) {
    let Some(Value::String(c)) = cmd.get(CREATE_INDEXES) else { return };
    let ns = Namespace { db: db.to_owned(), coll: c.clone() };
    if find(created, &ns).is_none() || reply.get(CREATED_AUTOMATICALLY).is_none() {
        return;
    }
    reply.insert(CREATED_AUTOMATICALLY, Value::Boolean(true));
    let Some(Value::Document(raw)) = reply.get(RAW) else { return };
    let mut marked = Document::new();
    for (shard, answer) in raw.iter() {
        let answer = match answer {
            Value::Document(a) if a.get(CREATED_AUTOMATICALLY).is_some() => {
                let mut a = a.clone();
                a.insert(CREATED_AUTOMATICALLY, Value::Boolean(true));
                Value::Document(a)
            }
            other => other.clone(),
        };
        marked.push(shard, answer);
    }
    reply.insert(RAW, Value::Document(marked));
}

/// Gives the transaction's collections their names before its commit. A name another client
/// took in the meantime puts back the ones already renamed and fails with the reference server's
/// `WriteConflict` (measured on 9.0.4); the caller aborts the transaction.
pub fn publish(inner: &mut dyn Backend, created: &[Created]) -> Result<(), CommandError> {
    for (i, c) in created.iter().enumerate() {
        let rename = CatalogCommand::Rename {
            from: c.scratch.clone(),
            to: c.real.clone(),
            drop_target: false,
            uuid: c.entry.uuid,
        };
        if let Err(e) = inner.catalog_write(rename) {
            unpublish(inner, &created[..i]);
            if e.code != NAMESPACE_EXISTS {
                return Err(e);
            }
            return Err(CommandError::new(
                WRITE_CONFLICT,
                format!(
                    "Caused by :: Collection namespace '{}' is already in use. :: Please retry your operation or multi-document transaction.",
                    full(&c.real)
                ),
            )
            .with_label(TRANSIENT_TRANSACTION_ERROR));
        }
    }
    Ok(())
}

/// Puts back under their own names the collections [`publish`] renamed, for a transaction that
/// did not commit; a collection another command moved since stays where it is.
pub fn unpublish(inner: &mut dyn Backend, created: &[Created]) {
    for c in created {
        let back = CatalogCommand::Rename {
            from: c.real.clone(),
            to: c.scratch.clone(),
            drop_target: false,
            uuid: c.entry.uuid,
        };
        // Moved or dropped by another client after it became visible: it is theirs now.
        let _ = inner.catalog_write(back);
    }
}

/// Drops the collections of a transaction that ended without committing; one Group 0 cannot drop
/// now is left to the keeper.
pub fn discard(inner: &mut dyn Backend, shared: &Shared, created: &[Created]) {
    for c in created {
        if inner.catalog_write(CatalogCommand::Drop { ns: c.scratch.clone() }).is_ok() {
            lock(shared).txn_scratch.remove(&c.scratch);
        }
    }
}

/// Forgets the collections of a committed transaction, which have their names now.
pub fn forget(shared: &Shared, created: &[Created]) {
    let mut g = lock(shared);
    for c in created {
        g.txn_scratch.remove(&c.scratch);
    }
}

/// Drops the collections this node's transactions created and that no running transaction of the
/// node has any more (an abort that could not drop them, or a restart); the node's keeper calls
/// it with [`super::txn::tick_idle`].
pub fn sweep(backend: &mut dyn Backend, shared: &Shared) {
    let node = backend.node();
    let Ok(all) = backend.collections() else { return };
    for (ns, _) in all {
        let left = is_scratch(&ns)
            && creator(&ns) == Some(node)
            && !lock(shared).txn_scratch.contains(&ns);
        if left {
            // Group 0 not answering now: the next round tries again.
            let _ = backend.catalog_write(CatalogCommand::Drop { ns });
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn scratch_names_carry_their_node() {
        let real = Namespace { db: "d".into(), coll: "c".into() };
        let s = scratch_name(&real, 7, TxnId(0xab));
        assert_eq!(s.db, "d");
        assert_eq!(s.coll, format!("{SCRATCH_PREFIX}7.{:032x}.c", 0xab));
        assert!(is_scratch(&s) && !is_scratch(&real));
        assert_eq!(creator(&s), Some(7));
        assert_eq!(creator(&real), None);
        let bad = Namespace { db: "d".into(), coll: format!("{SCRATCH_PREFIX}x.1") };
        assert_eq!(creator(&bad), None);
    }
}
