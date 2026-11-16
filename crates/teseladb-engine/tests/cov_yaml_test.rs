#![cfg(target_os = "linux")]
//! Tests of the YAML subset read for the I/O properties (block and flow collections, quoted
//! scalars and escapes, comments, nulls, conversions, parse errors with their position) and of
//! the decoding of one disk's properties.

use teseladb::engine::core::reactor::yaml::{self, Node};
use teseladb::engine::core::reactor::{DiskParams, decode_disk_params};

fn s(v: &str) -> Node {
    Node::Scalar(v.to_string())
}

fn load(text: &str) -> Node {
    yaml::load(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn error(text: &str) -> String {
    yaml::load(text).err().unwrap_or_else(|| panic!("{text:?} parsed")).to_string()
}

#[test]
fn block_mappings_and_sequences_nest() {
    let doc = load(
        "# I/O properties\n\
         disks:\n\
         \x20 - mountpoint: /var/lib/data   # the data disk\n\
         \x20   read_iops: 100\n\
         \x20   tags:\n\
         \x20     - fast\n\
         \x20     - 'ssd'\n\
         \x20 - mountpoints: [/a, \"/b\"]\n\
         \x20   empty:\n\
         name: \"quoted: value\"\n\
         nothing: ~\n\
         nil: null\n",
    );
    let disks = doc.get("disks").as_sequence().unwrap();
    assert_eq!(disks.len(), 2);
    assert_eq!(disks[0].get("mountpoint"), &s("/var/lib/data"));
    assert_eq!(disks[0].get("read_iops").as_string().unwrap(), "100");
    assert_eq!(disks[0].get("tags").as_string_vec().unwrap(), vec!["fast", "ssd"]);
    assert_eq!(disks[1].get("mountpoints").as_string_vec().unwrap(), vec!["/a", "/b"]);
    assert_eq!(disks[1].get("empty"), &Node::Null);
    assert_eq!(doc.get("name"), &s("quoted: value"));
    assert_eq!(doc.get("nothing"), &Node::Null);
    assert_eq!(doc.get("nil"), &Node::Null);
    assert!(!doc.get("missing").is_defined());
    assert!(!doc.get("missing").get("deeper").is_defined());
    assert!(doc.get("name").is_defined());
}

#[test]
fn flow_collections_and_escapes() {
    let doc = load(r#"m: {a: 1, 'b': [x, {c: "d\te\n\"\\\/\0"}], e: , f: {}}"#);
    let m = doc.get("m");
    assert_eq!(m.get("a"), &s("1"));
    let b = m.get("b").as_sequence().unwrap();
    assert_eq!(b[0], s("x"));
    assert_eq!(b[1].get("c"), &s("d\te\n\"\\/\0"));
    assert_eq!(m.get("e"), &Node::Null);
    assert_eq!(m.get("f"), &Node::Map(Vec::new()));

    let seq = load("[1, [2, 3], {k: v}, ]");
    let v = seq.as_sequence().unwrap();
    assert_eq!(v[0], s("1"));
    assert_eq!(v[1], Node::Sequence(vec![s("2"), s("3")]));
    assert_eq!(v[2].get("k"), &s("v"));

    let single = load("k: 'it''s'");
    assert_eq!(single.get("k").as_string().unwrap(), "it's");
    let hash_in_value = load("k: a#b");
    assert_eq!(hash_in_value.get("k"), &s("a#b"));
    let spaced_comment = load("k: a #b");
    assert_eq!(spaced_comment.get("k"), &s("a"));
}

#[test]
fn sequences_of_mappings_and_plain_scalars() {
    let doc = load("- a\n- b: 1\n  c: 2\n-\n  - nested\n- [x]\n");
    let v = doc.as_sequence().unwrap();
    assert_eq!(v[0], s("a"));
    assert_eq!(v[1].get("b"), &s("1"));
    assert_eq!(v[1].get("c"), &s("2"));
    assert_eq!(v[2], Node::Sequence(vec![s("nested")]));
    assert_eq!(v[3], Node::Sequence(vec![s("x")]));
    assert_eq!(load(""), Node::Null);
    assert_eq!(load("just text"), s("just text"));
}

#[test]
fn conversions_check_the_node_kind() {
    for t in ["true", "True", "TRUE", "yes", "Yes", "YES", "on", "On", "ON", "y", "Y"] {
        assert!(s(t).as_bool().unwrap(), "{t}");
    }
    for f in ["false", "False", "FALSE", "no", "No", "NO", "off", "Off", "OFF", "n", "N"] {
        assert!(!s(f).as_bool().unwrap(), "{f}");
    }
    let bad = s("maybe").as_bool().unwrap_err().to_string();
    assert!(bad.contains("yaml: error: bad conversion"), "{bad}");
    assert_eq!(s("1.5").as_f32().unwrap(), 1.5);
    assert!(s("fast").as_f32().is_err());
    assert!(Node::Null.as_string().is_err());
    assert!(s("x").as_sequence().is_err());
    assert!(s("x").as_string_vec().is_err());
    assert!(Node::Sequence(vec![Node::Null]).as_string_vec().is_err());
    assert_eq!(s("x").get("k"), &Node::Undefined);
}

#[test]
fn malformed_documents_report_where() {
    let e = error("k: {a: 1");
    assert!(e.contains("yaml: error at line 1"), "{e}");
    assert!(e.contains("end of map flow not found"), "{e}");
    assert!(error("k: [1, 2").contains("end of sequence flow not found"));
    assert!(error("k: [1, 2}").contains("end of sequence flow not found"));
    assert!(error("k: {a: 1]").contains("end of map flow not found"));
    assert!(error("k: \"open").contains("end of string not found"));
    assert!(error(r#"k: "\q""#).contains("unknown escape character"));
    assert!(error("k: {[a]: 1}").contains("illegal map value"));
    let e = error("a: 1\n  b: 2\n");
    assert!(e.contains("line 2"), "{e}");
    let e = error("- a\n   - b\n");
    assert!(e.contains("end of sequence not found") || e.contains("line 2"), "{e}");
    assert!(yaml::load_file("/nonexistent/io.yaml").is_err());
}

#[test]
fn files_load_like_strings() {
    let path = std::env::temp_dir().join(format!("cov_yaml_{}.yaml", std::process::id()));
    std::fs::write(&path, "disks:\n  - mountpoint: /\n").unwrap();
    let doc = yaml::load_file(path.to_str().unwrap()).unwrap();
    assert_eq!(doc.get("disks").as_sequence().unwrap()[0].get("mountpoint"), &s("/"));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn disk_parameters_decode_with_units_and_options() {
    let doc = load(
        "mountpoint: /data\n\
         read_iops: 1k\n\
         read_bandwidth: 512M\n\
         write_iops: 300\n\
         write_bandwidth: 1G\n\
         read_saturation_length: 128k\n\
         write_saturation_length: 64k\n\
         duplex: true\n\
         rate_factor: 0.5\n",
    );
    let mut p = DiskParams::default();
    assert!(decode_disk_params(&doc, &mut p).unwrap());
    assert_eq!(p.mountpoints, vec!["/data"]);
    assert_eq!(p.read_req_rate, 1024);
    assert_eq!(p.read_bytes_rate, 512 << 20);
    assert_eq!(p.write_req_rate, 300);
    assert_eq!(p.write_bytes_rate, 1 << 30);
    assert_eq!(p.read_saturation_length, 128 << 10);
    assert_eq!(p.write_saturation_length, 64 << 10);
    assert!(p.duplex);
    assert_eq!(p.rate_factor, 0.5);

    let several = load(
        "mountpoints: [/a, /b]\nread_iops: 1\nread_bandwidth: 2\nwrite_iops: 3\nwrite_bandwidth: 4\n",
    );
    let mut q = DiskParams::default();
    decode_disk_params(&several, &mut q).unwrap();
    assert_eq!(q.mountpoints, vec!["/a", "/b"]);
    assert_eq!(
        (q.read_req_rate, q.read_bytes_rate, q.write_req_rate, q.write_bytes_rate),
        (1, 2, 3, 4)
    );

    let missing_rate = load("mountpoint: /x\nread_iops: 1\n");
    assert!(decode_disk_params(&missing_rate, &mut DiskParams::default()).is_err());
    let bad_size = load(
        "mountpoint: /x\nread_iops: lots\nread_bandwidth: 1\nwrite_iops: 1\nwrite_bandwidth: 1\n",
    );
    assert!(decode_disk_params(&bad_size, &mut DiskParams::default()).is_err());
    let bad_duplex = load(
        "mountpoint: /x\nread_iops: 1\nread_bandwidth: 1\nwrite_iops: 1\nwrite_bandwidth: 1\nduplex: sometimes\n",
    );
    assert!(decode_disk_params(&bad_duplex, &mut DiskParams::default()).is_err());
}
