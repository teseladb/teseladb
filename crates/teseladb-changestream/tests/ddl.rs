//! 014 T017: the catalog events of a stream (FR-002, FR-014), as a `refrouter` 9.0.4 gives them
//! (measured on 01-11).

use teseladb_changestream::ddl::{ddl_key, index_change, invalidate_of, invalidates, shown};
use teseladb_changestream::stage::Scope;
use teseladb_changestream::token::{EventKind, ResumeToken};
use teseladb_hlc::Hlc;
use teseladb_bson::{Document, Value};

#[test]
fn a_catalog_change_goes_by_its_mark_and_position_before_the_documents_of_that_mark() {
    let mark = Hlc { physical: 5, logical: 1 };
    let k = ddl_key(mark, 2);
    assert_eq!((k.ts, k.txn, k.op, k.key.len()), (mark, 0, 2, 0));
    assert!(ddl_key(mark, 1) < k);
    let token = ResumeToken::new(k.clone(), EventKind::Drop);
    let inv = invalidate_of(&token);
    assert_eq!((inv.event, inv.kind), (k, EventKind::Invalidate));
}

#[test]
fn only_drop_rename_and_drop_database_show_without_expanded_events() {
    for kind in [
        EventKind::Insert,
        EventKind::Drop,
        EventKind::Rename,
        EventKind::DropDatabase,
        EventKind::Invalidate,
    ] {
        assert!(shown(kind, false), "{kind:?}");
    }
    for kind in
        [EventKind::Create, EventKind::CreateIndexes, EventKind::DropIndexes, EventKind::Modify]
    {
        assert!(!shown(kind, false) && shown(kind, true), "{kind:?}");
    }
}

#[test]
fn a_drop_or_rename_ends_a_collection_stream_and_a_drop_database_a_database_stream() {
    let w = ("db", Some("c"));
    assert!(invalidates(Scope::Collection, w, EventKind::Drop, ("db", Some("c"))));
    assert!(invalidates(Scope::Collection, w, EventKind::Rename, ("db", Some("c"))));
    assert!(!invalidates(Scope::Collection, w, EventKind::Drop, ("db", Some("d"))));
    assert!(!invalidates(Scope::Collection, w, EventKind::Drop, ("other", Some("c"))));
    assert!(!invalidates(Scope::Collection, w, EventKind::DropDatabase, ("db", None)));
    assert!(!invalidates(Scope::Collection, w, EventKind::Modify, ("db", Some("c"))));
    assert!(!invalidates(Scope::Collection, w, EventKind::Drop, ("db", None)));
    let d = ("db", None);
    assert!(invalidates(Scope::Database, d, EventKind::DropDatabase, ("db", None)));
    assert!(!invalidates(Scope::Database, d, EventKind::DropDatabase, ("x", None)));
    assert!(!invalidates(Scope::Database, d, EventKind::Drop, ("db", Some("c"))));
    assert!(!invalidates(Scope::Cluster, ("admin", None), EventKind::DropDatabase, ("db", None)));
}

#[test]
fn an_index_change_names_the_index_with_its_new_values_and_the_old_ones() {
    let key = Document::new().with("a", 1);
    let before = Document::new()
        .with("v", 2)
        .with("key", key.clone())
        .with("name", "a_1")
        .with("expireAfterSeconds", 10);
    let after = before.clone().with("hidden", true);
    let (index, options) = index_change(&before, &after);
    assert_eq!(index, Document::new().with("name", "a_1").with("hidden", true));
    assert_eq!(options, Document::new().with("hidden", false));
    let mut ttl = before.clone();
    ttl.insert("expireAfterSeconds", Value::Int32(20));
    let (index, options) = index_change(&before, &ttl);
    assert_eq!(index, Document::new().with("name", "a_1").with("expireAfterSeconds", 20));
    assert_eq!(options, Document::new().with("expireAfterSeconds", 10));
    let fresh =
        Document::new().with("v", 2).with("key", key).with("name", "a_1").with("sparse", true);
    let (_, options) = index_change(&Document::new(), &fresh);
    assert_eq!(options, Document::new(), "an option absent before and not a flag has no old value");
    let unnamed = Document::new().with("v", 2).with("hidden", true);
    let (index, _) = index_change(&Document::new(), &unnamed);
    assert_eq!(index, Document::new().with("hidden", true), "a specification without a name");
}
