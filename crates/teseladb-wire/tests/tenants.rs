//! 007 T038 (FR-025, DV-023): each request runs in a tenant, the one of the user its connection
//! authenticated as, or the one `$tenant` names on a connection of the default tenant; a tenant
//! sees only its databases, the keys of its collections carry it, and a user belongs to the
//! tenant of the session that created it.

mod common;

use common::{Mem, doc, field};
use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::space::KeyParts;
use teseladb_meta::security::Principal;
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const ACME: &str = "acme";
const PASSWORD: &str = "secret1";

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
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
