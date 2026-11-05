//! User commands as RefDb 9.0.4 answers them (013 T022, FR-009, FR-013; measured on a
//! `refrouter` with `--keyFile`, 30-10-2026): `createUser`, `updateUser`, `dropUser`,
//! `dropAllUsersFromDatabase`, `grantRolesToUser`, `revokeRolesFromUser`, `usersInfo` and
//! `invalidateUserCache`, over the users of Group 0, which keep no password.

mod common;

use common::{Mem, doc, field};
use teseladb_auth::scram::{ClientSession, Mechanism, ServerSession};
use teseladb_auth::users::{User, UserName};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_bson::{Document, RawDoc, Value};

const TEST: &str = "test";
/// A password that would show if it were stored.
const SECRET: &str = "Sup3r-S3cret-pa55";

fn s(x: &str) -> Value {
    Value::from(x)
}

fn arr(items: &[Value]) -> Value {
    Value::Array(items.to_vec())
}

fn error(code: i32, name: &str, msg: &str) -> Document {
    doc(&[("ok", 0.0.into()), ("errmsg", s(msg)), ("code", code.into()), ("codeName", s(name))])
}

fn ok() -> Document {
    doc(&[("ok", 1.0.into())])
}

fn stored(m: &Mem, db: &str, user: &str) -> Option<User> {
    let bytes = m.security.get(Principal::User, db, user)?;
    Some(User::from_document(&Document::from(RawDoc::from_bytes(bytes).unwrap())).unwrap())
}

/// Logs in with SCRAM against the stored credentials: the password works.
fn password_works(m: &Mem, db: &str, user: &str, pwd: &str, mech: Mechanism) -> bool {
    let Some(u) = stored(m, db, user) else { return false };
    let Some(creds) = u.credentials(mech).cloned() else { return false };
    let prepared = mech.prepare_password(user, pwd, false).unwrap();
    let (mut client, first) = ClientSession::start(mech, user, &prepared, "abcdefgh");
    let first = teseladb_auth::scram::ClientFirst::parse(&first).unwrap();
    let (mut server, server_first) = ServerSession::start(mech, &first, creds, "server");
    let fin = client.respond(&server_first).unwrap();
    server.finish(&fin).is_ok()
}

fn create(m: &mut Mem, user: &str, extra: &[(&str, Value)]) -> Document {
    let mut d = doc(&[("createUser", s(user))]);
    for (k, v) in extra {
        d.push(*k, v.clone());
    }
    m.run(TEST, d)
}

#[test]
fn create_stores_derived_credentials_never_the_password() {
    let mut m = Mem::new();
    let r = create(
        &mut m,
        "u1",
        &[
            ("pwd", s(SECRET)),
            (
                "roles",
                arr(&[
                    s("read"),
                    Value::Document(doc(&[("role", s("readWrite")), ("db", s("other"))])),
                ]),
            ),
            ("customData", Value::Document(doc(&[("x", 1.into())]))),
            (
                "authenticationRestrictions",
                arr(&[Value::Document(doc(&[
                    ("clientSource", arr(&[s("127.0.0.1")])),
                    ("serverAddress", arr(&[s("10.0.0.0/8"), s("::1")])),
                ]))]),
            ),
            ("writeConcern", Value::Document(doc(&[("w", 1.into())]))),
        ],
    );
    assert_eq!(r, ok());
    let u = stored(&m, TEST, "u1").unwrap();
    let doc_bytes = m.security.get(Principal::User, TEST, "u1").unwrap().to_vec();
    let keys: Vec<String> = Document::from(RawDoc::from_bytes(&doc_bytes).unwrap())
        .iter()
        .map(|(k, _)| k.to_owned())
        .collect();
    let expected = [
        "_id",
        "userId",
        "user",
        "db",
        "credentials",
        "authenticationRestrictions",
        "customData",
        "roles",
    ];
    assert_eq!(keys, expected);
    // Credentials with RefDb's iteration counts and salt lengths, no password anywhere.
    let sha1 = u.scram_sha1.as_ref().unwrap();
    let sha256 = u.scram_sha256.as_ref().unwrap();
    assert_eq!((sha1.iterations, sha1.salt.len()), (10_000, 16));
    assert_eq!((sha256.iterations, sha256.salt.len()), (15_000, 28));
    assert!(!contains(&doc_bytes, SECRET.as_bytes()) && !contains(&doc_bytes, b"pwd"));
    assert!(password_works(&m, TEST, "u1", SECRET, Mechanism::ScramSha1));
    assert!(password_works(&m, TEST, "u1", SECRET, Mechanism::ScramSha256));
    assert_eq!(u.user_id.map(|id| id[6] >> 4), Some(4), "userId is a version 4 UUID");
    let exists = error(51003, "Location51003", "User \"u1@test\" already exists");
    assert_eq!(create(&mut m, "u1", &[("pwd", s("p")), ("roles", arr(&[]))]), exists);
    // Repeated roles and mechanisms count once.
    let r = create(
        &mut m,
        "u2",
        &[
            ("pwd", s("p")),
            ("roles", arr(&[s("read"), s("read")])),
            ("mechanisms", arr(&[s("SCRAM-SHA-256"), s("SCRAM-SHA-256")])),
        ],
    );
    assert_eq!(r, ok());
    let u2 = stored(&m, TEST, "u2").unwrap();
    assert_eq!(u2.roles.len(), 1);
    assert!(u2.scram_sha1.is_none() && u2.scram_sha256.is_some());
    // A client-digested SCRAM-SHA-1 password.
    let digest = Mechanism::ScramSha1.prepare_password("u3", "p", false).unwrap();
    let digest = String::from_utf8(digest).unwrap();
    let r = create(
        &mut m,
        "u3",
        &[
            ("pwd", s(&digest)),
            ("roles", arr(&[])),
            ("mechanisms", arr(&[s("SCRAM-SHA-1")])),
            ("digestPassword", false.into()),
        ],
    );
    assert_eq!(r, ok());
    assert!(password_works(&m, TEST, "u3", "p", Mechanism::ScramSha1));
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn create_errors_as_refdb_gives_them() {
    let mut m = Mem::new();
    let roles = || ("roles", arr(&[]));
    let pwd = || ("pwd", s("p"));
    let bad = |msg: &str| error(2, "BadValue", msg);
    let cases: Vec<(&str, &str, Vec<(&str, Value)>, Document)> = vec![
        (
            TEST,
            "u",
            vec![pwd()],
            error(
                40414,
                "IDLFailedToParse",
                "BSON field 'createUser.roles' is missing but a required field",
            ),
        ),
        (
            TEST,
            "u",
            vec![roles()],
            bad(
                "Must provide a 'pwd' field for all user documents, except those with '$external' as the user's source db",
            ),
        ),
        (
            "$external",
            "CN=a",
            vec![pwd(), roles()],
            bad("Cannot set the password for users defined on the '$external' database"),
        ),
        (
            TEST,
            "u",
            vec![pwd(), ("roles", arr(&[s("nosuch")]))],
            error(31, "RoleNotFound", "Could not find role: nosuch@test"),
        ),
        (
            TEST,
            "u",
            vec![pwd(), ("roles", arr(&[s("readAnyDatabase")]))],
            error(31, "RoleNotFound", "Could not find role: readAnyDatabase@test"),
        ),
        (
            TEST,
            "u",
            vec![pwd(), roles(), ("mechanisms", arr(&[s("FOO")]))],
            bad("Unknown auth mechanism 'FOO'"),
        ),
        (
            TEST,
            "u",
            vec![pwd(), roles(), ("mechanisms", arr(&[]))],
            bad("mechanisms field must not be empty"),
        ),
        (
            TEST,
            "u",
            vec![pwd(), roles(), ("digestPassword", false.into())],
            bad("Use of SCRAM-SHA-256 requires undigested passwords"),
        ),
        (
            TEST,
            "u",
            vec![("pwd", s("")), roles()],
            error(
                50687,
                "Location50687",
                "Error preflighting UTF-8 conversion: U_STRING_NOT_TERMINATED_WARNING",
            ),
        ),
        (TEST, "", vec![pwd(), roles()], bad("User document needs 'user' field to be non-empty")),
        (
            TEST,
            "u",
            vec![pwd(), ("roles", s("read"))],
            error(
                14,
                "TypeMismatch",
                "BSON field 'createUser.roles' is the wrong type 'string', expected type 'array'",
            ),
        ),
        (
            TEST,
            "u",
            vec![("pwd", 5.into()), roles()],
            error(
                14,
                "TypeMismatch",
                "BSON field 'createUser.pwd' is the wrong type 'int', expected type 'string'",
            ),
        ),
        (
            TEST,
            "u",
            vec![pwd(), ("roles", arr(&[Value::Document(doc(&[("role", s("read"))]))]))],
            bad("RoleName must contain a field named: db"),
        ),
        (
            TEST,
            "u",
            vec![pwd(), ("roles", arr(&[5.into()]))],
            bad("Role name must be either a document or string"),
        ),
        (
            TEST,
            "u",
            vec![pwd(), roles(), ("customData", 5.into())],
            error(
                14,
                "TypeMismatch",
                "BSON field 'createUser.customData' is the wrong type 'int', expected type 'object'",
            ),
        ),
        (
            TEST,
            "u",
            vec![
                pwd(),
                roles(),
                ("authenticationRestrictions", arr(&[Value::Document(doc(&[("foo", 1.into())]))])),
            ],
            error(
                40415,
                "IDLUnknownField",
                "BSON field 'createUser.authenticationRestrictions.foo' is an unknown field.",
            ),
        ),
        (
            TEST,
            "u",
            vec![
                pwd(),
                roles(),
                (
                    "authenticationRestrictions",
                    arr(&[Value::Document(doc(&[("clientSource", s("bad"))]))]),
                ),
            ],
            error(
                14,
                "TypeMismatch",
                "BSON field 'createUser.authenticationRestrictions.clientSource' is the wrong type 'string', expected type 'array'",
            ),
        ),
        (
            TEST,
            "u",
            vec![
                pwd(),
                roles(),
                (
                    "authenticationRestrictions",
                    arr(&[Value::Document(doc(&[("clientSource", arr(&[s("300.0.0.1")]))]))]),
                ),
            ],
            bad("Invalid IP address in CIDR string"),
        ),
        (
            TEST,
            "u",
            vec![
                pwd(),
                roles(),
                (
                    "authenticationRestrictions",
                    arr(&[Value::Document(doc(&[("clientSource", arr(&[s("10.0.0.0/33")]))]))]),
                ),
            ],
            bad("Invalid IP address in CIDR string"),
        ),
    ];
    for (db, user, extra, expected) in cases {
        let mut d = doc(&[("createUser", s(user))]);
        for (k, v) in extra {
            d.push(k, v);
        }
        assert_eq!(m.run(db, d.clone()), expected, "{d:?}");
    }
    let r = m.run(TEST, doc(&[("createUser", 5.into()), ("pwd", s("p")), ("roles", arr(&[]))]));
    assert_eq!(
        r,
        error(
            14,
            "TypeMismatch",
            "BSON field 'createUser.createUser' is the wrong type 'int', expected type 'string'"
        )
    );
    let r = m.run(
        TEST,
        doc(&[
            ("createUser", s("u")),
            ("pwd", s("p")),
            ("roles", arr(&[])),
            ("mechanisms", arr(&[5.into()])),
        ]),
    );
    assert_eq!(field(&r, "code"), &Value::Int32(14));
    let r = m.run(
        TEST,
        doc(&[
            ("createUser", s("u")),
            ("pwd", s("p")),
            ("roles", arr(&[])),
            ("authenticationRestrictions", arr(&[5.into()])),
        ]),
    );
    assert_eq!(field(&r, "code"), &Value::Int32(14));
    assert!(m.security.users.is_empty());
    // A user of `$external` has no password.
    let r = m.run("$external", doc(&[("createUser", s("CN=a")), ("roles", arr(&[]))]));
    assert_eq!(r, ok());
    let info = m.run("$external", doc(&[("usersInfo", s("CN=a"))]));
    let Value::Array(users) = field(&info, "users") else { panic!() };
    let Value::Document(u) = &users[0] else { panic!() };
    assert_eq!(field(u, "mechanisms"), &arr(&[s("external")]));
}

#[test]
fn update_grant_revoke_and_drop() {
    let mut m = Mem::new();
    create(&mut m, "u1", &[("pwd", s("p")), ("roles", arr(&[s("read")]))]);
    let upd = |m: &mut Mem, extra: &[(&str, Value)]| {
        let mut d = doc(&[("updateUser", s("u1"))]);
        for (k, v) in extra {
            d.push(*k, v.clone());
        }
        m.run(TEST, d)
    };
    let r = upd(
        &mut m,
        &[
            ("pwd", s("q")),
            ("customData", Value::Document(doc(&[("y", 2.into())]))),
            ("roles", arr(&[s("dbAdmin")])),
        ],
    );
    assert_eq!(r, ok());
    assert!(password_works(&m, TEST, "u1", "q", Mechanism::ScramSha256));
    assert!(!password_works(&m, TEST, "u1", "p", Mechanism::ScramSha256));
    let u = stored(&m, TEST, "u1").unwrap();
    assert_eq!(u.roles, vec![teseladb_auth::users::RoleName::new("dbAdmin", TEST)]);
    // Dropping a mechanism without a password, and a password for one mechanism.
    assert_eq!(upd(&mut m, &[("mechanisms", arr(&[s("SCRAM-SHA-1")]))]), ok());
    assert!(stored(&m, TEST, "u1").unwrap().scram_sha256.is_none());
    let more = upd(&mut m, &[("mechanisms", arr(&[s("SCRAM-SHA-256")]))]);
    assert_eq!(field(&more, "code"), &Value::Int32(2));
    assert_eq!(upd(&mut m, &[("mechanisms", arr(&[s("SCRAM-SHA-256")])), ("pwd", s("z"))]), ok());
    assert!(password_works(&m, TEST, "u1", "z", Mechanism::ScramSha256));
    assert!(stored(&m, TEST, "u1").unwrap().scram_sha1.is_none());
    assert_eq!(upd(&mut m, &[("pwd", s("w"))]), ok());
    assert!(password_works(&m, TEST, "u1", "w", Mechanism::ScramSha256));
    let restrict = arr(&[Value::Document(doc(&[("clientSource", arr(&[s("::1/128")]))]))]);
    assert_eq!(upd(&mut m, &[("authenticationRestrictions", restrict)]), ok());
    assert!(stored(&m, TEST, "u1").unwrap().restrictions.is_some());
    assert_eq!(
        upd(&mut m, &[]),
        error(2, "BadValue", "Must specify at least one field to update in updateUser")
    );
    let r = m.run(TEST, doc(&[("updateUser", s("nobody")), ("pwd", s("q"))]));
    assert_eq!(r, error(11, "UserNotFound", "User nobody@test not found"));
    let r = m.run("$external", doc(&[("updateUser", s("CN=a")), ("pwd", s("q"))]));
    assert_eq!(
        r,
        error(
            2,
            "BadValue",
            "Cannot set the password for users defined on the '$external' database"
        )
    );
    let r = upd(&mut m, &[("roles", arr(&[s("zzz")]))]);
    assert_eq!(r, error(31, "RoleNotFound", "Could not find role: zzz@test"));

    let grant = |m: &mut Mem, name: &str, roles: Value| {
        m.run(TEST, doc(&[("grantRolesToUser", s(name)), ("roles", roles)]))
    };
    assert_eq!(grant(&mut m, "u1", arr(&[s("read"), s("readWrite"), s("read")])), ok());
    let roles: Vec<String> =
        stored(&m, TEST, "u1").unwrap().roles.iter().map(|r| r.role.clone()).collect();
    assert_eq!(roles, ["dbAdmin", "read", "readWrite"]);
    assert_eq!(
        grant(&mut m, "u1", arr(&[s("zzz")])),
        error(31, "RoleNotFound", "Could not find role: zzz@test")
    );
    assert_eq!(
        grant(&mut m, "nobody", arr(&[s("read")])),
        error(11, "UserNotFound", "Could not find user \"nobody\" for db \"test\"")
    );
    assert_eq!(
        grant(&mut m, "u1", arr(&[])),
        error(2, "BadValue", "grantRolesToUser command requires a non-empty \"roles\" array")
    );
    let r = m.run(TEST, doc(&[("grantRolesToUser", s("u1"))]));
    assert_eq!(
        r,
        error(
            40414,
            "IDLFailedToParse",
            "BSON field 'grantRolesToUser.roles' is missing but a required field"
        )
    );
    let revoke =
        doc(&[("revokeRolesFromUser", s("u1")), ("roles", arr(&[s("read"), s("notheld")]))]);
    assert_eq!(m.run(TEST, revoke), error(31, "RoleNotFound", "Could not find role: notheld@test"));
    let revoke =
        doc(&[("revokeRolesFromUser", s("u1")), ("roles", arr(&[s("read"), s("enableSharding")]))]);
    assert_eq!(m.run(TEST, revoke), ok());
    let roles: Vec<String> =
        stored(&m, TEST, "u1").unwrap().roles.iter().map(|r| r.role.clone()).collect();
    assert_eq!(roles, ["dbAdmin", "readWrite"]);

    create(&mut m, "u2", &[("pwd", s("p")), ("roles", arr(&[]))]);
    m.run("other", doc(&[("createUser", s("u3")), ("pwd", s("p")), ("roles", arr(&[]))]));
    assert_eq!(m.run(TEST, doc(&[("dropUser", s("u2"))])), ok());
    assert_eq!(
        m.run(TEST, doc(&[("dropUser", s("u2"))])),
        error(11, "UserNotFound", "User 'u2@test' not found")
    );
    create(&mut m, "u4", &[("pwd", s("p")), ("roles", arr(&[]))]);
    let r = m.run(TEST, doc(&[("dropAllUsersFromDatabase", 1.into())]));
    assert_eq!(r, doc(&[("n", 2.into()), ("ok", 1.0.into())]));
    assert_eq!(
        m.run(TEST, doc(&[("dropAllUsersFromDatabase", 1.into())])),
        doc(&[("n", 0.into()), ("ok", 1.0.into())])
    );
    assert!(stored(&m, "other", "u3").is_some());
}

/// A change computed from a document that changed since is computed again; a write that keeps
/// failing gives up with `WriteConflict`.
#[test]
fn concurrent_changes_are_retried() {
    let mut m = Mem::new();
    create(&mut m, "u1", &[("pwd", s("p")), ("roles", arr(&[]))]);
    // Before the write, another change of the same user: the expected document no longer
    // matches, so the command reads again and applies on top of it.
    let user = stored(&m, TEST, "u1").unwrap();
    let mut changed = user.clone();
    changed.roles.push(teseladb_auth::users::RoleName::new("read", TEST));
    let old = m.security.get(Principal::User, TEST, "u1").unwrap().to_vec();
    let op = SecurityOp::named(
        Principal::User,
        TEST,
        "u1",
        Expect::Exactly(old),
        Some(changed.to_document().to_bytes().unwrap()),
    );
    m.security.apply(&[op]).unwrap();
    m.fail_security_writes = 1;
    let r = m.run(TEST, doc(&[("grantRolesToUser", s("u1")), ("roles", arr(&[s("dbAdmin")]))]));
    assert_eq!(r, ok());
    let roles: Vec<String> =
        stored(&m, TEST, "u1").unwrap().roles.iter().map(|r| r.role.clone()).collect();
    assert_eq!(roles, ["read", "dbAdmin"]);
    m.fail_security_writes = usize::MAX;
    let r = m.run(TEST, doc(&[("grantRolesToUser", s("u1")), ("roles", arr(&[s("dbOwner")]))]));
    assert_eq!(field(&r, "code"), &Value::Int32(112));
    let r = m.run(TEST, doc(&[("dropAllUsersFromDatabase", 1.into())]));
    assert_eq!(field(&r, "code"), &Value::Int32(112));
    // A user removed between the read and the write.
    m.fail_security_writes = 0;
    m.drop_before_write = Some(UserName::new("u1", TEST));
    let r = m.run(TEST, doc(&[("updateUser", s("u1")), ("pwd", s("x"))]));
    assert_eq!(r, error(11, "UserNotFound", "User u1@test not found"));
}

#[test]
fn users_info_as_refdb_shows_it() {
    let mut m = Mem::new();
    create(
        &mut m,
        "u1",
        &[
            ("pwd", s("p")),
            ("roles", arr(&[s("read")])),
            ("customData", Value::Document(doc(&[("x", 1.into())]))),
        ],
    );
    create(&mut m, "u0", &[("pwd", s("p")), ("roles", arr(&[]))]);
    m.run("other", doc(&[("createUser", s("u0")), ("pwd", s("p")), ("roles", arr(&[]))]));
    let names = |r: &Document| -> Vec<String> {
        let Value::Array(users) = field(r, "users") else { panic!("{r:?}") };
        users
            .iter()
            .map(|u| match u {
                Value::Document(d) => match field(d, "_id") {
                    Value::String(id) => id.clone(),
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let r = m.run(TEST, doc(&[("usersInfo", 1.into())]));
    assert_eq!(names(&r), ["test.u0", "test.u1"]);
    let Value::Array(users) = field(&r, "users") else { panic!() };
    let Value::Document(u1) = &users[1] else { panic!() };
    let keys: Vec<&str> = u1.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "userId", "user", "db", "customData", "roles", "mechanisms"]);
    assert_eq!(field(u1, "mechanisms"), &arr(&[s("SCRAM-SHA-1"), s("SCRAM-SHA-256")]));
    let r = m.run(
        TEST,
        doc(&[
            ("usersInfo", s("u1")),
            ("showCredentials", true.into()),
            ("showCustomData", false.into()),
        ]),
    );
    let Value::Array(users) = field(&r, "users") else { panic!() };
    let Value::Document(u1) = &users[0] else { panic!() };
    let keys: Vec<&str> = u1.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "userId", "user", "db", "credentials", "roles", "mechanisms"]);
    let both = arr(&[s("u1"), Value::Document(doc(&[("user", s("u0")), ("db", s("other"))]))]);
    assert_eq!(names(&m.run(TEST, doc(&[("usersInfo", both)]))), ["other.u0", "test.u1"]);
    let all = Value::Document(doc(&[("forAllDBs", true.into())]));
    let r = m.run(
        TEST,
        doc(&[("usersInfo", all.clone()), ("filter", Value::Document(doc(&[("user", s("u0"))])))]),
    );
    assert_eq!(names(&r), ["other.u0", "test.u0"]);
    let r = m.run(
        TEST,
        doc(&[
            ("usersInfo", 1.0.into()),
            ("filter", Value::Document(doc(&[("roles.role", s("read"))]))),
        ]),
    );
    assert_eq!(names(&r), ["test.u1"]);
    assert_eq!(names(&m.run(TEST, doc(&[("usersInfo", s("nobody"))]))), Vec::<String>::new());
    assert_eq!(names(&m.run(TEST, doc(&[("usersInfo", Value::Int64(1))]))).len(), 2);
    let cases = [
        (
            Value::Document(doc(&[("user", s("u1"))])),
            error(2, "BadValue", "UserName must contain a field named: db"),
        ),
        (
            Value::Document(doc(&[("forAllDBs", false.into())])),
            error(2, "BadValue", "UserName contains an unknown field named: 'forAllDBs"),
        ),
        (5.5.into(), error(2, "BadValue", "UserName must be either a string or an object")),
        (true.into(), error(2, "BadValue", "UserName must be either a string or an object")),
    ];
    for (v, expected) in cases {
        assert_eq!(m.run(TEST, doc(&[("usersInfo", v)])), expected);
    }
    let r = m.run(TEST, doc(&[("usersInfo", 1.into()), ("filter", 5.into())]));
    assert_eq!(
        r,
        error(
            14,
            "TypeMismatch",
            "BSON field 'usersInfo.filter' is the wrong type 'int', expected type 'object'"
        )
    );
    let r = m.run(TEST, doc(&[("usersInfo", 1.into()), ("showCredentials", s("yes"))]));
    assert_eq!(
        r,
        error(
            14,
            "TypeMismatch",
            "BSON field 'usersInfo.showCredentials' is the wrong type 'string', expected types '[int, double, long, decimal, bool]'"
        )
    );
}

/// Through the node's dispatch, which checks the fields every strict command checks and the
/// write concern the user commands take.
#[test]
fn strict_fields_and_write_concern_through_dispatch() {
    use teseladb_wire::commands::{NodeSessions, Timeouts};
    use teseladb_wire::connection::Connection;
    use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
    use teseladb_wire::msg::OpMsg;
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(NodeSessions::new(Timeouts::default()));
    let info = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    };
    let mut run = |cmd: Document| {
        let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", TEST).to_bytes().unwrap());
        let mut c = Connection::new(1);
        c.receive(&bytes);
        let req = c.next_request().unwrap().unwrap();
        let shared = std::mem::take(&mut m.shared);
        let a = dispatch_with(&mut c, &req, &info, Some((&mut m, &shared)));
        m.shared = shared;
        match a {
            Action::Reply(d) => d,
            other => panic!("{other:?}"),
        }
    };
    let wc = Value::Document(doc(&[("w", 1.into())]));
    let create =
        doc(&[("createUser", s("u")), ("pwd", s("p")), ("roles", arr(&[])), ("foo", 1.into())]);
    let r = run(create);
    assert_eq!(field(&r, "errmsg"), &s("BSON field 'createUser.foo' is an unknown field."));
    let create = doc(&[
        ("createUser", s("u")),
        ("pwd", s("p")),
        ("roles", arr(&[])),
        ("writeConcern", wc.clone()),
    ]);
    assert_eq!(field(&run(create), "ok"), &Value::Double(1.0));
    let r = run(doc(&[("usersInfo", s("x")), ("foo", 1.into())]));
    assert_eq!(field(&r, "errmsg"), &s("BSON field 'usersInfo.foo' is an unknown field."));
    let drop = doc(&[("dropUser", s("u")), ("writeConcern", wc.clone())]);
    assert_eq!(field(&run(drop), "ok"), &Value::Double(1.0));
    // `_mergeAuthzCollections` (013 FR-030) takes its four fields and the generic ones.
    let merge = |extra: (&str, Value)| {
        doc(&[("_mergeAuthzCollections", 1.into()), ("db", s("")), ("drop", false.into()), extra])
    };
    let r = run(merge(("foo", 1.into())));
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field '_mergeAuthzCollections.foo' is an unknown field.")
    );
    let r = run(merge(("writeConcern", wc)));
    let only_admin = "_mergeAuthzCollections may only be run against the admin database.";
    assert_eq!(field(&r, "errmsg"), &s(only_admin));
}

#[test]
fn invalidate_user_cache_only_on_admin() {
    let mut m = Mem::new();
    assert_eq!(m.run("admin", doc(&[("invalidateUserCache", 1.into())])), ok());
    let r = m.run(TEST, doc(&[("invalidateUserCache", 1.into())]));
    assert_eq!(
        r,
        error(
            13,
            "Unauthorized",
            "invalidateUserCache may only be run against the admin database."
        )
    );
}

#[test]
fn update_edges() {
    let mut m = Mem::new();
    create(&mut m, "u", &[("pwd", s("p")), ("roles", arr(&[s("read")]))]);
    let upd = |m: &mut Mem, pairs: &[(&str, Value)]| {
        let mut d = doc(&[("updateUser", s("u"))]);
        for (k, v) in pairs {
            d.push(*k, v.clone());
        }
        m.run(TEST, d)
    };
    assert_eq!(upd(&mut m, &[("customData", Value::Document(doc(&[("z", 1.into())])))]), ok());
    assert_eq!(upd(&mut m, &[("mechanisms", arr(&[s("SCRAM-SHA-1"), s("SCRAM-SHA-256")]))]), ok());
    assert_eq!(upd(&mut m, &[("mechanisms", arr(&[s("SCRAM-SHA-256")]))]), ok());
    let u = stored(&m, TEST, "u").unwrap();
    assert!(u.scram_sha1.is_none() && u.scram_sha256.is_some());
    let grant = doc(&[("grantRolesToUser", s("u")), ("roles", arr(&[s("read")]))]);
    assert_eq!(m.run(TEST, grant), ok());
    assert_eq!(stored(&m, TEST, "u").unwrap().roles.len(), 1);
    // A password SASLprep refuses.
    let r = upd(&mut m, &[("pwd", s("a\u{0007}b"))]);
    assert_eq!(field(&r, "codeName"), &s("BadValue"));
}

/// `usersInfo` with `authenticatedMechanism` (013 T045, FR-033), as RefDb Enterprise 9.0.4
/// answers it with LDAP authorization: a known mechanism gives the stored documents, as without
/// it, also for every user; an unknown one and a value that is not a string are refused.
#[test]
fn users_info_with_an_authenticated_mechanism() {
    let mut m = Mem::new();
    create(&mut m, "u1", &[("pwd", s("p")), ("roles", arr(&[s("read")]))]);
    let plain = m.run(TEST, doc(&[("usersInfo", s("u1"))]));
    for mech in ["REFDB-X509", "PLAIN", "SCRAM-SHA-256", "GSSAPI"] {
        let r = m.run(TEST, doc(&[("usersInfo", s("u1")), ("authenticatedMechanism", s(mech))]));
        assert_eq!(r, plain, "{mech}");
    }
    let all = m.run(TEST, doc(&[("usersInfo", 1.into())]));
    let r = m.run(TEST, doc(&[("usersInfo", 1.into()), ("authenticatedMechanism", s("PLAIN"))]));
    assert_eq!(r, all);
    let r = m.run(TEST, doc(&[("usersInfo", s("nobody")), ("authenticatedMechanism", s("PLAIN"))]));
    assert_eq!(field(&r, "users"), &arr(&[]));
    let r = m.run(TEST, doc(&[("usersInfo", s("u1")), ("authenticatedMechanism", s("bogus"))]));
    assert_eq!(
        r,
        error(
            2,
            "BadValue",
            "Unknown authentication mechanism 'bogus'. Supported mechanisms: REFDB-X509, PLAIN, GSSAPI, SCRAM-SHA-1, SCRAM-SHA-256, REFDB-AWS, REFDB-OIDC"
        )
    );
    let r = m.run(TEST, doc(&[("usersInfo", s("u1")), ("authenticatedMechanism", 5.into())]));
    assert_eq!(
        r,
        error(
            14,
            "TypeMismatch",
            "BSON field 'usersInfo.authenticatedMechanism' is the wrong type 'int', expected type 'string'"
        )
    );
}
