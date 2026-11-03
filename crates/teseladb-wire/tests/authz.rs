//! Authorization of each command (013 T023, FR-011) as RefDb 9.0.4 answers it on a `refrouter`
//! (measured 30-10-2026): `Unauthorized` with the command quoted as it came in the body of the
//! message, the cursors of other users, the filtered listings, the default time limit and a node
//! started with `--insecure`, which does not authorize.

mod common;

use common::{Mem, doc, field};
use teseladb_auth::actions::ActionType;
use teseladb_auth::authz_rules::has_rule;
use teseladb_auth::privileges::{Privilege, Resource};
use teseladb_auth::roles::Role;
use teseladb_auth::sasl::SaslMechanism;
use teseladb_auth::scram::{Credentials, Mechanism};
use teseladb_auth::users::{RoleName, User, UserName};
use teseladb_bson::{Document, Value};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_wire::commands::auth::Authenticated;
use teseladb_wire::commands::authorize::{QUOTE_KEEP, REDACTED};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::{HEADER_LEN, Header, opcode};

const ADMIN: &str = "admin";
const TEST: &str = "test";
const UNAUTHORIZED: i32 = 13;

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

/// Stores a user or role document as Group 0 would, replacing the one of that name.
fn store(m: &mut Mem, kind: Principal, db: &str, name: &str, doc: Document) {
    let expect =
        if m.security.get(kind, db, name).is_some() { Expect::Present } else { Expect::Absent };
    let op = SecurityOp::named(kind, db, name, expect, Some(doc.to_bytes().unwrap()));
    m.security.apply(&[op]).unwrap();
}

fn add_user(m: &mut Mem, name: &str, roles: &[(&str, &str)]) {
    let mut u = User::new(UserName::new(name, ADMIN));
    u.roles = roles.iter().map(|(r, d)| RoleName::new(*r, *d)).collect();
    // A stored user of a database other than `$external` has credentials, or RefDb does not
    // use it (measured).
    let m256 = Mechanism::ScramSha256;
    u.scram_sha256 = Some(Credentials::derive(m256, b"pw", &vec![1; m256.salt_len()], 4096));
    store(m, Principal::User, ADMIN, name, u.to_document());
}

fn add_role(m: &mut Mem, name: &str, privileges: Vec<Privilege>) {
    let role = Role {
        name: RoleName::new(name, ADMIN),
        privileges,
        roles: Vec::new(),
        restrictions: None,
    };
    store(m, Principal::Role, ADMIN, name, role.to_document());
}

fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(NodeSessions::new(Timeouts::default()));
    add_user(&mut m, "reader", &[("read", TEST)]);
    add_user(&mut m, "writer", &[("readWrite", TEST)]);
    add_user(&mut m, "root", &[("root", ADMIN)]);
    add_user(&mut m, "nobody", &[]);
    add_role(
        &mut m,
        "fast",
        vec![Privilege {
            resource: Resource::Cluster,
            actions: vec![ActionType::BypassDefaultMaxTimeMS],
        }],
    );
    add_user(&mut m, "fast", &[("fast", ADMIN)]);
    m
}

/// A connection authenticated as `user` on a node that requires authentication.
fn conn(user: &str) -> Connection {
    let mut c = Connection::new(9);
    c.auth.required = true;
    c.auth.user = Some(Authenticated {
        name: UserName::new(user, ADMIN),
        mechanism: SaslMechanism::ScramSha256,
        login_ms: 0,
        roles: None,
        tenant: 0,
    });
    c
}

fn bson(d: &Document) -> Vec<u8> {
    d.to_bytes().unwrap()
}

/// An `OP_MSG` with `body` and the document sequences of `seqs`.
fn message(body: &Document, seqs: &[(&str, Vec<Document>)]) -> Vec<u8> {
    let mut sections = vec![0u8];
    sections.extend_from_slice(&bson(body));
    for (id, docs) in seqs {
        let mut payload = id.as_bytes().to_vec();
        payload.push(0);
        for d in docs {
            payload.extend_from_slice(&bson(d));
        }
        sections.push(1);
        sections.extend_from_slice(&((payload.len() + 4) as i32).to_le_bytes());
        sections.extend_from_slice(&payload);
    }
    let mut out = Vec::new();
    let len = HEADER_LEN + 4 + sections.len();
    Header { length: len as i32, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }
        .write(&mut out);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&sections);
    out
}

fn request(body: &Document, seqs: &[(&str, Vec<Document>)]) -> Request {
    let mut c = Connection::new(1);
    c.receive(&message(body, seqs));
    c.next_request().unwrap().unwrap()
}

fn send(m: &mut Mem, c: &mut Connection, req: &Request) -> Document {
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(c, req, &info(), Some((m, &shared)));
    m.shared = shared;
    match a {
        Action::Reply(mut d) => {
            d.remove("$clusterTime");
            d.remove("operationTime");
            d
        }
        other => panic!("{other:?}"),
    }
}

fn run(m: &mut Mem, c: &mut Connection, db: &str, cmd: Document) -> Document {
    send(m, c, &request(&cmd.with("$db", db), &[]))
}

fn code(d: &Document) -> Option<i32> {
    match d.get("code") {
        Some(Value::Int32(n)) => Some(*n),
        _ => None,
    }
}

fn msg(d: &Document) -> &str {
    match d.get("errmsg") {
        Some(Value::String(s)) => s,
        other => panic!("{other:?} in {d:?}"),
    }
}

fn ok(d: &Document) -> bool {
    d.get("ok") == Some(&Value::Double(1.0))
}

fn names(d: &Document, path: &str) -> Vec<String> {
    let Value::Array(a) = field(d, path) else { panic!("{d:?}") };
    a.iter()
        .map(|x| match x {
            Value::Document(e) => match e.get("name") {
                Some(Value::String(s)) => s.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn every_listed_command_has_its_rule() {
    let mut m = node();
    let mut c = conn("root");
    let r = run(&mut m, &mut c, ADMIN, Document::new().with("listCommands", 1));
    let Value::Document(cmds) = field(&r, "commands") else { panic!("{r:?}") };
    assert!(cmds.iter().count() > 40);
    for (name, _) in cmds.iter() {
        assert!(has_rule(name), "{name} has no authorization rule");
    }
}

#[test]
fn unauthorized_quotes_the_body_without_the_sequences() {
    let mut m = node();
    let mut c = conn("reader");
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![0xab; 16] });
    // `documents` in a sequence: left out of the message.
    let body = Document::new().with("insert", "c").with("lsid", lsid.clone()).with("$db", TEST);
    let docs = vec![Document::new().with("x", 1)];
    let r = send(&mut m, &mut c, &request(&body, &[("documents", docs)]));
    assert_eq!(code(&r), Some(UNAUTHORIZED));
    assert_eq!(
        msg(&r),
        "not authorized on test to execute command { insert: \"c\", lsid: { id: UUID(\"abababab-abab-abab-abab-abababababab\") }, $db: \"test\" }"
    );
    assert_eq!(r.get("codeName"), Some(&Value::from("Unauthorized")));
    // `documents` in the body: quoted.
    let body = Document::new()
        .with("insert", "c")
        .with("documents", vec![Value::Document(Document::new().with("x", 1))])
        .with("ordered", true)
        .with("$db", TEST);
    let r = send(&mut m, &mut c, &request(&body, &[]));
    assert_eq!(
        msg(&r),
        "not authorized on test to execute command { insert: \"c\", documents: [ { x: 1 } ], ordered: true, $db: \"test\" }"
    );
}

#[test]
fn quoted_strings_are_cut_and_passwords_hidden() {
    let mut m = node();
    let mut c = conn("reader");
    let mut at = |n: usize| {
        let cmd = Document::new()
            .with("find", "c")
            .with("filter", Document::new().with("s", "a".repeat(n)));
        let r = run(&mut m, &mut conn("nobody"), TEST, cmd);
        msg(&r).to_owned()
    };
    assert!(at(159).contains(&format!("\"{}\"", "a".repeat(159))));
    let cut = format!("\"{}...\"", "a".repeat(QUOTE_KEEP));
    assert!(at(160).contains(&cut));
    // Inside an array, and a character never split.
    let cmd = Document::new().with("find", "c").with(
        "filter",
        Document::new().with("s", vec![Value::from(format!("a{}", "é".repeat(100)))]),
    );
    let r = run(&mut m, &mut conn("nobody"), TEST, cmd);
    assert!(msg(&r).contains(&format!("[ \"a{}...\" ]", "é".repeat(74))), "{}", msg(&r));
    for name in ["createUser", "updateUser"] {
        let cmd = Document::new()
            .with(name, "z")
            .with("pwd", "secret")
            .with("roles", Vec::<Value>::new());
        let r = run(&mut m, &mut c, TEST, cmd);
        assert_eq!(code(&r), Some(UNAUTHORIZED));
        assert!(msg(&r).contains(&format!("pwd: \"{REDACTED}\"")), "{}", msg(&r));
        assert!(!msg(&r).contains("secret"));
    }
    let cmd =
        Document::new().with("find", "c").with("filter", Document::new().with("pwd", "secret"));
    assert!(msg(&run(&mut m, &mut conn("nobody"), TEST, cmd)).contains("pwd: \"secret\""));
}

#[test]
fn built_in_roles_allow_what_they_hold() {
    let mut m = node();
    let mut reader = conn("reader");
    let find = || Document::new().with("find", "c");
    assert!(ok(&run(&mut m, &mut reader, TEST, find())));
    assert_eq!(code(&run(&mut m, &mut reader, "other", find())), Some(UNAUTHORIZED));
    let insert = || {
        Document::new()
            .with("insert", "c")
            .with("documents", vec![Value::Document(Document::new().with("x", 1))])
    };
    assert_eq!(code(&run(&mut m, &mut reader, TEST, insert())), Some(UNAUTHORIZED));
    let mut writer = conn("writer");
    assert!(ok(&run(&mut m, &mut writer, TEST, insert())));
    let mut root = conn("root");
    assert!(ok(&run(&mut m, &mut root, "other", insert())));
    // A user removed after it authenticated has no privileges left.
    let mut gone = conn("ghost");
    assert_eq!(code(&run(&mut m, &mut gone, TEST, find())), Some(UNAUTHORIZED));
    // `ping` and `hello` need nothing; `connectionStatus` neither.
    let mut nobody = conn("nobody");
    assert!(ok(&run(&mut m, &mut nobody, ADMIN, Document::new().with("ping", 1))));
    assert!(ok(&run(&mut m, &mut nobody, ADMIN, Document::new().with("connectionStatus", 1))));
    assert!(ok(&run(&mut m, &mut nobody, ADMIN, Document::new().with("whatsmyuri", 1))));
    assert_eq!(
        code(&run(&mut m, &mut nobody, ADMIN, Document::new().with("serverStatus", 1))),
        Some(UNAUTHORIZED)
    );
}

#[test]
fn a_node_without_authentication_authorizes_nothing() {
    let mut m = node();
    let mut c = conn("nobody");
    c.auth.required = false;
    let r = run(
        &mut m,
        &mut c,
        TEST,
        Document::new()
            .with("insert", "c")
            .with("documents", vec![Value::Document(Document::new())]),
    );
    assert!(ok(&r), "{r:?}");
    assert!(c.bypass_default_max_time);
}

#[test]
fn the_default_time_limit_follows_the_privilege() {
    let mut m = node();
    let mut slow = conn("reader");
    run(&mut m, &mut slow, TEST, Document::new().with("find", "c"));
    assert!(!slow.bypass_default_max_time);
    let mut fast = conn("fast");
    run(&mut m, &mut fast, TEST, Document::new().with("find", "c"));
    assert!(fast.bypass_default_max_time);
}

#[test]
fn listings_show_only_what_the_user_may_see() {
    let mut m = node();
    let mut root = conn("root");
    for db in [TEST, "other"] {
        let ins = Document::new()
            .with("insert", "c")
            .with("documents", vec![Value::Document(Document::new())]);
        assert!(ok(&run(&mut m, &mut root, db, ins)));
    }
    let ins = Document::new()
        .with("insert", "d")
        .with("documents", vec![Value::Document(Document::new())]);
    assert!(ok(&run(&mut m, &mut root, TEST, ins)));
    let list = || Document::new().with("listDatabases", 1).with("nameOnly", true);
    let all = names(&run(&mut m, &mut root, ADMIN, list()), "databases");
    assert!(all.contains(&"other".to_owned()) && all.contains(&TEST.to_owned()), "{all:?}");
    assert_eq!(names(&run(&mut m, &mut conn("reader"), ADMIN, list()), "databases"), vec![TEST]);
    assert!(names(&run(&mut m, &mut conn("nobody"), ADMIN, list()), "databases").is_empty());
    let strict = list().with("authorizedDatabases", false);
    assert_eq!(code(&run(&mut m, &mut conn("reader"), ADMIN, strict)), Some(UNAUTHORIZED));
    let forced = list().with("authorizedDatabases", true);
    assert_eq!(names(&run(&mut m, &mut root, ADMIN, forced), "databases").len(), all.len());
    // `listCollections` with `authorizedCollections`: the collections a privilege covers.
    add_role(
        &mut m,
        "onlyC",
        vec![Privilege {
            resource: Resource::Ns { db: TEST.into(), collection: "c".into() },
            actions: vec![ActionType::Find],
        }],
    );
    add_user(&mut m, "onlyc", &[("onlyC", ADMIN)]);
    let lc = Document::new()
        .with("listCollections", 1)
        .with("nameOnly", true)
        .with("authorizedCollections", true);
    let r = run(&mut m, &mut conn("onlyc"), TEST, lc.clone());
    assert_eq!(names(&r, "cursor.firstBatch"), vec!["c"]);
    let r = run(&mut m, &mut conn("reader"), TEST, lc.clone());
    assert_eq!(names(&r, "cursor.firstBatch"), vec!["c", "d"]);
    let plain = Document::new().with("listCollections", 1).with("nameOnly", true);
    assert_eq!(code(&run(&mut m, &mut conn("onlyc"), TEST, plain)), Some(UNAUTHORIZED));
    assert_eq!(code(&run(&mut m, &mut conn("onlyc"), "other", lc)), Some(UNAUTHORIZED));
}

#[test]
fn cursors_belong_to_their_user() {
    let mut m = node();
    let mut root = conn("root");
    for _ in 0..3 {
        let ins = Document::new()
            .with("insert", "c")
            .with("documents", vec![Value::Document(Document::new())]);
        run(&mut m, &mut root, TEST, ins);
    }
    let open = |m: &mut Mem, c: &mut Connection| {
        let r = run(m, c, TEST, Document::new().with("find", "c").with("batchSize", 0));
        match field(&r, "cursor.id") {
            Value::Int64(id) => *id,
            other => panic!("{other:?}"),
        }
    };
    let mut reader = conn("reader");
    let mine = open(&mut m, &mut reader);
    let more = |id: i64| {
        Document::new()
            .with("getMore", Value::Int64(id))
            .with("collection", "c")
            .with("batchSize", 1)
    };
    // Another user's cursor.
    let r = run(&mut m, &mut conn("writer"), TEST, more(mine));
    assert_eq!(code(&r), Some(UNAUTHORIZED));
    assert_eq!(
        msg(&r),
        format!(
            "cursor id {mine} was not created by the authenticated user :: caused by :: User not authorized to access cursor"
        )
    );
    // Its own, once `find` is gone.
    add_user(&mut m, "reader", &[]);
    let mut m2 = m;
    let r = run(&mut m2, &mut reader, TEST, more(mine));
    assert_eq!(msg(&r), format!("not authorized for getMore with cursor id {mine}"));
    add_user(&mut m2, "reader", &[("read", TEST)]);
    assert!(ok(&run(&mut m2, &mut reader, TEST, more(mine))));
    // An unknown cursor is the command's to answer.
    let r = run(&mut m2, &mut conn("nobody"), TEST, more(12345));
    assert_ne!(code(&r), Some(UNAUTHORIZED));
    // `killCursors`: one's own without privileges, another's with `killAnyCursor`.
    let theirs = open(&mut m2, &mut reader);
    let kill =
        |id: i64| Document::new().with("killCursors", "c").with("cursors", vec![Value::Int64(id)]);
    let r = run(&mut m2, &mut conn("nobody"), TEST, kill(theirs));
    assert_eq!(code(&r), Some(UNAUTHORIZED));
    assert!(msg(&r).starts_with("not authorized on test to execute command { killCursors: \"c\""));
    assert!(ok(&run(&mut m2, &mut root, TEST, kill(theirs))));
    let own = open(&mut m2, &mut reader);
    add_user(&mut m2, "reader", &[]);
    assert!(ok(&run(&mut m2, &mut reader, TEST, kill(own))));
    assert!(ok(&run(&mut m2, &mut conn("nobody"), TEST, kill(999))));
}

#[test]
fn a_collection_named_by_uuid_is_authorized_by_its_name() {
    let mut m = node();
    let mut root = conn("root");
    let ins = Document::new()
        .with("insert", "c")
        .with("documents", vec![Value::Document(Document::new())]);
    run(&mut m, &mut root, TEST, ins);
    let r = run(&mut m, &mut root, TEST, Document::new().with("listCollections", 1));
    let Value::Binary { bytes, .. } = field(&r, "cursor.firstBatch.0.info.uuid").clone() else {
        panic!("{r:?}")
    };
    add_role(
        &mut m,
        "onlyC",
        vec![Privilege {
            resource: Resource::Ns { db: TEST.into(), collection: "c".into() },
            actions: vec![ActionType::Find],
        }],
    );
    add_user(&mut m, "onlyc", &[("onlyC", ADMIN)]);
    let by_uuid = Document::new().with("find", Value::Binary { subtype: 4, bytes: bytes.clone() });
    let r = run(&mut m, &mut conn("onlyc"), TEST, by_uuid);
    assert_ne!(code(&r), Some(UNAUTHORIZED), "{r:?}");
    let unknown = Document::new().with("find", Value::Binary { subtype: 4, bytes: vec![9; 16] });
    assert_eq!(code(&run(&mut m, &mut conn("onlyc"), TEST, unknown)), Some(UNAUTHORIZED));
    let other_db = Document::new().with("find", Value::Binary { subtype: 4, bytes });
    assert_eq!(code(&run(&mut m, &mut conn("onlyc"), "other", other_db)), Some(UNAUTHORIZED));
}

#[test]
fn commands_the_node_runs_for_itself_are_not_filtered() {
    let mut m = node();
    let ins = doc(&[
        ("insert", Value::from("c")),
        ("documents", Value::Array(vec![Value::Document(Document::new())])),
    ]);
    assert!(ok(&run(&mut m, &mut conn("root"), "other", ins)));
    // Outside an operation of a connection there are no privileges to filter with.
    let r = m
        .run(ADMIN, doc(&[("listDatabases", Value::Int32(1)), ("nameOnly", Value::Boolean(true))]));
    assert!(names(&r, "databases").contains(&"other".to_owned()), "{r:?}");
}

#[test]
fn listing_filters_apply_only_where_measured() {
    let mut m = node();
    let mut root = conn("root");
    let ins = Document::new()
        .with("insert", "c")
        .with("documents", vec![Value::Document(Document::new())]);
    assert!(ok(&run(&mut m, &mut root, TEST, ins)));
    // With `listCollections` on the database the list is whole; without `nameOnly` too.
    let lc = Document::new()
        .with("listCollections", 1)
        .with("nameOnly", true)
        .with("authorizedCollections", true);
    assert_eq!(names(&run(&mut m, &mut root, TEST, lc), "cursor.firstBatch"), vec!["c"]);
    let named = Document::new().with("listCollections", 1).with("nameOnly", true);
    assert_eq!(
        names(&run(&mut m, &mut conn("reader"), TEST, named), "cursor.firstBatch"),
        vec!["c"]
    );
    let partial = Document::new().with("listCollections", 1).with("authorizedCollections", true);
    assert_eq!(
        names(&run(&mut m, &mut conn("reader"), TEST, partial), "cursor.firstBatch"),
        vec!["c"]
    );
}

#[test]
fn a_connection_without_a_user_has_no_privileges() {
    let mut m = node();
    let shared = std::mem::take(&mut m.shared);
    let mut c = Connection::new(3);
    c.auth.required = true;
    let req = request(&Document::new().with("ping", 1).with("$db", ADMIN), &[]);
    let p = teseladb_wire::commands::authorize::authorize(&mut c, &req, "ping", &mut m, &shared);
    assert_eq!(p.unwrap(), Some(Vec::new()));
}

#[test]
fn a_node_without_its_lease_authorizes_nothing() {
    let mut m = node();
    m.lease_lost = true;
    let r = run(&mut m, &mut conn("root"), TEST, Document::new().with("find", "c"));
    assert_eq!(code(&r), Some(13436));
    assert_eq!(r.get("codeName"), Some(&Value::from("NotPrimaryOrSecondary")));
    assert_eq!(msg(&r), teseladb_wire::commands::authorize::LEASE_LOST);
    // The handshake and the commands open before authenticating still answer.
    assert!(ok(&run(&mut m, &mut conn("root"), ADMIN, Document::new().with("ping", 1))));
    m.lease_lost = false;
    assert!(ok(&run(&mut m, &mut conn("root"), TEST, Document::new().with("find", "c"))));
}

#[test]
fn an_operation_sees_the_lease_of_its_node() {
    use teseladb_wire::commands::Backend;
    use teseladb_wire::ops::Guarded;
    let mut m = node();
    let shared = std::mem::take(&mut m.shared);
    m.lease_lost = true;
    assert!(!Guarded::new(&mut m, &shared, 1).authorization_lease());
    m.lease_lost = false;
    assert!(Guarded::new(&mut m, &shared, 1).authorization_lease());
}

/// 013 FR-030: who may write `admin.system.users` and `admin.system.roles` directly, as RefDb
/// 9.0.4 decides it (measured on a `refnode` with `--auth`): `root` and `restore` write the users
/// but not the roles, `userAdminAnyDatabase` and `backup` only read both, the others neither.
#[test]
fn direct_writes_to_users_and_roles_need_their_privileges() {
    let mut m = node();
    let all = ["root", "restore", "userAdminAnyDatabase", "backup", "readWriteAnyDatabase"];
    for r in all {
        add_user(&mut m, &format!("u_{r}"), &[(r, ADMIN)]);
    }
    let json = |t: &str| teseladb_bson::extjson::parse(t).unwrap();
    let expected = [
        ("root", [true, true, true, false]),
        ("restore", [true, true, false, false]),
        ("userAdminAnyDatabase", [true, false, true, false]),
        ("backup", [true, false, true, false]),
        ("readWriteAnyDatabase", [false, false, false, false]),
    ];
    for (role, [find_users, write_users, find_roles, write_roles]) in expected {
        let mut c = conn(&format!("u_{role}"));
        let id = format!("q.{role}");
        for (coll, find, write) in
            [("system.users", find_users, write_users), ("system.roles", find_roles, write_roles)]
        {
            let r =
                run(&mut m, &mut c, ADMIN, json(&format!(r#"{{"find": "{coll}", "limit": 1}}"#)));
            assert_eq!(ok(&r), find, "{role} find {coll}: {r:?}");
            let ins = format!(
                r#"{{"insert": "{coll}", "documents": [{{"_id": "{id}", "user": "{id}", "role": "{id}", "db": "q"}}]}}"#
            );
            let r = run(&mut m, &mut c, ADMIN, json(&ins));
            assert_eq!(code(&r).is_none(), write, "{role} insert {coll}: {r:?}");
            let upd = format!(
                r#"{{"update": "{coll}", "updates": [{{"q": {{"_id": "{id}"}}, "u": {{"$set": {{"x": 1}}}}}}]}}"#
            );
            let r = run(&mut m, &mut c, ADMIN, json(&upd));
            assert_eq!(code(&r).is_none(), write, "{role} update {coll}: {r:?}");
            let del = format!(
                r#"{{"delete": "{coll}", "deletes": [{{"q": {{"_id": "{id}"}}, "limit": 1}}]}}"#
            );
            let r = run(&mut m, &mut c, ADMIN, json(&del));
            assert_eq!(code(&r).is_none(), write, "{role} delete {coll}: {r:?}");
        }
    }
}

/// 013 FR-030: a stored user or role RefDb would not use: `hello` with `saslSupportedMechs`
/// names the fault, `saslStart` answers `Authentication failed.` (measured on RefDb 9.0.4).
#[test]
fn malformed_stored_users_fail_the_login_as_refdb_does() {
    let mut m = node();
    let json = |t: &str| teseladb_bson::extjson::parse(t).unwrap();
    let mut u = User::new(UserName::new("strrole", ADMIN));
    let m256 = Mechanism::ScramSha256;
    u.scram_sha256 = Some(Credentials::derive(m256, b"p", &vec![1; m256.salt_len()], 4096));
    let mut d = u.to_document();
    d.insert("roles", Value::Array(vec![Value::from("readWrite")]));
    store(&mut m, Principal::User, ADMIN, "strrole", d);
    let mut c = Connection::new(9);
    c.auth.required = true;
    let hello = json(r#"{"hello": 1, "saslSupportedMechs": "admin.strrole"}"#);
    let r = run(&mut m, &mut c, ADMIN, hello);
    assert_eq!(code(&r), Some(2), "{r:?}");
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("RoleName must contain a '.' separated database.role pair"))
    );
    let payload = Value::Binary { subtype: 0, bytes: b"n,,n=strrole,r=abcdefghijklmnop".to_vec() };
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", "SCRAM-SHA-256")
        .with("payload", payload);
    let r = run(&mut m, &mut c, ADMIN, start);
    assert_eq!(code(&r), Some(18), "{r:?}");
}

/// A time series lives in one namespace (015 FR-006, T022): a privilege on the collection reads
/// its measurements, one on its `system_buckets` does not, and neither reads its buckets with
/// `rawData` without `performRawDataOperations` (measured on a `refnode` 9.0.4 with
/// authentication on 02-11-2026: code 13).
#[test]
fn a_time_series_is_read_with_privileges_on_its_own_namespace() {
    let mut m = node();
    let ts = Document::new().with("timeField", "t").with("metaField", "m");
    let r = m.run(TEST, Document::new().with("create", "s").with("timeseries", ts));
    assert!(ok(&r), "{r:?}");
    let on = |resource: Resource| Privilege { resource, actions: vec![ActionType::Find] };
    add_role(&mut m, "onColl", vec![on(Resource::Ns { db: TEST.into(), collection: "s".into() })]);
    add_role(
        &mut m,
        "onBuckets",
        vec![on(Resource::SystemBuckets { db: Some(TEST.into()), bucket: "s".into() })],
    );
    add_user(&mut m, "a", &[("onColl", ADMIN)]);
    add_user(&mut m, "b", &[("onBuckets", ADMIN)]);
    let find = || Document::new().with("find", "s");
    let raw = || find().with("rawData", true);
    let mut a = conn("a");
    assert!(ok(&run(&mut m, &mut a, TEST, find())));
    assert!(ok(&run(&mut m, &mut a, TEST, Document::new().with("count", "s"))));
    assert_eq!(code(&run(&mut m, &mut a, TEST, raw())), Some(UNAUTHORIZED));
    let mut b = conn("b");
    assert_eq!(code(&run(&mut m, &mut b, TEST, find())), Some(UNAUTHORIZED));
    assert_eq!(code(&run(&mut m, &mut b, TEST, raw())), Some(UNAUTHORIZED));
    // `rawData` asks for `performRawDataOperations` too, and that action alone reads nothing.
    let ns = || Resource::Ns { db: TEST.into(), collection: "s".into() };
    let raw_ops = ActionType::PerformRawDataOperations;
    add_role(
        &mut m,
        "raw",
        vec![Privilege { resource: ns(), actions: vec![ActionType::Find, raw_ops] }],
    );
    add_role(&mut m, "rawOnly", vec![Privilege { resource: ns(), actions: vec![raw_ops] }]);
    add_user(&mut m, "r", &[("raw", ADMIN)]);
    add_user(&mut m, "o", &[("rawOnly", ADMIN)]);
    let mut r = conn("r");
    assert!(ok(&run(&mut m, &mut r, TEST, raw())));
    assert!(ok(&run(&mut m, &mut r, TEST, find())));
    let mut o = conn("o");
    assert_eq!(code(&run(&mut m, &mut o, TEST, raw())), Some(UNAUTHORIZED));
    assert_eq!(code(&run(&mut m, &mut o, TEST, find())), Some(UNAUTHORIZED));
}

/// What a write answers: its first write error's code, the command's, or `None` when it went.
fn refusal(r: &Document) -> Option<i32> {
    match r.get("writeErrors") {
        Some(_) => match field(r, "writeErrors.0.code") {
            Value::Int32(n) => Some(*n),
            other => panic!("{other:?}"),
        },
        None => code(r),
    }
}

#[test]
fn bypass_write_blocking_mode_lets_its_user_write_while_user_writes_are_blocked() {
    const BLOCKED: i32 = 371;
    let mut m = node();
    add_role(
        &mut m,
        "unblocked",
        vec![
            Privilege {
                resource: Resource::Cluster,
                actions: vec![ActionType::BypassWriteBlockingMode],
            },
            Privilege {
                resource: Resource::Ns { db: TEST.into(), collection: String::new() },
                actions: vec![ActionType::Insert],
            },
        ],
    );
    add_user(&mut m, "unblocked", &[("unblocked", ADMIN)]);
    let mut root = conn("root");
    let insert = || {
        Document::new()
            .with("insert", "c")
            .with("documents", vec![Value::Document(Document::new())])
    };
    let block = |on: bool| Document::new().with("setUserWriteBlockMode", 1).with("global", on);
    assert!(ok(&run(&mut m, &mut root, TEST, insert())));
    assert!(ok(&run(&mut m, &mut root, TEST, Document::new().with("create", "d"))));
    assert!(ok(&run(&mut m, &mut root, ADMIN, block(true))));
    // Without the privilege: blocked, writes and changes of the catalog.
    let mut writer = conn("writer");
    assert_eq!(refusal(&run(&mut m, &mut writer, TEST, insert())), Some(BLOCKED));
    let drop_d = || Document::new().with("drop", "d");
    assert_eq!(refusal(&run(&mut m, &mut writer, TEST, drop_d())), Some(BLOCKED));
    // A node that does not authorize (`--insecure`) blocks everyone, as a RefDb without `--auth`
    // (measured on a `refnode` 9.0.4 on 03-11-2026).
    let mut open = conn("root");
    open.auth.required = false;
    assert_eq!(refusal(&run(&mut m, &mut open, TEST, insert())), Some(BLOCKED));
    // With it, as `root` has through `restore` (measured on a `refrouter` 9.0.4 on 03-11-2026).
    let mut unblocked = conn("unblocked");
    assert_eq!(refusal(&run(&mut m, &mut unblocked, TEST, insert())), None);
    assert_eq!(refusal(&run(&mut m, &mut root, TEST, insert())), None);
    let idx = Document::new().with("createIndexes", "c").with(
        "indexes",
        vec![Value::Document(
            Document::new().with("key", Document::new().with("y", 1)).with("name", "y"),
        )],
    );
    assert_eq!(refusal(&run(&mut m, &mut root, TEST, idx)), None);
    assert_eq!(refusal(&run(&mut m, &mut root, TEST, drop_d())), None);
    assert!(ok(&run(&mut m, &mut root, ADMIN, block(false))));
    assert_eq!(refusal(&run(&mut m, &mut writer, TEST, insert())), None);
}

/// `connectionStatus` with `showPrivileges` lists the privileges of the user's roles, merged by
/// resource, as a `refrouter` 9.0.4 does (measured on 03-11-2026; its order changes from one start
/// to the next, and the battery compares them as a multiset).
#[test]
fn connection_status_shows_the_users_privileges() {
    let mut m = node();
    let status = || Document::new().with("connectionStatus", 1).with("showPrivileges", true);
    let r = run(&mut m, &mut conn("fast"), ADMIN, status());
    let want = vec![Value::Document(
        Document::new()
            .with("resource", Document::new().with("cluster", true))
            .with("actions", vec![Value::from("bypassDefaultMaxTimeMS")]),
    )];
    assert_eq!(field(&r, "authInfo.authenticatedUserPrivileges"), &Value::Array(want), "{r:?}");
    let r = run(&mut m, &mut conn("root"), ADMIN, status());
    let Value::Array(all) = field(&r, "authInfo.authenticatedUserPrivileges") else { panic!() };
    let cluster = all.iter().find_map(|p| match p {
        Value::Document(d)
            if d.get("resource")
                == Some(&Value::Document(Document::new().with("cluster", true))) =>
        {
            d.get("actions").cloned()
        }
        _ => None,
    });
    let Some(Value::Array(actions)) = cluster else { panic!("{r:?}") };
    assert!(actions.contains(&Value::from("bypassWriteBlockingMode")), "{actions:?}");
    let r = run(&mut m, &mut conn("nobody"), ADMIN, status());
    assert_eq!(field(&r, "authInfo.authenticatedUserPrivileges"), &Value::Array(Vec::new()));
}

/// A pattern of `killAllSessionsByPattern` with `users` or `roles` asks for `impersonate`, which
/// not even `root` has; without authorization nothing is asked (measured on 03-11-2026).
#[test]
fn kill_all_sessions_by_pattern_with_users_asks_for_impersonate() {
    let mut m = node();
    let by =
        |p: Document| Document::new().with("killAllSessionsByPattern", vec![Value::Document(p)]);
    let users = || by(Document::new().with("users", Vec::<Value>::new()));
    let r = run(&mut m, &mut conn("root"), ADMIN, users());
    assert_eq!(code(&r), Some(UNAUTHORIZED));
    assert_eq!(msg(&r), "Not authorized to impersonate in killAllSessionsByPattern");
    let roles = by(Document::new().with("roles", Vec::<Value>::new()));
    assert_eq!(code(&run(&mut m, &mut conn("root"), ADMIN, roles)), Some(UNAUTHORIZED));
    assert!(ok(&run(&mut m, &mut conn("root"), ADMIN, by(Document::new()))));
    let mut open = conn("root");
    open.auth.required = false;
    assert!(ok(&run(&mut m, &mut open, ADMIN, users())));
}

/// The `uid` of a user's sessions: the SHA-256 of `user@db`.
fn uid_of(user: &str) -> Value {
    Value::Binary {
        subtype: 0,
        bytes: teseladb_hash::sha256(format!("{user}@{ADMIN}").as_bytes()).to_vec(),
    }
}

/// Session `n`.
fn session(n: u8) -> Value {
    Value::Binary { subtype: 4, bytes: vec![n; 16] }
}

/// `reader` opens a cursor in session 1 and `writer` one in session 2; each pattern of
/// `killAllSessionsByPattern` or kill command `kill` builds then runs as `killer`, and the result
/// is which of the two cursors goes on.
fn survivors(killer: &str, kill: Document) -> (bool, bool) {
    let mut m = node();
    add_role(
        &mut m,
        "imp",
        vec![Privilege {
            resource: Resource::Cluster,
            actions: vec![ActionType::Impersonate, ActionType::KillAnySession],
        }],
    );
    add_user(&mut m, "imp", &[("imp", ADMIN), ("root", ADMIN)]);
    let mut root = conn("root");
    let docs: Vec<Value> =
        (0..3).map(|i| Value::Document(Document::new().with("_id", i))).collect();
    run(&mut m, &mut root, TEST, Document::new().with("insert", "c").with("documents", docs));
    let lsid = |n: u8| Document::new().with("id", session(n));
    let open = |m: &mut Mem, user: &str, n: u8| {
        let find = Document::new().with("find", "c").with("batchSize", 1).with("lsid", lsid(n));
        let r = run(m, &mut conn(user), TEST, find);
        match field(&r, "cursor.id") {
            Value::Int64(id) => *id,
            other => panic!("{other:?}"),
        }
    };
    let ids = [open(&mut m, "reader", 1), open(&mut m, "writer", 2)];
    let r = run(&mut m, &mut conn(killer), ADMIN, kill);
    assert!(ok(&r), "{r:?}");
    let alive = |m: &mut Mem, user: &str, n: u8, id: i64| {
        let more = Document::new()
            .with("getMore", Value::Int64(id))
            .with("collection", "c")
            .with("batchSize", 1)
            .with("lsid", lsid(n));
        ok(&run(m, &mut conn(user), TEST, more))
    };
    (alive(&mut m, "reader", 1, ids[0]), alive(&mut m, "writer", 2, ids[1]))
}

fn by(patterns: Vec<Document>) -> Document {
    let list: Vec<Value> = patterns.into_iter().map(Value::Document).collect();
    Document::new().with("killAllSessionsByPattern", list)
}

fn lsid_of(n: u8, user: &str) -> Document {
    Document::new().with("id", session(n)).with("uid", uid_of(user))
}

/// Each session belongs to the user who started it, and the kills reach the sessions of their
/// owners as on a `refrouter` 9.0.4 with authentication (measured on 03-11-2026): a `uid` reaches
/// every session of its user, an `lsid` the one session with its `id` and `uid`, a `uid` next to
/// an `lsid` wins, and `users` and `roles` choose nothing.
#[test]
fn kill_all_sessions_by_pattern_reaches_the_owners_sessions() {
    let uid = |user: &str| Document::new().with("uid", uid_of(user));
    assert_eq!(survivors("root", by(vec![uid("reader")])), (false, true));
    assert_eq!(survivors("root", by(vec![uid("root")])), (true, true));
    let nobody = Value::Binary { subtype: 0, bytes: teseladb_hash::sha256(b"").to_vec() };
    assert_eq!(survivors("root", by(vec![Document::new().with("uid", nobody)])), (true, true));
    let lsid = |n: u8, user: &str| Document::new().with("lsid", lsid_of(n, user));
    assert_eq!(survivors("root", by(vec![lsid(1, "reader")])), (false, true));
    assert_eq!(survivors("root", by(vec![lsid(1, "writer")])), (true, true));
    let both = lsid(1, "reader").with("uid", uid_of("writer"));
    assert_eq!(survivors("root", by(vec![both])), (true, false));
    assert_eq!(survivors("root", by(vec![uid("reader"), lsid(2, "writer")])), (false, false));
    let user = |u: &str| Value::Document(Document::new().with("user", u).with("db", ADMIN));
    let users = |u: &str| Document::new().with("users", vec![user(u)]);
    assert_eq!(survivors("imp", by(vec![users("reader")])), (false, false));
    assert_eq!(survivors("imp", by(vec![users("nosuch")])), (false, false));
    let role = Value::Document(Document::new().with("role", "read").with("db", TEST));
    assert_eq!(
        survivors("imp", by(vec![Document::new().with("roles", vec![role])])),
        (false, false)
    );
    assert_eq!(
        survivors("imp", by(vec![users("reader").with("uid", uid_of("writer"))])),
        (true, false)
    );
    assert_eq!(survivors("root", by(vec![])), (false, false));
    assert_eq!(survivors("root", by(vec![Document::new()])), (false, false));
}

/// `killAllSessions` reaches the sessions of the users it names, every session with none; and
/// `killSessions` reaches a session of the user who runs it unless the `lsid` names its `uid`
/// (measured on 03-11-2026).
#[test]
fn kill_sessions_and_kill_all_sessions_reach_the_owners_sessions() {
    let user = |u: &str| Value::Document(Document::new().with("user", u).with("db", ADMIN));
    let all = |users: Vec<Value>| Document::new().with("killAllSessions", users);
    assert_eq!(survivors("root", all(vec![user("reader")])), (false, true));
    assert_eq!(survivors("root", all(vec![user("nosuch")])), (true, true));
    assert_eq!(survivors("root", all(vec![])), (false, false));
    let kill = |lsid: Document| Document::new().with("killSessions", vec![Value::Document(lsid)]);
    let id = |n: u8| Document::new().with("id", session(n));
    assert_eq!(survivors("root", kill(id(1))), (true, true));
    assert_eq!(survivors("reader", kill(id(1))), (false, true));
    assert_eq!(survivors("reader", kill(id(2))), (true, true));
    assert_eq!(survivors("root", kill(lsid_of(1, "reader"))), (false, true));
}

/// `$listLocalSessions` gives each session with its owner's `uid` and name, by default those of
/// the user who runs it (measured on a `refrouter` 9.0.4 on 03-11-2026).
#[test]
fn local_sessions_carry_their_owner() {
    let mut m = node();
    let lsid = |n: u8| Document::new().with("id", session(n));
    run(&mut m, &mut conn("reader"), TEST, Document::new().with("find", "c").with("lsid", lsid(1)));
    run(&mut m, &mut conn("writer"), TEST, Document::new().with("find", "c").with("lsid", lsid(2)));
    let list = |spec: Document| {
        let stage = Value::Document(Document::new().with("$listLocalSessions", spec));
        Document::new()
            .with("aggregate", 1)
            .with("pipeline", vec![stage])
            .with("cursor", Document::new())
    };
    let r = run(&mut m, &mut conn("reader"), ADMIN, list(Document::new()));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(batch.len(), 1, "{r:?}");
    let Value::Document(only) = &batch[0] else { panic!("{r:?}") };
    assert_eq!(field(only, "_id.id"), &session(1));
    assert_eq!(field(only, "_id.uid"), &uid_of("reader"));
    assert_eq!(field(only, "user"), &Value::from("reader@admin"));
    let r = run(&mut m, &mut conn("root"), ADMIN, list(Document::new().with("allUsers", true)));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(batch.len(), 2, "{r:?}");
}

/// The record of a session in `config.system.sessions` carries its owner's `uid` and, with a
/// user, `user: {name: "user@db"}` (measured on a `refrouter` 9.0.4 on 03-11-2026).
#[test]
fn the_sessions_record_carries_the_owner() {
    use teseladb_wire::commands::registry::{SESSIONS_COLL, SESSIONS_DB, refresh};
    let mut m = node();
    let lsid = Document::new().with("id", session(1));
    let touch = Document::new().with("refreshSessions", vec![Value::Document(lsid)]);
    assert!(ok(&run(&mut m, &mut conn("reader"), ADMIN, touch)));
    let shared = std::mem::take(&mut m.shared);
    let now = m.now;
    assert_eq!(refresh(&mut m, &shared, 0, now).unwrap().written, 1);
    m.shared = shared;
    let r = m.run(SESSIONS_DB, Document::new().with("find", SESSIONS_COLL));
    assert_eq!(field(&r, "cursor.firstBatch.0._id.uid"), &uid_of("reader"));
    assert_eq!(field(&r, "cursor.firstBatch.0.user.name"), &Value::from("reader@admin"));
}
