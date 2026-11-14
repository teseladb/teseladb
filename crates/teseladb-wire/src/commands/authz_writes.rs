//! Direct writes to `admin.system.users` and `admin.system.roles` (013 FR-030), as the reference
//! router 9.0.4 takes them (measured, 31-10-2026): `insert`, `update`, `delete` and `findAndModify`
//! run the usual semantics on the documents of Group 0 in their insertion order, any document is
//! accepted, a document without `_id` gets an ObjectId, and the indexes `_id_` and
//! `user_1_db_1` (or `role_1_db_1`) refuse duplicates with 11000. The whole command is worked
//! out on a copy of the documents and goes to Group 0 as one batch that expects each touched
//! document as it was read; another change in between makes it start again. Each command that
//! changed something leaves one `directAuthMutation` event in the audit log, as the reference
//! server Enterprise 9.0.4 writes it: the first document inserted or updated, or the last removed.

use teseladb_auth::audit::AuditType;
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::unique::duplicate_key;
use teseladb_meta::catalog::Namespace;
use teseladb_meta::security::{
    Expect, ID_FIELD, KeyError, Principal, SecurityOp, id_key, unique_keys,
};
use teseladb_mql::update::{Update, UpdateContext};
use teseladb_mql::{Filter, FilterUse, MatchContext};

use super::authz_collections::Generated;
use super::crud::{Modify, now_of, upsert_doc, with_id};
use super::mutation::{
    Image, MutationResult, Stored, executor_error, executor_of, from_mql, stored_bytes,
};
use super::names::full;
use super::{Backend, Ctx};
use crate::error::{BAD_VALUE, CANNOT_INDEX_PARALLEL_ARRAYS, CommandError, DUPLICATE_KEY};
use crate::msg::command_limits;
use crate::types::wrong_type;

/// The users or roles a write to `ns` changes, when `ns` is one of their collections.
pub fn kind_of(ns: &Namespace) -> Option<Principal> {
    match Generated::of(ns)? {
        Generated::Users => Some(Principal::User),
        Generated::Roles => Some(Principal::Role),
        Generated::Version => None,
    }
}

/// `operation` of a `directAuthMutation` event.
const INSERT: &str = "insert";
const UPDATE: &str = "update";
const REMOVE: &str = "remove";

/// A document of the collection as the command sees it: as it was read and as it is now.
pub(crate) struct Entry {
    pub(crate) read: Option<Vec<u8>>,
    pub(crate) now: Option<Document>,
}

/// The documents of a collection kept in Group 0 while a command works on a copy of them
/// (`admin.system.users` and `admin.system.roles` here, `config.settings` in
/// [`super::config_settings`]).
pub(crate) trait Staged {
    /// The documents, in their order.
    fn entries(&mut self) -> &mut Vec<Entry>;
    /// The error the reference server answers if `bytes` cannot join the documents other than
    /// `skip`.
    fn check(&self, bytes: &[u8], skip: Option<usize>) -> Result<(), CommandError>;
    /// A document inserted, updated or removed by `operation`.
    fn note(&mut self, operation: &'static str, doc: &Document);
}

impl Staged for Sim {
    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn check(&self, bytes: &[u8], skip: Option<usize>) -> Result<(), CommandError> {
        Sim::check(self, bytes, skip)
    }

    fn note(&mut self, operation: &'static str, doc: &Document) {
        Sim::note(self, operation, doc);
    }
}

/// The documents of one collection while a command works on them.
struct Sim {
    kind: Principal,
    ns: String,
    entries: Vec<Entry>,
    /// The `directAuthMutation` event: operation and document.
    event: Option<(&'static str, Document)>,
}

impl Sim {
    fn load<B: Backend + ?Sized>(backend: &mut B, kind: Principal) -> Result<Sim, CommandError> {
        let mut entries = Vec::new();
        for bytes in backend.principals(kind, None)? {
            let now = Some(Document::from(RawDoc::from_bytes(&bytes)?));
            entries.push(Entry { read: Some(bytes), now });
        }
        let coll = Generated::of_kind(kind)?;
        let ns = full(&Namespace { db: teseladb_auth::roles::ADMIN_DB.into(), coll: coll.into() });
        Ok(Sim { kind, ns, entries, event: None })
    }

    fn note(&mut self, operation: &'static str, doc: &Document) {
        if operation == REMOVE || self.event.is_none() {
            self.event = Some((operation, doc.clone()));
        }
    }

    /// The 11000 or 171 the reference server answers if `bytes` cannot join the documents other
    /// than `skip`.
    fn check(&self, bytes: &[u8], skip: Option<usize>) -> Result<(), CommandError> {
        let id = id_key(bytes).map_err(key_error)?;
        let mine = unique_keys(self.kind, bytes).map_err(|e| match e {
            KeyError::ParallelArrays => CommandError::new(
                CANNOT_INDEX_PARALLEL_ARRAYS,
                format!("cannot index parallel arrays [db] [{}]", self.kind.name_field()),
            ),
            other => key_error(other),
        })?;
        for (i, e) in self.entries.iter().enumerate() {
            let Some(other) = e.now.as_ref().filter(|_| Some(i) != skip) else { continue };
            let other = other.to_bytes()?;
            if id_key(&other).map_err(key_error)? == id {
                let raw = RawDoc::from_bytes(bytes)?;
                let value = raw.get(ID_FIELD).map(Value::from).unwrap_or(Value::Null);
                let pattern = Document::new().with(ID_FIELD, 1);
                return Err(self.duplicate("_id_", &pattern, &[value]));
            }
            let theirs = unique_keys(self.kind, &other).map_err(key_error)?;
            if let Some(k) = mine.iter().find(|k| theirs.iter().any(|t| t.key == k.key)) {
                let pattern = Document::new().with(self.kind.name_field(), 1).with("db", 1);
                return Err(self.duplicate(self.kind.unique_index(), &pattern, &k.values));
            }
        }
        Ok(())
    }

    fn duplicate(&self, index: &str, pattern: &Document, values: &[Value]) -> CommandError {
        match duplicate_key(&self.ns, index, pattern, values) {
            Ok(d) => {
                let mut e = CommandError::new(DUPLICATE_KEY, d.message);
                e.extra.push("keyPattern", d.key_pattern);
                e.extra.push("keyValue", d.key_value);
                e
            }
            Err(e) => CommandError::from(e),
        }
    }

    /// The batch that brings Group 0 to these documents.
    fn ops(&self) -> Result<Vec<SecurityOp>, CommandError> {
        let mut ops = Vec::new();
        for e in &self.entries {
            let now = e.now.as_ref().map(Document::to_bytes).transpose()?;
            let op = match (&e.read, now) {
                (None, Some(doc)) => SecurityOp::put(self.kind, Expect::Absent, doc),
                (Some(read), None) => SecurityOp::remove(self.kind, read.clone()),
                (Some(read), Some(doc)) if read != &doc => {
                    SecurityOp::put(self.kind, Expect::Exactly(read.clone()), doc)
                }
                _ => continue,
            };
            ops.push(op.map_err(key_error)?);
        }
        Ok(ops)
    }
}

impl Generated {
    fn of_kind(kind: Principal) -> Result<&'static str, CommandError> {
        match kind {
            Principal::User => Ok(Generated::Users.name()),
            Principal::Role => Ok(Generated::Roles.name()),
            Principal::ApiKey => Err(super::authz_collections::no_collection()),
        }
    }
}

/// The fields `createUser`, `updateUser` and the role grants write in a user.
pub const USER_FIELDS: [&str; 4] =
    ["credentials", "authenticationRestrictions", "customData", "roles"];
/// The fields `updateRole` and the privilege and role grants write in a role.
pub const ROLE_FIELDS: [&str; 3] = ["privileges", "roles", "authenticationRestrictions"];

/// The stored document `old` with the fields `owned` taken from `new`, as the reference server's
/// `$set` and `$unset` change them: a field it had keeps its place, a new one goes at the end, one
/// `new` does not have goes; `_id` and every other field stay as they were.
pub fn merged(old: &[u8], new: &Document, owned: &[&str]) -> Result<Vec<u8>, CommandError> {
    let mut out = Document::new();
    for (k, v) in RawDoc::from_bytes(old)?.iter() {
        match (owned.contains(&k), new.get(k)) {
            (false, _) => out.push(k, Value::from(v)),
            (true, Some(n)) => out.push(k, n.clone()),
            (true, None) => {}
        }
    }
    let mut added: Vec<&str> =
        owned.iter().copied().filter(|f| out.get(f).is_none() && new.get(f).is_some()).collect();
    added.sort_unstable();
    for f in added {
        if let Some(v) = new.get(f) {
            out.push(f, v.clone());
        }
    }
    Ok(out.to_bytes()?)
}

/// `roles` of stored document `doc` without the elements `{role, db}` of `names` (what
/// `$pull: {roles: {role, db}}` takes, other fields of the element aside); `None` if it takes
/// nothing.
pub fn without_roles(
    doc: &[u8],
    names: &[teseladb_auth::users::RoleName],
) -> Result<Option<Vec<u8>>, CommandError> {
    let d = Document::from(RawDoc::from_bytes(doc)?);
    let Some(Value::Array(roles)) = d.get("roles") else { return Ok(None) };
    let gone = |v: &Value| match v {
        Value::Document(r) => names.iter().any(|n| {
            r.get("role") == Some(&Value::from(n.role.as_str()))
                && r.get("db") == Some(&Value::from(n.db.as_str()))
        }),
        _ => false,
    };
    if !roles.iter().any(gone) {
        return Ok(None);
    }
    let kept: Vec<Value> = roles.iter().filter(|v| !gone(v)).cloned().collect();
    let mut d = d.clone();
    d.insert("roles", Value::Array(kept));
    Ok(Some(d.to_bytes()?))
}

/// The error of a stored document that has no keys: a fault of the node.
pub fn key_error(e: KeyError) -> CommandError {
    CommandError::new(BAD_VALUE, format!("a document that cannot be indexed: {e:?}"))
}

/// Works the command out with `step` on fresh copies until Group 0 takes its batch, then records
/// its `directAuthMutation` event.
fn commit<B: Backend + ?Sized, T>(
    ctx: &mut Ctx<'_, B>,
    kind: Principal,
    mut step: impl FnMut(&mut B, &mut Sim) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    for _ in 0..super::users::MAX_ATTEMPTS {
        let mut sim = Sim::load(ctx.backend, kind)?;
        let out = step(ctx.backend, &mut sim)?;
        let ops = sim.ops()?;
        if !ops.is_empty() && ctx.backend.security_write(ops)?.is_err() {
            continue;
        }
        if let Some((operation, doc)) = sim.event {
            let param = Document::new()
                .with("document", doc)
                .with("ns", sim.ns.as_str())
                .with("operation", operation);
            ctx.backend.audit_event(AuditType::DirectAuthMutation, param);
        }
        return Ok(out);
    }
    Err(super::users::conflict())
}

/// `insert` of `docs`: the reply's `n` and `writeErrors`.
pub fn insert<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    kind: Principal,
    docs: &[Value],
    ordered: bool,
) -> Result<Document, CommandError> {
    let shared = ctx.shared;
    let (n, errors) = commit(ctx, kind, |backend, sim| {
        let (mut n, mut errors) = (0i64, Vec::new());
        for (i, v) in docs.iter().enumerate() {
            let added = match v {
                Value::Document(d) => with_id(backend, shared, d).and_then(|doc| {
                    let bytes = stored_bytes(&doc, Stored::Insert)?;
                    sim.check(&bytes, None)?;
                    Ok(doc)
                }),
                other => Err(wrong_type(&format!("insert.documents.{i}"), other, "object")),
            };
            match added {
                Ok(doc) => {
                    sim.note(INSERT, &doc);
                    sim.entries.push(Entry { read: None, now: Some(doc) });
                    n += 1;
                }
                Err(e) => {
                    errors.push(super::crud::write_error(i, &e));
                    if ordered {
                        break;
                    }
                }
            }
        }
        Ok((n, errors))
    })?;
    let mut reply = Document::new().with("n", super::crud::count(n));
    if !errors.is_empty() {
        reply.push("writeErrors", errors);
    }
    Ok(reply)
}

/// Runs `statements` (already checked; an error is that statement's) as one command: the result
/// of each, up to the first failure of an ordered command.
pub fn modify<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    kind: Principal,
    statements: &[Result<Modify, CommandError>],
    ordered: bool,
) -> Result<Vec<Result<MutationResult, CommandError>>, CommandError> {
    let shared = ctx.shared;
    commit(ctx, kind, |backend, sim| {
        let mut out = Vec::new();
        for st in statements {
            let r = st.clone().and_then(|m| statement(backend, shared, sim, &m));
            let failed = r.is_err();
            out.push(r);
            if failed && ordered {
                break;
            }
        }
        Ok(out)
    })
}

/// A filter, update or projection read within a command's limits.
fn parsed_filter(filter: &Document) -> Result<Filter, CommandError> {
    let bytes = filter.to_bytes_with(command_limits())?;
    let raw = RawDoc::from_bytes_with(&bytes, command_limits())?;
    Filter::parse(raw, FilterUse::Query).map_err(from_mql)
}

fn parsed_update(m: &Modify) -> Result<Option<Update>, CommandError> {
    let Some(u) = &m.update else { return Ok(None) };
    let holder = Document::new()
        .with("u", u.clone())
        .with("af", m.array_filters.clone().map_or(Value::Null, Value::Array));
    let bytes = holder.to_bytes_with(command_limits())?;
    let h = RawDoc::from_bytes_with(&bytes, command_limits())?;
    let af = match h.get("af") {
        Some(teseladb_bson::RawValue::Array(a)) => Some(a),
        _ => None,
    };
    let u = h.get("u").ok_or(CommandError::new(BAD_VALUE, "bad update"))?;
    Update::parse(u, af).map(Some).map_err(from_mql)
}

/// One update or delete statement on the documents.
pub(crate) fn statement<B: Backend + ?Sized, S: Staged>(
    backend: &mut B,
    shared: &super::Shared,
    sim: &mut S,
    m: &Modify,
) -> Result<MutationResult, CommandError> {
    let filter = parsed_filter(&m.filter)?;
    let update = parsed_update(m)?;
    let during = executor_error(executor_of(m.image));
    let mctx = MatchContext {
        collation: None,
        ext: &super::engines::NodeExtensions,
        vars: m.vars.as_ref(),
    };
    let mut order: Vec<usize> =
        (0..sim.entries().len()).filter(|i| sim.entries()[*i].now.is_some()).collect();
    if let Some(sort) = &m.sort {
        let mut keyed = Vec::new();
        for i in order {
            let bytes = sim.entries()[i].now.as_ref().map(Document::to_bytes).transpose()?;
            let bytes = bytes.unwrap_or_default();
            keyed.push((sort.keys(RawDoc::from_bytes(&bytes)?, None), i));
        }
        keyed.sort_by(|a, b| sort.compare_keys(&a.0, &b.0, None));
        order = keyed.into_iter().map(|(_, i)| i).collect();
    }
    let mut r = MutationResult::default();
    for i in order {
        let Some(doc) = sim.entries()[i].now.clone() else { continue };
        let bytes = doc.to_bytes()?;
        let raw = RawDoc::from_bytes(&bytes)?;
        let position = filter.match_position(raw, &mctx).map_err(from_mql);
        let position = match (&update, position) {
            (Some(_), Err(e)) => return Err(during(e)),
            (_, p) => p?,
        };
        let Some(position) = position else { continue };
        r.n += 1;
        match &update {
            None => {
                sim.note(REMOVE, &doc);
                sim.entries()[i].now = None;
                if m.image != Image::None {
                    r.value = Some(doc);
                }
            }
            Some(u) => {
                let uctx =
                    UpdateContext { matcher: mctx, position, now: now_of(backend), insert: false };
                let out = u.apply(raw, &uctx).map_err(from_mql).map_err(&during)?;
                if out.modified {
                    let new = stored_bytes(&out.doc, Stored::Update).map_err(&during)?;
                    sim.check(&new, Some(i)).map_err(&during)?;
                    sim.note(UPDATE, &out.doc);
                    sim.entries()[i].now = Some(out.doc.clone());
                    r.n_modified += 1;
                }
                r.value = match m.image {
                    Image::None => None,
                    Image::Before => Some(doc),
                    Image::After => Some(out.doc),
                };
            }
        }
        if !m.multi {
            break;
        }
    }
    if let (0, true, Some(u)) = (r.n, m.upsert, &m.update) {
        let op = executor_of(m.image);
        let doc = upsert_doc(backend, shared, op, (m, None), u)?;
        let bytes = stored_bytes(&doc, Stored::Upsert).map_err(&during)?;
        sim.check(&bytes, None).map_err(&during)?;
        sim.note(INSERT, &doc);
        r.upserted = Some(doc.get(ID_FIELD).cloned().unwrap_or(Value::Null));
        if m.image == Image::After {
            r.value = Some(doc.clone());
        }
        sim.entries().push(Entry { read: None, now: Some(doc) });
    }
    Ok(r)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_document_without_keys_is_a_fault_of_the_node() {
        let e = key_error(KeyError::NoId);
        assert_eq!(e.code, BAD_VALUE);
        assert!(e.errmsg.contains("NoId"), "{e:?}");
    }
}
