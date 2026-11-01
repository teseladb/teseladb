//! The catalog events of a stream (014 FR-002, FR-014): where they go in the merge, which of
//! them a stream shows, which end it (`invalidate`) and what `collMod` of an index describes. The
//! catalog changes come from Group 0 with the mark of their entry and their position in it; a
//! `refrouter` 9.0.4 shows `drop`, `rename` and `dropDatabase` to every stream and the other types
//! only with `showExpandedEvents`; the `drop` or `rename` of a watched collection and the
//! `dropDatabase` of a watched database are followed by an `invalidate` that closes the cursor,
//! with the same place in the merge (measured on 01-11).

use teseladb_bson::{Document, Value};
use teseladb_hlc::Hlc;

use crate::order::EventKey;
use crate::stage::Scope;
use crate::token::{EventKind, ResumeToken};

/// Where catalog change `op` of the Group 0 entry at `mark` goes in the merge.
pub fn ddl_key(mark: Hlc, op: u32) -> EventKey {
    EventKey { ts: mark, txn: 0, op, key: Vec::new() }
}

/// Whether a stream shows an event of type `kind` with `expanded` (`showExpandedEvents`).
pub fn shown(kind: EventKind, expanded: bool) -> bool {
    expanded
        || matches!(
            kind,
            EventKind::Insert
                | EventKind::Update
                | EventKind::Replace
                | EventKind::Delete
                | EventKind::Drop
                | EventKind::Rename
                | EventKind::DropDatabase
                | EventKind::Invalidate
        )
}

/// Whether event `kind` on database `db` (collection `coll` for a collection's event) ends a
/// stream of `scope` opened on `watched_db` (and `watched_coll`).
pub fn invalidates(
    scope: Scope,
    (watched_db, watched_coll): (&str, Option<&str>),
    kind: EventKind,
    (db, coll): (&str, Option<&str>),
) -> bool {
    match scope {
        Scope::Collection => {
            matches!(kind, EventKind::Drop | EventKind::Rename)
                && db == watched_db
                && coll.is_some()
                && coll == watched_coll
        }
        Scope::Database => kind == EventKind::DropDatabase && db == watched_db,
        Scope::Cluster => false,
    }
}

/// The `invalidate` that follows the event of `token`: the same place in the merge.
pub fn invalidate_of(token: &ResumeToken) -> ResumeToken {
    ResumeToken::new(token.event.clone(), EventKind::Invalidate)
}

/// Options of an index whose absence means `false`.
const FALSE_BY_DEFAULT: [&str; 3] = ["hidden", "prepareUnique", "unique"];

/// What `collMod` of an index changed, from its specification `before` and `after`: the
/// `operationDescription.index` (its name and the new values) and the `stateBeforeChange
/// .indexOptions` (the values before, `false` for an absent flag). Measured: hiding `a_1` gives
/// `{name: "a_1", hidden: true}` and `{hidden: false}`.
pub fn index_change(before: &Document, after: &Document) -> (Document, Document) {
    let mut index = Document::new();
    if let Some(name) = after.get("name") {
        index.push("name", name.clone());
    }
    let mut options = Document::new();
    for (k, v) in after.iter().filter(|(k, _)| !matches!(*k, "name" | "key" | "v")) {
        if before.get(k) == Some(v) {
            continue;
        }
        index.push(k, v.clone());
        match before.get(k) {
            Some(old) => options.push(k, old.clone()),
            None if FALSE_BY_DEFAULT.contains(&k) => options.push(k, Value::Boolean(false)),
            None => {}
        }
    }
    (index, options)
}
