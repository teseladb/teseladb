//! 007 T052 (FR-032, SC-009): the limits of a tenant on each of three nodes. Past
//! `maxConnections` a connection closes as it signs in, without an answer; past `opsPerSecond`,
//! `maxConcurrentOps` or `maxWriteBytesPerSecond` a command fails without running with 462
//! `IngressRequestRateLimitExceeded` and the labels `SystemOverloadedError` and `RetryableError`;
//! each node applies the whole limits, and the service user is held by none of them.

mod common;
mod secured;

use std::io::Read;
use std::time::Duration;

use common::{Client, batch, docs, ok, until};
use secured::{PASSWORD, ROOT_PASSWORD, admin_out as admin, login, secured as start};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;
use teseladb_server::tenant_limits::{RETRYABLE, SYSTEM_OVERLOADED};
use teseladb_wire::error::RETRYABLE_WRITE_ERROR;

const RATE_LIMITED: i32 = 462;

fn code(r: &Document) -> Option<i32> {
    match r.get("code") {
        Some(Value::Int32(c)) => Some(*c),
        _ => None,
    }
}

fn labels(r: &Document) -> Vec<String> {
    match r.get("errorLabels") {
        Some(Value::Array(all)) => all
            .iter()
            .filter_map(|l| match l {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Three secured nodes with tenant `acme`, its user `ann` and `root` signed in on the first.
fn cluster() -> (Vec<Node<MemDisk>>, Client) {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    nodes.extend((0..2).map(|_| start(seed.clone())));
    admin(&nodes[0], &["tenant", "create", "acme"]);
    let mut root = Client::new(&nodes[0]);
    assert!(ok(&login(&mut root, "root", ROOT_PASSWORD)));
    let create = Document::new()
        .with("createUser", "ann")
        .with("pwd", PASSWORD)
        .with("roles", Value::Array(vec![Value::from("readWriteAnyDatabase")]))
        .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
        .with("$tenant", "acme");
    assert!(ok(&root.run("admin", create)));
    (nodes, root)
}

/// Waits until `node` holds limit `key` of `acme` at `value`: a change reaches the other nodes a
/// moment after the node that took it.
fn reached(node: &Node<MemDisk>, key: &str, value: u64) {
    let want = format!("\"{key}\":{value}");
    until(&format!("{key} {value} on the node"), || {
        let out = admin(node, &["--format", "json", "tenant", "limits", "acme"]);
        out.replace([' ', '\n'], "").contains(&want).then_some(())
    });
}

/// A connection of Ann on `node`, signed in once the user and the limits reached it.
fn ann(node: &Node<MemDisk>) -> Client {
    until("Ann signs in", || {
        let mut c = Client::new(node);
        ok(&login(&mut c, "ann", PASSWORD)).then_some(c)
    })
}

/// Whether the node closed `c` without answering.
fn closed(c: &mut Client) -> bool {
    c.s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut b = [0u8; 1];
    matches!(c.s.read(&mut b), Ok(0) | Err(_))
}

/// A connection of Ann on `node` that sends the last step of its sign-in without reading the
/// answer.
fn third_sign_in(node: &Node<MemDisk>) -> Client {
    let mut third = Client::new(node);
    third.s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mech = teseladb_auth::scram::Mechanism::ScramSha256;
    let prepared = mech.prepare_password("ann", PASSWORD, false).unwrap();
    let (mut s, first) =
        teseladb_auth::scram::ClientSession::start(mech, "ann", &prepared, "limits-nonce-01");
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", mech.name())
        .with("payload", Value::Binary { subtype: 0, bytes: first })
        .with("options", Document::new().with("skipEmptyExchange", true));
    let r = third.run("admin", start);
    let fin = s.respond(&secured::payload(&r)).unwrap();
    let next = Document::new()
        .with("saslContinue", 1)
        .with("conversationId", 1)
        .with("payload", Value::Binary { subtype: 0, bytes: fin });
    third.send("admin", next);
    third
}

fn stop(nodes: Vec<Node<MemDisk>>) {
    for n in nodes.into_iter().rev() {
        n.stop();
    }
}

#[test]
fn each_node_takes_max_connections_of_the_tenant_and_closes_the_next_one_without_an_answer() {
    let (nodes, _root) = cluster();
    admin(&nodes[0], &["tenant", "limits", "acme", "--max-connections", "2"]);
    for node in &nodes {
        reached(node, "maxConnections", 2);
        // The node applies the limit a moment after it holds it: until then it takes any number.
        // Once it does, the third signs in and the node closes it before answering.
        let (a, b) = until("the third connection closed without an answer", || {
            let (a, b) = (ann(node), ann(node));
            let mut third = third_sign_in(node);
            closed(&mut third).then_some((a, b))
        });
        // One closes, and another takes its place.
        drop(a);
        until("a place again", || {
            let mut c = Client::new(node);
            ok(&login(&mut c, "ann", PASSWORD)).then_some(())
        });
        drop(b);
    }
    stop(nodes);
}

#[test]
fn commands_past_the_tenant_rate_concurrency_or_bytes_fail_with_462_and_its_labels() {
    let (nodes, _root) = cluster();
    admin(&nodes[0], &["tenant", "limits", "acme", "--ops-per-second", "20"]);
    // Four connections ask as fast as they can for three seconds: the node serves the bucket's
    // second and then 20 a second, give or take the 100 ms a command may wait.
    let mut c = ann(&nodes[1]);
    let find = || Document::new().with("find", "c").with("filter", Document::new());
    reached(&nodes[1], "opsPerSecond", 20);
    // Once the node applies it, 40 commands in a row take at least the second of the 20 past the
    // bucket's; then a second fills the bucket again.
    until("the rate limit applied on the node", || {
        let started = std::time::Instant::now();
        let served = (0..40).filter(|_| ok(&c.run("app", find()))).count();
        (served == 40 && started.elapsed() >= Duration::from_millis(900)).then_some(())
    });
    std::thread::sleep(Duration::from_millis(1100));
    let window = Duration::from_secs(3);
    let loads: Vec<std::thread::JoinHandle<(usize, Option<Document>)>> = (0..4)
        .map(|_| {
            let mut conn = ann(&nodes[1]);
            std::thread::spawn(move || {
                let (end, mut served, mut refused) = (std::time::Instant::now() + window, 0, None);
                while std::time::Instant::now() < end {
                    let r = conn.run("app", find());
                    if ok(&r) {
                        served += 1;
                    } else {
                        refused = Some(r);
                    }
                }
                (served, refused)
            })
        })
        .collect();
    let results: Vec<(usize, Option<Document>)> =
        loads.into_iter().map(|t| t.join().unwrap()).collect();
    let served: usize = results.iter().map(|(n, _)| n).sum();
    assert!((70..=90).contains(&served), "{served} served in 3 s against 20 per second");
    let refused = results.iter().find_map(|(_, r)| r.clone()).expect("a refused command");
    assert_eq!(code(&refused), Some(RATE_LIMITED), "{refused:?}");
    assert_eq!(
        refused.get("codeName"),
        Some(&Value::String("IngressRequestRateLimitExceeded".into()))
    );
    assert_eq!(labels(&refused), ["SystemOverloadedError", "RetryableError"]);
    // `ping` spends no token.
    assert!(ok(&c.run("admin", Document::new().with("ping", 1))));
    // Bytes written per second.
    admin(
        &nodes[0],
        &[
            "tenant",
            "limits",
            "acme",
            "--ops-per-second",
            "0",
            "--max-write-bytes-per-second",
            "20000",
        ],
    );
    // Each write of 60 KB leaves the bucket of 20 KB a second 40 KB below zero, so the next one
    // waits three seconds, far more than a command may wait, however slow the writes are.
    let mut w = ann(&nodes[2]);
    let big = |i: i32| {
        let filler = "x".repeat(60_000);
        Document::new().with("insert", "big").with(
            "documents",
            Value::Array(vec![Value::Document(Document::new().with("_id", i).with("f", filler))]),
        )
    };
    reached(&nodes[2], "maxWriteBytesPerSecond", 20_000);
    // The node applies it a moment after it holds it: until then every write passes.
    let mut next_id = 0;
    let refused = until("a write past the bytes per second", || {
        (0..3).find_map(|_| {
            next_id += 1;
            let r = w.run("app", big(next_id));
            (!ok(&r)).then_some(r)
        })
    });
    assert_eq!(code(&refused), Some(RATE_LIMITED), "{refused:?}");
    assert_eq!(labels(&refused), [SYSTEM_OVERLOADED, RETRYABLE]);
    // A retryable write (its session and `txnNumber`) also takes the label a driver retries a
    // write on (D-148).
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] });
    let retried = until("a retryable write past the bytes per second", || {
        (0..3).find_map(|_| {
            next_id += 1;
            let cmd = big(next_id)
                .with("lsid", lsid.clone())
                .with("txnNumber", Value::Int64(i64::from(next_id)));
            let r = w.run("app", cmd);
            (!ok(&r)).then_some(r)
        })
    });
    assert_eq!(code(&retried), Some(RATE_LIMITED), "{retried:?}");
    assert_eq!(labels(&retried), [SYSTEM_OVERLOADED, RETRYABLE, RETRYABLE_WRITE_ERROR]);
    // The service user is held by no limit.
    admin(
        &nodes[0],
        &[
            "tenant",
            "limits",
            "acme",
            "--max-write-bytes-per-second",
            "0",
            "--ops-per-second",
            "5",
            "--service-user",
            "ann",
        ],
    );
    let mut svc = ann(&nodes[0]);
    let all_ok = (0..40).map(|_| svc.run("app", find())).all(|r| ok(&r));
    assert!(all_ok, "the service user is not limited");
    stop(nodes);
}

#[test]
fn a_tenant_runs_at_most_max_concurrent_ops_at_once_on_a_node() {
    let (nodes, _root) = cluster();
    let mut slow = ann(&nodes[0]);
    assert!(
        ok(&slow.run("app", Document::new().with("insert", "c").with("documents", docs(0..1))))
    );
    admin(&nodes[0], &["tenant", "limits", "acme", "--max-concurrent-ops", "1"]);
    let mut other = ann(&nodes[0]);
    until("the limit on the node", || {
        // One command that takes a second holds the only slot (a node of a test has no host
        // sleep for `sleep()`, so the call keeps its thread busy instead).
        let sleepy = Document::new().with("find", "c").with(
            "filter",
            Document::new().with(
                "$where",
                "var s = Date.now(); while (Date.now() - s < 1000) {} return true;",
            ),
        );
        slow.send("app", sleepy);
        std::thread::sleep(Duration::from_millis(200));
        let r = other.run("app", Document::new().with("find", "c"));
        let held = code(&r) == Some(RATE_LIMITED);
        let _ = slow.receive();
        held.then_some(())
    });
    // Another node has its own slot.
    let mut elsewhere = ann(&nodes[1]);
    let r = elsewhere.run("app", Document::new().with("find", "c"));
    assert_eq!(batch(&r, "firstBatch").len(), 1, "{r:?}");
    stop(nodes);
}
