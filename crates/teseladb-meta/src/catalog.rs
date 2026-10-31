//! The catalog in Group 0 (006 FR-027, 007 data model §1 and contract `catalog.md`): the
//! collections of each database with their identifier (`u32`, never reused), their RefDb UUID
//! and their options as the BSON document the client gave. The proposer generates the UUID, so
//! applying stays deterministic. A dropped collection's identifier waits in [`Catalog::dropped`]
//! until its documents are deleted (`Purged`). A database stays in [`Catalog::databases`] from its
//! first collection until `dropDatabase`, also with no collection left, as `config.databases`
//! of a `refrouter` (D-077).

use std::collections::{BTreeMap, BTreeSet};

use teseladb_hlc::Hlc;
use teseladb_types::{DecodeError, Decoder, Encoder};

/// A collection's namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Namespace {
    /// Database.
    pub db: String,
    /// Collection.
    pub coll: String,
}

/// A collection of the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionEntry {
    /// Identifier, never reused.
    pub id: u32,
    /// RefDb's UUID.
    pub uuid: [u8; 16],
    /// Options (`validator`, `validationLevel`, `collation`, …) as a BSON document.
    pub options: Vec<u8>,
    /// Group 0 index of its creation.
    pub created_index: u64,
}

/// Why a catalog command did not change anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// The namespace already has a collection.
    NamespaceExists,
    /// The namespace has no collection.
    NamespaceNotFound,
    /// The collection already has an index with that name (007, kind 41).
    IndexExists,
    /// No such index (007, kind 41).
    IndexNotFound,
    /// The command adds a database whose name differs only in case from one of the catalog
    /// (13297 `DatabaseDifferCase` in RefDb).
    DatabaseDifferCase,
}

/// A catalog command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogCommand {
    /// Creates a collection; with `if_absent` (an implicit creation) an existing one is the
    /// result instead of an error.
    Create {
        /// Namespace.
        ns: Namespace,
        /// Its UUID.
        uuid: [u8; 16],
        /// Options.
        options: Vec<u8>,
        /// Implicit creation.
        if_absent: bool,
    },
    /// Drops a collection.
    Drop {
        /// Namespace.
        ns: Namespace,
    },
    /// Renames a collection; to another database it gets `uuid`.
    Rename {
        /// Source.
        from: Namespace,
        /// Target.
        to: Namespace,
        /// Drop an existing target.
        drop_target: bool,
        /// UUID for a move to another database.
        uuid: [u8; 16],
    },
    /// Replaces a collection's options.
    Modify {
        /// Namespace.
        ns: Namespace,
        /// New options.
        options: Vec<u8>,
    },
    /// Drops every collection of a database.
    DropDatabase {
        /// Database.
        db: String,
    },
    /// Records a database without creating a collection: a write to a database that has none
    /// (`refrouter` creates its entry in `config.databases`, D-077).
    CreateDatabase {
        /// Database.
        db: String,
    },
    /// The documents of dropped collection `id` are deleted.
    Purged {
        /// Collection identifier.
        id: u32,
    },
}

const CREATE: u8 = 0;
const DROP: u8 = 1;
const RENAME: u8 = 2;
const MODIFY: u8 = 3;
const DROP_DATABASE: u8 = 4;
const PURGED: u8 = 5;
const CREATE_DATABASE: u8 = 6;

fn put_ns(e: &mut Encoder, ns: &Namespace) {
    e.str(&ns.db).str(&ns.coll);
}

fn get_ns(d: &mut Decoder<'_>) -> Result<Namespace, DecodeError> {
    Ok(Namespace { db: d.str()?.to_owned(), coll: d.str()?.to_owned() })
}

fn get_uuid(d: &mut Decoder<'_>) -> Result<[u8; 16], DecodeError> {
    let mut u = [0u8; 16];
    u.copy_from_slice(d.raw(16)?);
    Ok(u)
}

impl CatalogCommand {
    /// Writes the command after its Group 0 kind.
    pub fn encode(&self, e: &mut Encoder) {
        match self {
            CatalogCommand::Create { ns, uuid, options, if_absent } => {
                e.u8(CREATE);
                put_ns(e, ns);
                e.raw(uuid).bytes(options).bool(*if_absent);
            }
            CatalogCommand::Drop { ns } => {
                e.u8(DROP);
                put_ns(e, ns);
            }
            CatalogCommand::Rename { from, to, drop_target, uuid } => {
                e.u8(RENAME);
                put_ns(e, from);
                put_ns(e, to);
                e.bool(*drop_target).raw(uuid);
            }
            CatalogCommand::Modify { ns, options } => {
                e.u8(MODIFY);
                put_ns(e, ns);
                e.bytes(options);
            }
            CatalogCommand::DropDatabase { db } => {
                e.u8(DROP_DATABASE).str(db);
            }
            CatalogCommand::Purged { id } => {
                e.u8(PURGED).u32(*id);
            }
            CatalogCommand::CreateDatabase { db } => {
                e.u8(CREATE_DATABASE).str(db);
            }
        }
    }

    /// Reads what [`CatalogCommand::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<CatalogCommand, DecodeError> {
        let at = d.position();
        Ok(match d.u8()? {
            CREATE => CatalogCommand::Create {
                ns: get_ns(d)?,
                uuid: get_uuid(d)?,
                options: d.bytes()?.to_vec(),
                if_absent: d.bool()?,
            },
            DROP => CatalogCommand::Drop { ns: get_ns(d)? },
            RENAME => CatalogCommand::Rename {
                from: get_ns(d)?,
                to: get_ns(d)?,
                drop_target: d.bool()?,
                uuid: get_uuid(d)?,
            },
            MODIFY => CatalogCommand::Modify { ns: get_ns(d)?, options: d.bytes()?.to_vec() },
            DROP_DATABASE => CatalogCommand::DropDatabase { db: d.str()?.to_owned() },
            PURGED => CatalogCommand::Purged { id: d.u32()? },
            CREATE_DATABASE => CatalogCommand::CreateDatabase { db: d.str()?.to_owned() },
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        })
    }
}

/// What applying a catalog command gives.
pub type CatalogResult = Result<Option<CollectionEntry>, CatalogError>;

/// The catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Collections by namespace.
    pub collections: BTreeMap<Namespace, CollectionEntry>,
    /// Last collection identifier given.
    pub last_collection: u32,
    /// Dropped collections whose documents are still to delete.
    pub dropped: BTreeSet<u32>,
    /// Databases that got a collection or a write and were not dropped since, with the Group 0
    /// index that added them (D-077).
    pub dbs: BTreeMap<String, u64>,
    /// The mark of the entry that added each database of [`Catalog::dbs`]: a `snapshot` read at
    /// an earlier `atClusterTime` is `MigrationConflict` (008 FR-012, measured on 9.0.4).
    pub db_marks: BTreeMap<String, Hlc>,
}

impl Catalog {
    /// The collection of `ns`.
    pub fn get(&self, ns: &Namespace) -> Option<&CollectionEntry> {
        self.collections.get(ns)
    }

    /// The databases, in order: those that got a collection and were not dropped since, with or
    /// without collections now (D-077).
    pub fn databases(&self) -> Vec<String> {
        self.dbs.keys().cloned().collect()
    }

    /// The databases in the order they were added, as `refrouter` reads `config.databases`.
    pub fn databases_by_creation(&self) -> Vec<String> {
        let mut v: Vec<(&String, &u64)> = self.dbs.iter().collect();
        v.sort_by_key(|(_, at)| **at);
        v.into_iter().map(|(db, _)| db.clone()).collect()
    }

    fn add_db(&mut self, db: &str, (index, mark): (u64, Option<Hlc>)) {
        if !self.dbs.contains_key(db) {
            self.dbs.insert(db.to_owned(), index);
            if let Some(m) = mark {
                self.db_marks.insert(db.to_owned(), m);
            }
        }
    }

    /// The collections of database `db`, in name order.
    pub fn in_db(&self, db: &str) -> Vec<(&Namespace, &CollectionEntry)> {
        self.collections.iter().filter(|(n, _)| n.db == db).collect()
    }

    /// Whether adding database `db` clashes with another that differs only in ASCII case, as
    /// RefDb compares them.
    fn differs_in_case(&self, db: &str) -> bool {
        !self.dbs.contains_key(db) && self.dbs.keys().any(|d| d.eq_ignore_ascii_case(db))
    }

    fn drop_ns(&mut self, ns: &Namespace) -> Option<CollectionEntry> {
        let c = self.collections.remove(ns)?;
        self.dropped.insert(c.id);
        Some(c)
    }

    /// Applies `cmd` at Group 0 index `index`: the collection created or found, if any.
    pub fn apply(&mut self, index: u64, cmd: &CatalogCommand) -> CatalogResult {
        self.apply_at(index, None, cmd)
    }

    /// [`Catalog::apply`] of the entry with HLC mark `mark`, which a database it adds keeps.
    pub fn apply_at(
        &mut self,
        index: u64,
        mark: Option<Hlc>,
        cmd: &CatalogCommand,
    ) -> CatalogResult {
        match cmd {
            CatalogCommand::Create { ns, uuid, options, if_absent } => {
                if let Some(c) = self.collections.get(ns) {
                    return if *if_absent {
                        Ok(Some(c.clone()))
                    } else {
                        Err(CatalogError::NamespaceExists)
                    };
                }
                if self.differs_in_case(&ns.db) {
                    return Err(CatalogError::DatabaseDifferCase);
                }
                self.last_collection += 1;
                let c = CollectionEntry {
                    id: self.last_collection,
                    uuid: *uuid,
                    options: options.clone(),
                    created_index: index,
                };
                self.collections.insert(ns.clone(), c.clone());
                self.add_db(&ns.db, (index, mark));
                Ok(Some(c))
            }
            CatalogCommand::Drop { ns } => {
                self.drop_ns(ns).map(|_| None).ok_or(CatalogError::NamespaceNotFound)
            }
            CatalogCommand::Rename { from, to, drop_target, uuid } => {
                if !self.collections.contains_key(from) {
                    return Err(CatalogError::NamespaceNotFound);
                }
                if self.differs_in_case(&to.db) {
                    return Err(CatalogError::DatabaseDifferCase);
                }
                if self.collections.contains_key(to) {
                    if !drop_target {
                        return Err(CatalogError::NamespaceExists);
                    }
                    self.drop_ns(to);
                }
                let mut c = self.collections.remove(from).ok_or(CatalogError::NamespaceNotFound)?;
                if from.db != to.db {
                    c.uuid = *uuid;
                }
                self.collections.insert(to.clone(), c);
                self.add_db(&to.db, (index, mark));
                Ok(None)
            }
            CatalogCommand::Modify { ns, options } => match self.collections.get_mut(ns) {
                Some(c) => {
                    c.options = options.clone();
                    Ok(None)
                }
                None => Err(CatalogError::NamespaceNotFound),
            },
            CatalogCommand::DropDatabase { db } => {
                let names: Vec<Namespace> =
                    self.collections.keys().filter(|n| &n.db == db).cloned().collect();
                for n in names {
                    self.drop_ns(&n);
                }
                self.dbs.remove(db);
                self.db_marks.remove(db);
                Ok(None)
            }
            CatalogCommand::Purged { id } => {
                self.dropped.remove(id);
                Ok(None)
            }
            CatalogCommand::CreateDatabase { db } => {
                if self.differs_in_case(db) {
                    return Err(CatalogError::DatabaseDifferCase);
                }
                self.add_db(db, (index, mark));
                Ok(None)
            }
        }
    }

    /// Writes the catalog into Group 0's snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        self.encode_with(e, true);
    }

    /// [`Catalog::encode`], with the databases' marks or without them (the snapshot format before
    /// they existed).
    pub fn encode_with(&self, e: &mut Encoder, marks: bool) {
        e.u32(self.last_collection).var(self.collections.len() as u64);
        for (ns, c) in &self.collections {
            put_ns(e, ns);
            e.u32(c.id).raw(&c.uuid).bytes(&c.options).u64(c.created_index);
        }
        e.var(self.dropped.len() as u64);
        for id in &self.dropped {
            e.u32(*id);
        }
        e.var(self.dbs.len() as u64);
        for (db, at) in &self.dbs {
            e.str(db).u64(*at);
        }
        if marks {
            e.var(self.db_marks.len() as u64);
            for (db, m) in &self.db_marks {
                e.str(db).u64(m.physical).u32(m.logical);
            }
        }
    }

    /// Reads what [`Catalog::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Catalog, DecodeError> {
        Catalog::decode_with(d, true)
    }

    /// Reads what [`Catalog::encode_with`] wrote; without marks, no database has one.
    pub fn decode_with(d: &mut Decoder<'_>, marks: bool) -> Result<Catalog, DecodeError> {
        let mut c = Catalog { last_collection: d.u32()?, ..Catalog::default() };
        for _ in 0..d.var()? {
            let ns = get_ns(d)?;
            let entry = CollectionEntry {
                id: d.u32()?,
                uuid: get_uuid(d)?,
                options: d.bytes()?.to_vec(),
                created_index: d.u64()?,
            };
            c.collections.insert(ns, entry);
        }
        for _ in 0..d.var()? {
            c.dropped.insert(d.u32()?);
        }
        for _ in 0..d.var()? {
            let db = d.str()?.to_owned();
            c.dbs.insert(db, d.u64()?);
        }
        if !marks {
            return Ok(c);
        }
        for _ in 0..d.var()? {
            let db = d.str()?.to_owned();
            let mark = Hlc { physical: d.u64()?, logical: d.u32()? };
            c.db_marks.insert(db, mark);
        }
        Ok(c)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_database_keeps_the_mark_that_added_it() {
        let mut c = Catalog::default();
        let ns = Namespace { db: "d".into(), coll: "c".into() };
        let create = |ns: Namespace, uuid| CatalogCommand::Create {
            ns,
            uuid,
            options: vec![],
            if_absent: false,
        };
        c.apply_at(1, Some(Hlc { physical: 7, logical: 2 }), &create(ns.clone(), [1; 16])).unwrap();
        let other = Namespace { coll: "e".into(), ..ns.clone() };
        c.apply_at(2, Some(Hlc { physical: 9, logical: 0 }), &create(other, [2; 16])).unwrap();
        // A command applied without a mark adds none.
        let unmarked = Namespace { db: "u".into(), coll: "c".into() };
        c.apply(3, &create(unmarked, [3; 16])).unwrap();
        assert!(!c.db_marks.contains_key("u"));
        assert_eq!(c.db_marks.get("d"), Some(&Hlc { physical: 7, logical: 2 }));
        let mut e = Encoder::new();
        c.encode(&mut e);
        let bytes = e.finish();
        let back = Catalog::decode(&mut Decoder::new(&bytes)).unwrap();
        assert_eq!(back.db_marks, c.db_marks);
        c.apply(4, &CatalogCommand::DropDatabase { db: "d".into() }).unwrap();
        assert!(c.db_marks.is_empty());
    }
}
