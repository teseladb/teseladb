//! 013 T024: users and roles in Group 0 (FR-009, FR-013): batches that apply all or nothing as
//! every replica applies them, their errors kept for idempotent retries, and the state in the
//! snapshot.

use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{
    Group0Error, Group0Result, Group0Sm, SNAPSHOT_VERSION, SNAPSHOT_VERSION_WITH_NAMED_SECURITY,
    SNAPSHOT_VERSION_WITHOUT_META1,
};
use teseladb_meta::request::RequestId;
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_multiraft::StateMachine;
use teseladb_bson::Document;
use teseladb_types::NodeId;

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: NodeAddrs { name: b"n1".to_vec(), cores: 2, ..NodeAddrs::default() },
        cert_fingerprint: [0; 32],
    };
    sm.apply_command(1, &boot);
    sm
}

fn apply(sm: &mut Group0Sm, index: u64, cmd: Group0Command) -> Group0Result {
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd.clone()));
    sm.apply_command(index, &cmd)
}

/// The document of `name` in `admin`, with `tag` to tell versions apart.
fn doc(kind: Principal, name: &str, tag: &str) -> Vec<u8> {
    Document::new()
        .with("_id", format!("admin.{name}"))
        .with(kind.name_field(), name)
        .with("db", "admin")
        .with("tag", tag)
        .to_bytes()
        .unwrap()
}

fn put(kind: Principal, name: &str, expect: Expect, tag: &str) -> SecurityOp {
    SecurityOp::put(kind, expect, doc(kind, name, tag)).unwrap()
}

#[test]
fn users_and_roles_change_in_batches_and_travel_in_the_snapshot() {
    let mut sm = cluster();
    let u = Principal::User;
    let r = Principal::Role;
    let create = Group0Command::Security(vec![
        put(u, "root", Expect::Absent, "user"),
        put(r, "ops", Expect::Absent, "role"),
    ]);
    assert_eq!(apply(&mut sm, 2, create.clone()), Group0Result::Ok);
    assert_eq!(apply(&mut sm, 3, create), Group0Result::Err(Group0Error::PrincipalExists(u)));
    let mut nobody = put(r, "nobody", Expect::Present, "");
    nobody.doc = None;
    let missing = Group0Command::Security(vec![nobody]);
    let not_found = Group0Result::Err(Group0Error::PrincipalNotFound(r));
    assert_eq!(apply(&mut sm, 4, missing), not_found);
    let stale =
        Group0Command::Security(vec![put(u, "root", Expect::Exactly(b"old".to_vec()), "x")]);
    assert_eq!(apply(&mut sm, 5, stale), Group0Result::Err(Group0Error::SecurityConflict));
    assert_eq!(sm.security.version, 1);
    assert_eq!(sm.security.get(u, "admin", "root"), Some(doc(u, "root", "user").as_slice()));

    // An idempotent retry gets the stored error back without applying anything.
    let req = RequestId { origin: NodeId(1), id: 1 };
    let again = Group0Command::Idempotent {
        req,
        cmd: Box::new(Group0Command::Security(vec![put(r, "ops", Expect::Absent, "r2")])),
    };
    let exists = Group0Result::Err(Group0Error::PrincipalExists(r));
    assert_eq!(apply(&mut sm, 6, again.clone()), exists);
    assert_eq!(apply(&mut sm, 7, again), exists);

    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).unwrap();
    assert_eq!(copy, sm);
    assert_eq!(copy.requests.get(req), Some(&exists));
    for e in [
        Group0Error::PrincipalExists(u),
        Group0Error::PrincipalNotFound(u),
        Group0Error::SecurityConflict,
    ] {
        let req = RequestId { origin: NodeId(2), id: 9 };
        sm.requests.record(req, Group0Result::Err(e));
        let mut back = Group0Sm::default();
        back.restore(&sm.snapshot()).unwrap();
        assert_eq!(back.requests.get(req), Some(&Group0Result::Err(e)));
    }
}

/// 013 T043: the snapshot keeps the documents in insertion order; one of the previous version,
/// which kept them by name, still restores, in name order.
#[test]
fn the_previous_snapshot_version_restores_its_users_and_roles_in_name_order() {
    let mut sm = cluster();
    let u = Principal::User;
    let names = ["zz", "aa", "mm"];
    let ops = names.iter().map(|n| put(u, n, Expect::Absent, "")).collect();
    assert_eq!(apply(&mut sm, 2, Group0Command::Security(ops)), Group0Result::Ok);
    let order = |sm: &Group0Sm| -> Vec<Vec<u8>> {
        sm.security.list(u, None).into_iter().map(<[u8]>::to_vec).collect()
    };
    let inserted: Vec<Vec<u8>> = names.iter().map(|n| doc(u, n, "")).collect();
    assert_eq!(order(&sm), inserted);
    let mut copy = Group0Sm::default();
    copy.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION)).unwrap();
    assert_eq!(order(&copy), inserted);
    assert_eq!(SNAPSHOT_VERSION_WITHOUT_META1, SNAPSHOT_VERSION_WITH_NAMED_SECURITY + 1);
    let mut old = Group0Sm::default();
    old.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITH_NAMED_SECURITY)).unwrap();
    let by_name: Vec<Vec<u8>> = ["aa", "mm", "zz"].iter().map(|n| doc(u, n, "")).collect();
    assert_eq!(order(&old), by_name);
    assert_eq!(old.security.version, sm.security.version);
}

/// 019 T020: API keys live next to the users, found by the SHA-256 of the key (their `_id`),
/// unique by `keyId` and `db`, kept in the snapshot; a snapshot from before them has none.
#[test]
fn api_keys_are_found_by_their_hash_and_survive_the_snapshot() {
    use teseladb_meta::group0::SNAPSHOT_VERSION_WITHOUT_API_KEYS;
    use teseladb_meta::security::string_id;
    let mut sm = cluster();
    let k = Principal::ApiKey;
    assert_eq!(k.unique_index(), "keyId_1_db_1");
    let key = |hash: &str, id: &str| {
        Document::new()
            .with("_id", hash)
            .with("keyId", id)
            .with("db", "admin")
            .with("user", "ann")
            .to_bytes()
            .unwrap()
    };
    let put = |doc: Vec<u8>| SecurityOp::put(k, Expect::Absent, doc).unwrap();
    let ops = vec![put(key("h1", "k1")), put(key("h2", "k2"))];
    assert_eq!(apply(&mut sm, 2, Group0Command::Security(ops)), Group0Result::Ok);
    assert_eq!(sm.security.get_by_id(k, &string_id("h2")), Some(key("h2", "k2").as_slice()));
    assert_eq!(sm.security.get_by_id(k, &string_id("h3")), None);
    assert_eq!(sm.security.get(k, "admin", "k1"), Some(key("h1", "k1").as_slice()));
    // Another key with the same `keyId` in the same database is refused.
    let twin = vec![put(key("h3", "k1"))];
    assert_eq!(
        apply(&mut sm, 3, Group0Command::Security(twin)),
        Group0Result::Err(Group0Error::PrincipalExists(k))
    );
    let gone = vec![SecurityOp::remove(k, key("h1", "k1")).unwrap()];
    assert_eq!(apply(&mut sm, 4, Group0Command::Security(gone.clone())), Group0Result::Ok);
    assert_eq!(
        apply(&mut sm, 5, Group0Command::Security(gone)),
        Group0Result::Err(Group0Error::PrincipalNotFound(k))
    );
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).unwrap();
    assert_eq!(copy.security.list(k, None), sm.security.list(k, None));
    assert_eq!(copy.security.get_by_id(k, &string_id("h2")), Some(key("h2", "k2").as_slice()));
    let mut old = Group0Sm::default();
    old.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_API_KEYS)).unwrap();
    assert!(old.security.keys.is_empty());
    assert_eq!(old.security.users, sm.security.users);
}
