//! The YAML subset of the I/O properties, branch by branch: every null spelling, comments after
//! tabs, document markers, tabs and newlines inside flow collections, flow text continued on
//! more indented lines, keys that are null or quoted in the middle, single-pair mappings in flow
//! sequences, dash lines (`-` alone, `-<tab>`, `- - x`, `- key:` with a nested value), and every
//! malformed shape with the error it reports (yaml-cpp's messages, which the original reports).
#![cfg(target_os = "linux")]

use teseladb::engine::core::reactor::yaml::{self, Node};

fn s(v: &str) -> Node {
    Node::Scalar(v.to_string())
}

fn map(kv: &[(&str, Node)]) -> Node {
    Node::Map(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
}

fn load(text: &str) -> Node {
    yaml::load(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

fn error(text: &str) -> String {
    yaml::load(text)
        .err()
        .unwrap_or_else(|| panic!("{text:?} parsed"))
        .to_string()
}

#[test]
fn every_null_spelling_is_null() {
    for v in ["~", "null", "Null", "NULL"] {
        assert_eq!(load(&format!("k: {v}")).get("k"), &Node::Null, "{v}");
    }
    // An empty plain scalar inside a flow collection, and a null key.
    assert_eq!(load("{a: , b: 1}").get("a"), &Node::Null);
    assert_eq!(load("{: v}").get(""), &s("v"));
    assert_eq!(load("~: 1").get(""), &s("1"));
    assert_eq!(load("- ~: 2").as_sequence().unwrap()[0].get(""), &s("2"));
}

#[test]
fn comments_after_tabs_and_document_markers_are_skipped() {
    let doc = load("---\na: 1\t# after a tab\nb: x#not a comment\n...\n");
    assert_eq!(doc.get("a"), &s("1"));
    assert_eq!(doc.get("b"), &s("x#not a comment"));
    assert_eq!(load("---\n...\n"), Node::Null);
}

#[test]
fn flow_collections_take_tabs_and_newlines() {
    let doc = load("k: [1,\t2,\n   3]\nm: {a: 1,\n  b: [x,\n    y]}\n");
    assert_eq!(doc.get("k"), &Node::Sequence(vec![s("1"), s("2"), s("3")]));
    assert_eq!(doc.get("m").get("b"), &Node::Sequence(vec![s("x"), s("y")]));
    // A key without a value in a flow mapping is null.
    let m = load("{a, b: 1}");
    assert_eq!(m.get("a"), &Node::Null);
    assert_eq!(m.get("b"), &s("1"));
}

#[test]
fn flow_sequences_hold_single_pair_mappings() {
    assert_eq!(load("[a: b]"), Node::Sequence(vec![map(&[("a", s("b"))])]));
    assert_eq!(
        load("[: b, c]"),
        Node::Sequence(vec![map(&[("", s("b"))]), s("c")])
    );
}

#[test]
fn keys_quote_only_at_their_start_and_end_at_colon_blank() {
    // A quote in the middle of a key is part of the key.
    assert_eq!(load("a'b: c").get("a'b"), &s("c"));
    // A colon followed by a tab ends the key; a colon at the end leaves a null value.
    assert_eq!(load("a:\tv").get("a"), &s("v"));
    assert_eq!(load("a:").get("a"), &Node::Null);
    // A colon not followed by a blank is part of a plain scalar.
    assert_eq!(load("k: a:b").get("k"), &s("a:b"));
}

#[test]
fn dash_lines_in_every_form() {
    let doc = load("-\tx\n- - y\n  - z\n- -\n- a:\n    b: 1\n-\n- w\n");
    let v = doc.as_sequence().unwrap();
    assert_eq!(v[0], s("x"));
    assert_eq!(v[1], Node::Sequence(vec![s("y"), s("z")]));
    assert_eq!(v[2], Node::Sequence(vec![Node::Null]));
    assert_eq!(v[3], map(&[("a", map(&[("b", s("1"))]))]));
    // `-` followed by a dash line at the same indentation is a null item.
    assert_eq!(v[4], Node::Null);
    assert_eq!(v[5], s("w"));
    // A document that starts with a bare dash.
    assert_eq!(load("-\n"), Node::Sequence(vec![Node::Null]));
    // A key with an empty value followed by a dash line at the same indentation owns the
    // sequence; followed by another key it is null.
    let doc = load("a:\n- 1\n- 2\nb: 3\n");
    assert_eq!(doc.get("a"), &Node::Sequence(vec![s("1"), s("2")]));
    assert_eq!(doc.get("b"), &s("3"));
    let doc = load("a:\nb: 1\n");
    assert_eq!(doc.get("a"), &Node::Null);
}

#[test]
fn malformed_flow_collections() {
    assert!(
        error("{a: 1,").contains("end of map flow not found"),
        "{}",
        error("{a: 1,")
    );
    assert!(error("{a: 1 b: 2}").contains("end of map flow not found"));
    assert!(error("[a,").contains("end of sequence flow not found"));
    assert!(error("[\"a\" b]").contains("end of sequence flow not found"));
    // A plain item may hold blanks, and a colon after it makes a single-pair mapping.
    assert_eq!(
        load("[a b: c d]"),
        Node::Sequence(vec![map(&[("a b", s("c d"))])])
    );
    assert!(error("{[a]: b}").contains("illegal map value"));
    assert!(error("[[a]: b]").contains("illegal map value"));
    assert!(error("[a[b]]").contains("unknown token"));
    assert!(error("{a: b{c}}").contains("unknown token"));
    // Flow text followed by more text on the line.
    assert!(error("k: [1] x").contains("unknown token"));
    // A flow collection never closed, with no more indented line to continue it.
    assert!(error("k: [1,\nm: 2").contains("end of sequence flow not found"));
}

#[test]
fn malformed_scalars() {
    assert!(error("k: \"x\" y").contains("unknown token"));
    // A backslash that ends the text cannot escape anything.
    assert!(error("k: \"ab\\").contains("end of string not found"));
    assert!(error("k: \"a\\qb\"").contains("unknown escape character"));
}

#[test]
fn malformed_blocks() {
    // A tab used as indentation.
    let e = error("a:\n\tb: 1");
    assert!(e.contains("unknown token") && e.contains("line 2"), "{e}");
    // A scalar followed by a more indented line.
    assert!(error("x\n  y").contains("illegal map value"));
    // A line of a mapping that is not a key.
    let e = error("a: 1\nb");
    assert!(
        e.contains("end of map not found") && e.contains("line 2"),
        "{e}"
    );
    // A mapping followed by a dash line at its indentation: the mapping ends there and the
    // document has text left.
    assert!(error("a: 1\n- b").contains("end of map not found"));
    // A scalar document with a second line.
    assert!(error("a\nb").contains("end of map not found"));
}
