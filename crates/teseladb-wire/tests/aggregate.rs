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

/// The bytes of a size field, whatever its numeric type.
fn bytes_of(v: &Value) -> u64 {
    match v {
        Value::Int32(n) => u64::try_from(*n).expect("a size"),
        Value::Int64(n) => u64::try_from(*n).expect("a size"),
        Value::Double(x) => *x as u64,
        other => panic!("{other:?} is no size"),
    }
}

/// Checks the index sizes of `stats` (a `collStats` reply or `storageStats`) against RefDb's
/// `want`: the same indexes in the same order, each in whole 4 KiB blocks, and totals that add up.
/// The sizes themselves are TeselaDB's (DV-037).
fn check_index_sizes(got: &Document, want: &Document, what: &str) {
    let (Value::Document(g), Value::Document(w)) =
        (field(got, "indexSizes"), field(want, "indexSizes"))
    else {
        panic!("{what}: {got:?}")
    };
    assert_eq!(keys(g), keys(w), "{what}: indexSizes");
    assert_eq!(field(got, "nindexes"), field(want, "nindexes"), "{what}: nindexes");
    let sizes: Vec<u64> = g.iter().map(|(_, v)| bytes_of(v)).collect();
    assert!(sizes.iter().all(|s| *s >= 4096 && s % 4096 == 0), "{what}: {sizes:?}");
    assert_eq!(bytes_of(field(got, "totalIndexSize")), sizes.iter().sum::<u64>(), "{what}");
    assert_eq!(
        bytes_of(field(got, "totalSize")),
        bytes_of(field(got, "storageSize")) + bytes_of(field(got, "totalIndexSize")),
        "{what}"
    );
}

/// `collStats`, `$collStats` and `dbStats` count and measure every index (010 T035, FR-018),
/// against a `refrouter` 9.0.4 (`tests/aggregate/collstats_indexes.out`, without `refengine` and
/// `indexDetails`, DV-038).
#[test]
fn coll_stats_measure_every_index() {
    let mut m = Mem::new();
    load(&mut m, "c", C);
    load(&mut m, "o", O);
    for line in include_str!("aggregate/collstats_indexes.out").lines().filter(|l| !l.is_empty()) {
        let (cmd, want) = line.split_once('\t').expect("a command and its reply");
        let want = extjson::parse(want.strip_prefix("=> ").expect("a reply")).expect("reply");
        let got = m.run(DB, extjson::parse(cmd).expect("command"));
        assert_eq!(got.get("ok"), Some(&Value::Double(1.0)), "{cmd}: {got:?}");
        if cmd.starts_with("{\"collStats\"") {
            check_index_sizes(&got, &want, cmd);
        } else if cmd.starts_with("{\"aggregate\"") {
            let (got_first, want_first) = (first_doc(&got), first_doc(&want));
            let (Value::Document(g), Value::Document(w)) =
                (field(&got_first, "storageStats"), field(&want_first, "storageStats"))
            else {
                panic!("{cmd}: {got:?}")
            };
            check_index_sizes(g, w, cmd);
        } else if cmd.starts_with("{\"dbStats\"") {
            assert_eq!(field(&got, "indexes"), field(&want, "indexes"), "{cmd}");
            let c = m.run(DB, doc(&[("collStats", "c".into())]));
            let o = m.run(DB, doc(&[("collStats", "o".into())]));
            let total =
                bytes_of(field(&c, "totalIndexSize")) + bytes_of(field(&o, "totalIndexSize"));
            assert_eq!(bytes_of(field(&got, "indexSize")), total, "{cmd}");
        }
    }
    // The sizes are the stored keys: more documents make a secondary index bigger.
    let before = m.run(DB, doc(&[("collStats", "c".into())]));
    let many: Vec<Value> = (100..2100)
        .map(|i| Value::Document(Document::new().with("_id", i).with("g", format!("value-{i:06}"))))
        .collect();
    let r = m.run(DB, Document::new().with("insert", "c").with("documents", Value::Array(many)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let after = m.run(DB, doc(&[("collStats", "c".into())]));
    let size = |r: &Document, name: &str| bytes_of(field(r, &format!("indexSizes.{name}")));
    assert!(size(&after, "g_1") > size(&before, "g_1"), "{before:?}\n{after:?}");
    assert!(size(&after, "_id_") > size(&before, "_id_"), "{before:?}\n{after:?}");
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
        // A shard's part of an index command's `raw` is the node's (D-102).
        let json = json.replace("sh1/lc12-sh1:27018", "node1/:0");
        let mut r = extjson::parse(&json).expect("reply");
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

/// `$listLocalSessions` and `$listSessions` (`tests/aggregate/sessions.out`, 010 T010): their
/// arguments, the namespaces they run on and the users they list, as a `refrouter` 9.0.4 answers;
/// a line `{db, cmd}` runs `cmd` on database `db`.
#[test]
fn session_stages_answer_as_refdb() {
    let mut m = Mem::new();
    let mut wrong = Vec::new();
    // And `$currentOp` (T033), on `admin` only.
    let lines = include_str!("aggregate/sessions.out")
        .lines()
        .chain(include_str!("aggregate/current_op.out").lines());
    for line in lines.filter(|l| !l.is_empty()) {
        let (cmd, want) = line.split_once('\t').expect("a command and its reply");
        let spec = extjson::parse(cmd).expect("command");
        let got = match (spec.get("db"), spec.get("cmd")) {
            (Some(Value::String(db)), Some(Value::Document(c))) => {
                let r = m.run(db, c.clone());
                match (r.get("code"), r.get("codeName"), r.get("errmsg")) {
                    (Some(Value::Int32(code)), Some(Value::String(n)), Some(Value::String(e))) => {
                        format!("!! {code}\t{n}\t{e}")
                    }
                    _ => format!("=> {}", canonical(&normal(r))),
                }
            }
            _ => ours(&mut m, cmd),
        };
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

/// `$listLocalSessions` lists the sessions the node holds, each as its record of
/// `config.system.sessions` writes it, for every user, the command's or none of a list.
#[test]
fn local_sessions_list_what_the_node_holds() {
    let mut m = Mem::new();
    for _ in 0..2 {
        let r = m.run("admin", doc(&[("startSession", Value::Int32(1))]));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let count = |m: &mut Mem, spec: &str| -> Value {
        let cmd = extjson::parse(&format!(
            r#"{{"aggregate":1,"pipeline":[{{"$listLocalSessions":{spec}}},{{"$count":"n"}}],"cursor":{{}}}}"#
        ))
        .expect("command");
        let r = m.run(DB, cmd);
        let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
        match c.get("firstBatch") {
            Some(Value::Array(b)) if b.is_empty() => Value::Int32(0),
            Some(Value::Array(b)) => match &b[0] {
                Value::Document(d) => d.get("n").cloned().unwrap_or(Value::Null),
                other => other.clone(),
            },
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(count(&mut m, "{}"), Value::Int32(2));
    assert_eq!(count(&mut m, r#"{"allUsers":true}"#), Value::Int32(2));
    assert_eq!(count(&mut m, r#"{"users":[]}"#), Value::Int32(2));
    assert_eq!(count(&mut m, r#"{"users":[{"user":"a","db":"b"}]}"#), Value::Int32(0));
    let cmd = extjson::parse(
        r#"{"aggregate":1,"pipeline":[{"$listLocalSessions":{}},{"$limit":1}],"cursor":{}}"#,
    )
    .expect("command");
    let r = m.run(DB, cmd);
    let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
    let Some(Value::Array(b)) = c.get("firstBatch") else { panic!("{r:?}") };
    let Some(Value::Document(first)) = b.first() else { panic!("{r:?}") };
    assert_eq!(first.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["_id", "lastUse"]);
    let Some(Value::Document(id)) = first.get("_id") else { panic!("{first:?}") };
    assert_eq!(id.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["id", "uid"]);
}

/// `$geoNear` (`tests/aggregate/geo_near*.out`, 010 T010): the index it reads, its distances on
/// the sphere and the plane, its options and its errors, as a `refrouter` 9.0.4 answers; an error's
/// line breaks are written `\n`.
#[test]
fn geo_near_answers_as_refdb() {
    for file in [include_str!("aggregate/geo_near.out"), include_str!("aggregate/geo_near2.out")] {
        let mut m = Mem::new();
        let mut wrong = Vec::new();
        for line in file.lines().filter(|l| !l.is_empty()) {
            let (cmd, want) = line.split_once('\t').expect("a command and its reply");
            let got = ours(&mut m, cmd).replace('\n', "\\n");
            // A `refrouter` names its shard where the node names itself.
            let want = unsharded(want).replace("sh1/lc12-sh1:27018", "node1/:0");
            if got != want {
                wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
            }
        }
        assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
    }
}

/// `$listClusterCatalog` (`tests/aggregate/cluster_catalog.out`, 010 T046): the collections and
/// views of the database with their namespace, options, UUID and `_id` index, none sharded, its
/// flags and its errors, as a `refrouter` 9.0.4 answers; the UUIDs are the node's own.
#[test]
fn cluster_catalog_answers_as_refdb() {
    let mut m = Mem::new();
    load(&mut m, "c", C);
    load(&mut m, "o", O);
    let uuids = |text: &str| -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(i) = rest.find("\"uuid\":{\"$binary\":{\"base64\":\"") {
            let start = i + "\"uuid\":{\"$binary\":{\"base64\":\"".len();
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            let end = rest.find('"').unwrap_or(0);
            out.push_str("UUID");
            rest = &rest[end..];
        }
        out.push_str(rest);
        out.replace("\"sh1\"", "\"node1\"")
    };
    let mut wrong = Vec::new();
    for line in include_str!("aggregate/cluster_catalog.out").lines().filter(|l| !l.is_empty()) {
        let (cmd, want) = line.split_once('\t').expect("a command and its reply");
        let got = uuids(&ours(&mut m, cmd));
        let want = match want.strip_prefix("=> ") {
            Some(json) => {
                format!("=> {}", canonical(&normal(extjson::parse(json).expect("reply"))))
            }
            None => want.to_owned(),
        };
        let want = uuids(&want);
        if got != want {
            wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}

/// Sorts by array fields (`tests/aggregate/array_sort.out`, 010 T045, FR-025): ascending by an
/// array's least element and descending by its greatest, the empty array before `null` and a
/// missing field, with and without a multikey index, as a `refrouter` 9.0.4 sorts.
#[test]
fn array_sorts_answer_as_refdb() {
    check_files(&[include_str!("aggregate/array_sort.out")]);
}

/// `{$meta: "recordId"}` and `showRecordId` (DV-047): a `refrouter` 9.0.4 gives each document the
/// 64-bit identifier of its RefEngine record (measured 01-11-2026: 1 to 8 in insertion order, in
/// `find` and `aggregate`, and `$recordId` with `showRecordId`). A TeselaDB document has no record
/// identifier, only its key in the range, so `find`, its projection and every stage leave the
/// field out, the same in each.
#[test]
fn record_ids_are_left_out_everywhere() {
    let mut m = Mem::new();
    load(&mut m, "c", C);
    for cmd in [
        r#"{"aggregate":"c","pipeline":[{"$project":{"r":{"$meta":"recordId"}}}],"cursor":{}}"#,
        r#"{"aggregate":"c","pipeline":[{"$addFields":{"r":{"$meta":"recordId"}}}],"cursor":{}}"#,
        r#"{"find":"c","projection":{"r":{"$meta":"recordId"}}}"#,
        r#"{"find":"c","projection":{"_id":1,"r":{"$meta":"recordId"}}}"#,
        r#"{"find":"c","showRecordId":true}"#,
    ] {
        let r = m.run(DB, extjson::parse(cmd).expect("command"));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{cmd}: {r:?}");
        let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{cmd}: {r:?}") };
        assert_eq!(batch.len(), 8, "{cmd}: {r:?}");
        for d in batch {
            let Value::Document(d) = d else { panic!("{d:?}") };
            assert!(d.get("r").is_none() && d.get("$recordId").is_none(), "{cmd}: {d:?}");
        }
    }
}

/// Metadata no stage gives, read first in a pipeline (`tests/aggregate/meta_sort.out`, measured
/// on a `refrouter` 9.0.4 on 01-11-2026): a `$sort` that only `$match` and `$sort` precede fails
/// with 40218 behind the words of the query system's `DISTINCT_SCAN` check, any other stage
/// without them, and the pipeline of a `$lookup` or `$unionWith` when it runs (an executor error,
/// none for a `$lookup` without input).
#[test]
fn missing_metadata_first_answers_as_refdb() {
    check_files(&[include_str!("aggregate/meta_sort.out")]);
}

/// `{$meta: "indexKey"}` (`tests/aggregate/index_key.out`, measured on a `refrouter` 9.0.4 on
/// 01-11-2026): the key of the index entry each document was read by, field by field as the key
/// pattern names them, through `aggregate` and `find`; the first entry of a multikey document in
/// the scan's order, `{_id: v}` through the index of `_id`, and nothing after a collection scan
/// or a read of one `_id`.
#[test]
fn index_key_metadata_answers_as_refdb() {
    check_files(&[include_str!("aggregate/index_key.out")]);
}

/// `$indexStats` (`tests/aggregate/index_stats.out`, measured on a `refrouter` 9.0.4 on
/// 01-11-2026): each index of the collection with its key, specification, host, shard and how
/// many reads its plans scanned it (`accesses.ops`, since `accesses.since`); an empty object as
/// its only argument, first in the pipeline, on a collection.
#[test]
fn index_stats_answer_as_refdb() {
    check_files(&[include_str!("aggregate/index_stats.out")]);
}

/// `validate` against a `refrouter` 9.0.4 (`tests/validate/*.out`, 010 FR-017, T034): the reply
/// with the shard's under `raw`, each index's entries and details, a multikey and a sparse
/// index, and the errors of its arguments. The collection's `uuid` and the shard's cluster times
/// differ by nature and are left out of the comparison.
#[test]
fn validate_answers_as_refdb() {
    for file in [include_str!("validate/basic.out"), include_str!("validate/options.out")] {
        let mut m = Mem::new();
        load(&mut m, "c", C);
        load(&mut m, "o", O);
        let mut wrong = Vec::new();
        for line in file.lines().filter(|l| !l.is_empty()) {
            let (cmd, want) = line.split_once('\t').expect("a command and its reply");
            let got = comparable(&ours(&mut m, cmd));
            let want = comparable(&unsharded(want));
            if got != want {
                wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
            }
        }
        assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
    }
}

/// A `validate` reply without what differs by nature: the `uuid`, and in the shard's part under
/// `raw`, named by the shard alone, the cluster's times.
fn comparable(reply: &str) -> String {
    let Some(json) = reply.strip_prefix("=> ") else { return reply.to_owned() };
    let mut r = extjson::parse(json).expect("reply");
    r.remove("uuid");
    if let Some(Value::Document(raw)) = r.get("raw").cloned() {
        let mut shards = Document::new();
        for (k, v) in raw.iter() {
            let Value::Document(mut part) = v.clone() else { continue };
            for f in ["uuid", "$clusterTime", "$configTime", "$topologyTime", "operationTime"] {
                part.remove(f);
            }
            shards.push(k.replacen("sh1", "node1", 1), normal(part));
        }
        r.insert("raw", shards);
    }
    format!("=> {}", canonical(&r))
}

/// `validate` of an index that lost an entry and kept one of a document gone, as a `refnode`
/// 9.0.4 reports them (measured 01-11-2026 with the fail points `skipIndexNewRecords` and
/// `skipUnindexingDocumentWhenDeleted`): the entries under `missingIndexEntries` and
/// `extraIndexEntries`, their counts in `warnings`, the index in `errors` and its details, and
/// the advice. TeselaDB has no record ids (DV-075).
#[test]
fn validate_reports_missing_and_extra_entries() {
    let mut m = Mem::new();
    load(&mut m, "v", r#"[{"_id":1,"a":1},{"_id":2,"a":2}]"#);
    let before: Vec<Vec<u8>> = m.data.keys().cloned().collect();
    ours(&mut m, r#"{"createIndexes":"v","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    let entries: Vec<Vec<u8>> = m.data.keys().filter(|k| !before.contains(k)).cloned().collect();
    // The entry of `a: 2`, the last of the index, goes; so does the document `_id: 1`.
    let last = entries.iter().filter(|k| entries.iter().all(|o| o.len() <= k.len())).max();
    m.data.remove(last.expect("an entry"));
    let one = Document::new().with("_id", 1).with("a", 1).to_bytes().expect("small");
    let doc_key = m.data.iter().find(|(_, v)| **v == one).map(|(k, _)| k.clone());
    m.data.remove(&doc_key.expect("the document"));
    let got = ours(&mut m, r#"{"validate":"v"}"#);
    let r = extjson::parse(got.strip_prefix("=> ").expect("a reply")).expect("json");
    let shown = |k: &str| canonical(&Document::new().with(k, r.get(k).cloned().expect(k)));
    assert_eq!(shown("valid"), r#"{"valid":false}"#);
    assert_eq!(
        shown("warnings"),
        r#"{"warnings":["Detected 1 missing index entries.","Detected 1 extra index entries."]}"#
    );
    assert_eq!(shown("errors"), r#"{"errors":["Found errors in a_1"]}"#);
    assert_eq!(
        shown("extraIndexEntries"),
        r#"{"extraIndexEntries":[{"indexName":"a_1","indexKey":{"a":{"$numberInt":"1"}}}]}"#
    );
    assert_eq!(
        shown("missingIndexEntries"),
        concat!(
            r#"{"missingIndexEntries":[{"indexName":"a_1","indexKey":{"a":{"$numberInt":"2"}},"#,
            r#""idKey":{"_id":{"$numberInt":"2"}}}]}"#
        )
    );
    assert_eq!(
        shown("keysPerIndex"),
        r#"{"keysPerIndex":{"_id_":{"$numberInt":"1"},"a_1":{"$numberInt":"1"}}}"#
    );
    assert_eq!(
        shown("indexDetails"),
        concat!(
            r#"{"indexDetails":{"_id_":{"valid":true,"spec":{"v":{"$numberInt":"2"},"#,
            r#""key":{"_id":{"$numberInt":"1"}},"name":"_id_"},"isMultikey":false},"#,
            r#""a_1":{"valid":false,"spec":{"v":{"$numberInt":"2"},"key":{"a":{"$numberInt":"1"}},"#,
            r#""name":"a_1"},"isMultikey":false,"#,
            r#""errors":["Index with name 'a_1' has inconsistencies."]}}}"#
        )
    );
    assert!(shown("advice").contains("A corrupt namespace has been detected."));
}
