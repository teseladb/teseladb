//! `admin.system.users`, `admin.system.roles` and `admin.system.version` generated from Group 0,
//! `getParameter` of `authSchemaVersion` and `_mergeAuthzCollections`, which the reference dump
//! tool --dumpDbUsersAndRoles` and the reference restore tool --restoreDbUsersAndRoles` use (013
//! T043, FR-030), as the reference server 9.0.4 answers them (measured on 31-10-2026 on a reference
//! replica set member and the reference router).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_meta::security::Principal;

const ADMIN: &str = "admin";
const APP: &str = "app";

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn s(x: &str) -> Value {
    Value::from(x)
}

/// Users `app.u1` (roles `read` and `r1`, custom data) and `other.o1`, and role `app.r1`.
fn seeded() -> Mem {
    let mut m = Mem::new();
    let role = json(
        r#"{"createRole": "r1", "privileges": [{"resource": {"db": "app", "collection": "c"}, "actions": ["find"]}], "roles": []}"#,
    );
    assert_eq!(field(&m.run(APP, role), "ok"), &Value::Double(1.0));
    let u1 = json(
        r#"{"createUser": "u1", "pwd": "p1", "roles": ["read", "r1"], "customData": {"k": 1}}"#,
    );
    assert_eq!(field(&m.run(APP, u1), "ok"), &Value::Double(1.0));
    let o1 = json(r#"{"createUser": "o1", "pwd": "p1", "roles": []}"#);
    assert_eq!(field(&m.run("other", o1), "ok"), &Value::Double(1.0));
    m
}

fn first_batch(r: &Document) -> Vec<Document> {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?} in {r:?}"),
    }
}

fn ids(docs: &[Document]) -> Vec<String> {
    docs.iter()
        .map(|d| match d.get("_id") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn keys(d: &Document) -> Vec<String> {
    d.iter().map(|(k, _)| k.to_owned()).collect()
}

fn error(r: &Document) -> (i32, String, String) {
    let code = match field(r, "code") {
        Value::Int32(n) => *n,
        other => panic!("{other:?}"),
    };
    let name = match field(r, "codeName") {
        Value::String(x) => x.clone(),
        other => panic!("{other:?}"),
    };
    let msg = match field(r, "errmsg") {
        Value::String(x) => x.clone(),
        other => panic!("{other:?}"),
    };
    (code, name, msg)
}

#[test]
fn the_users_and_roles_read_as_collections_of_admin() {
    let mut m = seeded();
    let r = m.run(
        ADMIN,
        json(r#"{"find": "system.users", "filter": {}, "projection": {"credentials": 0}}"#),
    );
    assert_eq!(field(&r, "cursor.ns"), &s("admin.system.users"));
    assert_eq!(field(&r, "cursor.id"), &Value::Int64(0));
    let users = first_batch(&r);
    assert_eq!(ids(&users), ["app.u1", "other.o1"]);
    assert_eq!(keys(&users[0]), ["_id", "userId", "user", "db", "customData", "roles"]);
    assert_eq!(keys(&users[1]), ["_id", "userId", "user", "db", "roles"]);
    assert_eq!(field(&users[0], "customData.k"), &Value::Int32(1));
    // The documents carry the derived credentials, never a password.
    let r = m.run(ADMIN, json(r#"{"find": "system.users", "filter": {"db": "app"}}"#));
    let u1 = &first_batch(&r)[0];
    assert!(matches!(field(u1, "credentials.SCRAM-SHA-256.storedKey"), Value::String(_)));
    assert!(!format!("{u1:?}").contains("p1\""));
    let r = m.run(
        ADMIN,
        json(r#"{"find": "system.users", "filter": {"db": "app"}, "projection": {"_id": 1}}"#),
    );
    assert_eq!(first_batch(&r), [json(r#"{"_id": "app.u1"}"#)]);
    let r = m.run(ADMIN, json(r#"{"find": "system.roles", "filter": {}}"#));
    assert_eq!(
        first_batch(&r),
        [json(
            r#"{"_id": "app.r1", "role": "r1", "db": "app", "privileges": [{"resource": {"db": "app", "collection": "c"}, "actions": ["find"]}], "roles": []}"#
        )]
    );
    let r = m.run(ADMIN, json(r#"{"find": "system.version", "filter": {}}"#));
    assert_eq!(
        first_batch(&r),
        [json(r#"{"_id": "featureCompatibilityVersion", "version": "9.0"}"#)]
    );
    // Only `admin` has them.
    let r = m.run(APP, json(r#"{"find": "system.users", "filter": {}}"#));
    assert_eq!(field(&r, "cursor.ns"), &s("app.system.users"));
    assert!(first_batch(&r).is_empty());
}

#[test]
fn sorted_reads_with_batches_counts_and_distinct_values() {
    let mut m = seeded();
    let r = m.run(
        ADMIN,
        json(r#"{"find": "system.users", "filter": {}, "sort": {"user": -1}, "projection": {"user": 1}, "batchSize": 1}"#),
    );
    assert_eq!(first_batch(&r), [json(r#"{"_id": "app.u1", "user": "u1"}"#)]);
    let id = field(&r, "cursor.id").clone();
    assert_ne!(id, Value::Int64(0));
    let more = m.run(ADMIN, doc(&[("getMore", id), ("collection", s("system.users"))]));
    assert_eq!(
        field(&more, "cursor.nextBatch"),
        &Value::Array(vec![Value::Document(json(r#"{"_id": "other.o1", "user": "o1"}"#))])
    );
    let r = m.run(ADMIN, json(r#"{"find": "system.users", "filter": {}, "skip": 1, "limit": 1, "projection": {"_id": 1}}"#));
    assert_eq!(first_batch(&r), [json(r#"{"_id": "other.o1"}"#)]);
    let count = |m: &mut Mem, cmd: &str| field(&m.run(ADMIN, json(cmd)), "n").clone();
    assert_eq!(count(&mut m, r#"{"count": "system.users"}"#), Value::Int32(2));
    assert_eq!(
        count(&mut m, r#"{"count": "system.users", "query": {"db": "app"}}"#),
        Value::Int32(1)
    );
    assert_eq!(count(&mut m, r#"{"count": "system.users", "skip": 1}"#), Value::Int32(1));
    assert_eq!(count(&mut m, r#"{"count": "system.roles"}"#), Value::Int32(1));
    assert_eq!(count(&mut m, r#"{"count": "system.version"}"#), Value::Int32(1));
    let r = m.run(ADMIN, json(r#"{"distinct": "system.users", "key": "db"}"#));
    assert_eq!(field(&r, "values"), &Value::Array(vec![s("app"), s("other")]));
    let r = m.run(
        ADMIN,
        json(r#"{"distinct": "system.users", "key": "roles.role", "query": {"db": "app"}}"#),
    );
    assert_eq!(field(&r, "values"), &Value::Array(vec![s("r1"), s("read")]));
    // A limit bounds the count; a collation compares as it says.
    assert_eq!(count(&mut m, r#"{"count": "system.users", "limit": 1}"#), Value::Int32(1));
    let r = m.run(
        ADMIN,
        json(r#"{"find": "system.users", "filter": {"user": "U1"}, "projection": {"_id": 1}, "collation": {"locale": "en", "strength": 1}}"#),
    );
    assert_eq!(first_batch(&r), [json(r#"{"_id": "app.u1"}"#)]);
    let r = m.run(
        ADMIN,
        json(r#"{"distinct": "system.users", "key": "db", "query": {"user": "O1"}, "collation": {"locale": "en", "strength": 2}}"#),
    );
    assert_eq!(field(&r, "values"), &Value::Array(vec![s("other")]));
}

#[test]
fn their_indexes_and_their_entries_in_list_collections() {
    let mut m = Mem::new();
    // Before the first user and the first role only `system.version` exists.
    let r = m.run(ADMIN, json(r#"{"listCollections": 1, "nameOnly": true}"#));
    assert_eq!(first_batch(&r), [json(r#"{"name": "system.version", "type": "collection"}"#)]);
    let mut m = seeded();
    let r = m.run(ADMIN, json(r#"{"listCollections": 1, "nameOnly": true}"#));
    let names: Vec<Value> = first_batch(&r).iter().map(|d| field(d, "name").clone()).collect();
    assert_eq!(names, [s("system.users"), s("system.version"), s("system.roles")]);
    let r = m.run(ADMIN, json(r#"{"listCollections": 1, "filter": {"name": "system.roles"}}"#));
    let entry = &first_batch(&r)[0];
    assert_eq!(keys(entry), ["name", "type", "options", "info", "idIndex"]);
    assert_eq!(field(entry, "info.readOnly"), &Value::Boolean(false));
    assert!(matches!(field(entry, "info.uuid"), Value::Binary { subtype: 4, .. }));
    assert_eq!(
        field(entry, "idIndex"),
        &Value::Document(json(r#"{"v": 2, "key": {"_id": 1}, "name": "_id_"}"#))
    );
    let r = m.run(APP, json(r#"{"listCollections": 1, "nameOnly": true}"#));
    assert!(first_batch(&r).iter().all(|d| field(d, "name") != &s("system.users")));
    let indexes =
        |m: &mut Mem, coll: &str| first_batch(&m.run(ADMIN, doc(&[("listIndexes", s(coll))])));
    assert_eq!(
        indexes(&mut m, "system.users"),
        [
            json(r#"{"v": 2, "key": {"_id": 1}, "name": "_id_"}"#),
            json(r#"{"v": 2, "key": {"user": 1, "db": 1}, "name": "user_1_db_1", "unique": true}"#),
        ]
    );
    assert_eq!(
        indexes(&mut m, "system.roles"),
        [
            json(r#"{"v": 2, "key": {"_id": 1}, "name": "_id_"}"#),
            json(r#"{"v": 2, "key": {"role": 1, "db": 1}, "name": "role_1_db_1", "unique": true}"#),
        ]
    );
    assert_eq!(
        indexes(&mut m, "system.version"),
        [json(r#"{"v": 2, "key": {"_id": 1}, "name": "_id_"}"#)]
    );
}

#[test]
fn the_auth_schema_version_is_5_and_cannot_change() {
    let mut m = Mem::new();
    let r = m.run(ADMIN, json(r#"{"getParameter": 1, "authSchemaVersion": 1}"#));
    assert_eq!(r, json(r#"{"authSchemaVersion": 5, "ok": 1.0}"#));
    let r = m.run(ADMIN, json(r#"{"setParameter": 1, "authSchemaVersion": 5}"#));
    assert_eq!(
        error(&r),
        (
            20,
            "IllegalOperation".into(),
            "not allowed to change [authSchemaVersion] at runtime".into()
        )
    );
}

/// Copies the stored users and roles of `app` into `admin.tempusers` and `admin.temproles`,
/// as the reference restore tool does with the files of a dump, with `edit` applied to the copies.
fn copy_to_temp(m: &mut Mem, edit: impl Fn(&mut Vec<Document>, &mut Vec<Document>)) {
    let mut users =
        first_batch(&m.run(ADMIN, json(r#"{"find": "system.users", "filter": {"db": "app"}}"#)));
    let mut roles =
        first_batch(&m.run(ADMIN, json(r#"{"find": "system.roles", "filter": {"db": "app"}}"#)));
    edit(&mut users, &mut roles);
    for (coll, docs) in [("tempusers", users), ("temproles", roles)] {
        let docs: Vec<Value> = docs.into_iter().map(Value::Document).collect();
        if docs.is_empty() {
            continue;
        }
        let r = m.run(ADMIN, doc(&[("insert", s(coll)), ("documents", Value::Array(docs))]));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
}

fn merge(m: &mut Mem, drop: bool, db: &str) -> Document {
    m.run(
        ADMIN,
        doc(&[
            ("_mergeAuthzCollections", 1.into()),
            ("tempUsersCollection", s("admin.tempusers")),
            ("tempRolesCollection", s("admin.temproles")),
            ("drop", Value::Boolean(drop)),
            ("db", s(db)),
        ]),
    )
}

fn names(m: &Mem, kind: Principal) -> Vec<String> {
    m.security
        .list(kind, None)
        .into_iter()
        .map(|d| {
            let d = teseladb_bson::RawDoc::from_bytes(d).unwrap();
            let text = |f| match d.get(f) {
                Some(teseladb_bson::RawValue::String(t)) => t.to_owned(),
                _ => String::new(),
            };
            format!("{}.{}", text("db"), text(kind.name_field()))
        })
        .collect()
}

fn custom_k(m: &mut Mem, id: &str) -> Value {
    let r = m.run(
        ADMIN,
        doc(&[("find", s("system.users")), ("filter", Value::Document(doc(&[("_id", s(id))])))]),
    );
    field(&first_batch(&r)[0], "customData.k").clone()
}

fn with_custom_k(d: &Document, k: i32) -> Document {
    let mut out = Document::new();
    for (key, v) in d.iter() {
        match key {
            "customData" => out.push(key, Value::Document(doc(&[("k", k.into())]))),
            _ => out.push(key, v.clone()),
        }
    }
    out
}

fn renamed(d: &Document, user: &str) -> Document {
    let mut out = Document::new();
    for (key, v) in d.iter() {
        match key {
            "_id" => out.push(key, s(&format!("app.{user}"))),
            "user" => out.push(key, s(user)),
            "userId" => {}
            _ => out.push(key, v.clone()),
        }
    }
    out
}

#[test]
fn merging_without_drop_only_adds_what_is_missing() {
    let mut m = seeded();
    copy_to_temp(&mut m, |users, roles| {
        let changed = with_custom_k(&users[0], 2);
        let added = renamed(&users[0], "u3");
        *users = vec![changed, added];
        roles[0].push("authenticationRestrictions", Value::Array(Vec::new()));
    });
    let r = merge(&mut m, false, APP);
    assert_eq!(r, json(r#"{"ok": 1.0}"#));
    assert_eq!(names(&m, Principal::User), ["app.u1", "other.o1", "app.u3"]);
    // The user that was already there keeps what it had.
    assert_eq!(custom_k(&mut m, "app.u1"), Value::Int32(1));
    assert_eq!(custom_k(&mut m, "app.u3"), Value::Int32(1));
    let u3 = m.security.get(Principal::User, APP, "u3").expect("u3").to_vec();
    let u3 = teseladb_auth::users::User::from_document(&Document::from(
        teseladb_bson::RawDoc::from_bytes(&u3).unwrap(),
    ))
    .unwrap();
    assert_eq!(u3.user_id, None, "a merged user is stored as given, without a userId");
    let roles = first_batch(&m.run(ADMIN, json(r#"{"find": "system.roles", "filter": {}}"#)));
    assert!(roles[0].get("authenticationRestrictions").is_none());
    // The reference router refuses to drop collections of `admin`, the temporary ones too (measured
    // on 9.0.4): the reference restore tool leaves them there.
    for coll in ["tempusers", "temproles", "other"] {
        let r = m.run(ADMIN, doc(&[("drop", s(coll))]));
        let msg = "Cannot drop collection in 'admin' database in sharded cluster";
        assert_eq!(error(&r), (20, "IllegalOperation".to_owned(), msg.to_owned()), "{coll}");
    }
}

#[test]
fn merging_with_drop_makes_the_database_match_the_temporary_collections() {
    let mut m = seeded();
    let u2 = json(r#"{"createUser": "u2", "pwd": "p", "roles": []}"#);
    m.run(APP, u2);
    let ro = json(r#"{"createRole": "ro", "privileges": [], "roles": []}"#);
    m.run("other", ro);
    copy_to_temp(&mut m, |users, roles| {
        let u1 = users.iter().find(|u| u.get("user") == Some(&s("u1"))).cloned().unwrap();
        let other = renamed(&u1, "u3");
        *users = vec![with_custom_k(&u1, 2), other];
        let mut r1 = Document::new();
        for (k, v) in roles[0].iter() {
            r1.push(k, if k == "privileges" { Value::Array(Vec::new()) } else { v.clone() });
        }
        *roles = vec![r1];
    });
    let r = merge(&mut m, true, APP);
    assert_eq!(r, json(r#"{"ok": 1.0}"#));
    // `u2` of `app` goes; `other.o1` and `other.ro` stay out of it.
    assert_eq!(names(&m, Principal::User), ["app.u1", "other.o1", "app.u3"]);
    assert_eq!(custom_k(&mut m, "app.u1"), Value::Int32(2));
    assert_eq!(names(&m, Principal::Role), ["app.r1", "other.ro"]);
    let r1 = &first_batch(
        &m.run(ADMIN, json(r#"{"find": "system.roles", "filter": {"_id": "app.r1"}}"#)),
    )[0];
    assert_eq!(field(r1, "privileges"), &Value::Array(Vec::new()));
    // A temporary document of another database is left out.
    m.run(ADMIN, json(r#"{"delete": "tempusers", "deletes": [{"q": {}, "limit": 0}]}"#));
    let stray =
        json(r#"{"_id": "app.u9", "user": "u9", "db": "zzz", "credentials": {}, "roles": []}"#);
    m.run(
        ADMIN,
        doc(&[
            ("insert", s("tempusers")),
            ("documents", Value::Array(vec![Value::Document(stray)])),
        ]),
    );
    assert_eq!(merge(&mut m, false, APP), json(r#"{"ok": 1.0}"#));
    assert_eq!(names(&m, Principal::User), ["app.u1", "other.o1", "app.u3"]);
    // With every database and drop, only what the temporary collections hold is left.
    assert_eq!(merge(&mut m, true, ""), json(r#"{"ok": 1.0}"#));
    assert_eq!(names(&m, Principal::User), ["zzz.u9"]);
    assert_eq!(names(&m, Principal::Role), ["app.r1"]);
}

#[test]
fn a_missing_temporary_collection_counts_as_empty() {
    let mut m = seeded();
    let r = m.run(
        ADMIN,
        json(r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.nope", "drop": true, "db": "app"}"#),
    );
    assert_eq!(r, json(r#"{"ok": 1.0}"#));
    assert_eq!(names(&m, Principal::User), ["other.o1"]);
    // Roles were not named: they stay.
    assert_eq!(names(&m, Principal::Role), ["app.r1"]);
}

#[test]
fn merge_errors_as_the_reference() {
    let mut m = seeded();
    let cases: [(&str, &str, (i32, &str, &str)); 9] = [
        (
            APP,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.tempusers", "drop": false, "db": "app"}"#,
            (
                13,
                "Unauthorized",
                "_mergeAuthzCollections may only be run against the admin database.",
            ),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "drop": false, "db": "app"}"#,
            (
                2,
                "BadValue",
                "Must provide at least one of \"tempUsersCollection\" and \"tempRolesCollection\"",
            ),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "", "tempRolesCollection": "", "drop": false, "db": "app"}"#,
            (
                2,
                "BadValue",
                "Must provide at least one of \"tempUsersCollection\" and \"tempRolesCollection\"",
            ),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.tempusers", "drop": false}"#,
            (
                40414,
                "IDLFailedToParse",
                "BSON field '_mergeAuthzCollections.db' is missing but a required field",
            ),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": 5, "drop": false, "db": "app"}"#,
            (
                14,
                "TypeMismatch",
                "BSON field '_mergeAuthzCollections.tempUsersCollection' is the wrong type 'int', expected type 'string'",
            ),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.tempusers", "drop": "x", "db": "app"}"#,
            (
                14,
                "TypeMismatch",
                "BSON field '_mergeAuthzCollections.drop' is the wrong type 'string', expected type 'bool'",
            ),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.tempusers", "drop": false, "db": 5}"#,
            (
                14,
                "TypeMismatch",
                "BSON field '_mergeAuthzCollections.db' is the wrong type 'int', expected type 'string'",
            ),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "tempusers", "drop": false, "db": "app"}"#,
            (73, "InvalidNamespace", "Invalid namespace specified: tempusers"),
        ),
        (
            ADMIN,
            r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.", "drop": false, "db": "app"}"#,
            (73, "InvalidNamespace", "Invalid namespace specified: admin"),
        ),
    ];
    for (db, cmd, (code, name, msg)) in cases {
        let r = m.run(db, json(cmd));
        assert_eq!(error(&r), (code, name.to_owned(), msg.to_owned()), "{cmd}");
    }
    assert_eq!(names(&m, Principal::User), ["app.u1", "other.o1"]);
}

#[test]
fn malformed_temporary_documents_change_nothing() {
    let cred = r#"{"SCRAM-SHA-256": {"iterationCount": 15000, "salt": "3oRZALmEZEsGFtMP+d141FjZWbtBqPEgu297EQ==", "storedKey": "Aroy9y+RhG1wBkeaAvDadZ3XOCXHHzVImHe59k+zKQQ=", "serverKey": "P2rsqns20CktpqIq7psdyxx2k7LpjVPaGlTq8kEQA5g="}}"#;
    let users: [(String, (i32, &str, &str)); 7] = [
        (
            r#"{"_id": "t.a", "user": "a", "db": "t", "roles": []}"#.into(),
            (10065, "Location10065", "invalid parameter: expected an object ()"),
        ),
        (
            format!(r#"{{"_id": "t.a", "user": "a", "db": "t", "credentials": {cred}}}"#),
            (10065, "Location10065", "invalid parameter: expected an object ()"),
        ),
        (
            format!(r#"{{"_id": "t.a", "db": "t", "credentials": {cred}, "roles": []}}"#),
            (4, "NoSuchKey", "Missing expected field \"user\""),
        ),
        (
            format!(r#"{{"_id": "t.a", "user": "a", "credentials": {cred}, "roles": []}}"#),
            (4, "NoSuchKey", "Missing expected field \"db\""),
        ),
        (
            r#"{"_id": "t.a", "user": "a", "db": "t", "credentials": "x", "roles": []}"#.into(),
            (10065, "Location10065", "invalid parameter: expected an object (credentials)"),
        ),
        (
            format!(
                r#"{{"_id": "t.a", "user": "a", "db": "t", "credentials": {cred}, "roles": [{{"role": "read"}}]}}"#
            ),
            (4, "NoSuchKey", "Missing expected field \"db\""),
        ),
        (
            format!(
                r#"{{"_id": "t.a", "user": 5, "db": "t", "credentials": {cred}, "roles": []}}"#
            ),
            (14, "TypeMismatch", "\"user\" had the wrong type. Expected string, found int"),
        ),
    ];
    for (text, (code, name, msg)) in users {
        let mut m = seeded();
        m.run(
            ADMIN,
            doc(&[
                ("insert", s("tempusers")),
                ("documents", Value::Array(vec![Value::Document(json(&text))])),
            ]),
        );
        let r = m.run(ADMIN, json(r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.tempusers", "drop": true, "db": ""}"#));
        assert_eq!(error(&r), (code, name.to_owned(), msg.to_owned()), "{text}");
        assert_eq!(names(&m, Principal::User), ["app.u1", "other.o1"], "{text}");
    }
    let roles: [(&str, (i32, &str, &str)); 5] = [
        (
            r#"{"_id": "t.r", "role": "r", "db": "t", "roles": []}"#,
            (10065, "Location10065", "invalid parameter: expected an object ()"),
        ),
        (
            r#"{"_id": "t.r", "role": "r", "db": "t", "privileges": []}"#,
            (10065, "Location10065", "invalid parameter: expected an object ()"),
        ),
        (
            r#"{"_id": "t.r", "db": "t", "privileges": [], "roles": []}"#,
            (4, "NoSuchKey", "Missing expected field \"role\""),
        ),
        (
            r#"{"_id": "t.r", "role": "r", "db": "t", "privileges": [{"resource": {"db": "t", "collection": ""}, "actions": ["bogusAction"]}], "roles": []}"#,
            (9, "FailedToParse", "Unrecognized action privilege strings: bogusAction"),
        ),
        (
            r#"{"_id": "t.r", "role": "r", "db": "t", "privileges": [{"resource": {"db": "t"}, "actions": ["find"]}], "roles": []}"#,
            (
                9,
                "FailedToParse",
                "resource pattern must contain 'collection' or 'systemBuckets' specifier",
            ),
        ),
    ];
    for (text, (code, name, msg)) in roles {
        let mut m = seeded();
        m.run(
            ADMIN,
            doc(&[
                ("insert", s("temproles")),
                ("documents", Value::Array(vec![Value::Document(json(text))])),
            ]),
        );
        let r = m.run(ADMIN, json(r#"{"_mergeAuthzCollections": 1, "tempRolesCollection": "admin.temproles", "drop": true, "db": ""}"#));
        assert_eq!(error(&r), (code, name.to_owned(), msg.to_owned()), "{text}");
        assert_eq!(names(&m, Principal::Role), ["app.r1"], "{text}");
    }
}

#[test]
fn external_users_merge_with_their_external_credentials() {
    let mut m = Mem::new();
    let x = json(
        r#"{"_id": "$external.x", "user": "x", "db": "$external", "credentials": {"external": true}, "roles": [{"role": "read", "db": "app"}]}"#,
    );
    m.run(
        ADMIN,
        doc(&[("insert", s("tempusers")), ("documents", Value::Array(vec![Value::Document(x)]))]),
    );
    let r = m.run(ADMIN, json(r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.tempusers", "drop": false, "db": ""}"#));
    assert_eq!(r, json(r#"{"ok": 1.0}"#));
    let r = m.run("$external", json(r#"{"usersInfo": "x"}"#));
    assert_eq!(field(&r, "users.0.mechanisms"), &Value::Array(vec![s("external")]));
}

#[test]
fn a_merge_of_roles_alone_and_retries_after_a_concurrent_change() {
    let mut m = seeded();
    let added =
        json(r#"{"_id": "app.r2", "role": "r2", "db": "app", "privileges": [], "roles": []}"#);
    m.run(
        ADMIN,
        doc(&[
            ("insert", s("temproles")),
            ("documents", Value::Array(vec![Value::Document(added)])),
        ]),
    );
    // The first write finds another change and the merge starts again.
    m.fail_security_writes = 1;
    let r = m.run(ADMIN, json(r#"{"_mergeAuthzCollections": 1, "tempRolesCollection": "admin.temproles", "drop": false, "db": "app"}"#));
    assert_eq!(r, json(r#"{"ok": 1.0}"#));
    assert_eq!(names(&m, Principal::Role), ["app.r1", "app.r2"]);
    assert_eq!(names(&m, Principal::User), ["app.u1", "other.o1"], "users not named stay");
    // Nothing to change writes nothing.
    let r = m.run(ADMIN, json(r#"{"_mergeAuthzCollections": 1, "tempRolesCollection": "admin.temproles", "drop": false, "db": "app"}"#));
    assert_eq!(r, json(r#"{"ok": 1.0}"#));
    // Changes that always meet another one give up.
    m.run(ADMIN, json(r#"{"delete": "temproles", "deletes": [{"q": {}, "limit": 0}]}"#));
    m.fail_security_writes = usize::MAX;
    let r = m.run(ADMIN, json(r#"{"_mergeAuthzCollections": 1, "tempRolesCollection": "admin.temproles", "drop": true, "db": "app"}"#));
    assert_eq!(error(&r).0, 112, "{r:?}");
}

#[test]
fn more_malformed_documents() {
    let cases: [(&str, &str, &str); 3] = [
        (
            "tempRolesCollection",
            r#"{"_id": "t.r", "role": "r", "db": "t", "privileges": [], "roles": [3]}"#,
            "User and role names must be either strings or objects",
        ),
        (
            "tempRolesCollection",
            r#"{"_id": "t.r", "role": "r", "db": "t", "privileges": 5, "roles": []}"#,
            "invalid parameter: expected an object (privileges)",
        ),
        (
            "tempUsersCollection",
            r#"{"_id": "t.a", "user": "a", "db": "t", "credentials": {}, "roles": "read"}"#,
            "invalid parameter: expected an object (roles)",
        ),
    ];
    for (field_name, text, msg) in cases {
        let mut m = seeded();
        let coll = if field_name == "tempRolesCollection" { "temproles" } else { "tempusers" };
        m.run(
            ADMIN,
            doc(&[
                ("insert", s(coll)),
                ("documents", Value::Array(vec![Value::Document(json(text))])),
            ]),
        );
        let cmd = doc(&[
            ("_mergeAuthzCollections", 1.into()),
            (field_name, s(&format!("admin.{coll}"))),
            ("drop", false.into()),
            ("db", s("")),
        ]);
        let (code, name) = if msg.starts_with("User and role") {
            (2, "BadValue")
        } else {
            (10065, "Location10065")
        };
        assert_eq!(error(&m.run(ADMIN, cmd)), (code, name.to_owned(), msg.to_owned()), "{text}");
    }
    // The document is stored as it is, a `userId` of the wrong size too (measured).
    let mut m = Mem::new();
    let u = json(
        r#"{"_id": "t.a", "user": "a", "db": "t", "userId": {"$binary": {"base64": "AAE=", "subType": "04"}}, "credentials": {}, "roles": ["read"]}"#,
    );
    let insert = Value::Array(vec![Value::Document(u.clone())]);
    m.run(ADMIN, doc(&[("insert", s("tempusers")), ("documents", insert)]));
    let r = m.run(ADMIN, json(r#"{"_mergeAuthzCollections": 1, "tempUsersCollection": "admin.tempusers", "drop": false, "db": ""}"#));
    assert_eq!(r, json(r#"{"ok": 1.0}"#));
    let stored = m.security.get(Principal::User, "t", "a").unwrap().to_vec();
    assert_eq!(Document::from(teseladb_bson::RawDoc::from_bytes(&stored).unwrap()), u);
}

/// 013 T043: `aggregate` reads the users and roles of Group 0, as the reference dump tool
/// --dumpDbUsersAndRoles` does with `[{$match: {db: …}}]`.
#[test]
fn aggregate_reads_the_users_and_roles_of_group_0() {
    let mut m = seeded();
    let cmd = r#"{"aggregate": "system.users", "pipeline": [{"$match": {"db": "app"}}, {"$project": {"_id": 1}}], "cursor": {}}"#;
    assert_eq!(ids(&first_batch(&m.run(ADMIN, json(cmd)))), ["app.u1"]);
    let cmd =
        r#"{"aggregate": "system.roles", "pipeline": [{"$match": {"db": "app"}}], "cursor": {}}"#;
    assert_eq!(ids(&first_batch(&m.run(ADMIN, json(cmd)))), ["app.r1"]);
    let cmd = r#"{"aggregate": "system.version", "pipeline": [], "cursor": {}}"#;
    assert_eq!(ids(&first_batch(&m.run(ADMIN, json(cmd)))), ["featureCompatibilityVersion"]);
}

/// 013 T043: with `drop`, a temporary user under another `_id` replaces the stored one.
#[test]
fn a_merged_user_with_another_id_replaces_the_stored_one() {
    let mut m = seeded();
    copy_to_temp(&mut m, |users, _| {
        let u1 = users.iter().find(|u| u.get("user") == Some(&s("u1"))).cloned().unwrap();
        let mut moved = Document::new();
        for (k, v) in u1.iter() {
            moved.push(k, if k == "_id" { s("elsewhere") } else { v.clone() });
        }
        *users = vec![moved];
    });
    assert_eq!(merge(&mut m, true, APP), json(r#"{"ok": 1.0}"#));
    assert_eq!(names(&m, Principal::User), ["other.o1", "app.u1"]);
    let stored = m.security.get(Principal::User, APP, "u1").unwrap().to_vec();
    let stored = Document::from(teseladb_bson::RawDoc::from_bytes(&stored).unwrap());
    assert_eq!(stored.get("_id"), Some(&s("elsewhere")));
}
