//! `$clusterTime` signed in every reply and checked when a client sends it (013 T025, FR-014),
//! as a `refrouter` 9.0.4 does with and without `--keyFile` (measured 30-10-2026,
//! `scripts/authz-probe/ct_probe.py`): the reading errors, the order against the other checks
//! (unknown command, authentication, generic arguments, then `$clusterTime`, then authorization),
//! and the signatures of a node with keys.

mod common;

use std::sync::Arc;

use common::{Mem, doc, field};
use teseladb_auth::clustertime::{HmacKey, KEY_LEN, KeyRing, Time, sign};
use teseladb_auth::sasl::SaslMechanism;
use teseladb_auth::users::{RoleName, User, UserName};
use teseladb_bson::{Decimal128, Document, Value};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_wire::cluster_time::{GivenTime, parse};
use teseladb_wire::commands::auth::Authenticated;
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch, dispatch_with};
use teseladb_wire::error::{
    COMMAND_NOT_FOUND, IDL_FAILED_TO_PARSE, KEY_NOT_FOUND, NON_CONFORMANT_BSON,
    TIME_PROOF_MISMATCH, TYPE_MISMATCH, UNAUTHORIZED, UNSUPPORTED_FORMAT,
};
use teseladb_wire::hello::{restamp_cluster_time, with_cluster_time};
use teseladb_wire::msg::{HEADER_LEN, Header, opcode};

const ADMIN: &str = "admin";
const NOW: ClusterTime = ClusterTime { time: 1_793_376_615, increment: 1 };
const KEY_ID: i64 = 7_702_466_350_031_044_650;

fn ring() -> KeyRing {
    KeyRing { keys: vec![HmacKey { id: KEY_ID, key: [3; KEY_LEN], expires_at: NOW.time + 1000 }] }
}

fn info(keys: Option<KeyRing>) -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: NOW,
        now_ms: 0,
        keys: keys.map(Arc::new),
        replica_set: None,
    }
}

fn request(cmd: &Document, op_code: i32) -> Request {
    let body = cmd.to_bytes().unwrap();
    let mut m = Vec::new();
    if op_code == opcode::OP_MSG {
        let len = (HEADER_LEN + 5 + body.len()) as i32;
        Header { length: len, request_id: 1, response_to: 0, op_code }.write(&mut m);
        m.extend_from_slice(&[0, 0, 0, 0, 0]);
    } else {
        let coll = b"admin.$cmd\0";
        let len = (HEADER_LEN + 4 + coll.len() + 8 + body.len()) as i32;
        Header { length: len, request_id: 1, response_to: 0, op_code }.write(&mut m);
        m.extend_from_slice(&0u32.to_le_bytes());
        m.extend_from_slice(coll);
        m.extend_from_slice(&0i32.to_le_bytes());
        m.extend_from_slice(&(-1i32).to_le_bytes());
    }
    m.extend_from_slice(&body);
    let mut c = Connection::new(1);
    c.receive(&m);
    c.next_request().unwrap().unwrap()
}

fn reply(a: Action) -> Document {
    match a {
        Action::Reply(d) => d,
        other => panic!("not a reply: {other:?}"),
    }
}

/// `ping` with `$clusterTime` `ct` on a node with `keys`, without users.
fn ping(ct: Value, keys: Option<KeyRing>) -> Document {
    let cmd = Document::new().with("ping", 1).with("$db", ADMIN).with("$clusterTime", ct);
    reply(dispatch(&mut Connection::new(2), &request(&cmd, opcode::OP_MSG), &info(keys)))
}

fn code(d: &Document) -> (i32, String) {
    match (d.get("code"), d.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => (0, String::new()),
    }
}

fn ts(secs: u32, inc: u32) -> Value {
    Value::Timestamp { time: secs, increment: inc }
}

fn hash(bytes: &[u8], subtype: u8) -> Value {
    Value::Binary { subtype, bytes: bytes.to_vec() }
}

fn signed(t: Value, h: Value, id: Value) -> Value {
    let sig = Document::new().with("hash", h).with("keyId", id);
    Value::Document(Document::new().with("clusterTime", t).with("signature", sig))
}

fn signature_of(d: &Document) -> (Vec<u8>, i64) {
    let Some(Value::Document(ct)) = d.get("$clusterTime") else { panic!("no $clusterTime") };
    let Some(Value::Document(sig)) = ct.get("signature") else { panic!("no signature") };
    match (sig.get("hash"), sig.get("keyId")) {
        (Some(Value::Binary { subtype: 0, bytes }), Some(Value::Int64(id))) => (bytes.clone(), *id),
        other => panic!("bad signature {other:?}"),
    }
}

#[test]
fn replies_are_signed_with_the_key_of_the_time() {
    let d = ping(Value::Null, Some(ring()));
    let at = Time { secs: NOW.time, inc: NOW.increment };
    assert_eq!(signature_of(&d), (sign(&[3; KEY_LEN], at).to_vec(), KEY_ID));
    // An error reply too.
    let e = ping(Value::Int32(5), Some(ring()));
    assert_eq!(code(&e).0, TYPE_MISMATCH.code);
    assert_eq!(signature_of(&e).1, KEY_ID);
    // Without keys, or before the node has one for the time: zeros and key 0.
    assert_eq!(signature_of(&ping(Value::Null, None)), (vec![0; KEY_LEN], 0));
    assert_eq!(signature_of(&ping(Value::Null, Some(KeyRing::default()))), (vec![0; KEY_LEN], 0));
}

#[test]
fn a_later_time_needs_a_valid_signature_on_a_node_with_keys() {
    let later = Time { secs: NOW.time + 10, inc: 1 };
    let good = sign(&[3; KEY_LEN], later);
    let t = ts(later.secs, later.inc);
    let ok = ping(signed(t.clone(), hash(&good, 0), Value::Int64(KEY_ID)), Some(ring()));
    assert_eq!(ok.get("ok"), Some(&Value::Double(1.0)));
    let bad = ping(signed(t.clone(), hash(&[1; 20], 0), Value::Int64(KEY_ID)), Some(ring()));
    assert_eq!(
        code(&bad),
        (TIME_PROOF_MISMATCH.code, "Proof does not match the cluster time".into())
    );
    let unknown = ping(signed(t.clone(), hash(&[1; 20], 0), Value::Int32(5)), Some(ring()));
    let msg = format!(
        "No keys found for HMAC that is valid for time: {{ ts: Timestamp({}, 1) }} with id: 5",
        later.secs
    );
    assert_eq!(code(&unknown), (KEY_NOT_FOUND.code, msg));
    // No signature: key 0.
    let bare = Value::Document(Document::new().with("clusterTime", t.clone()));
    let (c, m) = code(&ping(bare.clone(), Some(ring())));
    assert_eq!((c, m.ends_with("with id: 0")), (KEY_NOT_FOUND.code, true));
    // A time not past the node's own passes whatever it carries.
    let past = signed(ts(NOW.time, 0), hash(&[1; 20], 0), Value::Int64(9));
    assert_eq!(ping(past, Some(ring())).get("ok"), Some(&Value::Double(1.0)));
    // Without keys any signature passes.
    let forged = signed(t, hash(&[1; 20], 0), Value::Int64(9));
    assert_eq!(ping(forged, None).get("ok"), Some(&Value::Double(1.0)));
    assert_eq!(ping(bare, None).get("ok"), Some(&Value::Double(1.0)));
}

#[test]
fn key_ids_are_read_as_refdb_reads_them() {
    let later = ts(NOW.time + 10, 1);
    let cases = [
        (Value::Double(5.9), 5),
        (Value::Double(-7.9), -7),
        (Value::Double(f64::NAN), 0),
        (Value::Double(1e30), i64::MAX),
        (Value::Double(f64::NEG_INFINITY), i64::MIN),
        (Value::Decimal128("7.9".parse::<Decimal128>().unwrap()), 8),
        (Value::Decimal128(Decimal128::nan()), 0),
        (Value::Decimal128("1e40".parse::<Decimal128>().unwrap()), i64::MAX),
    ];
    for (v, id) in cases {
        let d = ping(signed(later.clone(), hash(&[1; 20], 0), v), Some(ring()));
        let (c, m) = code(&d);
        assert_eq!(c, KEY_NOT_FOUND.code);
        assert!(m.ends_with(&format!("with id: {id}")), "{m}");
    }
}

#[test]
fn malformed_times_get_refdb_errors() {
    let p = "ping.$clusterTime";
    let t = ts(1, 1);
    let missing = |f: &str| {
        (IDL_FAILED_TO_PARSE.code, format!("BSON field '{p}.{f}' is missing but a required field"))
    };
    let wrong = |f: &str, ty: &str, want: &str| {
        (
            TYPE_MISMATCH.code,
            format!("BSON field '{p}{f}' is the wrong type '{ty}', expected type '{want}'"),
        )
    };
    let sig_only = |s: Document| Value::Document(Document::new().with("signature", s));
    let with_sig = |s: Value| {
        Value::Document(Document::new().with("clusterTime", t.clone()).with("signature", s))
    };
    let id_types = "expected types '[int, long, decimal, double]'";
    let cases: Vec<(Value, (i32, String))> = vec![
        (Value::Int32(5), wrong("", "int", "object")),
        (Value::Document(Document::new()), missing("clusterTime")),
        (Value::Document(Document::new().with("clusterTime", Value::Null)), missing("clusterTime")),
        (
            Value::Document(Document::new().with("clusterTime", 5)),
            wrong(".clusterTime", "int", "timestamp"),
        ),
        (with_sig(Value::Int32(5)), wrong(".signature", "int", "object")),
        (with_sig(Value::Document(Document::new())), missing("signature.hash")),
        (with_sig(Value::Document(Document::new().with("keyId", 1))), missing("signature.hash")),
        (
            with_sig(Value::Document(Document::new().with("hash", hash(&[1; 20], 0)))),
            missing("signature.keyId"),
        ),
        (
            with_sig(Value::Document(
                Document::new().with("hash", hash(&[1; 20], 0)).with("keyId", Value::Null),
            )),
            missing("signature.keyId"),
        ),
        (
            with_sig(Value::Document(Document::new().with("hash", Value::Null).with("keyId", 1))),
            missing("signature.hash"),
        ),
        (
            with_sig(Value::Document(Document::new().with("hash", "x").with("keyId", 1))),
            wrong(".signature.hash", "string", "binData"),
        ),
        (
            with_sig(Value::Document(
                Document::new().with("hash", hash(&[1; 20], 0)).with("keyId", "x"),
            )),
            (
                TYPE_MISMATCH.code,
                format!("BSON field '{p}.signature.keyId' is the wrong type 'string', {id_types}"),
            ),
        ),
        // Read in the order the fields come: the signature before a later bad time.
        (
            Value::Document(
                Document::new()
                    .with("signature", Document::new().with("keyId", 1))
                    .with("clusterTime", 5),
            ),
            missing("signature.hash"),
        ),
        (
            Value::Document(
                Document::new()
                    .with("clusterTime", 5)
                    .with("signature", Document::new().with("keyId", 1)),
            ),
            wrong(".clusterTime", "int", "timestamp"),
        ),
        (
            sig_only(Document::new().with("hash", hash(&[1; 20], 0)).with("keyId", 1)),
            missing("clusterTime"),
        ),
        // A short hash, even for a past time and on a node without keys.
        (
            with_sig(Value::Document(
                Document::new().with("hash", hash(&[1; 5], 0)).with("keyId", 1),
            )),
            (UNSUPPORTED_FORMAT.code, "Unsupported SHA1Block hash length: 5".into()),
        ),
    ];
    for (ct, want) in cases {
        for keys in [None, Some(ring())] {
            assert_eq!(code(&ping(ct.clone(), keys)), want, "{ct:?}");
        }
    }
    let names = [
        (1, "function"),
        (2, "byte(deprecated)"),
        (3, "UUID(deprecated)"),
        (4, "UUID"),
        (5, "MD5"),
        (6, "encrypt"),
        (8, "sensitive"),
        (9, "vector"),
        (10, "invalid"),
        (0x80, "Custom"),
        (0x90, "invalid"),
    ];
    for (subtype, name) in names {
        let d = ping(
            with_sig(Value::Document(
                Document::new().with("hash", hash(&[1; 20], subtype)).with("keyId", 1),
            )),
            None,
        );
        let msg = format!(
            "BSON field '{p}.signature.hash' is the wrong binData type '{name}', expected type 'general'"
        );
        assert_eq!(code(&d), (TYPE_MISMATCH.code, msg));
    }
    // A column binary that is not a valid column fails the BSON check first (measured).
    let column =
        with_sig(Value::Document(Document::new().with("hash", hash(&[1; 20], 7)).with("keyId", 1)));
    let msg = "Invalid BSON column in element with field name '$clusterTime.signature.hash' in object with unknown _id";
    assert_eq!(code(&ping(column, None)), (NON_CONFORMANT_BSON.code, msg.to_owned()));
    // An empty column is a valid one: then its name.
    let empty =
        with_sig(Value::Document(Document::new().with("hash", hash(&[0], 7)).with("keyId", 1)));
    let msg = format!(
        "BSON field '{p}.signature.hash' is the wrong binData type 'column', expected type 'general'"
    );
    assert_eq!(code(&ping(empty, None)), (TYPE_MISMATCH.code, msg));
    // `null`, unknown fields and a repeated field (the last one counts).
    assert_eq!(ping(Value::Null, Some(ring())).get("ok"), Some(&Value::Double(1.0)));
    let extra = Document::new()
        .with("clusterTime", t.clone())
        .with(
            "signature",
            Document::new().with("hash", hash(&[1; 20], 0)).with("keyId", 1).with("x", 1),
        )
        .with("x", 1);
    let given = parse(&Document::new().with("$clusterTime", extra), "ping").unwrap();
    assert_eq!(
        given,
        Some(GivenTime { time: Time { secs: 1, inc: 1 }, signature: Some(([1; 20], 1)) })
    );
    let null_sig = Document::new().with("clusterTime", t).with("signature", Value::Null);
    let given = parse(&Document::new().with("$clusterTime", null_sig), "ping").unwrap();
    assert_eq!(given.unwrap().signature, None);
    let twice = Document::new().with("clusterTime", ts(1, 1)).with("clusterTime", ts(2, 2));
    let given = parse(&Document::new().with("$clusterTime", twice), "ping").unwrap();
    assert_eq!(given.unwrap().time, Time { secs: 2, inc: 2 });
}

#[test]
fn the_check_comes_after_the_generic_arguments_and_before_authorization() {
    let forged = signed(ts(NOW.time + 10, 1), hash(&[1; 20], 0), Value::Int64(KEY_ID));
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(NodeSessions::new(Timeouts::default()));
    let user = User::new(UserName::new("nobody", ADMIN));
    let mut u = user.clone();
    u.roles = Vec::<RoleName>::new();
    let op = SecurityOp::named(
        Principal::User,
        ADMIN,
        "nobody",
        Expect::Absent,
        Some(u.to_document().to_bytes().unwrap()),
    );
    m.security.apply(&[op]).unwrap();
    let mut run = |c: &mut Connection, cmd: Document| {
        let shared = std::mem::take(&mut m.shared);
        let a = dispatch_with(
            c,
            &request(&cmd, opcode::OP_MSG),
            &info(Some(ring())),
            Some((&mut m, &shared)),
        );
        m.shared = shared;
        reply(a)
    };
    let find = || {
        Document::new().with("find", "c").with("$db", "test").with("$clusterTime", forged.clone())
    };
    // Not authenticated: `find` needs authentication first; `ping` and `hello` are checked.
    let mut anon = Connection::new(3);
    anon.auth.required = true;
    assert_eq!(code(&run(&mut anon, find())).0, UNAUTHORIZED.code);
    let ping =
        Document::new().with("ping", 1).with("$db", ADMIN).with("$clusterTime", forged.clone());
    assert_eq!(code(&run(&mut anon, ping)).0, TIME_PROOF_MISMATCH.code);
    let hello =
        Document::new().with("hello", 1).with("$db", ADMIN).with("$clusterTime", forged.clone());
    assert_eq!(code(&run(&mut anon, hello)).0, TIME_PROOF_MISMATCH.code);
    let unknown =
        Document::new().with("nosuch", 1).with("$db", ADMIN).with("$clusterTime", forged.clone());
    assert_eq!(code(&run(&mut anon, unknown)).0, COMMAND_NOT_FOUND.code);
    // Authenticated without privileges: the time before `Unauthorized`.
    let mut nobody = Connection::new(4);
    nobody.auth.required = true;
    nobody.auth.user = Some(Authenticated {
        name: UserName::new("nobody", ADMIN),
        mechanism: SaslMechanism::ScramSha256,
        login_ms: 0,
        roles: None,
        tenant: 0,
    });
    assert_eq!(code(&run(&mut nobody, find())).0, TIME_PROOF_MISMATCH.code);
    // A bad generic argument first.
    let bad_max = find().with("maxTimeMS", "x");
    assert_eq!(code(&run(&mut nobody, bad_max)).0, TYPE_MISMATCH.code);
    // The commands take `$clusterTime` as a generic argument: checking it is dispatch's job.
    let r = m.run("test", doc(&[("find", "c".into()), ("$clusterTime", forged.clone())]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    // The `OP_QUERY` handshake is checked too.
    let legacy = Document::new().with("isMaster", 1).with("$clusterTime", forged);
    let d = reply(dispatch(
        &mut Connection::new(5),
        &request(&legacy, opcode::OP_QUERY),
        &info(Some(ring())),
    ));
    assert_eq!(code(&d).0, TIME_PROOF_MISMATCH.code);
}

/// A reply stamped before its command moves to the later time as if it had been stamped then,
/// signed for that time; a reply without the two fields at its end is left as it is.
#[test]
fn a_reply_moves_to_a_later_cluster_time_as_if_stamped_then() {
    let later = ClusterTime { time: NOW.time + 3, increment: 9 };
    for keys in [None, Some(ring())] {
        let before = with_cluster_time(Document::new().with("ok", 1.0), &info(keys.clone()));
        let mut moved = before.clone();
        assert!(restamp_cluster_time(&mut moved, later, keys.as_ref()));
        let then = NodeInfo { cluster_time: later, ..info(keys.clone()) };
        assert_eq!(moved, with_cluster_time(Document::new().with("ok", 1.0), &then));
        // Something after them, or a reply without them, stays.
        for mut other in [before.clone().with("x", 1), Document::new().with("ok", 1.0)] {
            let kept = other.clone();
            assert!(!restamp_cluster_time(&mut other, later, keys.as_ref()));
            assert_eq!(other, kept);
        }
    }
}

/// A reply that ends with the two fields in other shapes keeps what it does not have: only the
/// times and the signature's parts that are there change.
#[test]
fn a_restamp_changes_only_the_parts_that_are_there() {
    let later = ClusterTime { time: NOW.time + 3, increment: 9 };
    let ts = Value::Timestamp { time: later.time, increment: later.increment };
    let mut plain = Document::new().with("$clusterTime", 1).with("operationTime", 2);
    assert!(restamp_cluster_time(&mut plain, later, None));
    assert_eq!(plain, Document::new().with("$clusterTime", 1).with("operationTime", ts.clone()));
    let bare = Document::new().with("signature", Document::new().with("hash", 1));
    let mut partial = Document::new().with("$clusterTime", bare.clone()).with("operationTime", 2);
    assert!(restamp_cluster_time(&mut partial, later, Some(&ring())));
    assert_eq!(partial, Document::new().with("$clusterTime", bare).with("operationTime", ts));
}

/// The `operationTime` of a write is its commit's mark, as a `refrouter` 9.0.4 answers the optime of
/// a write: a change stream that starts at it includes the write. `$clusterTime` goes on with
/// the node's clock, which may be ahead (found by the battery's `cs-start-at-operation-time` on
/// 04-11-2026: the node answered a later mark and the stream missed the write).
#[test]
fn the_operation_time_of_a_write_is_its_commit() {
    let mut m = Mem::new();
    m.mark_ahead = 5;
    let mut c = Connection::new(1);
    let run = |m: &mut Mem, c: &mut Connection, cmd: Document| {
        let shared = std::mem::take(&mut m.shared);
        let a = dispatch_with(c, &request(&cmd, opcode::OP_MSG), &info(None), Some((m, &shared)));
        m.shared = shared;
        reply(a)
    };
    let insert = doc(&[
        ("insert", Value::from("c")),
        ("documents", Value::Array(vec![Value::Document(doc(&[("_id", 1.into())]))])),
        ("$db", Value::from("db")),
    ]);
    let r = run(&mut m, &mut c, insert);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let commit = m.committed.expect("a commit");
    let (time, increment) = teseladb_hlc::to_timestamp(commit);
    assert_eq!(field(&r, "operationTime"), &Value::Timestamp { time, increment });
    let node = Value::Timestamp { time: NOW.time, increment: NOW.increment };
    assert_eq!(field(&r, "$clusterTime.clusterTime"), &node, "the node's, ahead");
    // A read answers the node's time, as before.
    let find = doc(&[("find", Value::from("c")), ("$db", Value::from("db"))]);
    let r = run(&mut m, &mut c, find);
    assert_eq!(field(&r, "operationTime"), &node);
}

/// Runs `ping` with `$clusterTime` `t` on a node without keys whose wall clock is `m.now`.
fn ping_on(m: &mut Mem, t: Value) -> Document {
    let sig = Document::new().with("hash", hash(&[0; 20], 0)).with("keyId", Value::Int64(0));
    let ct = Document::new().with("clusterTime", t).with("signature", sig);
    let cmd = Document::new().with("ping", 1).with("$db", ADMIN).with("$clusterTime", ct);
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(
        &mut Connection::new(4),
        &request(&cmd, opcode::OP_MSG),
        &info(None),
        Some((m, &shared)),
    );
    m.shared = shared;
    reply(a)
}

/// A client's `$clusterTime` ahead of the node's clock moves the node's HLC up to it before the
/// command runs, as a `refrouter` 9.0.4 does (008 FR-013, measured 04-11-2026): a write that follows
/// lands above what the client saw, even on another node.
#[test]
fn a_clients_cluster_time_moves_the_nodes_clock() {
    let mut m = Mem::new();
    let secs = (m.now / 1000) as u32;
    let r = ping_on(&mut m, ts(secs, 5));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(m.observed, vec![teseladb_hlc::from_timestamp((secs, 5))]);
    // One behind the node's clock passes and leaves it alone.
    let r = ping_on(&mut m, ts(secs - 3600, 1));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

/// One more than 1 s ahead of the node's wall clock fails with 205 and the message of a `refrouter`
/// 9.0.4, which allows 365 days (DV-101): the HLC's bound on disk would keep the node from
/// starting again until its wall clock passed it.
#[test]
fn a_cluster_time_too_far_ahead_fails_with_205() {
    let mut m = Mem::new();
    let secs = (m.now / 1000) as u32;
    let r = ping_on(&mut m, ts(secs + 2, 1));
    let msg = format!(
        "New $clusterTime, {}, is too far from this node's wall clock time, {secs}.",
        secs + 2
    );
    assert_eq!(code(&r), (205, msg), "{r:?}");
    assert_eq!(field(&r, "codeName"), &Value::from("ClusterTimeFailsRateLimiter"));
    assert!(m.observed.is_empty(), "the clock is left alone");
    // Within the second it is taken.
    let r = ping_on(&mut m, ts(secs + 1, 0));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}
