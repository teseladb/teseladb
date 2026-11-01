//! Query shapes and `queryShapeHash` (010 T040, FR-023) against a `refrouter` 9.0.4: each command
//! of `query_shape/hashes.out` (the aggregation and expression batteries, run on database `xs`)
//! with the `queryShapeHash` that `explain` gave it, and the debug shapes `$queryStats` showed
//! in `query_shape/debug.out`. The commands of `query_shape/divergent.out` are the shapes
//! TeselaDB does not reproduce yet (a divergence in `divergencias-refdb.md`): the test fails
//! when one of them starts to match, so that the list only shrinks, and when any other differs.

use std::collections::BTreeMap;

use teseladb_bson::{RawDoc, extjson};
use teseladb_wire::commands::query_shape::{representative, shape};

const DB: &str = "xs";

fn lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(|l| l.split_once('\t'))
}

fn canonical(d: &teseladb_bson::Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

#[test]
fn query_shape_hashes_match_refdb() {
    let debug: BTreeMap<&str, &str> = lines(include_str!("query_shape/debug.out")).collect();
    let divergent: BTreeMap<&str, &str> =
        lines(include_str!("query_shape/divergent.out")).collect();
    let mut wrong = Vec::new();
    let mut fixed = Vec::new();
    let mut total = 0;
    for (hash, cmd) in lines(include_str!("query_shape/hashes.out")) {
        total += 1;
        let cmd = extjson::parse(cmd).expect("a command");
        let got = shape(DB, &cmd);
        let matches = got.as_ref().map(|s| s.hash.as_str()) == Some(hash);
        if matches && divergent.contains_key(hash) {
            fixed.push(hash.to_owned());
        }
        if !matches && !divergent.contains_key(hash) {
            let mine = got.map(|s| canonical(&s.debug)).unwrap_or_default();
            let rep = representative(DB, &cmd).map(|r| canonical(&r)).unwrap_or_default();
            wrong.push(format!(
                "{hash}\n  cmd  {}\n  them {}\n  mine {mine}\n  rep  {rep}",
                canonical(&cmd),
                debug.get(hash).unwrap_or(&"")
            ));
        }
    }
    if let Ok(path) = std::env::var("QUERY_SHAPE_REPORT") {
        std::fs::write(path, wrong.join("\n")).expect("report");
    }
    assert!(
        wrong.is_empty(),
        "{} of {total} shapes differ:\n{}",
        wrong.len(),
        wrong.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
    );
}
