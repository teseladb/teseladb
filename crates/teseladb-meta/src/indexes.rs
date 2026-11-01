//! Index definitions in the catalog of Group 0 (007 FR-001, contract `catalog.md`): secondary
//! indexes with an identifier that is never reused (0 is the documents of a collection, so they
//! start at 1), their key specification and options as the BSON document the client gave, their
//! hash buckets (D-008) and their state; and search indexes by collection and name with their
//! type, definition and status. Both hang from the collection identifier, so a rename keeps them,
//! and they go when the dropped collection's documents are purged. Group 0 kind 41; how indexes
//! are built and used is 009's and 011's.

use std::collections::BTreeMap;

use teseladb_types::{DecodeError, Decoder, Encoder};

use crate::catalog::CatalogError;

/// State of a secondary index (data model §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexState {
    /// Just created (009 FR-006): writes delete the entries of the versions they replace and add
    /// none, queries do not use it.
    DeleteOnly,
    /// Being built: writes maintain it, queries do not use it.
    Building,
    /// Usable.
    Ready,
    /// Being dropped.
    Dropping,
}

/// Type of a search index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchIndexKind {
    /// `search`.
    Search,
    /// `vectorSearch`.
    VectorSearch,
}

/// Status of a search index, as `$listSearchIndexes` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchIndexStatus {
    /// `PENDING`.
    Pending,
    /// `BUILDING`.
    Building,
    /// `READY`.
    Ready,
    /// `FAILED`.
    Failed,
    /// `STALE`.
    Stale,
    /// `DELETING`.
    Deleting,
}

macro_rules! coded {
    ($t:ty, $($v:path = $c:expr),+ $(,)?) => {
        impl $t {
            fn code(self) -> u8 {
                match self { $($v => $c),+ }
            }

            fn from_code(c: u8) -> Option<Self> {
                match c { $($c => Some($v),)+ _ => None }
            }
        }
    };
}

coded!(
    IndexState,
    IndexState::Building = 0,
    IndexState::Ready = 1,
    IndexState::Dropping = 2,
    IndexState::DeleteOnly = 3,
);
coded!(SearchIndexKind, SearchIndexKind::Search = 0, SearchIndexKind::VectorSearch = 1);
coded!(
    SearchIndexStatus,
    SearchIndexStatus::Pending = 0,
    SearchIndexStatus::Building = 1,
    SearchIndexStatus::Ready = 2,
    SearchIndexStatus::Failed = 3,
    SearchIndexStatus::Stale = 4,
    SearchIndexStatus::Deleting = 5,
);

fn get_coded<T>(d: &mut Decoder<'_>, from: fn(u8) -> Option<T>) -> Result<T, DecodeError> {
    let at = d.position();
    from(d.u8()?).ok_or(DecodeError::InvalidValue { offset: at })
}

/// Reads the 12 bytes of a search index identifier.
fn object_id(d: &mut Decoder<'_>) -> Result<[u8; 12], DecodeError> {
    let at = d.position();
    d.raw(12)?.try_into().map_err(|_| DecodeError::InvalidValue { offset: at })
}

/// A secondary index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Identifier, never reused (from 1).
    pub id: u32,
    /// Its collection.
    pub collection: u32,
    /// Name.
    pub name: String,
    /// Key specification and options, as a BSON document.
    pub spec: Vec<u8>,
    /// Hash buckets (D-008; 0 = none).
    pub buckets: u8,
    /// State.
    pub state: IndexState,
}

/// A search index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchIndexEntry {
    /// Identifier, the 12 bytes of the `ObjectId` the commands show as `id`.
    pub id: [u8; 12],
    /// Its collection.
    pub collection: u32,
    /// Name.
    pub name: String,
    /// Type.
    pub kind: SearchIndexKind,
    /// Definition, as a BSON document.
    pub definition: Vec<u8>,
    /// Status.
    pub status: SearchIndexStatus,
    /// Version of the definition: 0 when created, one more on each update
    /// (`latestDefinitionVersion.version`).
    pub version: u32,
    /// When the current definition was given, in milliseconds since the epoch
    /// (`latestDefinitionVersion.createdAt`).
    pub defined_ms: i64,
}

/// A command on the indexes of the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexCommand {
    /// Creates a secondary index in state `DeleteOnly`.
    Create {
        /// Collection.
        collection: u32,
        /// Name.
        name: String,
        /// Key specification and options.
        spec: Vec<u8>,
        /// Hash buckets.
        buckets: u8,
    },
    /// Changes the state of index `id`.
    SetState {
        /// Index.
        id: u32,
        /// New state.
        state: IndexState,
    },
    /// Replaces the specification of index `id` (`collMod` of an index): its options change,
    /// its identifier, name and state stay.
    SetSpec {
        /// Index.
        id: u32,
        /// The new specification.
        spec: Vec<u8>,
    },
    /// Index `new`, built under another name, takes the place of index `old` of the same
    /// collection (`collMod` to `unique: true`): it gets `old`'s name and becomes ready, and `old`
    /// goes to `Dropping` under `new`'s former name.
    Swap {
        /// The index replaced.
        old: u32,
        /// The index that replaces it.
        new: u32,
    },
    /// Drops index `id`.
    Drop {
        /// Index.
        id: u32,
    },
    /// Creates a search index in status `Pending`.
    CreateSearch {
        /// Identifier.
        id: [u8; 12],
        /// When it was asked for, in milliseconds since the epoch.
        at_ms: i64,
        /// Collection.
        collection: u32,
        /// Name.
        name: String,
        /// Type.
        kind: SearchIndexKind,
        /// Definition.
        definition: Vec<u8>,
    },
    /// Replaces a search index's definition, one version later; it goes back to `Pending`.
    UpdateSearch {
        /// When it was asked for, in milliseconds since the epoch.
        at_ms: i64,
        /// Collection.
        collection: u32,
        /// Name.
        name: String,
        /// Definition.
        definition: Vec<u8>,
    },
    /// Changes a search index's status.
    SetSearchStatus {
        /// Collection.
        collection: u32,
        /// Name.
        name: String,
        /// Status.
        status: SearchIndexStatus,
    },
    /// Drops a search index.
    DropSearch {
        /// Collection.
        collection: u32,
        /// Name.
        name: String,
    },
}

const CREATE: u8 = 0;
const SET_STATE: u8 = 1;
const DROP: u8 = 2;
const CREATE_SEARCH: u8 = 3;
const UPDATE_SEARCH: u8 = 4;
const SET_SEARCH_STATUS: u8 = 5;
const DROP_SEARCH: u8 = 6;
const SET_SPEC: u8 = 7;
const SWAP: u8 = 8;

impl IndexCommand {
    /// Writes the command after its Group 0 kind.
    pub fn encode(&self, e: &mut Encoder) {
        match self {
            IndexCommand::Create { collection, name, spec, buckets } => {
                e.u8(CREATE).u32(*collection).str(name).bytes(spec).u8(*buckets);
            }
            IndexCommand::SetState { id, state } => {
                e.u8(SET_STATE).u32(*id).u8(state.code());
            }
            IndexCommand::SetSpec { id, spec } => {
                e.u8(SET_SPEC).u32(*id).bytes(spec);
            }
            IndexCommand::Swap { old, new } => {
                e.u8(SWAP).u32(*old).u32(*new);
            }
            IndexCommand::Drop { id } => {
                e.u8(DROP).u32(*id);
            }
            IndexCommand::CreateSearch { id, at_ms, collection, name, kind, definition } => {
                e.u8(CREATE_SEARCH).raw(id).i64(*at_ms).u32(*collection).str(name);
                e.u8(kind.code()).bytes(definition);
            }
            IndexCommand::UpdateSearch { at_ms, collection, name, definition } => {
                e.u8(UPDATE_SEARCH).i64(*at_ms).u32(*collection).str(name).bytes(definition);
            }
            IndexCommand::SetSearchStatus { collection, name, status } => {
                e.u8(SET_SEARCH_STATUS).u32(*collection).str(name).u8(status.code());
            }
            IndexCommand::DropSearch { collection, name } => {
                e.u8(DROP_SEARCH).u32(*collection).str(name);
            }
        }
    }

    /// Reads what [`IndexCommand::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<IndexCommand, DecodeError> {
        let at = d.position();
        Ok(match d.u8()? {
            CREATE => IndexCommand::Create {
                collection: d.u32()?,
                name: d.str()?.to_owned(),
                spec: d.bytes()?.to_vec(),
                buckets: d.u8()?,
            },
            SET_STATE => {
                IndexCommand::SetState { id: d.u32()?, state: get_coded(d, IndexState::from_code)? }
            }
            SET_SPEC => IndexCommand::SetSpec { id: d.u32()?, spec: d.bytes()?.to_vec() },
            SWAP => IndexCommand::Swap { old: d.u32()?, new: d.u32()? },
            DROP => IndexCommand::Drop { id: d.u32()? },
            CREATE_SEARCH => IndexCommand::CreateSearch {
                id: object_id(d)?,
                at_ms: d.i64()?,
                collection: d.u32()?,
                name: d.str()?.to_owned(),
                kind: get_coded(d, SearchIndexKind::from_code)?,
                definition: d.bytes()?.to_vec(),
            },
            UPDATE_SEARCH => IndexCommand::UpdateSearch {
                at_ms: d.i64()?,
                collection: d.u32()?,
                name: d.str()?.to_owned(),
                definition: d.bytes()?.to_vec(),
            },
            SET_SEARCH_STATUS => IndexCommand::SetSearchStatus {
                collection: d.u32()?,
                name: d.str()?.to_owned(),
                status: get_coded(d, SearchIndexStatus::from_code)?,
            },
            DROP_SEARCH => {
                IndexCommand::DropSearch { collection: d.u32()?, name: d.str()?.to_owned() }
            }
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        })
    }
}

/// The indexes of the catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Indexes {
    /// Secondary indexes by identifier.
    pub indexes: BTreeMap<u32, IndexEntry>,
    /// Last index identifier given.
    pub last_index: u32,
    /// Search indexes by collection and name.
    pub search: BTreeMap<(u32, String), SearchIndexEntry>,
}

impl Indexes {
    /// The secondary indexes of collection `collection`, by identifier.
    pub fn of(&self, collection: u32) -> impl Iterator<Item = &IndexEntry> {
        self.indexes.values().filter(move |i| i.collection == collection)
    }

    /// The search indexes of collection `collection`, by name.
    pub fn search_of(&self, collection: u32) -> impl Iterator<Item = &SearchIndexEntry> {
        self.search.values().filter(move |i| i.collection == collection)
    }

    fn search_mut(&mut self, c: u32, name: &str) -> Result<&mut SearchIndexEntry, CatalogError> {
        self.search.get_mut(&(c, name.to_owned())).ok_or(CatalogError::IndexNotFound)
    }

    /// Applies `cmd`; `live` says whether a collection identifier exists. The index created, if
    /// any.
    pub fn apply(
        &mut self,
        cmd: &IndexCommand,
        live: impl Fn(u32) -> bool,
    ) -> Result<Option<IndexEntry>, CatalogError> {
        match cmd {
            IndexCommand::Create { collection, name, spec, buckets } => {
                if !live(*collection) {
                    return Err(CatalogError::NamespaceNotFound);
                }
                if self.of(*collection).any(|i| &i.name == name) {
                    return Err(CatalogError::IndexExists);
                }
                self.last_index += 1;
                let entry = IndexEntry {
                    id: self.last_index,
                    collection: *collection,
                    name: name.clone(),
                    spec: spec.clone(),
                    buckets: *buckets,
                    state: IndexState::DeleteOnly,
                };
                self.indexes.insert(entry.id, entry.clone());
                Ok(Some(entry))
            }
            IndexCommand::SetState { id, state } => {
                let i = self.indexes.get_mut(id).ok_or(CatalogError::IndexNotFound)?;
                i.state = *state;
                Ok(None)
            }
            IndexCommand::SetSpec { id, spec } => {
                let i = self.indexes.get_mut(id).ok_or(CatalogError::IndexNotFound)?;
                i.spec = spec.clone();
                Ok(None)
            }
            IndexCommand::Swap { old, new } => {
                let same = match (self.indexes.get(old), self.indexes.get(new)) {
                    (Some(o), Some(n)) => o.collection == n.collection,
                    _ => false,
                };
                if !same || old == new {
                    return Err(CatalogError::IndexNotFound);
                }
                let old_name = self.indexes[old].name.clone();
                let new_name = std::mem::replace(
                    &mut self.indexes.get_mut(new).ok_or(CatalogError::IndexNotFound)?.name,
                    old_name,
                );
                let o = self.indexes.get_mut(old).ok_or(CatalogError::IndexNotFound)?;
                o.name = new_name;
                o.state = IndexState::Dropping;
                let n = self.indexes.get_mut(new).ok_or(CatalogError::IndexNotFound)?;
                n.state = IndexState::Ready;
                Ok(None)
            }
            IndexCommand::Drop { id } => {
                self.indexes.remove(id).map(|_| None).ok_or(CatalogError::IndexNotFound)
            }
            IndexCommand::CreateSearch { id, at_ms, collection, name, kind, definition } => {
                if !live(*collection) {
                    return Err(CatalogError::NamespaceNotFound);
                }
                let key = (*collection, name.clone());
                if self.search.contains_key(&key) {
                    return Err(CatalogError::IndexExists);
                }
                let entry = SearchIndexEntry {
                    id: *id,
                    collection: *collection,
                    name: name.clone(),
                    kind: *kind,
                    definition: definition.clone(),
                    status: SearchIndexStatus::Pending,
                    version: 0,
                    defined_ms: *at_ms,
                };
                self.search.insert(key, entry);
                Ok(None)
            }
            IndexCommand::UpdateSearch { at_ms, collection, name, definition } => {
                let s = self.search_mut(*collection, name)?;
                s.definition = definition.clone();
                s.status = SearchIndexStatus::Pending;
                s.version += 1;
                s.defined_ms = *at_ms;
                Ok(None)
            }
            IndexCommand::SetSearchStatus { collection, name, status } => {
                self.search_mut(*collection, name)?.status = *status;
                Ok(None)
            }
            IndexCommand::DropSearch { collection, name } => self
                .search
                .remove(&(*collection, name.clone()))
                .map(|_| None)
                .ok_or(CatalogError::IndexNotFound),
        }
    }

    /// The documents of collection `id` were purged: its indexes go with them.
    pub fn forget_collection(&mut self, id: u32) {
        self.indexes.retain(|_, i| i.collection != id);
        self.search.retain(|(c, _), _| *c != id);
    }

    /// Writes the indexes into Group 0's snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        e.u32(self.last_index).var(self.indexes.len() as u64);
        for i in self.indexes.values() {
            e.u32(i.id).u32(i.collection).str(&i.name).bytes(&i.spec);
            e.u8(i.buckets).u8(i.state.code());
        }
        e.var(self.search.len() as u64);
        for s in self.search.values() {
            e.raw(&s.id).u32(s.collection).str(&s.name).u8(s.kind.code()).bytes(&s.definition);
            e.u8(s.status.code()).u32(s.version).i64(s.defined_ms);
        }
    }

    /// Reads what [`Indexes::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Indexes, DecodeError> {
        let mut x = Indexes { last_index: d.u32()?, ..Indexes::default() };
        for _ in 0..d.var()? {
            let i = IndexEntry {
                id: d.u32()?,
                collection: d.u32()?,
                name: d.str()?.to_owned(),
                spec: d.bytes()?.to_vec(),
                buckets: d.u8()?,
                state: get_coded(d, IndexState::from_code)?,
            };
            x.indexes.insert(i.id, i);
        }
        for _ in 0..d.var()? {
            let s = SearchIndexEntry {
                id: object_id(d)?,
                collection: d.u32()?,
                name: d.str()?.to_owned(),
                kind: get_coded(d, SearchIndexKind::from_code)?,
                definition: d.bytes()?.to_vec(),
                status: get_coded(d, SearchIndexStatus::from_code)?,
                version: d.u32()?,
                defined_ms: d.i64()?,
            };
            x.search.insert((s.collection, s.name.clone()), s);
        }
        Ok(x)
    }
}
