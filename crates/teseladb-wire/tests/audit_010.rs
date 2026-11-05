//! Adversarial cases of the 010 audit (T038, third pass), each measured on a `refnode` 9.0.4 on
//! 02-11-2026: documents past 16 MiB inside and at the end of a pipeline, pipelines nested in
//! `$lookup` and `$facet`, and a `$group` that writes to disk under a collation.

mod common;

use std::sync::Arc;

use common::{Mem, doc, field};
use teseladb_exec::pipeline::spill::MemorySpill;
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "advx";

fn cmd(text: &str) -> Document {
    extjson::parse(text).expect("a command")
}

/// A node with five documents of 4 MiB in `b` and five short strings in `c`.
fn node() -> Mem {
    let mut m = Mem::new();
    let big = "x".repeat(4 << 20);
    for i in 0..5 {
        let r =
            m.run(DB, cmd(&format!(r#"{{"insert":"b","documents":[{{"_id":{i},"s":"{big}"}}]}}"#)));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let docs = r#"[{"_id":1,"s":"b"},{"_id":2,"s":"B"},{"_id":3,"s":"a"},{"_id":4,"s":"A"},{"_id":5,"s":"b"}]"#;
    let r = m.run(DB, cmd(&format!(r#"{{"insert":"c","documents":{docs}}}"#)));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    m
}

fn batch(r: &Document) -> Vec<Document> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn too_large(r: &Document, size: i64) {
    assert_eq!(field(r, "code"), &Value::Int32(10334), "{r:?}");
    let tail =
        format!("Serializing Document failed :: caused by :: Size {size} exceeds maximum 16793600");
    assert!(matches!(field(r, "errmsg"), Value::String(s) if s.ends_with(&tail)), "{r:?}");
}

#[test]
fn a_facet_or_lookup_result_past_16_mib_fails() {
    let mut m = node();
    let r = m.run(
        DB,
        cmd(
            r#"{"aggregate":"b","pipeline":[{"$facet":{"x":[{"$project":{"s":1}}]}}],"cursor":{}}"#,
        ),
    );
    too_large(&r, 20_971_658);
    let r = m.run(
        DB,
        cmd(r#"{"aggregate":"b","pipeline":[{"$limit":1},{"$lookup":{"from":"b","pipeline":[],"as":"j"}}],"cursor":{}}"#),
    );
    too_large(&r, 25_165_979);
}

#[test]
fn large_values_inside_a_pipeline_pass_when_its_output_is_small() {
    let mut m = node();
    let r = m.run(
        DB,
        cmd(r#"{"aggregate":"b","pipeline":[{"$limit":1},{"$lookup":{"from":"b","pipeline":[],"as":"j"}},{"$unwind":"$j"},{"$project":{"_id":1}}],"cursor":{}}"#),
    );
    assert_eq!(batch(&r).len(), 5, "{r:?}");
    for p in [
        r#"[{"$group":{"_id":null,"p":{"$push":"$s"}}},{"$project":{"n":{"$size":"$p"}}}]"#,
        r#"[{"$group":{"_id":null,"p":{"$addToSet":{"$concat":["$s",{"$toString":"$_id"}]}}}},{"$project":{"n":{"$size":"$p"}}}]"#,
        r#"[{"$group":{"_id":"$_id","p":{"$push":"$s"}}},{"$group":{"_id":null,"p":{"$push":"$p"}}},{"$project":{"n":{"$size":"$p"}}}]"#,
    ] {
        let r = m.run(DB, cmd(&format!(r#"{{"aggregate":"b","pipeline":{p},"cursor":{{}}}}"#)));
        let b = batch(&r);
        assert_eq!(b.len(), 1, "{p}: {r:?}");
        assert_eq!(b[0].get("n"), Some(&Value::Int32(5)), "{p}: {r:?}");
    }
}

#[test]
fn pipelines_nest_in_lookup_and_facet() {
    let mut m = node();
    let r = m.run(
        DB,
        cmd(r#"{"aggregate":"c","pipeline":[{"$limit":1},{"$lookup":{"from":"c","pipeline":[{"$facet":{"n":[{"$lookup":{"from":"c","pipeline":[{"$count":"k"}],"as":"z"}},{"$count":"m"}]}}],"as":"j"}}],"cursor":{}}"#),
    );
    let b = batch(&r);
    assert_eq!(b.len(), 1, "{r:?}");
    let counted = Value::Document(doc(&[("m", Value::Int32(5))]));
    let j = Value::Array(vec![Value::Document(doc(&[("n", Value::Array(vec![counted]))]))]);
    assert_eq!(b[0].get("j"), Some(&j), "{r:?}");
}

#[test]
fn a_group_on_disk_keeps_the_first_key_of_equal_strings_in_its_collation() {
    let store = Arc::new(MemorySpill::default());
    let mut m = node();
    {
        let mut g = m.shared.lock().unwrap();
        g.spill.store = Some(store.clone());
        g.spill.stage_bytes = Some(10);
    }
    let text = r#"{"aggregate":"c","pipeline":[{"$group":{"_id":"$s","ids":{"$push":"$_id"}}}],"cursor":{},"allowDiskUse":ADU,"collation":{"locale":"en","strength":2}}"#;
    let r = m.run(DB, cmd(&text.replace("ADU", "true")));
    // The node reads `c` in its natural order, the order of its keys: 1, 2, 5, 4, 3 (DV-060),
    // so `A` (4) comes before `a` (3), and each group keeps the key of its first document.
    let groups: Vec<(Value, Value)> = batch(&r)
        .iter()
        .map(|d| {
            let key = d.get("_id").cloned().unwrap_or(Value::Null);
            (key, d.get("ids").cloned().unwrap_or(Value::Null))
        })
        .collect();
    let ids = |list: &[i32]| Value::Array(list.iter().map(|n| Value::Int32(*n)).collect());
    assert_eq!(
        groups,
        [(Value::from("A"), ids(&[4, 3])), (Value::from("b"), ids(&[1, 2, 5]))],
        "{r:?}"
    );
    assert!(store.written() > 0);
    let r = m.run(DB, cmd(&text.replace("ADU", "false")));
    assert_eq!(field(&r, "code"), &Value::Int32(292), "{r:?}");
}
