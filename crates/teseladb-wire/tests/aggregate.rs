//! `aggregate` (010 FR-004, FR-011) against what a `refrouter` 9.0.4 answered on 31-10-2026: each
//! line of `tests/aggregate/command.out` is a command, a tab and the reply (`=> ` and the reply
//! without the cluster's time fields) or the error (`!! `, the code, its name and the message).
//! The commands read collections `c` and `o` of database `xa`, with the documents of
//! `tests/aggregate/c.json` and `o.json`. An open cursor's identifier is random: only whether it
//! is 0 is compared.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, RawDoc, Value, extjson};

const C: &str = include_str!("aggregate/c.json");
const O: &str = include_str!("aggregate/o.json");
const DB: &str = "xa";

fn load(m: &mut Mem, coll: &str, text: &str) {
    let holder = extjson::parse(&format!(r#"{{"d":{text}}}"#)).expect("documents");
    let docs = holder.get("d").cloned().expect("d");
    let r = m.run(DB, Document::new().with("insert", coll).with("documents", docs));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

/// A reply with an open cursor's identifier replaced by 1.
fn normal(mut r: Document) -> Document {
    if let Some(Value::Document(c)) = r.get("cursor").cloned() {
        let mut c2 = Document::new();
        for (k, v) in c.iter() {
            let v = match (k, v) {
                ("id", Value::Int64(n)) if *n != 0 => Value::Int64(1),
                _ => v.clone(),
            };
            c2.push(k, v);
        }
        r.insert("cursor", c2);
    }
    let mut out = Document::new();
    for (k, v) in r.iter() {
        // RefDb's `ok` is the number 1; the types of `ok` are not part of the comparison.
        let v = if k == "ok" { Value::Double(1.0) } else { v.clone() };
        out.push(k, v);
    }
    out
}

fn ours(m: &mut Mem, cmd: &str) -> String {
    let c = extjson::parse(cmd).expect("command");
    // The dispatcher reads the generic arguments and refuses unknown fields first.
    let name = c.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default();
    if let Err(e) = teseladb_wire::generic::parse(&c, &name) {
        let r = e.to_document();
        return match (r.get("code"), r.get("codeName"), r.get("errmsg")) {
            (Some(Value::Int32(code)), Some(Value::String(name)), Some(Value::String(msg))) => {
                format!("!! {code}\t{name}\t{msg}")
            }
            _ => format!("=> {}", canonical(&r)),
        };
    }
    let r = m.run(DB, c);
    match (r.get("ok"), r.get("code"), r.get("codeName"), r.get("errmsg")) {
        (
            Some(Value::Double(x)),
            Some(Value::Int32(code)),
            Some(Value::String(name)),
            Some(Value::String(msg)),
        ) if *x == 0.0 => {
            format!("!! {code}\t{name}\t{msg}")
        }
        _ => format!("=> {}", canonical(&normal(r))),
    }
}

#[test]
fn aggregate_answers_as_refdb() {
    let mut m = Mem::new();
    load(&mut m, "c", C);
    load(&mut m, "o", O);
    let mut wrong = Vec::new();
    for line in include_str!("aggregate/command.out").lines().filter(|l| !l.is_empty()) {
        let (cmd, want) = line.split_once('\t').expect("a command and its reply");
        let got = ours(&mut m, cmd);
        let want = match want.strip_prefix("=> ") {
            Some(json) => {
                format!("=> {}", canonical(&normal(extjson::parse(json).expect("reply"))))
            }
            None => want.to_owned(),
        };
        if got != want {
            wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}

#[test]
fn get_more_continues_an_aggregate_cursor() {
    let mut m = Mem::new();
    load(&mut m, "c", C);
    let first = m.run(
        DB,
        extjson::parse(
            r#"{"aggregate":"c","pipeline":[{"$project":{"_id":1}}],"cursor":{"batchSize":3}}"#,
        )
        .expect("command"),
    );
    let Value::Int64(id) = field(&first, "cursor.id") else { panic!("{first:?}") };
    let Some(Value::Document(c)) = first.get("cursor") else { panic!("{first:?}") };
    assert_ne!(*id, 0);
    let mut seen = match c.get("firstBatch") {
        Some(Value::Array(a)) => a.len(),
        other => panic!("{other:?}"),
    };
    loop {
        let r = m.run(
            DB,
            doc(&[
                ("getMore", Value::Int64(*id)),
                ("collection", "c".into()),
                ("batchSize", 3.into()),
            ]),
        );
        let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
        seen += match c.get("nextBatch") {
            Some(Value::Array(a)) => a.len(),
            other => panic!("{other:?}"),
        };
        if c.get("id") == Some(&Value::Int64(0)) {
            break;
        }
    }
    assert_eq!(seen, 8);
}

/// The field names of a document, in order.
fn keys(d: &Document) -> Vec<String> {
    d.iter().map(|(k, _)| k.to_owned()).collect()
}

/// The first document of an `aggregate` reply.
fn first_doc(r: &Document) -> Document {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => match a.first() {
            Some(Value::Document(d)) => d.clone(),
            other => panic!("{other:?} in {r:?}"),
        },
        other => panic!("{other:?} in {r:?}"),
    }
}

/// `$collStats` against a `refrouter` 9.0.4 (`tests/aggregate/collstats.out`, 015 FR-010): the
/// errors as they are; a reply by its fields and the values TeselaDB shares with RefDb. The
/// sizes are TeselaDB's (DV-037), and there is no `shard`, `refengine`, `indexDetails` nor
/// `indexBuilds` (DV-038).
#[test]
fn coll_stats_has_refdb_shape() {
    let mut m = Mem::new();
    load(&mut m, "c", C);
    let skipped = ["shard", "refengine", "indexDetails", "indexBuilds"];
    for line in include_str!("aggregate/collstats.out").lines().filter(|l| !l.is_empty()) {
        let (cmd, want) = line.split_once('\t').expect("a command and its reply");
        if !cmd.starts_with("{\"aggregate\"") {
            let r = m.run(DB, extjson::parse(cmd).expect("command"));
            assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{cmd}: {r:?}");
            continue;
        }
        let got = ours(&mut m, cmd);
        let Some(json) = want.strip_prefix("=> ") else {
            assert_eq!(got, want, "{cmd}");
            continue;
        };
        let want_doc = first_doc(&extjson::parse(json).expect("reply"));
        let got_doc = first_doc(&m.run(DB, extjson::parse(cmd).expect("command")));
        let wanted: Vec<String> =
            keys(&want_doc).into_iter().filter(|k| !skipped.contains(&k.as_str())).collect();
        assert_eq!(keys(&got_doc), wanted, "{cmd}");
        for section in ["storageStats", "latencyStats", "queryExecStats"] {
            if let (Some(Value::Document(w)), Some(Value::Document(g))) =
                (want_doc.get(section), got_doc.get(section))
            {
                let wanted: Vec<String> =
                    keys(w).into_iter().filter(|k| !skipped.contains(&k.as_str())).collect();
                assert_eq!(keys(g), wanted, "{cmd}: {section}");
            }
        }
        for path in [
            "count",
            "storageStats.count",
            "storageStats.capped",
            "storageStats.max",
            "storageStats.nindexes",
            "storageStats.scaleFactor",
        ] {
            let (w, g) = (lookup(&want_doc, path), lookup(&got_doc, path));
            if w.is_some() {
                assert_eq!(g, w, "{cmd}: {path}");
            }
        }
        if let (Some(Value::Document(w)), Some(Value::Document(g))) = (
            lookup(&want_doc, "storageStats.indexSizes"),
            lookup(&got_doc, "storageStats.indexSizes"),
        ) {
            assert_eq!(keys(&g), keys(&w), "{cmd}: indexSizes");
        }
    }
}

/// The value at a dotted `path` of `d`.
fn lookup(d: &Document, path: &str) -> Option<Value> {
    let mut cur = Value::Document(d.clone());
    for seg in path.split('.') {
        cur = match cur {
            Value::Document(x) => x.get(seg).cloned()?,
            _ => return None,
        };
    }
    Some(cur)
}

/// `collStats` of a time series as a `refrouter` 9.0.4 shows it (015 FR-010): its index is on the
/// `metaField` and `timeField` (none without a `metaField`), and `avgObjSize` is the integer 0.
/// `storageStats.scale` as a number of any type: a double truncated, a decimal rounded half to
/// even (`scaleFactor` measured on a `refrouter` 9.0.4).
#[test]
fn coll_stats_scale_takes_every_numeric_type() {
    let mut m = Mem::new();
    load(&mut m, "c", C);
    for (scale, want) in [
        (r#"{"$numberDecimal":"2"}"#, 2),
        (r#"{"$numberDecimal":"2.5"}"#, 2),
        (r#"{"$numberDecimal":"1.5"}"#, 2),
        ("2.9", 2),
        (r#"{"$numberLong":"3"}"#, 3),
    ] {
        let cmd = format!(
            r#"{{"aggregate":"c","pipeline":[{{"$collStats":{{"storageStats":{{"scale":{scale}}}}}}},
                {{"$project":{{"_id":0,"f":"$storageStats.scaleFactor"}}}}],"cursor":{{}}}}"#
        );
        let r = m.run(DB, extjson::parse(&cmd).expect("command"));
        assert_eq!(first_doc(&r).get("f"), Some(&Value::Int32(want)), "{scale}: {r:?}");
    }
}

#[test]
fn coll_stats_of_a_time_series_shows_its_index() {
    let mut m = Mem::new();
    for cmd in [
        r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#,
        r#"{"create":"ts2","timeseries":{"timeField":"t"}}"#,
    ] {
        let r = m.run(DB, extjson::parse(cmd).expect("command"));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    }
    let with = m.run(DB, doc(&[("collStats", "ts".into())]));
    let Value::Document(sizes) = field(&with, "indexSizes") else { panic!("{with:?}") };
    assert_eq!(keys(sizes), vec!["m_1_t_1".to_owned()]);
    assert_eq!(field(&with, "nindexes"), &Value::Int32(1));
    assert_eq!(field(&with, "avgObjSize"), &Value::Int32(0));
    let without = m.run(DB, doc(&[("collStats", "ts2".into())]));
    let Value::Document(sizes) = field(&without, "indexSizes") else { panic!("{without:?}") };
    assert!(sizes.is_empty());
    assert_eq!(field(&without, "nindexes"), &Value::Int32(0));
}

/// A reply of a `refrouter` with the node in place of the shard: the documents counted by node and
/// the node an error came from, `node<N>` for `sh<N>` (D-102).
fn unsharded(want: &str) -> String {
    if let Some(json) = want.strip_prefix("=> ") {
        let mut r = extjson::parse(json).expect("reply");
        if let Some(Value::Document(s)) = r.get("shards").cloned() {
            let mut nodes = Document::new();
            for (k, v) in s.iter() {
                nodes.push(k.replacen("sh", "node", 1), v.clone());
            }
            r.insert("shards", nodes);
        }
        return format!("=> {}", canonical(&normal(r)));
    }
    want.replace("failed on: sh1 :: caused by :: ", "failed on: node1 :: caused by :: ")
}

/// Reads on views against a `refrouter` 9.0.4 (`tests/aggregate/views.out` and `views2.out`, 015
/// FR-001): `find`, `count`, `distinct` and the stages that read a view, with the options an
/// aggregation refuses and the view's collation.
#[test]
fn reads_on_views_answer_as_refdb() {
    check_files(&[include_str!("aggregate/views.out"), include_str!("aggregate/views2.out")]);
}

/// Collation in `aggregate` against a `refrouter` 9.0.4 (010 FR-014): the commands of
/// `tests/compat/scenarios/aggregation-collation.json` (`collation.out`), and how a `$group`
/// keeps values that compare equal in RefDb's slot-based engine and in its classic one
/// (`sbe.out`, `sbe2.out`, `mintie.out`).
#[test]
fn collation_answers_as_refdb() {
    check_files(&[
        include_str!("aggregate/collation.out"),
        include_str!("aggregate/sbe.out"),
        include_str!("aggregate/sbe2.out"),
        include_str!("aggregate/mintie.out"),
    ]);
}

/// Constants that fail in a filter or a pipeline fail the command before any document is read,
/// as RefDb optimises its expressions when it parses them (`optimize.out`, 010 FR-005).
#[test]
fn failing_constants_answer_as_refdb() {
    check_files(&[include_str!("aggregate/optimize.out")]);
}

/// `$out` and `$merge` against a `refrouter` 9.0.4 (`out_merge.out`, 010 FR-012, T029): the
/// collection `$out` replaces or creates, `$merge` with each of its modes and its pipeline, and
/// the errors of their arguments and places.
#[test]
fn out_and_merge_answer_as_refdb() {
    check_files(&[include_str!("aggregate/out_merge.out")]);
}

/// Updates with a pipeline in `update` and `findAndModify` against a `refrouter` 9.0.4
/// (`update_pipeline.out`, 010 FR-013, T030): `$$NOW`, `$$CLUSTER_TIME`, `$$USER_ROLES`, `let`
/// and `c`, an upsert, `$replaceWith`, the stages an update refuses and an undefined variable.
#[test]
fn pipeline_updates_answer_as_refdb() {
    check_files(&[include_str!("aggregate/update_pipeline.out")]);
}

/// Runs each file's commands on a node of its own over `c` and `o`, against the replies of a
/// `refrouter` (without what only a sharded cluster gives).
fn check_files(files: &[&str]) {
    for file in files {
        let mut m = Mem::new();
        load(&mut m, "c", C);
        load(&mut m, "o", O);
        let mut wrong = Vec::new();
        for line in file.lines().filter(|l| !l.is_empty()) {
            let (cmd, want) = line.split_once('\t').expect("a command and its reply");
            // `listCollections` shows the collections' `uuid`, which differ (015 T005 tests it).
            if cmd.starts_with("{\"listCollections\"") {
                continue;
            }
            let got = ours(&mut m, cmd);
            let want = unsharded(want);
            if got != want {
                wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
            }
        }
        assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
    }
}
