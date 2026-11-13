//! The network scenarios against a remote server that refuses reads and updates: a server given
//! by `--target`, with no administration port, whose failed operations count as errors and give
//! the report its first error (002 FR-019) or end the scenario with it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Output};

use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};
use teseladb_bson::Document;

/// The commands the server refuses; it answers `ok: 1` to every other one.
const REFUSED: [&str; 2] = ["find", "update"];
/// The command that lists the search indexes, which a scenario waits on (011): the server lists
/// one `READY` index, as a node does once it built it.
const LIST_SEARCH_INDEXES: &str = "listSearchIndexes";
const REFUSAL: &str = "refused by the test server";

/// The label of a conflict a client repeats.
const TRANSIENT: &str = "TransientTransactionError";

/// Answers one connection until the client closes it; with `transient`, its refusals carry the
/// label of a conflict.
fn serve(mut s: TcpStream, transient: bool) {
    loop {
        let mut whole = vec![0u8; HEADER_LEN];
        if s.read_exact(&mut whole).is_err() {
            return;
        }
        let header = Header::parse(&whole);
        whole.resize(usize::try_from(header.length).expect("length"), 0);
        s.read_exact(&mut whole[HEADER_LEN..]).expect("message");
        let body = OpMsg::parse(&whole).expect("OP_MSG").body.expect("body");
        let name = body.iter().next().expect("name").0.to_owned();
        let name = name.as_str();
        let reply = if REFUSED.contains(&name) {
            let refusal = Document::new().with("ok", 0.0).with("errmsg", REFUSAL).with("code", 13);
            if transient {
                refusal.with("errorLabels", vec![teseladb_bson::Value::String(TRANSIENT.into())])
            } else {
                refusal
            }
        } else if name == LIST_SEARCH_INDEXES {
            ready_search_index()
        } else {
            Document::new().with("ok", 1.0)
        };
        let bytes = reply.to_bytes().expect("bson");
        if s.write_all(&OpMsg::encode(1, header.request_id, 0, &bytes)).is_err() {
            return;
        }
    }
}

/// The answer of `listSearchIndexes` with one index `default` built and queryable.
fn ready_search_index() -> Document {
    use teseladb_bson::Value;
    let index =
        Document::new().with("name", "default").with("status", "READY").with("queryable", true);
    let cursor = Document::new()
        .with("id", Value::Int64(0))
        .with("firstBatch", vec![Value::Document(index)]);
    Document::new().with("cursor", cursor).with("ok", 1.0)
}

/// A server on loopback that answers each connection in its own thread, for the whole test run.
fn refusing_server() -> SocketAddr {
    server(false)
}

/// The same server, whose refusals are conflicts a client repeats.
fn conflicting_server() -> SocketAddr {
    server(true)
}

fn server(transient: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            std::thread::spawn(move || serve(s, transient));
        }
    });
    addr
}

fn bench(target: SocketAddr, args: &[&str]) -> Output {
    let uri = format!("refdb://{target}");
    let short = ["--duration", "100ms", "--warmup", "0s", "--records", "20", "--format", "json"];
    Command::new(env!("CARGO_BIN_EXE_teseladb-bench"))
        .args(args)
        .args(short)
        .args(["--target", &uri])
        .output()
        .expect("run")
}

#[test]
fn reads_that_fail_are_counted_and_the_first_one_noted() {
    let server = refusing_server();
    for scenario in ["reference", "ycsb-c"] {
        let out = bench(server, &["--scenario", scenario]);
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{scenario}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains(&format!("first error: {REFUSAL}")), "{scenario}: {text}");
    }
}

#[test]
fn transactions_that_fail_end_their_scenario_with_the_first_error() {
    let server = refusing_server();
    for scenario in ["txn-readers", "contention"] {
        let out = bench(server, &["--scenario", scenario]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{scenario}: {err}");
        assert!(err.contains("the first:") && err.contains(REFUSAL), "{scenario}: {err}");
    }
}

#[test]
fn a_group_that_does_not_give_one_group_per_document_is_an_error() {
    let out = bench(refusing_server(), &["--scenario", "group-large"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("the $group gave None groups of 20 documents"), "{err}");
}

#[test]
fn a_remote_server_is_not_split() {
    let out = bench(refusing_server(), &["--scenario", "index-write"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("same_range.p50") && !text.contains("other_range.p50"), "{text}");
    assert!(text.contains("other_range needs a local cluster"), "{text}");
}

#[test]
fn transactions_in_conflict_are_aborted_and_counted() {
    let out = bench(conflicting_server(), &["--scenario", "contention"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("\"name\":\"conflict_aborts\",\"unit\":\"ratio\",\"value\":1"), "{text}");
    assert!(!text.contains("commit.p50"), "{text}");
}

#[test]
fn time_series_keep_ingesting_until_the_duration_ends() {
    // Every insert answers at once: the hour of each sensor is written before the duration ends,
    // and the clients go on until it does; then the server refuses the scenario's reads.
    let out = bench(refusing_server(), &["--scenario", "timeseries"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains(REFUSAL), "{err}");
}
