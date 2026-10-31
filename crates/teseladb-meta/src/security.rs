//! Users and roles of the cluster in Group 0 (013 FR-009, FR-013, FR-030, contract `catalog.md`):
//! the documents of `admin.system.users` and `admin.system.roles` exactly as they were written,
//! by the user commands or by a direct write to those collections, kept in the order they were
//! inserted (an update keeps the place, a delete and a new insert go to the end), with the two
//! indexes RefDb 9.0.4 has on them: `_id` and the unique `user_1_db_1` or `role_1_db_1`, where
//! a missing field counts as `null` and an array gives one key per element (measured on a
//! `refrouter`). A change is one atomic batch of puts and deletes, each addressing a document by
//! its `_id` with what it expects to find there, so that the commands that read, change and
//! write back (`updateUser`, `dropRole` taking the role from every user) lose nothing to a
//! concurrent change: a batch whose expectations fail changes nothing. Every applied batch moves
//! [`Security::version`], which the nodes compare to know their copy is current. Group 0 kind 43.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_keys::{Direction, encode};
use teseladb_types::{DecodeError, Decoder, Encoder};

/// Whether an entry is a user or a role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
    /// A user (`admin.system.users`).
    User,
    /// A role (`admin.system.roles`).
    Role,
}

impl Principal {
    /// The field that names it, the first of its unique index.
    pub fn name_field(self) -> &'static str {
        match self {
            Principal::User => USER_FIELD,
            Principal::Role => ROLE_FIELD,
        }
    }

    /// The name of its unique index.
    pub fn unique_index(self) -> &'static str {
        match self {
            Principal::User => "user_1_db_1",
            Principal::Role => "role_1_db_1",
        }
    }
}

/// `user`.
pub const USER_FIELD: &str = "user";
/// `role`.
pub const ROLE_FIELD: &str = "role";
/// `db`, the second field of the unique indexes.
pub const DB_FIELD: &str = "db";
/// `_id`.
pub const ID_FIELD: &str = "_id";

/// What a change expects to find under its `_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    /// Nothing: the `_id` is free.
    Absent,
    /// Some document, whichever.
    Present,
    /// Exactly this document, the one the change was computed from.
    Exactly(Vec<u8>),
}

/// One change of a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityOp {
    /// User or role.
    pub kind: Principal,
    /// The key of the document's `_id` ([`id_key`]).
    pub id: Vec<u8>,
    /// What must be there.
    pub expect: Expect,
    /// The new document, whose `_id` has key [`SecurityOp::id`]; `None` deletes the document.
    pub doc: Option<Vec<u8>>,
}

impl SecurityOp {
    /// Writes `doc` under its own `_id`.
    pub fn put(kind: Principal, expect: Expect, doc: Vec<u8>) -> Result<SecurityOp, KeyError> {
        Ok(SecurityOp { kind, id: id_key(&doc)?, expect, doc: Some(doc) })
    }

    /// The change of the document whose `_id` is `db.name`, the `_id` the user commands give.
    pub fn named(
        kind: Principal,
        db: &str,
        name: &str,
        expect: Expect,
        doc: Option<Vec<u8>>,
    ) -> SecurityOp {
        SecurityOp { kind, id: named_id(db, name), expect, doc }
    }

    /// Deletes `doc`, which must still be there as it is.
    pub fn remove(kind: Principal, doc: Vec<u8>) -> Result<SecurityOp, KeyError> {
        Ok(SecurityOp { kind, id: id_key(&doc)?, expect: Expect::Exactly(doc), doc: None })
    }
}

/// Why a batch changed nothing: the first expectation that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityError {
    /// The `_id` was taken ([`Expect::Absent`]), or a key of the new document's unique index
    /// belongs to another document.
    Exists(Principal),
    /// The `_id` was free ([`Expect::Present`] or [`Expect::Exactly`] on nothing).
    NotFound(Principal),
    /// Another document than the one expected, or a new document that cannot be indexed: a
    /// concurrent change; the caller reads again.
    Conflict,
}

/// Why a document has no keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// It is not BSON.
    Bson,
    /// It has no `_id`.
    NoId,
    /// Both fields of the unique index are arrays (RefDb's 171).
    ParallelArrays,
}

/// The key of the `_id` of `doc`: equal for equal values (`7` and `7.0`), as the `_id` index
/// compares them.
pub fn id_key(doc: &[u8]) -> Result<Vec<u8>, KeyError> {
    let raw = RawDoc::from_bytes(doc).map_err(|_| KeyError::Bson)?;
    let id = raw.get(ID_FIELD).ok_or(KeyError::NoId)?;
    Ok(key_of(&[id]))
}

/// The key of the unique index for `name` and `db` given as strings, the one a lookup by name
/// uses.
pub fn name_key(name: &str, db: &str) -> Vec<u8> {
    key_of(&[RawValue::String(name), RawValue::String(db)])
}

fn key_of(values: &[RawValue<'_>]) -> Vec<u8> {
    let fields: Vec<_> = values.iter().map(|v| (*v, Direction::Ascending)).collect();
    encode(&fields).0
}

/// One key of the unique index of a document, with the values it holds (for the words of a
/// duplicate key error).
#[derive(Clone, Debug, PartialEq)]
pub struct UniqueKey {
    /// The key.
    pub key: Vec<u8>,
    /// The name and the database it holds.
    pub values: [Value; 2],
}

/// The keys of the unique index of `kind` for `doc`: a missing field is `null`, an array gives
/// one key per element and an empty array the key `undefined`, as RefDb indexes them.
pub fn unique_keys(kind: Principal, doc: &[u8]) -> Result<Vec<UniqueKey>, KeyError> {
    let raw = RawDoc::from_bytes(doc).map_err(|_| KeyError::Bson)?;
    let spread = |field: &str| match raw.get(field) {
        None => (vec![RawValue::Null], false),
        Some(RawValue::Array(a)) => {
            let all: Vec<RawValue<'_>> = a.iter().collect();
            if all.is_empty() { (vec![RawValue::Undefined], true) } else { (all, true) }
        }
        Some(v) => (vec![v], false),
    };
    let (names, names_array) = spread(kind.name_field());
    let (dbs, dbs_array) = spread(DB_FIELD);
    if names_array && dbs_array {
        return Err(KeyError::ParallelArrays);
    }
    let mut out: Vec<UniqueKey> = Vec::new();
    for n in &names {
        for d in &dbs {
            let key = key_of(&[*n, *d]);
            if !out.iter().any(|k| k.key == key) {
                out.push(UniqueKey { key, values: [Value::from(*n), Value::from(*d)] });
            }
        }
    }
    Ok(out)
}

/// A document with its keys.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stored {
    doc: Vec<u8>,
    id: Vec<u8>,
    unique: Vec<Vec<u8>>,
}

impl Stored {
    fn of(kind: Principal, doc: Vec<u8>) -> Result<Stored, KeyError> {
        let id = id_key(&doc)?;
        let unique = unique_keys(kind, &doc)?.into_iter().map(|k| k.key).collect();
        Ok(Stored { doc, id, unique })
    }
}

/// The documents of one collection in insertion order, with their indexes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    /// By insertion sequence.
    entries: BTreeMap<u64, Stored>,
    /// Sequence by `_id` key.
    ids: BTreeMap<Vec<u8>, u64>,
    /// Sequence by unique key.
    unique: BTreeMap<Vec<u8>, u64>,
}

impl Table {
    /// How many documents it has.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Its documents in insertion order.
    pub fn docs(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.values().map(|s| s.doc.as_slice())
    }

    /// Keeps the documents `keep` takes.
    pub fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        let gone: Vec<u64> =
            self.entries.iter().filter(|(_, s)| !keep(&s.doc)).map(|(seq, _)| *seq).collect();
        for seq in gone {
            self.take(seq);
        }
    }

    /// Takes every document out.
    pub fn clear(&mut self) {
        *self = Table::default();
    }

    fn by_unique(&self, key: &[u8]) -> Option<u64> {
        self.unique.get(key).copied()
    }

    fn take(&mut self, seq: u64) -> Option<Vec<u8>> {
        let s = self.entries.remove(&seq)?;
        self.ids.remove(&s.id);
        for k in &s.unique {
            self.unique.remove(k);
        }
        Some(s.doc)
    }

    fn place(&mut self, seq: u64, s: Stored) {
        self.ids.insert(s.id.clone(), seq);
        for k in &s.unique {
            self.unique.insert(k.clone(), seq);
        }
        self.entries.insert(seq, s);
    }

    /// Adds `s` at the end; `false` if one of its keys is taken.
    fn push(&mut self, s: Stored) -> bool {
        if self.ids.contains_key(&s.id) || s.unique.iter().any(|k| self.unique.contains_key(k)) {
            return false;
        }
        let seq = self.entries.last_key_value().map_or(0, |(seq, _)| seq + 1);
        self.place(seq, s);
        true
    }

    fn apply(&mut self, op: &SecurityOp) -> Result<(), SecurityError> {
        let seq = self.ids.get(&op.id).copied();
        let have = seq.and_then(|s| self.entries.get(&s)).map(|s| s.doc.as_slice());
        match (&op.expect, have) {
            (Expect::Absent, Some(_)) => return Err(SecurityError::Exists(op.kind)),
            (Expect::Present | Expect::Exactly(_), None) => {
                return Err(SecurityError::NotFound(op.kind));
            }
            (Expect::Exactly(want), Some(have)) if want.as_slice() != have => {
                return Err(SecurityError::Conflict);
            }
            _ => {}
        }
        let Some(doc) = &op.doc else {
            if let Some(seq) = seq {
                self.take(seq);
            }
            return Ok(());
        };
        let stored = Stored::of(op.kind, doc.clone()).map_err(|_| SecurityError::Conflict)?;
        if stored.id != op.id {
            return Err(SecurityError::Conflict);
        }
        if stored.unique.iter().any(|k| self.by_unique(k).is_some_and(|s| Some(s) != seq)) {
            return Err(SecurityError::Exists(op.kind));
        }
        match seq {
            Some(seq) => {
                self.take(seq);
                self.place(seq, stored);
            }
            None => {
                self.push(stored);
            }
        }
        Ok(())
    }
}

/// The users and roles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Security {
    /// `admin.system.users` (boxed, like the roles, to keep Group 0's state small).
    pub users: Box<Table>,
    /// `admin.system.roles`.
    pub roles: Box<Table>,
    /// Batches applied so far.
    pub version: u64,
}

const USER: u8 = 0;
const ROLE: u8 = 1;
const USER_BY_ID: u8 = 2;
const ROLE_BY_ID: u8 = 3;
const ABSENT: u8 = 0;
const PRESENT: u8 = 1;
const EXACTLY: u8 = 2;

impl Security {
    /// The table of `kind`.
    pub fn table(&self, kind: Principal) -> &Table {
        match kind {
            Principal::User => &self.users,
            Principal::Role => &self.roles,
        }
    }

    fn table_mut(&mut self, kind: Principal) -> &mut Table {
        match kind {
            Principal::User => &mut self.users,
            Principal::Role => &mut self.roles,
        }
    }

    /// The document of `name` in `db`: the one whose unique key is these two strings.
    pub fn get(&self, kind: Principal, db: &str, name: &str) -> Option<&[u8]> {
        let t = self.table(kind);
        let seq = t.by_unique(&name_key(name, db))?;
        t.entries.get(&seq).map(|s| s.doc.as_slice())
    }

    /// The documents of `db` (every document with `None`) in insertion order: those whose `db`
    /// is that string or an array holding it, as the query `{db: …}` finds them.
    pub fn list(&self, kind: Principal, db: Option<&str>) -> Vec<&[u8]> {
        self.table(kind).docs().filter(|doc| db.is_none_or(|db| in_db(doc, db))).collect()
    }

    /// Takes the document of `name` in `db` out, without a batch.
    pub fn remove(&mut self, kind: Principal, db: &str, name: &str) -> Option<Vec<u8>> {
        let t = self.table_mut(kind);
        let seq = t.by_unique(&name_key(name, db))?;
        t.take(seq)
    }

    /// Applies `ops` all or none, in order: an op sees the ones before it.
    pub fn apply(&mut self, ops: &[SecurityOp]) -> Result<(), SecurityError> {
        let mut next = self.clone();
        for op in ops {
            next.table_mut(op.kind).apply(op)?;
        }
        next.version = self.version + 1;
        *self = next;
        Ok(())
    }

    /// Writes the state into Group 0's snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        e.u64(self.version);
        for table in [&self.users, &self.roles] {
            e.var(table.entries.len() as u64);
            for s in table.entries.values() {
                e.bytes(&s.doc);
            }
        }
    }

    /// Writes the state as snapshots before raw documents did: each document under the
    /// strings of its database and name (empty when the document does not have them), in that
    /// order.
    pub fn encode_named(&self, e: &mut Encoder) {
        e.u64(self.version);
        for (kind, table) in [(Principal::User, &self.users), (Principal::Role, &self.roles)] {
            let mut named: Vec<(String, String, &[u8])> = table
                .docs()
                .map(|doc| {
                    let raw = RawDoc::from_bytes(doc).ok();
                    let text = |f: &str| match raw.and_then(|r| r.get(f)) {
                        Some(RawValue::String(t)) => t.to_owned(),
                        _ => String::new(),
                    };
                    (text(DB_FIELD), text(kind.name_field()), doc)
                })
                .collect();
            named.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
            e.var(named.len() as u64);
            for (db, name, doc) in named {
                e.str(&db).str(&name).bytes(doc);
            }
        }
    }

    /// Reads what [`Security::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Security, DecodeError> {
        Security::decode_with(d, false)
    }

    /// Reads what [`Security::encode_named`] wrote: the order of the entries becomes the
    /// insertion order.
    pub fn decode_named(d: &mut Decoder<'_>) -> Result<Security, DecodeError> {
        Security::decode_with(d, true)
    }

    fn decode_with(d: &mut Decoder<'_>, named: bool) -> Result<Security, DecodeError> {
        let mut s = Security { version: d.u64()?, ..Security::default() };
        for kind in [Principal::User, Principal::Role] {
            for _ in 0..d.var()? {
                if named {
                    d.str()?;
                    d.str()?;
                }
                let at = d.position();
                let doc = d.bytes()?.to_vec();
                let stored =
                    Stored::of(kind, doc).map_err(|_| DecodeError::InvalidValue { offset: at })?;
                if !s.table_mut(kind).push(stored) {
                    return Err(DecodeError::InvalidValue { offset: at });
                }
            }
        }
        Ok(s)
    }
}

/// Whether the `db` of `doc` is `db` or an array holding it.
fn in_db(doc: &[u8], db: &str) -> bool {
    match RawDoc::from_bytes(doc).ok().and_then(|r| r.get(DB_FIELD)) {
        Some(RawValue::String(s)) => s == db,
        Some(RawValue::Array(a)) => a.iter().any(|v| v == RawValue::String(db)),
        _ => false,
    }
}

/// The `_id` key of a user or role written by a binary that addressed them by name: their `_id`
/// was always `db.name`.
fn named_id(db: &str, name: &str) -> Vec<u8> {
    let d = Document::new().with(ID_FIELD, format!("{db}.{name}"));
    d.to_bytes().ok().and_then(|b| id_key(&b).ok()).unwrap_or_default()
}

/// Encodes a batch, the payload of Group 0's `Security` command.
pub fn encode_ops(ops: &[SecurityOp], e: &mut Encoder) {
    e.var(ops.len() as u64);
    for op in ops {
        e.u8(match op.kind {
            Principal::User => USER_BY_ID,
            Principal::Role => ROLE_BY_ID,
        });
        e.bytes(&op.id);
        match &op.expect {
            Expect::Absent => e.u8(ABSENT),
            Expect::Present => e.u8(PRESENT),
            Expect::Exactly(doc) => e.u8(EXACTLY).bytes(doc),
        };
        e.bool(op.doc.is_some()).bytes(op.doc.as_deref().unwrap_or(&[]));
    }
}

/// Reads what [`encode_ops`] wrote, and the batches of a binary that addressed users and roles
/// by database and name (still in the Raft log of a node that ran it).
pub fn decode_ops(d: &mut Decoder<'_>) -> Result<Vec<SecurityOp>, DecodeError> {
    let at = d.position();
    let n = d.var()?;
    // Every op takes at least five bytes.
    if n > (d.remaining() / 5) as u64 {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    let mut ops = Vec::new();
    for _ in 0..n {
        let at = d.position();
        let (kind, id) = match d.u8()? {
            USER_BY_ID => (Principal::User, d.bytes()?.to_vec()),
            ROLE_BY_ID => (Principal::Role, d.bytes()?.to_vec()),
            code @ (USER | ROLE) => {
                let kind = if code == USER { Principal::User } else { Principal::Role };
                let (db, name) = (d.str()?, d.str()?);
                (kind, named_id(db, name))
            }
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        let at = d.position();
        let expect = match d.u8()? {
            ABSENT => Expect::Absent,
            PRESENT => Expect::Present,
            EXACTLY => Expect::Exactly(d.bytes()?.to_vec()),
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        let has = d.bool()?;
        let doc = d.bytes()?.to_vec();
        ops.push(SecurityOp { kind, id, expect, doc: has.then_some(doc) });
    }
    Ok(ops)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn doc(id: impl Into<Value>, name: impl Into<Value>, extra: i32) -> Vec<u8> {
        Document::new()
            .with(ID_FIELD, id)
            .with(USER_FIELD, name)
            .with(DB_FIELD, "test")
            .with("x", extra)
            .to_bytes()
            .unwrap()
    }

    fn put(expect: Expect, d: Vec<u8>) -> SecurityOp {
        SecurityOp::put(Principal::User, expect, d).unwrap()
    }

    #[test]
    fn batches_apply_all_or_nothing() {
        let mut s = Security::default();
        let u = Principal::User;
        let (a, b) = (doc("test.a", "a", 1), doc("test.b", "b", 2));
        s.apply(&[put(Expect::Absent, a.clone()), put(Expect::Absent, b.clone())]).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.get(u, "test", "a"), Some(a.as_slice()));
        let before = s.clone();
        let missing = doc("test.z", "z", 0);
        let cases = [
            (put(Expect::Absent, doc("test.a", "q", 9)), SecurityError::Exists(u)),
            (put(Expect::Absent, doc("other", "a", 9)), SecurityError::Exists(u)),
            (put(Expect::Present, missing.clone()), SecurityError::NotFound(u)),
            (put(Expect::Exactly(missing.clone()), missing.clone()), SecurityError::NotFound(u)),
            (put(Expect::Exactly(b.clone()), doc("test.a", "a", 3)), SecurityError::Conflict),
            (put(Expect::Exactly(b.clone()), doc("test.b", "a", 3)), SecurityError::Exists(u)),
        ];
        for (bad, e) in cases {
            // The first op would apply; the failing second undoes it.
            let first = put(Expect::Absent, doc("test.n", "n", 0));
            assert_eq!(s.apply(&[first, bad]), Err(e));
            assert_eq!(s, before);
        }
        // A document whose `_id` is not the op's, or that cannot be indexed, changes nothing.
        let mut wrong = put(Expect::Absent, doc("new", "n", 0));
        wrong.id = put(Expect::Absent, doc("other", "n", 0)).id;
        assert_eq!(s.apply(&[wrong]), Err(SecurityError::Conflict));
        let mut junk = put(Expect::Absent, doc("new", "n", 0));
        junk.doc = Some(b"junk".to_vec());
        assert_eq!(s.apply(&[junk]), Err(SecurityError::Conflict));
        let r = Principal::Role;
        let role =
            Document::new().with(ID_FIELD, "test.r").with(ROLE_FIELD, "r").with(DB_FIELD, "test");
        s.apply(&[
            put(Expect::Exactly(a.clone()), doc("test.a", "a", 3)),
            SecurityOp::remove(u, b.clone()).unwrap(),
            SecurityOp::put(r, Expect::Absent, role.to_bytes().unwrap()).unwrap(),
        ])
        .unwrap();
        assert_eq!(s.list(u, Some("test")), vec![doc("test.a", "a", 3).as_slice()]);
        assert_eq!(s.list(r, None).len(), 1);
        assert!(s.list(u, Some("other")).is_empty());
        let mut e = Encoder::new();
        s.encode(&mut e);
        let bytes = e.finish();
        assert_eq!(Security::decode(&mut Decoder::new(&bytes)), Ok(s.clone()));
        let mut e = Encoder::new();
        s.encode_named(&mut e);
        let bytes = e.finish();
        assert_eq!(Security::decode_named(&mut Decoder::new(&bytes)), Ok(s));
    }

    #[test]
    fn the_insertion_order_stays_and_a_new_insert_goes_last() {
        let mut s = Security::default();
        for n in ["zz", "aa", "mm"] {
            s.apply(&[put(Expect::Absent, doc(format!("t.{n}"), n, 0))]).unwrap();
        }
        let names = |s: &Security| -> Vec<String> {
            s.list(Principal::User, None)
                .into_iter()
                .map(|d| match RawDoc::from_bytes(d).unwrap().get(USER_FIELD) {
                    Some(RawValue::String(t)) => t.to_owned(),
                    _ => String::new(),
                })
                .collect()
        };
        let zz = doc("t.zz", "zz", 0);
        s.apply(&[put(Expect::Exactly(zz), doc("t.zz", "zz", 1))]).unwrap();
        assert_eq!(names(&s), ["zz", "aa", "mm"]);
        s.apply(&[SecurityOp::remove(Principal::User, doc("t.aa", "aa", 0)).unwrap()]).unwrap();
        s.apply(&[put(Expect::Absent, doc("t.aa", "aa", 0))]).unwrap();
        assert_eq!(names(&s), ["zz", "mm", "aa"]);
        // Deleting the last and adding another keeps it after the rest.
        s.apply(&[SecurityOp::remove(Principal::User, doc("t.aa", "aa", 0)).unwrap()]).unwrap();
        s.apply(&[put(Expect::Absent, doc("t.bb", "bb", 0))]).unwrap();
        assert_eq!(names(&s), ["zz", "mm", "bb"]);
        assert_eq!(s.remove(Principal::User, "test", "mm"), Some(doc("t.mm", "mm", 0)));
        assert_eq!(s.remove(Principal::User, "test", "mm"), None);
        s.users.retain(|d| d != doc("t.bb", "bb", 0).as_slice());
        assert_eq!(names(&s), ["zz"]);
        assert_eq!((s.users.len(), s.users.is_empty()), (1, false));
        s.users.clear();
        assert!(s.users.is_empty());
    }

    #[test]
    fn the_unique_index_takes_null_for_missing_fields_and_one_key_per_element() {
        let u = Principal::User;
        let bare = |id: i32| Document::new().with(ID_FIELD, id).to_bytes().unwrap();
        let mut s = Security::default();
        s.apply(&[put(Expect::Absent, bare(1))]).unwrap();
        assert_eq!(s.apply(&[put(Expect::Absent, bare(2))]), Err(SecurityError::Exists(u)));
        // `_id` 7 and 7.0 are the same key.
        s.apply(&[put(Expect::Absent, doc(7, "k1", 0))]).unwrap();
        assert_eq!(
            s.apply(&[put(Expect::Absent, doc(7.0, "k2", 0))]),
            Err(SecurityError::Exists(u))
        );
        let arr = doc("arr", Value::Array(vec!["a1".into(), "a2".into()]), 0);
        s.apply(&[put(Expect::Absent, arr.clone())]).unwrap();
        assert_eq!(s.get(u, "test", "a2"), Some(arr.as_slice()));
        assert_eq!(
            s.apply(&[put(Expect::Absent, doc("x", "a2", 0))]),
            Err(SecurityError::Exists(u))
        );
        let keys = unique_keys(u, &doc("e", Value::Array(vec![]), 0)).unwrap();
        assert_eq!(keys[0].values, [Value::Undefined, Value::from("test")]);
        let dup = doc("d", Value::Array(vec!["q".into(), "q".into()]), 0);
        assert_eq!(unique_keys(u, &dup).unwrap().len(), 1);
        let parallel = Document::new()
            .with(ID_FIELD, 1)
            .with(USER_FIELD, Value::Array(vec!["p".into()]))
            .with(DB_FIELD, Value::Array(vec!["k".into()]));
        assert_eq!(unique_keys(u, &parallel.to_bytes().unwrap()), Err(KeyError::ParallelArrays));
        assert_eq!(id_key(b"junk"), Err(KeyError::Bson));
        assert_eq!(unique_keys(u, b"junk"), Err(KeyError::Bson));
        assert_eq!(id_key(&Document::new().to_bytes().unwrap()), Err(KeyError::NoId));
        // `list` finds a database inside an array, as `{db: …}` does.
        let in_array = Document::new()
            .with(ID_FIELD, "ia")
            .with(USER_FIELD, "ia")
            .with(DB_FIELD, Value::Array(vec!["d1".into(), "d2".into()]));
        s.apply(&[put(Expect::Absent, in_array.to_bytes().unwrap())]).unwrap();
        assert_eq!(s.list(u, Some("d2")).len(), 1);
        assert_eq!(Principal::Role.unique_index(), "role_1_db_1");
        assert_eq!(Principal::User.unique_index(), "user_1_db_1");
    }

    #[test]
    fn a_snapshot_with_clashing_or_bad_documents_does_not_decode() {
        for docs in [vec![b"junk".to_vec()], vec![doc(1, "a", 0), doc(2, "a", 0)]] {
            let mut e = Encoder::new();
            e.u64(1).var(docs.len() as u64);
            for d in &docs {
                e.bytes(d);
            }
            e.var(0);
            let bytes = e.finish();
            assert!(Security::decode(&mut Decoder::new(&bytes)).is_err());
        }
    }

    #[test]
    fn ops_read_back_and_bad_codes_fail() {
        let ops = vec![
            put(Expect::Absent, doc("t.a", "a", 1)),
            SecurityOp { kind: Principal::Role, id: vec![1], expect: Expect::Present, doc: None },
            put(Expect::Exactly(b"2".to_vec()), doc("t.c", "c", 0)),
        ];
        let mut e = Encoder::new();
        encode_ops(&ops, &mut e);
        let bytes = e.finish();
        assert_eq!(decode_ops(&mut Decoder::new(&bytes)), Ok(ops));
        let mut bad_kind = bytes.clone();
        bad_kind[1] = 9;
        assert!(decode_ops(&mut Decoder::new(&bad_kind)).is_err());
        assert!(decode_ops(&mut Decoder::new(&[0x7f])).is_err());
        // A batch of the binary that addressed by name: its `_id` is `db.name`.
        let mut e = Encoder::new();
        e.var(2).u8(USER).str("t").str("a").u8(ABSENT).bool(true).bytes(&doc("t.a", "a", 1));
        e.u8(ROLE).str("t").str("r").u8(9);
        let legacy = e.finish();
        assert!(decode_ops(&mut Decoder::new(&legacy)).is_err());
        let mut e = Encoder::new();
        e.var(1).u8(ROLE).str("t").str("r").u8(PRESENT).bool(false).bytes(&[]);
        let legacy = e.finish();
        let ops = decode_ops(&mut Decoder::new(&legacy)).unwrap();
        let role = Document::new().with(ID_FIELD, "t.r").to_bytes().unwrap();
        assert_eq!(ops[0].id, id_key(&role).unwrap());
        assert_eq!(ops[0].kind, Principal::Role);
    }
}
