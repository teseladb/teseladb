//! The test state machine: `put`, `get`, `cas` and `delete` over byte keys, deterministic outputs,
//! snapshots that restore the same state and inputs it refuses.

use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_testkv::{Command, KvOutput, TestKv};
use teseladb_types::GroupId;

fn ctx(index: u64) -> ApplyCtx {
    ApplyCtx {
        group: GroupId(1),
        index,
        term: 1,
        ts: teseladb_hlc::Hlc { physical: 1_000 + index, logical: 0 },
    }
}

fn run(kv: &mut TestKv, index: u64, c: &Command) -> KvOutput {
    kv.apply(&c.encode(), &ctx(index)).unwrap()
}

fn key(k: &str) -> Vec<u8> {
    k.as_bytes().to_vec()
}

#[test]
fn put_get_cas_and_delete() {
    let mut kv = TestKv::default();
    let put = |k: &str, v: &str| Command::Put { key: key(k), value: key(v) };
    assert_eq!(run(&mut kv, 1, &put("a", "1")), KvOutput::Previous(None));
    assert_eq!(run(&mut kv, 2, &put("a", "2")), KvOutput::Previous(Some(key("1"))));
    assert_eq!(run(&mut kv, 3, &Command::Get { key: key("a") }), KvOutput::Value(Some(key("2"))));
    assert_eq!(run(&mut kv, 4, &Command::Get { key: key("b") }), KvOutput::Value(None));
    // Compare and swap: only with the expected current value (`None` = absent).
    let cas = |k: &str, expected: Option<&str>, new: Option<&str>| Command::Cas {
        key: key(k),
        expected: expected.map(key),
        new: new.map(key),
    };
    assert_eq!(
        run(&mut kv, 5, &cas("a", Some("1"), Some("3"))),
        KvOutput::Swapped(false, Some(key("2")))
    );
    assert_eq!(
        run(&mut kv, 6, &cas("a", Some("2"), Some("3"))),
        KvOutput::Swapped(true, Some(key("2")))
    );
    assert_eq!(run(&mut kv, 7, &cas("b", None, Some("x"))), KvOutput::Swapped(true, None));
    assert_eq!(
        run(&mut kv, 8, &cas("b", Some("x"), None)),
        KvOutput::Swapped(true, Some(key("x")))
    );
    assert_eq!(kv.get(b"b"), None);
    assert_eq!(
        run(&mut kv, 9, &Command::Delete { key: key("a") }),
        KvOutput::Previous(Some(key("3")))
    );
    assert_eq!(run(&mut kv, 10, &Command::Delete { key: key("a") }), KvOutput::Previous(None));
    assert_eq!(kv.applied_index(), 10);
    assert!(kv.is_empty());
}

#[test]
fn snapshots_restore_the_same_state() {
    let mut kv = TestKv::default();
    for i in 0..50u64 {
        let c = Command::Put {
            key: format!("k{}", i % 7).into_bytes(),
            value: i.to_le_bytes().to_vec(),
        };
        run(&mut kv, i + 1, &c);
    }
    kv.skip(51);
    let snap = kv.snapshot();
    let mut copy = TestKv::default();
    copy.restore(&snap).unwrap();
    assert_eq!(copy, kv);
    assert_eq!(copy.applied_index(), 51);
    assert_eq!(copy.len(), 7);
    let keys: Vec<&[u8]> = copy.iter().map(|(k, _)| k).collect();
    assert_eq!(keys.first(), Some(&&b"k0"[..]));
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "in key order");
    // Same entries from the same state, same outputs.
    let c = Command::Cas { key: key("k3"), expected: None, new: Some(key("z")) };
    assert_eq!(run(&mut kv, 52, &c), run(&mut copy, 52, &c));
    assert_eq!(kv, copy);
}

#[test]
fn malformed_inputs_are_refused() {
    let mut kv = TestKv::default();
    assert!(kv.apply(&[], &ctx(1)).is_err());
    assert!(kv.apply(&[9, 0], &ctx(1)).is_err());
    let mut put = Command::Put { key: key("a"), value: key("b") }.encode();
    put.push(0);
    assert!(kv.apply(&put, &ctx(1)).is_err(), "trailing bytes");
    assert!(kv.restore(&[1, 2, 3]).is_err());
    let mut snap = kv.snapshot();
    snap.push(0);
    assert!(kv.restore(&snap).is_err());
    assert_eq!(
        Command::decode(&Command::Get { key: key("q") }.encode()),
        Ok(Command::Get { key: key("q") })
    );
}
