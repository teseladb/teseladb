//! Role commands as RefDb 9.0.4 answers them (013 T022, FR-009, FR-010; measured on a
//! `refrouter` with `--keyFile`, 30-10-2026): custom roles with privileges and inherited roles,
//! their checks, the role graph without cycles, `dropRole` taking the role from every user and
//! role, `rolesInfo` in each of its forms and `usersInfo` with what a user inherits.

mod common;

use common::{Mem, doc, field};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_bson::{Document, Value};

const TEST: &str = "test";

fn json(text: &str) -> Document {
    teseladb_bson::extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn s(x: &str) -> Value {
    Value::from(x)
}

fn arr(items: &[Value]) -> Value {
    Value::Array(items.to_vec())
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn error(code: i32, name: &str, msg: &str) -> Document {
    doc(&[("ok", 0.0.into()), ("errmsg", s(msg)), ("code", code.into()), ("codeName", s(name))])
}

fn ok() -> Document {
    doc(&[("ok", 1.0.into())])
}

fn privilege(db: &str, coll: &str, actions: &[&str]) -> Value {
    let acts: Vec<Value> = actions.iter().map(|a| s(a)).collect();
    d(&[
        ("resource", d(&[("db", s(db)), ("collection", s(coll))])),
        ("actions", Value::Array(acts)),
    ])
}

fn role_ref(role: &str, db: &str) -> Value {
    d(&[("role", s(role)), ("db", s(db))])
}

fn keys(v: &Value) -> Vec<String> {
    match v {
        Value::Document(d) => d.iter().map(|(k, _)| k.to_owned()).collect(),
        other => panic!("{other:?}"),
    }
}

fn first(r: &Document, list: &str) -> Value {
    match field(r, list) {
        Value::Array(a) => a[0].clone(),
        other => panic!("{other:?}"),
    }
}

fn create(m: &mut Mem, name: &str, privs: &[Value], roles: &[Value]) -> Document {
    let cmd = doc(&[("createRole", s(name)), ("privileges", arr(privs)), ("roles", arr(roles))]);
    m.run(TEST, cmd)
}

/// r1: find on test.c and `read`; r2: inherits r1, with a restriction.
fn two_roles(m: &mut Mem) {
    assert_eq!(create(m, "r1", &[privilege(TEST, "c", &["insert", "find"])], &[s("read")]), ok());
    let restriction = arr(&[d(&[("clientSource", arr(&[s("127.0.0.1")]))])]);
    let cmd = doc(&[
        ("createRole", s("r2")),
        ("privileges", arr(&[])),
        ("roles", arr(&[role_ref("r1", TEST)])),
        ("authenticationRestrictions", restriction),
    ]);
    assert_eq!(m.run(TEST, cmd), ok());
}

#[test]
fn create_stores_the_role_and_checks_it() {
    let mut m = Mem::new();
    two_roles(&mut m);
    let stored = m.security.get(Principal::Role, TEST, "r1").unwrap();
    let stored = Document::from(teseladb_bson::RawDoc::from_bytes(stored).unwrap());
    let expected = doc(&[
        ("_id", s("test.r1")),
        ("role", s("r1")),
        ("db", s(TEST)),
        ("privileges", arr(&[privilege(TEST, "c", &["find", "insert"])])),
        ("roles", arr(&[role_ref("read", TEST)])),
    ]);
    assert_eq!(stored, expected);
    let bad = |msg: &str| error(2, "BadValue", msg);
    let cases = [
        (
            create(&mut m, "r1", &[], &[]),
            error(51002, "Location51002", "Role \"r1@test\" already exists"),
        ),
        (
            create(&mut m, "read", &[], &[]),
            bad("Cannot create roles with the same name as a built-in role"),
        ),
        (create(&mut m, "", &[], &[]), bad("Role name must be non-empty")),
        (
            create(&mut m, "r3", &[privilege("other", "c", &["find"])], &[]),
            error(
                49,
                "InvalidRoleModification",
                "Roles on the 'test' database cannot be granted privileges that target other databases or the cluster",
            ),
        ),
        (
            create(&mut m, "r3", &[privilege(TEST, "", &["nosuchaction"])], &[]),
            bad("Unknown action type in privilege set: 'nosuchaction'"),
        ),
        (
            create(&mut m, "r3", &[privilege(TEST, "", &[])], &[]),
            bad("'actions' field of privilege resource must not be empty"),
        ),
        (
            create(&mut m, "r3", &[d(&[("actions", arr(&[s("find")]))])], &[]),
            error(
                40414,
                "IDLFailedToParse",
                "BSON field 'createRole.privileges.resource' is missing but a required field",
            ),
        ),
        (
            create(
                &mut m,
                "r3",
                &[d(&[("resource", 5.into()), ("actions", arr(&[s("find")]))])],
                &[],
            ),
            error(
                14,
                "TypeMismatch",
                "BSON field 'createRole.privileges.resource' is the wrong type 'int', expected type 'object'",
            ),
        ),
        (
            create(&mut m, "r3", &[privilege(TEST, "", &["find"])], &[s("nosuch")]),
            error(
                31,
                "RoleNotFound",
                "Cannot grant roles to 'r3@test': Could not find role: nosuch@test",
            ),
        ),
    ];
    for (got, expected) in cases {
        assert_eq!(got, expected);
    }
    let r = m.run(TEST, doc(&[("createRole", s("r3")), ("roles", arr(&[]))]));
    assert_eq!(
        r,
        error(
            40414,
            "IDLFailedToParse",
            "BSON field 'createRole.privileges' is missing but a required field"
        )
    );
    let r = m.run(TEST, doc(&[("createRole", s("r3")), ("privileges", arr(&[]))]));
    assert_eq!(
        r,
        error(
            40414,
            "IDLFailedToParse",
            "BSON field 'createRole.roles' is missing but a required field"
        )
    );
    // A role of `admin` may name the cluster and every resource.
    let any =
        d(&[("resource", d(&[("anyResource", true.into())])), ("actions", arr(&[s("anyAction")]))]);
    let cmd = doc(&[
        ("createRole", s("r4")),
        ("privileges", arr(&[privilege("", "", &["find"]), any])),
        ("roles", arr(&[])),
    ]);
    assert_eq!(m.run("admin", cmd), ok());
}

#[test]
fn roles_info_in_each_form() {
    let mut m = Mem::new();
    two_roles(&mut m);
    let r = m.run(TEST, doc(&[("rolesInfo", 1.into())]));
    let Value::Array(list) = field(&r, "roles") else { panic!() };
    assert_eq!(list.len(), 2);
    assert_eq!(keys(&list[0]), ["_id", "role", "db", "roles", "isBuiltin", "inheritedRoles"]);
    let r = m.run(
        TEST,
        doc(&[
            ("rolesInfo", s("r2")),
            ("showPrivileges", true.into()),
            ("showAuthenticationRestrictions", true.into()),
        ]),
    );
    let r2 = first(&r, "roles");
    let expected = [
        "_id",
        "role",
        "db",
        "privileges",
        "roles",
        "authenticationRestrictions",
        "inheritedRoles",
        "inheritedPrivileges",
        "inheritedAuthenticationRestrictions",
        "isBuiltin",
    ];
    assert_eq!(keys(&r2), expected);
    let Value::Document(r2) = r2 else { panic!() };
    assert_eq!(field(&r2, "inheritedRoles"), &arr(&[role_ref("r1", TEST), role_ref("read", TEST)]));
    let Value::Array(privs) = field(&r2, "inheritedPrivileges") else { panic!() };
    assert_eq!(privs[0], privilege(TEST, "c", &["find", "insert"]));
    assert_eq!(privs.len(), 3, "r1's privilege, then read's on the database and on system.js");
    let restriction = arr(&[d(&[("clientSource", arr(&[s("127.0.0.1")]))])]);
    assert_eq!(field(&r2, "authenticationRestrictions"), &arr(std::slice::from_ref(&restriction)));
    assert_eq!(field(&r2, "inheritedAuthenticationRestrictions"), &arr(&[restriction]));
    // Listings with privileges and with restrictions.
    let r = m.run(TEST, doc(&[("rolesInfo", 1.into()), ("showPrivileges", true.into())]));
    assert_eq!(
        keys(&first(&r, "roles")),
        [
            "_id",
            "role",
            "db",
            "privileges",
            "roles",
            "isBuiltin",
            "inheritedRoles",
            "inheritedPrivileges"
        ]
    );
    let r = m.run(
        TEST,
        doc(&[("rolesInfo", 1.into()), ("showAuthenticationRestrictions", true.into())]),
    );
    let Value::Array(list) = field(&r, "roles") else { panic!() };
    assert_eq!(
        keys(&list[1]),
        [
            "_id",
            "role",
            "db",
            "roles",
            "authenticationRestrictions",
            "isBuiltin",
            "inheritedRoles",
            "inheritedAuthenticationRestrictions"
        ]
    );
    // Built-in roles: in a listing, and by name.
    let r = m.run(TEST, doc(&[("rolesInfo", 1.into()), ("showBuiltinRoles", true.into())]));
    let Value::Array(list) = field(&r, "roles") else { panic!() };
    assert_eq!(list.len(), 8);
    assert_eq!(keys(&list[0]), ["role", "db", "isBuiltin", "roles", "inheritedRoles"]);
    let r = m.run(
        "admin",
        doc(&[
            ("rolesInfo", 1.into()),
            ("showBuiltinRoles", true.into()),
            ("showPrivileges", true.into()),
        ]),
    );
    let Value::Array(list) = field(&r, "roles") else { panic!() };
    assert_eq!(list.len(), 21);
    assert_eq!(
        keys(&list[0]),
        ["role", "db", "isBuiltin", "roles", "inheritedRoles", "privileges", "inheritedPrivileges"]
    );
    let r = m.run(TEST, doc(&[("rolesInfo", s("read")), ("showPrivileges", true.into())]));
    assert_eq!(
        keys(&first(&r, "roles")),
        ["db", "role", "roles", "privileges", "inheritedRoles", "inheritedPrivileges", "isBuiltin"]
    );
    let r =
        m.run(TEST, doc(&[("rolesInfo", arr(&[s("r1"), role_ref("read", TEST), s("nosuch")]))]));
    let Value::Array(list) = field(&r, "roles") else { panic!() };
    assert_eq!(list.len(), 2);
    assert_eq!(keys(&list[1]), ["db", "role", "roles", "inheritedRoles", "isBuiltin"]);
    let r = m.run(TEST, doc(&[("rolesInfo", s("r2")), ("showPrivileges", s("asUserFragment"))]));
    let Value::Document(fragment) = field(&r, "userFragment") else { panic!("{r:?}") };
    assert_eq!(
        keys(&Value::Document(fragment.clone())),
        ["roles", "inheritedRoles", "inheritedPrivileges"]
    );
    let r = m.run(TEST, doc(&[("rolesInfo", 1.into()), ("showPrivileges", s("asUserFragment"))]));
    assert_eq!(
        r,
        error(20, "IllegalOperation", "Cannot get user fragment for all roles in a database")
    );
    let cases = [
        (
            d(&[("role", s("read"))]),
            error(2, "BadValue", "RoleName must contain a field named: db"),
        ),
        (5.into(), error(2, "BadValue", "RoleName must be either a string or an object")),
    ];
    for (v, expected) in cases {
        assert_eq!(m.run(TEST, doc(&[("rolesInfo", v)])), expected);
    }
    assert_eq!(field(&m.run(TEST, doc(&[("rolesInfo", s("nosuch"))])), "roles"), &arr(&[]));
    let r = m.run(TEST, doc(&[("rolesInfo", 1.into()), ("showBuiltinRoles", s("x"))]));
    assert_eq!(field(&r, "code"), &Value::Int32(14));
}

#[test]
fn update_grant_and_revoke() {
    let mut m = Mem::new();
    two_roles(&mut m);
    let run = |m: &mut Mem, pairs: &[(&str, Value)]| m.run(TEST, doc(pairs));
    assert_eq!(
        run(
            &mut m,
            &[("updateRole", s("r1")), ("privileges", arr(&[privilege(TEST, "d", &["update"])]))]
        ),
        ok()
    );
    assert_eq!(
        run(&mut m, &[("updateRole", s("r1"))]),
        error(2, "BadValue", "Must specify at least one field to update in updateRole")
    );
    assert_eq!(
        run(&mut m, &[("updateRole", s("nosuch")), ("roles", arr(&[]))]),
        error(31, "RoleNotFound", "Could not find role: nosuch@test")
    );
    assert_eq!(
        run(&mut m, &[("updateRole", s("read")), ("roles", arr(&[]))]),
        error(31, "RoleNotFound", "Role read@test not found")
    );
    let cycle = error(
        49,
        "InvalidRoleModification",
        "Granting roles to r1@test would introduce a cycle in the role graph",
    );
    assert_eq!(run(&mut m, &[("updateRole", s("r1")), ("roles", arr(&[s("r2")]))]), cycle);
    assert_eq!(run(&mut m, &[("grantRolesToRole", s("r1")), ("roles", arr(&[s("r2")]))]), cycle);
    assert_eq!(run(&mut m, &[("grantRolesToRole", s("r1")), ("roles", arr(&[s("r1")]))]), cycle);
    let restriction = arr(&[d(&[("serverAddress", arr(&[s("10.0.0.1")]))])]);
    assert_eq!(
        run(&mut m, &[("updateRole", s("r1")), ("authenticationRestrictions", restriction)]),
        ok()
    );
    let grant = arr(&[privilege(TEST, "d", &["remove"]), privilege(TEST, "e", &["find"])]);
    assert_eq!(run(&mut m, &[("grantPrivilegesToRole", s("r1")), ("privileges", grant)]), ok());
    assert_eq!(
        run(
            &mut m,
            &[
                ("revokePrivilegesFromRole", s("r1")),
                ("privileges", arr(&[privilege(TEST, "d", &["update"])]))
            ]
        ),
        ok()
    );
    let r = run(&mut m, &[("rolesInfo", s("r1")), ("showPrivileges", true.into())]);
    let Value::Document(r1) = first(&r, "roles") else { panic!() };
    assert_eq!(
        field(&r1, "privileges"),
        &arr(&[privilege(TEST, "d", &["remove"]), privilege(TEST, "e", &["find"])])
    );
    assert_eq!(
        run(
            &mut m,
            &[("grantRolesToRole", s("r1")), ("roles", arr(&[s("dbAdmin"), s("dbAdmin")]))]
        ),
        ok()
    );
    assert_eq!(
        run(
            &mut m,
            &[("revokeRolesFromRole", s("r1")), ("roles", arr(&[s("dbAdmin"), s("nosuch")]))]
        ),
        ok()
    );
    let builtin = error(2, "BadValue", "read@test is a built-in role and cannot be modified");
    assert_eq!(
        run(&mut m, &[("grantRolesToRole", s("read")), ("roles", arr(&[s("r1")]))]),
        builtin
    );
    assert_eq!(
        run(
            &mut m,
            &[
                ("grantPrivilegesToRole", s("read")),
                ("privileges", arr(&[privilege(TEST, "c", &["find"])]))
            ]
        ),
        builtin
    );
    assert_eq!(
        run(&mut m, &[("grantRolesToRole", s("r1")), ("roles", arr(&[]))]),
        error(2, "BadValue", "grantRolesToRole command requires a non-empty \"roles\" array")
    );
    assert_eq!(
        run(&mut m, &[("grantPrivilegesToRole", s("r1")), ("privileges", arr(&[]))]),
        error(
            2,
            "BadValue",
            "grantPrivilegesToRole command requires a non-empty \"privileges\" array"
        )
    );
    assert_eq!(
        run(&mut m, &[("grantPrivilegesToRole", s("r1"))]),
        error(
            40414,
            "IDLFailedToParse",
            "BSON field 'grantPrivilegesToRole.privileges' is missing but a required field"
        )
    );
    assert_eq!(
        run(&mut m, &[("grantRolesToRole", s("r1"))]),
        error(
            40414,
            "IDLFailedToParse",
            "BSON field 'grantRolesToRole.roles' is missing but a required field"
        )
    );
    let not_found = error(31, "RoleNotFound", "Role nosuch@test not found");
    assert_eq!(
        run(
            &mut m,
            &[
                ("grantPrivilegesToRole", s("nosuch")),
                ("privileges", arr(&[privilege(TEST, "c", &["find"])]))
            ]
        ),
        not_found
    );
    assert_eq!(
        run(&mut m, &[("grantRolesToRole", s("nosuch")), ("roles", arr(&[s("read")]))]),
        not_found
    );
    assert_eq!(
        run(&mut m, &[("grantRolesToRole", s("r1")), ("roles", arr(&[s("zzz")]))]),
        error(31, "RoleNotFound", "Could not find role: zzz@test")
    );
    m.fail_security_writes = usize::MAX;
    let r = run(&mut m, &[("grantRolesToRole", s("r1")), ("roles", arr(&[s("dbOwner")]))]);
    assert_eq!(field(&r, "code"), &Value::Int32(112));
    for cmd in [doc(&[("dropRole", s("r1"))]), doc(&[("dropAllRolesFromDatabase", 1.into())])] {
        assert_eq!(field(&m.run(TEST, cmd), "code"), &Value::Int32(112));
    }
    m.fail_security_writes = 0;
    m.drop_role_before_write = Some(("test".into(), "r1".into()));
    let r = run(&mut m, &[("grantRolesToRole", s("r1")), ("roles", arr(&[s("dbOwner")]))]);
    assert_eq!(r, not_found_r1());
}

fn not_found_r1() -> Document {
    error(31, "RoleNotFound", "Role r1@test not found")
}

#[test]
fn drop_takes_the_role_from_users_and_roles() {
    let mut m = Mem::new();
    two_roles(&mut m);
    let user =
        doc(&[("createUser", s("u")), ("pwd", s("p")), ("roles", arr(&[s("r1"), s("read")]))]);
    assert_eq!(m.run(TEST, user), ok());
    let r = m.run(TEST, doc(&[("usersInfo", s("u")), ("showPrivileges", true.into())]));
    let u = first(&r, "users");
    assert_eq!(
        keys(&u),
        [
            "_id",
            "userId",
            "user",
            "db",
            "mechanisms",
            "roles",
            "inheritedRoles",
            "inheritedPrivileges",
            "inheritedAuthenticationRestrictions"
        ]
    );
    assert_eq!(m.run(TEST, doc(&[("dropRole", s("r1"))])), ok());
    let r = m.run(TEST, doc(&[("rolesInfo", s("r2"))]));
    let Value::Document(r2) = first(&r, "roles") else { panic!() };
    assert_eq!(field(&r2, "roles"), &arr(&[]));
    let r = m.run(TEST, doc(&[("usersInfo", s("u"))]));
    let Value::Document(u) = first(&r, "users") else { panic!() };
    assert_eq!(field(&u, "roles"), &arr(&[role_ref("read", TEST)]));
    assert_eq!(
        m.run(TEST, doc(&[("dropRole", s("r1"))])),
        error(31, "RoleNotFound", "Could not find role: r1@test")
    );
    assert_eq!(
        m.run(TEST, doc(&[("dropRole", s("read"))])),
        error(2, "BadValue", "read@test is a built-in role and cannot be modified")
    );
    let r3 = doc(&[("createRole", s("r3")), ("privileges", arr(&[])), ("roles", arr(&[s("r2")]))]);
    assert_eq!(m.run(TEST, r3), ok());
    let user = doc(&[("grantRolesToUser", s("u")), ("roles", arr(&[s("r3")]))]);
    assert_eq!(m.run(TEST, user), ok());
    let r =
        m.run(TEST, doc(&[("usersInfo", s("u")), ("showAuthenticationRestrictions", true.into())]));
    let Value::Document(u) = first(&r, "users") else { panic!() };
    assert_eq!(
        field(&u, "inheritedRoles"),
        &arr(&[role_ref("read", TEST), role_ref("r3", TEST), role_ref("r2", TEST)])
    );
    assert_eq!(
        m.run(TEST, doc(&[("dropAllRolesFromDatabase", 1.into())])),
        doc(&[("n", 2.into()), ("ok", 1.0.into())])
    );
    assert_eq!(
        m.run(TEST, doc(&[("dropAllRolesFromDatabase", 1.into())])),
        doc(&[("n", 0.into()), ("ok", 1.0.into())])
    );
    let r = m.run(TEST, doc(&[("usersInfo", s("u"))]));
    let Value::Document(u) = first(&r, "users") else { panic!() };
    assert_eq!(field(&u, "roles"), &arr(&[role_ref("read", TEST)]));
}

/// Edge cases of the graph and of the stored documents: a role granted twice, a role that
/// vanished from under a user, documents that do not decode, drops that leave other users and
/// roles alone, and the remaining forms of `rolesInfo` and `usersInfo`.
#[test]
fn graph_and_document_edges() {
    let mut m = Mem::new();
    two_roles(&mut m);
    assert_eq!(
        m.run(TEST, doc(&[("grantRolesToRole", s("r2")), ("roles", arr(&[s("r1")]))])),
        ok()
    );
    let r3 =
        doc(&[("createRole", s("r3")), ("privileges", arr(&[])), ("roles", arr(&[s("read")]))]);
    assert_eq!(m.run(TEST, r3), ok());
    let user = |name: &str, roles: &[Value]| {
        doc(&[("createUser", s(name)), ("pwd", s("p")), ("roles", arr(roles))])
    };
    let custom = d(&[("x", 1.into())]);
    let restriction = arr(&[d(&[("clientSource", arr(&[s("127.0.0.1")]))])]);
    let mut u = user("u", &[s("r2")]);
    u.push("customData", custom);
    u.push("authenticationRestrictions", restriction);
    assert_eq!(m.run(TEST, u), ok());
    assert_eq!(m.run(TEST, user("v", &[s("read")])), ok());
    let r = m.run(
        TEST,
        doc(&[
            ("usersInfo", s("u")),
            ("showPrivileges", true.into()),
            ("showAuthenticationRestrictions", true.into()),
            ("showCredentials", true.into()),
        ]),
    );
    let expected = [
        "_id",
        "userId",
        "user",
        "db",
        "mechanisms",
        "credentials",
        "authenticationRestrictions",
        "customData",
        "roles",
        "inheritedRoles",
        "inheritedPrivileges",
        "inheritedAuthenticationRestrictions",
    ];
    assert_eq!(keys(&first(&r, "users")), expected);
    // A role removed under the user: what it gave is gone, nothing fails.
    m.security.remove(Principal::Role, TEST, "r1");
    let r = m.run(TEST, doc(&[("usersInfo", s("u")), ("showPrivileges", true.into())]));
    let Value::Document(u) = first(&r, "users") else { panic!() };
    assert_eq!(field(&u, "inheritedRoles"), &arr(&[role_ref("r2", TEST)]));
    // Dropping r3 leaves u, v and r2 as they are.
    assert_eq!(m.run(TEST, doc(&[("dropRole", s("r3"))])), ok());
    let r = m.run(TEST, doc(&[("usersInfo", s("v"))]));
    let Value::Document(v) = first(&r, "users") else { panic!() };
    assert_eq!(field(&v, "roles"), &arr(&[role_ref("read", TEST)]));
    let r = m.run(TEST, doc(&[("rolesInfo", 1.0.into())]));
    let Value::Array(list) = field(&r, "roles") else { panic!() };
    assert_eq!(list.len(), 1);
    let r = m.run(TEST, doc(&[("rolesInfo", 2.0.into())]));
    assert_eq!(r, error(2, "BadValue", "RoleName must be either a string or an object"));
    let r = m.run(TEST, doc(&[("rolesInfo", 1.into()), ("showPrivileges", s("x"))]));
    assert_eq!(field(&r, "code"), &Value::Int32(14));
    let r = m.run(TEST, doc(&[("usersInfo", d(&[("forAllDBs", true.into()), ("x", 1.into())]))]));
    assert_eq!(field(&r, "code"), &Value::Int32(2));
    // Stored documents written directly: what RefDb 9.0.4 answers for them (measured).
    let bad_role = json(
        r#"{"_id": "test.bad", "role": "bad", "db": "test", "roles": {"role": "read", "db": "test"}}"#,
    );
    let op = SecurityOp::put(Principal::Role, Expect::Absent, bad_role.to_bytes().unwrap());
    m.security.apply(&[op.unwrap()]).unwrap();
    let r = m.run(TEST, doc(&[("rolesInfo", s("bad"))]));
    let msg = "Failed fetching role 'bad@test': Invalid roles field, expected array";
    assert_eq!(r, error(2, "BadValue", msg));
    let bare = json(r#"{"_id": "test.bare", "user": "bare", "db": "test"}"#);
    let op = SecurityOp::put(Principal::User, Expect::Absent, bare.to_bytes().unwrap());
    m.security.apply(&[op.unwrap()]).unwrap();
    let r = m.run(TEST, doc(&[("usersInfo", s("bare"))]));
    let shown = json(r#"{"_id": "test.bare", "user": "bare", "db": "test", "mechanisms": null}"#);
    assert_eq!(field(&r, "users"), &Value::Array(vec![Value::Document(shown)]));
}
