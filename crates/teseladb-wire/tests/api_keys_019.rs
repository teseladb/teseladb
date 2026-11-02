//! 019 FR-016: the commands of the API keys, `createApiKey`, `listApiKeys` and `revokeApiKey`, on
//! a node that authorizes: a user acts on its own keys, the keys of another user need the
//! privileges on its database, a key carries only roles its user holds (and, when the caller
//! logged in with a key, only that key's), and a key's use moves `lastUsedAt` at most once a
//! minute. A node that does not authorize lets anyone act on any key.

mod common;

use common::{Mem, doc, field};
use teseladb_auth::apikey::{ApiKey, TOUCH_EVERY_MS};
use teseladb_auth::sasl::SaslMechanism;
use teseladb_auth::scram::{Credentials, Mechanism};
use teseladb_auth::users::{RoleName, User, UserName};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_wire::commands::auth::Authenticated;
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::{HEADER_LEN, Header, opcode};

const ADMIN: &str = "admin";
const TEST: &str = "test";
const UNAUTHORIZED: i32 = 13;
const BAD_VALUE: i32 = 2;
const USER_NOT_FOUND: i32 = 11;
const KEY_NOT_FOUND: i32 = 211;

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

fn add_user(m: &mut Mem, name: &str, roles: &[(&str, &str)]) {
    let mut u = User::new(UserName::new(name, ADMIN));
    u.roles = roles.iter().map(|(r, d)| RoleName::new(*r, *d)).collect();
    let m256 = Mechanism::ScramSha256;
    u.scram_sha256 = Some(Credentials::derive(m256, b"pw", &vec![1; m256.salt_len()], 4096));
    let op = SecurityOp::named(
        Principal::User,
        ADMIN,
        name,
        Expect::Absent,
        Some(u.to_document().to_bytes().unwrap()),
    );
    m.security.apply(&[op]).unwrap();
}

fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(NodeSessions::new(Timeouts::default()));
    add_user(&mut m, "alice", &[("read", TEST), ("readWrite", TEST)]);
    add_user(&mut m, "root", &[("root", ADMIN)]);
    add_user(&mut m, "nobody", &[]);
    m
}

/// A connection authenticated as `user`, with the roles of the key it logged in with if any.
fn conn(user: &str, key_roles: Option<Vec<RoleName>>) -> Connection {
    let mut c = Connection::new(9);
    c.auth.required = true;
    c.auth.user = Some(Authenticated {
        name: UserName::new(user, ADMIN),
        mechanism: SaslMechanism::ScramSha256,
        login_ms: 0,
        roles: key_roles,
        tenant: 0,
    });
    c
}

fn message(body: &Document) -> Vec<u8> {
    let mut sections = vec![0u8];
    sections.extend_from_slice(&body.to_bytes().unwrap());
    let mut out = Vec::new();
    let len = HEADER_LEN + 4 + sections.len();
    Header { length: len as i32, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }
        .write(&mut out);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&sections);
    out
}

fn run(m: &mut Mem, c: &mut Connection, cmd: Document) -> Document {
    let mut parse = Connection::new(1);
    parse.receive(&message(&cmd.with("$db", ADMIN)));
    let req: Request = parse.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(c, &req, &info(), Some((m, &shared)));
    m.shared = shared;
    match a {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

fn code(d: &Document) -> Option<i32> {
    match d.get("code") {
        Some(Value::Int32(n)) => Some(*n),
        _ => None,
    }
}

fn ok(d: &Document) -> bool {
    d.get("ok") == Some(&Value::Double(1.0))
}

fn roles(list: &[(&str, &str)]) -> Value {
    let docs =
        list.iter().map(|(r, d)| Value::Document(Document::new().with("role", *r).with("db", *d)));
    Value::Array(docs.collect())
}

fn create(user: &str) -> Document {
    Document::new().with("createApiKey", user).with("db", ADMIN)
}

fn key_ids(d: &Document) -> Vec<String> {
    let Some(Value::Array(keys)) = d.get("keys") else { panic!("{d:?}") };
    keys.iter()
        .map(|k| match k {
            Value::Document(k) => match k.get("keyId") {
                Some(Value::String(s)) => s.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

fn text(d: &Document, k: &str) -> String {
    match field(d, k) {
        Value::String(s) => s.clone(),
        other => panic!("{k}: {other:?} in {d:?}"),
    }
}

#[test]
fn a_user_manages_its_own_keys_and_another_users_need_privileges() {
    let mut m = node();
    let mut alice = conn("alice", None);
    let mut nobody = conn("nobody", None);
    let mut root = conn("root", None);

    // Its own key: every role it holds by default, or those it names among them.
    let all = run(&mut m, &mut alice, create("alice"));
    assert!(ok(&all), "{all:?}");
    assert!(text(&all, "key").len() > 20);
    let read_only =
        run(&mut m, &mut alice, create("alice").with("roles", roles(&[("read", TEST)])));
    assert!(ok(&read_only), "{read_only:?}");
    let more = create("alice").with("roles", roles(&[("dbAdmin", TEST)]));
    assert_eq!(code(&run(&mut m, &mut alice, more)), Some(BAD_VALUE));
    let too_long = create("alice").with("expiresAfterDays", 10_000);
    assert_eq!(code(&run(&mut m, &mut alice, too_long)), Some(BAD_VALUE));
    let no_name = Document::new().with("createApiKey", 1);
    assert_eq!(code(&run(&mut m, &mut alice, no_name)), Some(BAD_VALUE));
    // Another user's: refused without the privilege, and an unknown user is not found.
    assert_eq!(code(&run(&mut m, &mut nobody, create("alice"))), Some(UNAUTHORIZED));
    assert_eq!(code(&run(&mut m, &mut root, create("ghost"))), Some(USER_NOT_FOUND));
    assert!(ok(&run(&mut m, &mut root, create("nobody"))));

    // Logged in with a key of `read`, alice gives a new key at most that role.
    let mut with_key = conn("alice", Some(vec![RoleName::new("read", TEST)]));
    let wider = create("alice").with("roles", roles(&[("readWrite", TEST)]));
    assert_eq!(code(&run(&mut m, &mut with_key, wider)), Some(BAD_VALUE));
    assert!(ok(&run(&mut m, &mut with_key, create("alice"))));

    // Listing shows what the caller may see.
    let list = Document::new().with("listApiKeys", 1);
    assert_eq!(key_ids(&run(&mut m, &mut root, list.clone())).len(), 4);
    let alices = list.clone().with("user", "alice").with("db", ADMIN);
    assert_eq!(key_ids(&run(&mut m, &mut root, alices)).len(), 3);
    assert_eq!(key_ids(&run(&mut m, &mut alice, list.clone())).len(), 3);
    assert_eq!(key_ids(&run(&mut m, &mut nobody, list.clone())).len(), 1);

    // Revoking: only with the privilege or one's own; an unknown id is not found.
    let id = text(&all, "keyId");
    let revoke = |id: &str| Document::new().with("revokeApiKey", id);
    assert_eq!(code(&run(&mut m, &mut nobody, revoke(&id))), Some(UNAUTHORIZED));
    assert!(ok(&run(&mut m, &mut alice, revoke(&id))));
    assert_eq!(code(&run(&mut m, &mut alice, revoke(&id))), Some(KEY_NOT_FOUND));
    let bad = Document::new().with("revokeApiKey", 1);
    assert_eq!(code(&run(&mut m, &mut alice, bad)), Some(BAD_VALUE));
    assert_eq!(key_ids(&run(&mut m, &mut root, list)).len(), 3);
}

#[test]
fn using_a_key_moves_its_last_use_at_most_once_a_minute() {
    let mut m = node();
    let mut alice = conn("alice", None);
    let made = run(&mut m, &mut alice, create("alice"));
    assert!(ok(&made), "{made:?}");
    let stored = |m: &Mem| {
        let docs = m.security.list(Principal::ApiKey, None);
        let d = Document::from(RawDoc::from_bytes(docs[0]).unwrap());
        ApiKey::from_document(&d).unwrap()
    };
    let key = stored(&m);
    assert_eq!(key.last_used_ms, None);
    teseladb_wire::commands::api_keys::touch(&mut m, &key).unwrap();
    let used = stored(&m);
    assert!(used.last_used_ms.is_some());
    // Within the minute nothing changes.
    teseladb_wire::commands::api_keys::touch(&mut m, &used).unwrap();
    assert_eq!(stored(&m).last_used_ms, used.last_used_ms);
    m.now += u64::try_from(TOUCH_EVERY_MS).unwrap();
    teseladb_wire::commands::api_keys::touch(&mut m, &used).unwrap();
    assert!(stored(&m).last_used_ms > used.last_used_ms);
}

/// A node that does not authorize lets any client act on any user's keys.
#[test]
fn without_authorization_any_client_manages_any_key() {
    let mut m = node();
    let r = m.run(ADMIN, doc(&[("createApiKey", Value::from("nobody"))]));
    assert!(ok(&r), "{r:?}");
    let r = m.run(ADMIN, doc(&[("revokeApiKey", Value::from(text(&r, "keyId")))]));
    assert!(ok(&r), "{r:?}");
}

/// Another writer that changes the keys between the read and the write: a creation answers the
/// conflict, a revocation tries again and gives up after its attempts.
#[test]
fn concurrent_changes_of_the_keys_are_conflicts_or_retries() {
    let mut m = node();
    let mut alice = conn("alice", None);
    m.fail_security_writes = 1;
    let r = run(&mut m, &mut alice, create("alice"));
    assert!(!ok(&r) && code(&r).is_some(), "{r:?}");
    let made = run(&mut m, &mut alice, create("alice"));
    let revoke = Document::new().with("revokeApiKey", text(&made, "keyId"));
    m.fail_security_writes = usize::MAX;
    let r = run(&mut m, &mut alice, revoke.clone());
    assert!(!ok(&r) && code(&r).is_some(), "{r:?}");
    m.fail_security_writes = 1;
    assert!(ok(&run(&mut m, &mut alice, revoke)));
}

/// A caller logged in with a key that creates a key for another user gives it any role that user
/// holds; a stored key that does not read is an internal error, not a silent omission.
#[test]
fn a_key_for_another_user_and_a_stored_key_that_does_not_read() {
    let mut m = node();
    let mut root = conn("root", Some(vec![RoleName::new("root", ADMIN)]));
    let other = create("alice").with("roles", roles(&[("readWrite", TEST)]));
    assert!(ok(&run(&mut m, &mut root, other)));
    let bad = Document::new().with("_id", "admin.bad").with("keyId", "k").with("user", 1);
    let op = SecurityOp::put(Principal::ApiKey, Expect::Absent, bad.to_bytes().unwrap()).unwrap();
    m.security.apply(&[op]).unwrap();
    let r = run(&mut m, &mut root, Document::new().with("listApiKeys", 1));
    assert!(!ok(&r) && format!("{r:?}").contains("bad stored API key"), "{r:?}");
}
