//! 007 T038 (FR-025, DV-023): each request runs in a tenant, the one of the user its connection
//! authenticated as, or the one `$tenant` names on a connection of the default tenant; a tenant
//! sees only its databases, the keys of its collections carry it, and a user belongs to the
//! tenant of the session that created it.

mod common;

use common::{Mem, doc, field};
use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_keys::space::KeyParts;
use teseladb_meta::security::Principal;
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;
use teseladb_bson::{Document, RawDoc, Value};

const ACME: &str = "acme";
const PASSWORD: &str = "secret1";

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

fn request(cmd: &Document) -> Request {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut c = Connection::new(1);
    c.receive(&bytes);
    c.next_request().unwrap().unwrap()
}

fn run(m: &mut Mem, c: &mut Connection, db: &str, cmd: Document) -> Document {
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(c, &request(&cmd.with("$db", db)), &info(), Some((m, &shared)));
    m.shared = shared;
    match a {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

/// `cmd` in tenant `acme`.
fn as_acme(cmd: Document) -> Document {
    cmd.with("$tenant", ACME)
}

fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(NodeSessions::new(Timeouts::default()));
    m.tenants = vec![(1, ACME.to_owned())];
    m
}

fn insert(id: i32) -> Document {
    let one = Value::Document(doc(&[("_id", id.into())]));
    doc(&[("insert", "c".into()), ("documents", Value::Array(vec![one]))])
}

fn found(r: &Document) -> usize {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => a.len(),
        other => panic!("{other:?}"),
    }
}

fn names(r: &Document) -> Vec<String> {
    let Value::Array(dbs) = field(r, "databases") else { panic!("{r:?}") };
    dbs.iter()
        .map(|d| match d {
            Value::Document(d) => match d.get("name") {
                Some(Value::String(n)) => n.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

#[test]
fn each_tenant_sees_its_own_databases_and_its_keys_carry_it() {
    let (mut m, mut c) = (node(), Connection::new(9));
    assert!(ok(&run(&mut m, &mut c, "app", insert(1))));
    let r = run(&mut m, &mut c, "app", as_acme(insert(2)));
    assert!(ok(&r), "{r:?}");
    let r = run(&mut m, &mut c, "app", as_acme(insert(3)));
    assert!(ok(&r), "{r:?}");
    // The same name in each tenant is another database.
    let find = || doc(&[("find", "c".into())]);
    assert_eq!(found(&run(&mut m, &mut c, "app", find())), 1);
    assert_eq!(found(&run(&mut m, &mut c, "app", as_acme(find()))), 2);
    let list = || doc(&[("listDatabases", 1.into()), ("nameOnly", true.into())]);
    let has = |r: &Document, db: &str| names(r).iter().any(|n| n == db);
    assert!(has(&run(&mut m, &mut c, "admin", list()), "app"));
    assert!(has(&run(&mut m, &mut c, "admin", as_acme(list())), "app"));
    // A tenant's database that the default tenant does not have stays out of its list.
    assert!(ok(&run(&mut m, &mut c, "other", as_acme(insert(4)))));
    assert!(!has(&run(&mut m, &mut c, "admin", list()), "other"));
    assert!(has(&run(&mut m, &mut c, "admin", as_acme(list())), "other"));
    // Each document's key carries its collection's tenant.
    let tenants: Vec<u32> = m
        .data
        .keys()
        .filter_map(|k| match KeyParts::of(k) {
            Ok(KeyParts::Data { tenant, index: 0, .. }) => Some(tenant),
            _ => None,
        })
        .collect();
    assert_eq!(tenants.iter().filter(|t| **t == 0).count(), 1, "{tenants:?}");
    assert_eq!(tenants.iter().filter(|t| **t == 1).count(), 3, "{tenants:?}");
    // Dropping a tenant's database leaves the other's.
    let drop = doc(&[("dropDatabase", 1.into())]);
    assert!(ok(&run(&mut m, &mut c, "app", as_acme(drop))));
    assert_eq!(found(&run(&mut m, &mut c, "app", find())), 1);
    assert_eq!(found(&run(&mut m, &mut c, "app", as_acme(find()))), 0);
}

#[test]
fn a_tenant_nobody_created_is_refused() {
    let (mut m, mut c) = (node(), Connection::new(9));
    let r = run(&mut m, &mut c, "app", insert(1).with("$tenant", "nope"));
    assert_eq!(r.get("code"), Some(&Value::Int32(2)), "{r:?}");
    assert_eq!(r.get("errmsg"), Some(&Value::from("no tenant named 'nope'")));
    let r = run(&mut m, &mut c, "app", insert(1).with("$tenant", 1));
    assert_eq!(r.get("code"), Some(&Value::Int32(14)), "{r:?}");
    // A command run outside the node's table of operations is the default tenant's.
    let r = m.run("app", doc(&[("find", "c".into())]));
    assert_eq!(found(&r), 0, "{r:?}");
}

/// Logs connection `c` in as `user` of `admin` with SCRAM-SHA-256.
fn log_in(m: &mut Mem, c: &mut Connection, user: &str) {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(user, PASSWORD, false).unwrap();
    let (mut client, first) = ClientSession::start(mech, user, &prepared, "clientnonce0123");
    let bin = |b: &[u8]| Value::Binary { subtype: 0, bytes: b.to_vec() };
    let payload = |r: &Document| match field(r, "payload") {
        Value::Binary { bytes, .. } => bytes.clone(),
        other => panic!("{other:?}"),
    };
    let start = doc(&[
        ("saslStart", 1.into()),
        ("mechanism", mech.name().into()),
        ("payload", bin(&first)),
        ("options", Value::Document(doc(&[("skipEmptyExchange", true.into())]))),
    ]);
    let r = run(m, c, "admin", start);
    assert!(ok(&r), "{r:?}");
    let answer = client.respond(&payload(&r)).unwrap();
    let next =
        doc(&[("saslContinue", 1.into()), ("conversationId", 1.into()), ("payload", bin(&answer))]);
    let r = run(m, c, "admin", next);
    assert_eq!(field(&r, "done"), &Value::Boolean(true), "{r:?}");
}

#[test]
fn a_user_belongs_to_the_tenant_that_created_it_and_works_in_it() {
    let (mut m, mut c) = (node(), Connection::new(9));
    assert!(ok(&run(&mut m, &mut c, "app", as_acme(insert(1)))));
    let create = doc(&[
        ("createUser", "ann".into()),
        ("pwd", PASSWORD.into()),
        ("roles", Value::Array(vec![Value::from("root")])),
        ("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")])),
    ]);
    let r = run(&mut m, &mut c, "admin", as_acme(create));
    assert!(ok(&r), "{r:?}");
    let stored = m.security.get(Principal::User, "admin", "ann").unwrap();
    let stored = Document::from(RawDoc::from_bytes(stored).unwrap());
    assert_eq!(stored.get("tenantId"), Some(&Value::Int64(1)));
    // Logged in as `ann`, the connection works in `acme` without naming it.
    let mut ann = Connection::new(10);
    ann.auth.required = true;
    log_in(&mut m, &mut ann, "ann");
    let r = run(&mut m, &mut ann, "app", doc(&[("find", "c".into())]));
    assert_eq!(found(&r), 1, "{r:?}");
    // A connection of a tenant cannot name another tenant.
    let r = run(&mut m, &mut ann, "app", as_acme(doc(&[("find", "c".into())])));
    assert_eq!(r.get("code"), Some(&Value::Int32(13)), "{r:?}");
}

// 013 T035 (FR-024): the barrier between tenants. A user or role belongs to the tenant of the
// session that wrote it, and only that tenant sees it; a user of a tenant other than the default
// one has no privilege over the node's administration, which every tenant shares.

const GLOBEX: &str = "globex";

fn two_tenants() -> Mem {
    let mut m = node();
    m.tenants = vec![(1, ACME.to_owned()), (2, GLOBEX.to_owned())];
    m
}

fn in_tenant(tenant: &str, cmd: Document) -> Document {
    cmd.with("$tenant", tenant)
}

fn create_user(name: &str, roles: Vec<Value>) -> Document {
    doc(&[
        ("createUser", name.into()),
        ("pwd", PASSWORD.into()),
        ("roles", Value::Array(roles)),
        ("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")])),
    ])
}

fn code(r: &Document) -> Option<&Value> {
    r.get("code")
}

fn listed_users(r: &Document) -> Vec<String> {
    let Value::Array(users) = field(r, "users") else { panic!("{r:?}") };
    users
        .iter()
        .map(|u| match u {
            Value::Document(d) => match d.get("user") {
                Some(Value::String(n)) => n.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

fn listed_roles(r: &Document) -> Vec<String> {
    let Value::Array(roles) = field(r, "roles") else { panic!("{r:?}") };
    roles
        .iter()
        .map(|u| match u {
            Value::Document(d) => match d.get("role") {
                Some(Value::String(n)) => n.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn a_tenant_does_not_see_nor_change_the_users_of_another() {
    let (mut m, mut c) = (two_tenants(), Connection::new(9));
    let root = || vec![Value::from("root")];
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(ACME, create_user("ann", root())))));
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, create_user("bob", root())))));
    assert!(ok(&run(&mut m, &mut c, "admin", create_user("ops", root()))));
    // Each tenant lists only its users, whichever way it asks.
    let all = || doc(&[("usersInfo", Value::Document(doc(&[("forAllDBs", true.into())])))]);
    assert_eq!(listed_users(&run(&mut m, &mut c, "admin", in_tenant(ACME, all()))), ["ann"]);
    assert_eq!(listed_users(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, all()))), ["bob"]);
    assert_eq!(listed_users(&run(&mut m, &mut c, "admin", all())), ["ops"]);
    let one = |u: &str| doc(&[("usersInfo", u.into())]);
    assert!(listed_users(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, one("ann")))).is_empty());
    let system = || doc(&[("find", "system.users".into())]);
    assert_eq!(found(&run(&mut m, &mut c, "admin", in_tenant(ACME, system()))), 1);
    // Another tenant's user is not found to change or drop.
    let drop = doc(&[("dropUser", "ann".into())]);
    assert_eq!(
        code(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, drop.clone()))),
        Some(&11.into())
    );
    let update = doc(&[("updateUser", "ann".into()), ("pwd", "other1".into())]);
    assert_eq!(code(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, update))), Some(&11.into()));
    let grant = doc(&[("grantRolesToUser", "ann".into()), ("roles", Value::Array(root()))]);
    assert_eq!(code(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, grant))), Some(&11.into()));
    let drop_all = doc(&[("dropAllUsersFromDatabase", 1.into())]);
    let r = run(&mut m, &mut c, "admin", in_tenant(GLOBEX, drop_all));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    assert!(m.security.get(Principal::User, "admin", "ann").is_some());
    assert!(m.security.get(Principal::User, "admin", "ops").is_some());
    assert!(m.security.get(Principal::User, "admin", "bob").is_none());
    // Its own tenant still drops it.
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(ACME, drop))));
}

#[test]
fn a_user_written_to_system_users_belongs_to_the_tenant_that_wrote_it() {
    let (mut m, mut c) = (two_tenants(), Connection::new(9));
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(ACME, create_user("ann", vec![])))));
    let stored = m.security.get(Principal::User, "admin", "ann").unwrap().to_vec();
    let mut forged = Document::from(RawDoc::from_bytes(&stored).unwrap());
    forged.insert("_id", "admin.eve");
    forged.insert("user", "eve");
    // `globex` writes a user that claims to be `acme`'s: it is `globex`'s.
    forged.insert("tenantId", 1i64);
    let insert = doc(&[
        ("insert", "system.users".into()),
        ("documents", Value::Array(vec![Value::Document(forged)])),
    ]);
    let r = run(&mut m, &mut c, "admin", in_tenant(GLOBEX, insert));
    assert!(ok(&r), "{r:?}");
    let eve = m.security.get(Principal::User, "admin", "eve").unwrap();
    let eve = Document::from(RawDoc::from_bytes(eve).unwrap());
    assert_eq!(eve.get("tenantId"), Some(&Value::Int64(2)));
}

#[test]
fn a_tenant_does_not_see_nor_use_the_roles_of_another() {
    let (mut m, mut c) = (two_tenants(), Connection::new(9));
    let privilege = Value::Document(doc(&[
        ("resource", Value::Document(doc(&[("db", "app".into()), ("collection", "".into())]))),
        ("actions", Value::Array(vec![Value::from("find")])),
    ]));
    let create_role = doc(&[
        ("createRole", "reader".into()),
        ("privileges", Value::Array(vec![privilege])),
        ("roles", Value::Array(vec![])),
    ]);
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(ACME, create_role))));
    let stored = m.security.get(Principal::Role, "admin", "reader").unwrap();
    let stored = Document::from(RawDoc::from_bytes(stored).unwrap());
    assert_eq!(stored.get("tenantId"), Some(&Value::Int64(1)));
    let all = || doc(&[("rolesInfo", 1.into())]);
    assert_eq!(listed_roles(&run(&mut m, &mut c, "admin", in_tenant(ACME, all()))), ["reader"]);
    assert!(listed_roles(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, all()))).is_empty());
    assert!(listed_roles(&run(&mut m, &mut c, "admin", all())).is_empty());
    let system = || doc(&[("find", "system.roles".into())]);
    assert_eq!(found(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, system()))), 0);
    // `globex` can neither grant it nor drop it.
    let reader = Value::Document(doc(&[("role", "reader".into()), ("db", "admin".into())]));
    let user = create_user("bob", vec![reader.clone()]);
    assert_eq!(code(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, user))), Some(&31.into()));
    let drop = doc(&[("dropRole", "reader".into())]);
    assert_eq!(
        code(&run(&mut m, &mut c, "admin", in_tenant(GLOBEX, drop.clone()))),
        Some(&31.into())
    );
    let drop_all = doc(&[("dropAllRolesFromDatabase", 1.into())]);
    let r = run(&mut m, &mut c, "admin", in_tenant(GLOBEX, drop_all));
    assert_eq!(r.get("n"), Some(&Value::Int32(0)), "{r:?}");
    // `acme` grants it to its user and drops it.
    let user = create_user("ann", vec![reader]);
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(ACME, user))));
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(ACME, drop))));
}

#[test]
fn a_tenant_user_has_no_privilege_over_the_node() {
    let (mut m, mut c) = (two_tenants(), Connection::new(9));
    let root = vec![Value::from("root")];
    assert!(ok(&run(&mut m, &mut c, "admin", in_tenant(ACME, create_user("ann", root.clone())))));
    assert!(ok(&run(&mut m, &mut c, "admin", create_user("ops", root))));
    let mut ann = Connection::new(10);
    ann.auth.required = true;
    log_in(&mut m, &mut ann, "ann");
    // Its databases are its own to work with, `root` of its tenant.
    assert!(ok(&run(&mut m, &mut ann, "app", insert(1))));
    let list = doc(&[("listDatabases", 1.into()), ("nameOnly", true.into())]);
    assert!(ok(&run(&mut m, &mut ann, "admin", list)));
    // What every tenant shares is refused.
    for cmd in [
        doc(&[("getLog", "global".into())]),
        doc(&[("currentOp", 1.into())]),
        doc(&[("killOp", 1.into()), ("op", 1.into())]),
        doc(&[("setParameter", 1.into()), ("logLevel", 1.into())]),
        doc(&[("shutdown", 1.into())]),
        doc(&[("fsync", 1.into())]),
        doc(&[("serverStatus", 1.into())]),
    ] {
        let r = run(&mut m, &mut ann, "admin", cmd.clone());
        assert_eq!(code(&r), Some(&13.into()), "{cmd:?}: {r:?}");
    }
    // A user of the default tenant keeps them.
    let mut ops = Connection::new(11);
    ops.auth.required = true;
    log_in(&mut m, &mut ops, "ops");
    assert!(ok(&run(&mut m, &mut ops, "admin", doc(&[("getLog", "global".into())]))));
}

#[test]
fn a_cursor_of_a_tenant_is_not_found_from_another() {
    let (mut m, mut c) = (two_tenants(), Connection::new(9));
    for id in 1..=3 {
        assert!(ok(&run(&mut m, &mut c, "app", in_tenant(ACME, insert(id)))));
        assert!(ok(&run(&mut m, &mut c, "app", in_tenant(GLOBEX, insert(id)))));
    }
    let find = doc(&[("find", "c".into()), ("batchSize", 1.into())]);
    let r = run(&mut m, &mut c, "app", in_tenant(ACME, find));
    let Value::Int64(id) = *field(&r, "cursor.id") else { panic!("{r:?}") };
    let more = doc(&[("getMore", id.into()), ("collection", "c".into()), ("batchSize", 1.into())]);
    let r = run(&mut m, &mut c, "app", in_tenant(GLOBEX, more.clone()));
    assert_eq!(code(&r), Some(&43.into()), "{r:?}");
    let kill = doc(&[("killCursors", "c".into()), ("cursors", Value::Array(vec![id.into()]))]);
    let r = run(&mut m, &mut c, "app", in_tenant(GLOBEX, kill.clone()));
    assert_eq!(field(&r, "cursorsNotFound"), &Value::Array(vec![id.into()]), "{r:?}");
    let r = run(&mut m, &mut c, "app", in_tenant(ACME, more));
    assert!(ok(&r), "{r:?}");
    let r = run(&mut m, &mut c, "app", in_tenant(ACME, kill));
    assert_eq!(field(&r, "cursorsKilled"), &Value::Array(vec![id.into()]), "{r:?}");
}

/// Starts a SCRAM-SHA-256 login of `user` on `c`: the answer to `saslStart`, or the error.
fn start_login(m: &mut Mem, c: &mut Connection, user: &str) -> Document {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(user, PASSWORD, false).unwrap();
    let (mut client, first) = ClientSession::start(mech, user, &prepared, "clientnonce0123");
    let bin = |b: &[u8]| Value::Binary { subtype: 0, bytes: b.to_vec() };
    let start = doc(&[
        ("saslStart", 1.into()),
        ("mechanism", mech.name().into()),
        ("payload", bin(&first)),
        ("options", Value::Document(doc(&[("skipEmptyExchange", true.into())]))),
    ]);
    let r = run(m, c, "admin", start);
    if !ok(&r) {
        return r;
    }
    let Value::Binary { bytes, .. } = field(&r, "payload") else { panic!("{r:?}") };
    let answer = client.respond(bytes).unwrap();
    let next =
        doc(&[("saslContinue", 1.into()), ("conversationId", 1.into()), ("payload", bin(&answer))]);
    run(m, c, "admin", next)
}

// 007 T057 (FR-034): a user of a suspended tenant does not sign in, with 18 and the words of the
// contract; the default tenant still works in it with `$tenant`, and after `resume` the user
// signs in again.
#[test]
fn a_suspended_tenant_signs_nobody_in_and_the_default_one_still_works_in_it() {
    let (mut m, mut c) = (node(), Connection::new(9));
    assert!(ok(&run(&mut m, &mut c, "app", as_acme(insert(1)))));
    let create = doc(&[
        ("createUser", "ann".into()),
        ("pwd", PASSWORD.into()),
        ("roles", Value::Array(vec![Value::from("root")])),
        ("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")])),
    ]);
    assert!(ok(&run(&mut m, &mut c, "admin", as_acme(create))));
    m.suspended = vec![1];
    let mut ann = Connection::new(10);
    ann.auth.required = true;
    let r = start_login(&mut m, &mut ann, "ann");
    assert_eq!(r.get("code"), Some(&Value::Int32(18)), "{r:?}");
    assert_eq!(r.get("errmsg"), Some(&Value::String("tenant is suspended".into())), "{r:?}");
    assert!(ann.auth.user.is_none());
    // The default tenant copies or empties it with `$tenant`.
    let r = run(&mut m, &mut c, "app", as_acme(doc(&[("find", "c".into())])));
    assert_eq!(found(&r), 1, "{r:?}");
    m.suspended.clear();
    let r = start_login(&mut m, &mut ann, "ann");
    assert!(ok(&r) && r.get("done") == Some(&Value::Boolean(true)), "{r:?}");
}
