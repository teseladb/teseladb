//! Reads with a read preference no replica can satisfy (007 FR-013, D-093): a single node has no
//! follower. With `maxTimeMS` the read waits until it ends (`MaxTimeMSExpired`); without it, the
//! selection limit of a refrouter 9.0.4 (15 s, measured), then `FailedToSatisfyReadPreference`
//! naming the range's replica group.

mod common;

use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

fn secondary() -> Document {
    Document::new().with("mode", "secondary")
}

#[test]
fn a_preference_no_replica_satisfies_waits_and_then_fails() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("insert", "c").with("documents", docs(0..2)))));
    // The leader serves `secondaryPreferred` when no follower can.
    let pref = Document::new().with("mode", "secondaryPreferred");
    let find = Document::new().with("find", "c").with("$readPreference", pref);
    let rows = until("the leader's answer", || {
        let r = c.run("app", find.clone());
        ok(&r).then(|| batch(&r, "firstBatch"))
    });
    assert_eq!(rows.len(), 2);
    let find = Document::new().with("find", "c").with("$readPreference", secondary());
    let r = c.run("app", find.clone().with("maxTimeMS", 300));
    assert_eq!(r.get("code"), Some(&Value::Int32(50)), "{r:?}");
    let t0 = Instant::now();
    let r = c.run("app", find);
    assert!(t0.elapsed() >= Duration::from_secs(15), "{:?}", t0.elapsed());
    assert_eq!(r.get("code"), Some(&Value::Int32(133)), "{r:?}");
    let want =
        r#"Could not find host matching read preference { mode: "secondary" } for set range-1"#;
    assert_eq!(r.get("errmsg"), Some(&Value::String(want.to_owned())), "{r:?}");
    node.stop();
}
