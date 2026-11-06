//! The state of the machine in `/metrics` (007 FR-038), which TeselaDB Cloud watches on a
//! dedicated cluster (021 FR-048): the disk of the data directory, the process's resident memory
//! and the histogram of how long the clients' commands took on the node.

mod common;

use std::net::SocketAddr;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::admin_http::{Method, request};
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::Document;
use teseladb_raftlog::MemDisk;

/// The value of the sample `name` (without labels) in `text`.
fn sample(text: &str, name: &str) -> Option<f64> {
    text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

#[test]
fn the_node_gives_its_disk_its_memory_and_how_long_commands_take() {
    let dir = std::env::temp_dir().join(format!("teseladb-node-metrics-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = NodeConfig {
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        data_dir: Some(dir.clone()),
        ..config(Vec::new())
    };
    let n = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let mut c = Client::new(&n);
    assert!(ok(&c.run("app", Document::new().with("insert", "c").with("documents", docs(0..5)))));
    for _ in 0..4 {
        let found = c.run("app", Document::new().with("find", "c"));
        assert_eq!(batch(&found, "firstBatch").len(), 5);
    }
    let at = n.admin_addr().unwrap().to_string();
    let text = until("the commands counted", || {
        let (status, body) = request(&at, Method::Get, "/metrics", b"").unwrap();
        let text = String::from_utf8(body).unwrap();
        let count = sample(&text, "teseladb_command_duration_seconds_count").unwrap_or(0.0);
        (status == 200 && count >= 5.0).then_some(text)
    });
    let used = sample(&text, "teseladb_disk_used_bytes").expect("disk used");
    let total = sample(&text, "teseladb_disk_total_bytes").expect("disk total");
    assert!(total > 0.0 && used <= total, "{used} of {total}");
    assert!(sample(&text, "teseladb_process_resident_memory_bytes").expect("memory") > 0.0);
    assert!(text.contains("# TYPE teseladb_command_duration_seconds histogram"), "{text}");
    for le in ["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf"] {
        let bucket = format!("teseladb_command_duration_seconds_bucket{{le=\"{le}\"}} ");
        assert!(text.contains(&bucket), "{bucket} in {text}");
    }
    let all = "teseladb_command_duration_seconds_bucket{le=\"+Inf\"} ";
    let inf: f64 = text.lines().find_map(|l| l.strip_prefix(all)?.trim().parse().ok()).unwrap();
    let count = sample(&text, "teseladb_command_duration_seconds_count").unwrap();
    assert!((inf - count).abs() < f64::EPSILON, "every command falls in +Inf");
    assert!(sample(&text, "teseladb_command_duration_seconds_sum").unwrap() >= 0.0);
    n.stop();
    let _ = std::fs::remove_dir_all(dir);
}
