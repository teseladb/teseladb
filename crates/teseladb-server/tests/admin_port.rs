//! The administration port and `teseladb admin` (007 FR-020, contract `admin-cli.md`): `/healthz`,
//! `/metrics` and each subcommand against a running node, with the exit codes of the contract.

mod common;

use std::net::SocketAddr;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::admin_http::{Method, request};
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::Document;
use teseladb_raftlog::MemDisk;

fn with_admin(join: Vec<String>) -> NodeConfig {
    NodeConfig { admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))), ..config(join) }
}

/// Runs `teseladb admin --node <at> <args>`: exit code, standard output and error.
fn admin(at: &str, args: &[&str]) -> (i32, String, String) {
    let argv: Vec<String> =
        ["admin", "--node", at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn health_and_metrics_are_served_on_the_administration_port() {
    let n = Node::start(with_admin(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    assert_eq!(request(&at, Method::Get, "/healthz", b"").unwrap(), (200, Vec::new()));
    let metrics = until("the data range's replica", || {
        let (status, body) = request(&at, Method::Get, "/metrics", b"").unwrap();
        let text = String::from_utf8(body).unwrap();
        (status == 200 && text.contains("teseladb_range_leader{range=\"1\"} 1")).then_some(text)
    });
    assert!(metrics.contains("# TYPE teseladb_up gauge\nteseladb_up 1\n"), "{metrics}");
    assert!(metrics.contains("teseladb_range_applied_index{range=\"1\"} "), "{metrics}");
    assert_eq!(request(&at, Method::Get, "/nothing", b"").unwrap().0, 404);
    n.stop();
    assert!(request(&at, Method::Get, "/healthz", b"").is_err(), "the port closes with the node");
}

#[test]
fn every_subcommand_works_against_a_node() {
    let n = Node::start(with_admin(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let (code, out, _) = admin(&at, &["nodes"]);
    assert_eq!(code, 0);
    assert!(out.contains(&format!("{}\tvoter\tGroup 0 leader", n.node_id().0)), "{out}");
    let (code, out, _) = admin(&at, &["--format", "json", "nodes"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("{\"nodes\": [") || out.starts_with("{\"nodes\":["), "{out}");
    // The feature compatibility version the cluster runs and the one every node can go to, for whoever
    // changes the nodes' version (021 FR-041).
    let fcv = |at: &str| admin(at, &["--format", "json", "nodes"]).1.replace(' ', "");
    assert!(
        fcv(&at).contains(r#""featureCompatibilityVersion":{"current":"9.0","available":"9.0"}"#),
        "{}",
        fcv(&at)
    );
    // A collection to split, and its ranges.
    let mut c = Client::new(&n);
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..10)))));
    let (code, out, _) = admin(&at, &["ranges", "--collection", "db.c"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("1\t["), "{out}");
    let (code, _, err) = admin(&at, &["ranges", "--collection", "db.none"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: no collection db.none\n"));
    let (code, out, err) =
        admin(&at, &["range", "split", "1", "--at", r#"{"ns": "db.c", "_id": 5}"#]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"ok\""), "{out}");
    let two = until("the split", || {
        let (_, out, _) = admin(&at, &["ranges"]);
        (out.lines().count() == 2).then_some(out)
    });
    let right: u64 = two.lines().nth(1).unwrap().split('\t').next().unwrap().parse().unwrap();
    // The documents on both sides of the cut still read.
    let found = until("both sides of the cut", || {
        let r = c.run("db", Document::new().with("find", "c"));
        ok(&r).then(|| batch(&r, "firstBatch"))
    });
    assert_eq!(found.len(), 10);
    assert!(right > 1);
    let (code, out, err) = admin(&at, &["range", "merge", "1"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("\"right\": {right}")) || out.contains(&format!("\"right\":{right}")),
        "{out}"
    );
    let (code, _, err) = admin(&at, &["range", "merge", &right.to_string()]);
    assert_eq!((code, err.contains("no right neighbour")), (1, true), "{err}");
    let (code, _, err) = admin(&at, &["range", "split", "77"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: no range 77\n"));
    let set = Document::new().with("setFeatureCompatibilityVersion", "8.0").with("confirm", true);
    assert!(ok(&c.run("admin", set)));
    assert!(
        fcv(&at).contains(r#""featureCompatibilityVersion":{"current":"8.0","available":"9.0"}"#),
        "{}",
        fcv(&at)
    );
    // Cluster parameters, through the same command a client runs.
    let (code, _, err) =
        admin(&at, &["config", "set", "defaultMaxTimeMS", r#"{"readOperations": 250}"#]);
    assert_eq!(code, 0, "{err}");
    let (code, out, _) = admin(&at, &["config", "get", "defaultMaxTimeMS"]);
    assert_eq!(code, 0);
    assert!(out.contains("250"), "{out}");
    let (code, _, err) = admin(&at, &["config", "get", "noSuchParameter"]);
    assert_eq!(code, 1);
    assert!(err.starts_with("teseladb admin: "), "{err}");
    // A node started without a PKI issues no enrollment tokens.
    let (code, _, err) = admin(&at, &["token", "create", "--ttl", "30m"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: this node has no PKI (--insecure)\n"));
    let (code, _, err) = admin(&at, &["cert", "rotate"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: this node has no PKI (--insecure)\n"));
    n.stop();
    let (code, _, err) = admin(&at, &["nodes"]);
    assert_eq!(code, 1);
    assert!(err.starts_with(&format!("teseladb admin: cannot reach {at}: ")), "{err}");
}

/// What the port refuses, with the reason: a name that is not a namespace, a split key that is
/// not a document's or falls outside the range, a merge of a range that does not exist.
#[test]
fn refused_subcommands_say_why() {
    let n = Node::start(with_admin(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n);
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..10)))));
    let refused = |args: &[&str]| {
        let (code, _, err) = admin(&at, args);
        assert_eq!(code, 1, "{args:?}: {err}");
        err
    };
    assert!(refused(&["ranges", "--collection", "nodot"]).contains("not a namespace: nodot"));
    assert!(refused(&["ranges", "--collection", "db.c d"]).contains("no collection db.c d"));
    let split = |at: &str| refused(&["range", "split", "1", "--at", at]);
    assert!(split(r#"{"_id": 5}"#).contains("ns"));
    assert!(split(r#"{"ns": "db.c"}"#).contains("--at needs an _id"));
    // A capped collection's documents are keyed by insertion order, not by `_id`.
    let capped = Document::new().with("create", "cap").with("capped", true).with("size", 4096);
    assert!(ok(&c.run("db", capped)));
    assert!(split(r#"{"ns": "db.cap", "_id": 1}"#).contains("no _id key"));
    let (code, _, err) =
        admin(&at, &["range", "split", "1", "--at", r#"{"ns": "db.c", "_id": 5}"#]);
    assert_eq!(code, 0, "{err}");
    let two = until("the split", || {
        let (_, out, _) = admin(&at, &["ranges", "--collection", "db.c"]);
        (out.lines().count() == 2).then_some(out)
    });
    assert!(two.starts_with("1\t"), "{two}");
    // The new range starts at the cut: a key below it is not inside.
    let right = two.lines().nth(1).unwrap().split('\t').next().unwrap().to_owned();
    let below = refused(&["range", "split", &right, "--at", r#"{"ns": "db.c", "_id": 1}"#]);
    assert!(below.contains(&format!("not inside range {right}")), "{below}");
    // Range 1 now ends at the cut: the cut itself is past its end.
    let above = refused(&["range", "split", "1", "--at", r#"{"ns": "db.c", "_id": 5}"#]);
    assert!(above.contains("not inside range 1"), "{above}");
    assert_eq!(refused(&["range", "merge", "77"]), "teseladb admin: no range 77\n");
    n.stop();
}

#[test]
fn usage_errors_exit_with_2() {
    for args in [
        &[][..],
        &["nodes", "--at", "x"],
        &["--format", "xml", "nodes"],
        &["token", "create", "--ttl", "5y"],
        &["token", "create", "--ttl", "h"],
        &["range", "split", "x"],
        &["range", "merge", "x"],
        &["config", "get"],
        &["nodes", "--node"],
        &["nodes", "--weird", "1"],
    ] {
        let (code, _, err) = admin("127.0.0.1:1", args);
        assert_eq!(code, 2, "{args:?}: {err}");
    }
}

/// `node drain` and `node remove` (007 T021, FR-015): a node is removed only once it holds
/// nothing; a drained one gives its replicas to the others and closes its clients' port.
#[test]
fn a_drained_node_gives_its_replicas_away_and_closes_its_clients_port() {
    let n1 = Node::start(with_admin(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n1.wait_ready(READY));
    let seed = vec![n1.peer_addr().to_string()];
    let n2 = Node::start(with_admin(seed.clone()), || Ok(MemDisk::new()), None).unwrap();
    let n3 = Node::start(with_admin(seed), || Ok(MemDisk::new()), None).unwrap();
    assert!(n2.wait_ready(READY) && n3.wait_ready(READY));
    let three = [n1.node_id(), n2.node_id(), n3.node_id()];
    until("three replicas", || {
        let d = n1.view(READY)?.data_range?;
        (d.nodes() == three && !d.settling).then_some(())
    });
    let at = n1.admin_addr().unwrap().to_string();
    let id = n3.node_id().0.to_string();
    let (code, _, err) = admin(&at, &["node", "remove", &id]);
    assert_eq!(code, 1);
    assert!(err.contains("drain it first"), "{err}");
    let (code, _, err) = admin(&at, &["node", "drain", "77"]);
    assert_eq!(code, 1);
    assert!(err.contains("not in the cluster"), "{err}");
    let (code, out, _) = admin(&at, &["node", "drain", &id]);
    assert_eq!((code, out), (0, format!("node {id} draining\n")));
    until("the range leaves node 3", || {
        let d = n1.view(READY)?.data_range?;
        (!d.nodes().contains(&n3.node_id()) && !d.settling).then_some(())
    });
    let port = n3.client_addrs()[0];
    until("node 3 closes its clients' port", || {
        let mut c = std::net::TcpStream::connect(port).ok()?;
        c.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok()?;
        let hello = Document::new().with("hello", 1).with("$db", "admin");
        let msg = teseladb_wire::msg::OpMsg::encode(1, 0, 0, &hello.to_bytes().ok()?);
        std::io::Write::write_all(&mut c, &msg).ok()?;
        let mut b = [0u8; 1];
        matches!(std::io::Read::read(&mut c, &mut b), Ok(0) | Err(_)).then_some(())
    });
    until("node 3 leaves Group 0's voters", || {
        let v = n1.view(READY)?;
        (!v.voters.contains(&n3.node_id()) && v.voters.len() == 2).then_some(())
    });
    // The list of nodes says each one's state: whoever drained node 3 sees it removed (021 FR-040).
    until("the list says node 3 is removed", || {
        let (code, out, _) = admin(&at, &["--format", "json", "nodes"]);
        let text: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        let removed =
            format!("{{\"_id\":{id},\"voter\":false,\"learner\":false,\"state\":\"removed\"}}");
        (code == 0 && text.contains(&removed) && text.contains("\"state\":\"active\""))
            .then_some(())
    });
    for n in [n3, n2, n1] {
        n.stop();
    }
}

/// The leaders of each range, from `teseladb admin --format json ranges`: the node each range's
/// descriptor names (`leaderHint`).
fn leader_hints(at: &str) -> Vec<Option<i64>> {
    let (_, out, _) = admin(at, &["--format", "json", "ranges"]);
    let Ok(d) =
        teseladb_rest::media::decode(out.trim().as_bytes(), teseladb_rest::media::Format::Relaxed)
    else {
        return Vec::new();
    };
    let Some(teseladb_bson::Value::Array(ranges)) = d.get("ranges") else { return Vec::new() };
    ranges
        .iter()
        .map(|r| match r {
            teseladb_bson::Value::Document(r) => match r.get("leaderHint") {
                // Relaxed Extended JSON gives a small number back as an `int`.
                Some(teseladb_bson::Value::Int64(n)) => Some(*n),
                Some(teseladb_bson::Value::Int32(n)) => Some(i64::from(*n)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The leaders of the ranges publish themselves in Group 0, and the balancer's moves spread them
/// over the nodes until no node leads more than ⌈ranges / 10⌉ over another (007 T019, FR-010).
#[test]
fn range_leaders_publish_themselves_and_spread_over_the_nodes() {
    let n1 = Node::start(with_admin(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n1.wait_ready(READY));
    let seed = vec![n1.peer_addr().to_string()];
    let n2 = Node::start(with_admin(seed.clone()), || Ok(MemDisk::new()), None).unwrap();
    let n3 = Node::start(with_admin(seed), || Ok(MemDisk::new()), None).unwrap();
    assert!(n2.wait_ready(READY) && n3.wait_ready(READY));
    let at = n1.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n1);
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..60)))));
    let three = [n1.node_id(), n2.node_id(), n3.node_id()];
    until("three replicas", || {
        let d = n1.view(READY)?.data_range?;
        (d.nodes() == three && !d.settling).then_some(())
    });
    for (done, k) in [10, 20, 30, 40, 50].into_iter().enumerate() {
        // The documents' keys do not follow `_id`: the cut goes to whichever range holds it.
        let ids = until("the previous split", || {
            let (_, out, _) = admin(&at, &["ranges"]);
            let ids: Vec<String> =
                out.lines().filter_map(|l| Some(l.split('\t').next()?.to_owned())).collect();
            (ids.len() == done + 1).then_some(ids)
        });
        let key = format!("{{\"ns\": \"db.c\", \"_id\": {k}}}");
        until("a split", || {
            let cut = ids.iter().any(|id| admin(&at, &["range", "split", id, "--at", &key]).0 == 0);
            cut.then_some(())
        });
    }
    let spread = until("leaders on every node", || {
        let hints = leader_hints(&at);
        if hints.len() != 6 || hints.iter().any(Option::is_none) {
            return None;
        }
        let mut by_node = std::collections::BTreeMap::<i64, usize>::new();
        for h in hints.into_iter().flatten() {
            *by_node.entry(h).or_default() += 1;
        }
        let (most, fewest) = (by_node.values().max()?, by_node.values().min()?);
        (by_node.len() == 3 && most - fewest <= 1).then_some(by_node)
    });
    assert_eq!(spread.values().sum::<usize>(), 6, "{spread:?}");
    for n in [n3, n2, n1] {
        n.stop();
    }
}

/// A cluster that changes its nodes one at a time, as a change of size does (021 FR-040): a fourth node joins three,
/// the first is drained until removed, and a fifth joins. Group 0 answers the administration port within moments of
/// each change, through the node that stays.
#[test]
fn group_0_keeps_answering_while_nodes_join_and_leave_one_at_a_time() {
    const ANSWER: std::time::Duration = std::time::Duration::from_secs(15);
    let n1 = Node::start(with_admin(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n1.wait_ready(READY));
    let seed = vec![n1.peer_addr().to_string()];
    let n2 = Node::start(with_admin(seed.clone()), || Ok(MemDisk::new()), None).unwrap();
    let n3 = Node::start(with_admin(seed.clone()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n2.wait_ready(READY) && n3.wait_ready(READY));
    let at2 = n2.admin_addr().unwrap().to_string();
    let answers = |at: &str, what: &str| {
        let started = std::time::Instant::now();
        loop {
            let (code, _, _) = admin(at, &["nodes"]);
            if code == 0 {
                return;
            }
            assert!(started.elapsed() < ANSWER, "Group 0 did not answer {what} in {ANSWER:?}");
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    };
    answers(&at2, "with three nodes");
    let n4 = Node::start(with_admin(seed), || Ok(MemDisk::new()), None).unwrap();
    assert!(n4.wait_ready(READY));
    answers(&at2, "after a fourth node joined");
    let id1 = n1.node_id().0.to_string();
    let (code, _, err) = admin(&at2, &["node", "drain", &id1]);
    assert_eq!(code, 0, "{err}");
    until("node 1 is removed", || {
        let (code, out, _) = admin(&at2, &["--format", "json", "nodes"]);
        let text: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        (code == 0
            && text.contains(&format!(
                "{{\"_id\":{id1},\"voter\":false,\"learner\":false,\"state\":\"removed\"}}"
            )))
        .then_some(())
    });
    answers(&at2, "after node 1 left");
    let n5 = Node::start(with_admin(vec![n2.peer_addr().to_string()]), || Ok(MemDisk::new()), None)
        .unwrap();
    assert!(n5.wait_ready(READY));
    answers(&at2, "after a fifth node joined");
    for n in [n5, n4, n3, n2, n1] {
        n.stop();
    }
}
