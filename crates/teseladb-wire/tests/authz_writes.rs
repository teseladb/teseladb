//! 013 T043, FR-030: direct writes to `admin.system.users` and `admin.system.roles` as the
//! reference router 9.0.4 takes them (measured 31-10-2026): any document, the insertion order, the
//! indexes `_id_` and `user_1_db_1` / `role_1_db_1` with their 11000 and 171, the usual semantics
//! of `update`, `delete` and `findAndModify`, and `usersInfo` / `rolesInfo` showing what was
//! written.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_meta::security::Principal;

const ADMIN: &str = "admin";

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, db: &str, text: &str) -> Document {
    m.run(db, json(text))
}

/// The `_id`s of the stored documents in their order, as `find` gives them.
fn ids(m: &mut Mem, coll: &str) -> Vec<Value> {
    let projection = Value::Document(json(r#"{"_id": 1}"#));
    let r = m.run(ADMIN, doc(&[("find", Value::from(coll)), ("projection", projection)]));
    match field(&r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|d| match d {
                Value::Document(d) => d.get("_id").cloned().unwrap_or(Value::Null),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn names(items: &[&str]) -> Vec<Value> {
    items.iter().map(|s| Value::from(*s)).collect()
}

const CRED: &str = r#"{"SCRAM-SHA-256": {"iterationCount": 15000, "salt": "3oRZALmEZEsGFtMP+d141FjZWbtBqPEgu297EQ==", "storedKey": "Aroy9y+RhG1wBkeaAvDadZ3XOCXHHzVImHe59k+zKQQ=", "serverKey": "P2rsqns20CktpqIq7psdyxx2k7LpjVPaGlTq8kEQA5g="}}"#;

#[test]
fn users_keep_their_insertion_order() {
    let mut m = Mem::new();
    for n in ["zz", "aa", "mm"] {
        let r = run(&mut m, "ord", &format!(r#"{{"createUser": "{n}", "pwd": "p", "roles": []}}"#));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    }
    assert_eq!(ids(&mut m, "system.users"), names(&["ord.zz", "ord.aa", "ord.mm"]));
    run(&mut m, "ord", r#"{"updateUser": "zz", "customData": {"x": 1}}"#);
    assert_eq!(ids(&mut m, "system.users"), names(&["ord.zz", "ord.aa", "ord.mm"]));
    run(&mut m, "ord", r#"{"dropUser": "aa"}"#);
    run(&mut m, "ord", r#"{"createUser": "aa", "pwd": "p", "roles": []}"#);
    assert_eq!(ids(&mut m, "system.users"), names(&["ord.zz", "ord.mm", "ord.aa"]));
    // `usersInfo` sorts by `user`, then `db`.
    let r = run(&mut m, "ord", r#"{"usersInfo": 1}"#);
    let Value::Array(users) = field(&r, "users") else { panic!("{r:?}") };
    let shown: Vec<Value> = users
        .iter()
        .map(|u| match u {
            Value::Document(d) => d.get("_id").cloned().unwrap_or(Value::Null),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(shown, names(&["ord.aa", "ord.mm", "ord.zz"]));
}

#[test]
fn direct_inserts_take_any_document_and_refuse_duplicate_keys() {
    let mut m = Mem::new();
    assert_eq!(
        run(&mut m, ADMIN, r#"{"insert": "system.users", "documents": [{"_id": 1}]}"#),
        json(r#"{"n": 1, "ok": 1.0}"#)
    );
    let y = format!(
        r#"{{"_id": "x.y", "user": "y", "db": "x", "credentials": {CRED}, "roles": [{{"role": "read", "db": "x"}}]}}"#
    );
    let r = m.run(
        ADMIN,
        Document::new()
            .with("insert", "system.users")
            .with("documents", vec![Value::Document(json(&y))]),
    );
    assert_eq!(r, json(r#"{"n": 1, "ok": 1.0}"#));
    let dup = format!(
        r#"{{"insert": "system.users", "documents": [{{"_id": "x.y2", "user": "y", "db": "x", "credentials": {CRED}, "roles": []}}]}}"#
    );
    let r = run(&mut m, ADMIN, &dup);
    let want = json(
        r#"{"n": 0, "writeErrors": [{"index": 0, "code": 11000, "errmsg": "E11000 duplicate key error collection: admin.system.users index: user_1_db_1 dup key: { user: \"y\", db: \"x\" }", "keyPattern": {"user": 1, "db": 1}, "keyValue": {"user": "y", "db": "x"}}], "ok": 1.0}"#,
    );
    assert_eq!(r, want);
    // Without `_id` it gets an ObjectId, placed first.
    let noid = format!(
        r#"{{"insert": "system.users", "documents": [{{"user": "noid", "db": "x", "credentials": {CRED}, "roles": ["read"]}}]}}"#
    );
    assert_eq!(run(&mut m, ADMIN, &noid), json(r#"{"n": 1, "ok": 1.0}"#));
    let ids_now = ids(&mut m, "system.users");
    assert_eq!(ids_now.len(), 3);
    assert!(matches!(ids_now[2], Value::ObjectId(_)), "{ids_now:?}");
    // Missing fields count as null in the unique index; `_id` 7 and 7.0 are the same.
    let r = run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.roles", "documents": [{"_id": "rr1"}, {"_id": "rr2"}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "E11000 duplicate key error collection: admin.system.roles index: role_1_db_1 dup key: { role: null, db: null }"
        )
    );
    run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": 7, "user": "k1", "db": "k"}]}"#,
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": 7.0, "user": "k2", "db": "k"}]}"#,
    );
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "E11000 duplicate key error collection: admin.system.users index: _id_ dup key: { _id: 7.0 }"
        )
    );
    // Arrays: one key per element, parallel arrays refused, an array `_id` refused.
    run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "arr1", "user": ["a1", "a2"], "db": "k"}]}"#,
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "arr2", "user": "a2", "db": "k"}]}"#,
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000));
    let r = run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "arr3", "user": ["p1"], "db": ["k", "k2"]}]}"#,
    );
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from("cannot index parallel arrays [db] [user]")
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": [1], "user": "k3", "db": "k"}]}"#,
    );
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from("The '_id' value cannot be of type array")
    );
    // Unordered inserts go on after a failure; ordered ones stop.
    let r = run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "o1", "user": "o1", "db": "k"}, {"_id": "o1", "user": "o2", "db": "k"}, {"_id": "o3", "user": "o3", "db": "k"}], "ordered": false}"#,
    );
    assert_eq!(
        (field(&r, "n"), field(&r, "writeErrors.0.index")),
        (&Value::Int32(2), &Value::Int32(1))
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "o4", "user": "o4", "db": "k"}, {"_id": "o4", "user": "o5", "db": "k"}, {"_id": "o6", "user": "o6", "db": "k"}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    // A `createUser` whose `_id` another document took is a user that exists.
    run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "s1.qq", "user": "other", "db": "zzz"}]}"#,
    );
    let r = run(&mut m, "s1", r#"{"createUser": "qq", "pwd": "p", "roles": []}"#);
    assert_eq!(field(&r, "errmsg"), &Value::from("User \"qq@s1\" already exists"));
    assert_eq!(m.security.users.len(), 9);
}

#[test]
fn direct_updates_and_deletes_run_the_usual_semantics() {
    let mut m = Mem::new();
    let y = format!(
        r#"{{"insert": "system.users", "documents": [{{"_id": "x.y", "user": "y", "db": "x", "credentials": {CRED}, "roles": [{{"role": "read", "db": "x"}}]}}, {{"_id": "x.z", "user": "z", "db": "x", "credentials": {CRED}, "roles": []}}]}}"#
    );
    run(&mut m, ADMIN, &y);
    let r = run(
        &mut m,
        ADMIN,
        r#"{"update": "system.users", "updates": [{"q": {"_id": "x.y"}, "u": {"$set": {"customData": {"k": 1}}}}]}"#,
    );
    assert_eq!(r, json(r#"{"nModified": 1, "n": 1, "ok": 1.0}"#));
    // `usersInfo` shows what was written as it is.
    let r = run(&mut m, "x", r#"{"usersInfo": "y"}"#);
    let shown = json(
        r#"{"_id": "x.y", "user": "y", "db": "x", "roles": [{"role": "read", "db": "x"}], "customData": {"k": 1}, "mechanisms": ["SCRAM-SHA-256"]}"#,
    );
    assert_eq!(field(&r, "users"), &Value::Array(vec![Value::Document(shown)]));
    let r = run(
        &mut m,
        ADMIN,
        r#"{"update": "system.users", "updates": [{"q": {"_id": "x.y"}, "u": {"$set": {"_id": "other"}}}]}"#,
    );
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "Plan executor error during update :: caused by :: Performing an update on the path '_id' would modify the immutable field '_id'"
        )
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"update": "system.users", "updates": [{"q": {"_id": "x.z"}, "u": {"$set": {"user": "y"}}}]}"#,
    );
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "Plan executor error during update :: caused by :: E11000 duplicate key error collection: admin.system.users index: user_1_db_1 dup key: { user: \"y\", db: \"x\" }"
        )
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"update": "system.users", "updates": [{"q": {"db": "x"}, "u": {"$set": {"m": 1}}, "multi": true}]}"#,
    );
    assert_eq!(r, json(r#"{"nModified": 2, "n": 2, "ok": 1.0}"#));
    let r = run(
        &mut m,
        ADMIN,
        r#"{"update": "system.users", "updates": [{"q": {"_id": "nothere"}, "u": {"$set": {"user": "ups", "db": "k"}}, "upsert": true}]}"#,
    );
    assert_eq!(
        r,
        json(
            r#"{"nModified": 0, "n": 1, "upserted": [{"index": 0, "_id": "nothere"}], "ok": 1.0}"#
        )
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"findAndModify": "system.users", "query": {"_id": "x.y"}, "update": {"$unset": {"customData": 1}}}"#,
    );
    assert_eq!(
        field(&r, "lastErrorObject"),
        &Value::Document(json(r#"{"n": 1, "updatedExisting": true}"#))
    );
    assert_eq!(field(&r, "value.customData"), &Value::Document(json(r#"{"k": 1}"#)));
    // The update kept the place; a delete takes the documents out.
    assert_eq!(ids(&mut m, "system.users"), names(&["x.y", "x.z", "nothere"]));
    let r = run(
        &mut m,
        ADMIN,
        r#"{"delete": "system.users", "deletes": [{"q": {"db": "x"}, "limit": 0}]}"#,
    );
    assert_eq!(r, json(r#"{"n": 2, "ok": 1.0}"#));
    let r = run(&mut m, ADMIN, r#"{"findAndModify": "system.users", "query": {}, "remove": true}"#);
    assert_eq!(field(&r, "value._id"), &Value::from("nothere"));
    assert!(m.security.list(Principal::User, None).is_empty());
    // A role written directly is a role `rolesInfo` lists, in the order written.
    for r in ["rz", "ra"] {
        let ins = format!(
            r#"{{"insert": "system.roles", "documents": [{{"_id": "x.{r}", "role": "{r}", "db": "x", "privileges": [], "roles": []}}]}}"#
        );
        run(&mut m, ADMIN, &ins);
    }
    let r = run(&mut m, "x", r#"{"rolesInfo": 1}"#);
    let want = json(
        r#"{"a": [{"_id": "x.rz", "role": "rz", "db": "x", "roles": [], "isBuiltin": false, "inheritedRoles": []}, {"_id": "x.ra", "role": "ra", "db": "x", "roles": [], "isBuiltin": false, "inheritedRoles": []}]}"#,
    );
    assert_eq!(field(&r, "roles"), field(&want, "a"));
    // A write that another change beat starts again.
    m.fail_security_writes = 1;
    let r = run(
        &mut m,
        ADMIN,
        r#"{"delete": "system.roles", "deletes": [{"q": {"_id": "x.rz"}, "limit": 1}]}"#,
    );
    assert_eq!(r, json(r#"{"n": 1, "ok": 1.0}"#));
    m.fail_security_writes = usize::MAX;
    let r = run(&mut m, ADMIN, r#"{"delete": "system.roles", "deletes": [{"q": {}, "limit": 1}]}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(112));
}

#[test]
fn more_shapes_of_direct_writes_and_of_what_shows_them() {
    let mut m = Mem::new();
    // A user with fields of its own: `usersInfo` keeps them, computes `mechanisms` in the place
    // of a stored one, and with `showPrivileges` keeps the stored one (measured on 9.0.4).
    let e = format!(
        r#"{{"insert": "system.users", "documents": [{{"_id": "q.e", "zzz": 1, "user": "e", "db": "q", "credentials": {CRED}, "authenticationRestrictions": [{{"clientSource": ["1.2.3.4"]}}], "mechanisms": ["x"], "customData": {{"c": 1}}, "roles": [{{"role": "read", "db": "q"}}], "tail": 2}}]}}"#
    );
    run(&mut m, ADMIN, &e);
    let r = run(&mut m, "q", r#"{"usersInfo": ["e", "e"]}"#);
    let plain = json(
        r#"{"_id": "q.e", "zzz": 1, "user": "e", "db": "q", "mechanisms": ["SCRAM-SHA-256"], "customData": {"c": 1}, "roles": [{"role": "read", "db": "q"}], "tail": 2}"#,
    );
    assert_eq!(field(&r, "users"), &Value::Array(vec![Value::Document(plain)]));
    let r =
        run(&mut m, "q", r#"{"usersInfo": "e", "showPrivileges": true, "showCustomData": false}"#);
    let Value::Array(users) = field(&r, "users") else { panic!("{r:?}") };
    let Value::Document(u) = &users[0] else { panic!() };
    let keys: Vec<&str> = u.iter().map(|(k, _)| k).collect();
    let want = ["_id", "zzz", "user", "db", "mechanisms", "roles", "tail", "inheritedRoles"];
    assert_eq!(keys[..8], want);
    assert_eq!(u.get("mechanisms"), Some(&Value::Array(vec![Value::from("x")])));
    // Without credentials `mechanisms` is null, also with the inherited fields.
    run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "q.b", "user": "b", "db": "q", "roles": []}]}"#,
    );
    let r = run(&mut m, "q", r#"{"usersInfo": "b", "showPrivileges": true}"#);
    assert_eq!(field(&r, "users.0.mechanisms"), &Value::Null);
    // `updateUser` on a user written directly keeps its `_id` and fields and adds the new ones.
    let r = run(&mut m, "q", r#"{"updateUser": "e", "customData": {"c": 2}}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let stored = Document::from(
        teseladb_bson::RawDoc::from_bytes(m.security.get(Principal::User, "q", "e").unwrap())
            .unwrap(),
    );
    assert_eq!(stored.get("customData"), Some(&Value::Document(json(r#"{"c": 2}"#))));
    assert_eq!(stored.get("zzz"), Some(&Value::Int32(1)));
    let x = format!(
        r#"{{"insert": "system.users", "documents": [{{"_id": "custom", "user": "x", "db": "q", "credentials": {CRED}, "roles": []}}]}}"#
    );
    run(&mut m, ADMIN, &x);
    run(&mut m, "q", r#"{"updateUser": "x", "customData": {"n": 1}}"#);
    let stored = m.security.get(Principal::User, "q", "x").unwrap().to_vec();
    let stored = Document::from(teseladb_bson::RawDoc::from_bytes(&stored).unwrap());
    let keys: Vec<&str> = stored.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "user", "db", "credentials", "roles", "customData"]);
    assert_eq!(stored.get("_id"), Some(&Value::from("custom")));
    // A user without `roles` is left alone when a role goes; `dropUser` tries again after a
    // concurrent change.
    run(&mut m, "q", r#"{"createRole": "gone", "privileges": [], "roles": []}"#);
    run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "q.n", "user": "n", "db": "q"}]}"#,
    );
    assert_eq!(field(&run(&mut m, "q", r#"{"dropRole": "gone"}"#), "ok"), &Value::Double(1.0));
    m.fail_security_writes = 1;
    assert_eq!(field(&run(&mut m, "q", r#"{"dropUser": "n"}"#), "ok"), &Value::Double(1.0));
    // Roles written directly: `rolesInfo` leaves out the fields they lack, and
    // `dropAllRolesFromDatabase` takes even those without a name.
    run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.roles", "documents": [{"_id": "q.bare", "role": "bare", "db": "q"}, {"_id": "nameless", "db": "q"}]}"#,
    );
    let r = run(&mut m, "q", r#"{"rolesInfo": "bare", "showPrivileges": true}"#);
    let want = json(
        r#"{"_id": "q.bare", "role": "bare", "db": "q", "inheritedRoles": [], "inheritedPrivileges": [], "isBuiltin": false}"#,
    );
    assert_eq!(field(&r, "roles"), &Value::Array(vec![Value::Document(want)]));
    let r = run(&mut m, "q", r#"{"dropAllRolesFromDatabase": 1}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(2));
    // Unordered statements go on after a failure; a later statement no longer sees what an
    // earlier one deleted; `findAndModify` follows its `sort` and returns the upsert with `new`.
    let r = run(
        &mut m,
        ADMIN,
        r#"{"update": "system.users", "updates": [{"q": {"_id": "q.e"}, "u": {"$set": {"_id": 1}}}, {"q": {"_id": "q.e"}, "u": {"$set": {"k": 1}}}], "ordered": false}"#,
    );
    assert_eq!(
        (field(&r, "n"), field(&r, "writeErrors.0.index")),
        (&Value::Int32(1), &Value::Int32(0))
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"delete": "system.users", "deletes": [{"q": {"_id": "q.b"}, "limit": 1}, {"q": {"db": "q"}, "limit": 0}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(3));
    run(
        &mut m,
        ADMIN,
        r#"{"insert": "system.users", "documents": [{"_id": "a", "user": "a", "db": "s"}, {"_id": "b", "user": "b", "db": "s"}]}"#,
    );
    let r = run(
        &mut m,
        ADMIN,
        r#"{"findAndModify": "system.users", "query": {"db": "s"}, "sort": {"user": -1}, "remove": true}"#,
    );
    assert_eq!(field(&r, "value._id"), &Value::from("b"));
    let r = run(
        &mut m,
        ADMIN,
        r#"{"findAndModify": "system.users", "query": {"_id": "c"}, "update": {"$set": {"user": "c", "db": "s"}}, "upsert": true, "new": true}"#,
    );
    assert_eq!(
        field(&r, "value"),
        &Value::Document(json(r#"{"_id": "c", "db": "s", "user": "c"}"#))
    );
}
