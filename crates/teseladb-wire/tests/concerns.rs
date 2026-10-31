//! `readConcern`, `writeConcern` and the generic arguments of a command (FR-019, FR-021), with
//! the codes and messages a `refrouter` 9.0.4 gives (measured).

use teseladb_bson::{Decimal128, Document, Value};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::read::ReadConcern;
use teseladb_wire::concerns::{
    Level, ReadConcernArgs, Sync, WArg, WriteConcernArgs, canonical, check_support,
    default_write_concern, write_concern_error,
};
use teseladb_wire::error::CommandError;
use teseladb_wire::generic::{check_unknown_fields, is_generic, parse};

fn d(fields: &[(&str, Value)]) -> Document {
    let mut doc = Document::new();
    for (k, v) in fields {
        doc.push(*k, v.clone());
    }
    doc
}

fn ts(time: u32, increment: u32) -> Value {
    Value::Timestamp { time, increment }
}

fn find_with(field: &str, v: Value) -> Document {
    d(&[("find", "c".into()), (field, v)])
}

fn err(r: Result<impl std::fmt::Debug, CommandError>) -> (i32, String) {
    let e = r.expect_err("an error");
    (e.code.code, e.errmsg)
}

fn rc_err(rc: Value) -> (i32, String) {
    err(parse(&find_with("readConcern", rc), "find"))
}

fn wc_err(wc: Value) -> (i32, String) {
    let cmd =
        d(&[("insert", "c".into()), ("documents", Value::Array(vec![])), ("writeConcern", wc)]);
    err(parse(&cmd, "insert"))
}

fn wc_of(wc: Value) -> WriteConcernArgs {
    let cmd = d(&[("insert", "c".into()), ("writeConcern", wc)]);
    WriteConcernArgs::parse(&cmd, "insert").expect("valid").expect("given")
}

#[test]
fn read_concern_fields_and_their_errors() {
    let level = |l: &str| Value::Document(d(&[("level", l.into())]));
    for (l, want) in [("local", Level::Local), ("available", Level::Available)] {
        let g = parse(&find_with("readConcern", level(l)), "find").expect("valid");
        assert_eq!(g.read.and_then(|r| r.level), Some(want));
    }
    assert_eq!(
        rc_err(level("bogus")),
        (2, "Enumeration value 'bogus' for field 'readConcern.level' is not a valid value.".into())
    );
    let cases: Vec<(Document, i32, &str)> = vec![
        (
            d(&[("level", 5.into())]),
            14,
            "BSON field 'readConcern.level' is the wrong type 'int', expected type 'string'",
        ),
        (
            d(&[("level", "local".into()), ("foo", 1.into())]),
            40415,
            "BSON field 'readConcern.foo' is an unknown field.",
        ),
        (
            d(&[("afterClusterTime", 5.into())]),
            14,
            "\"afterClusterTime\" had the wrong type. Expected 17, found int",
        ),
        (
            d(&[("afterClusterTime", Value::Null)]),
            14,
            "\"afterClusterTime\" had the wrong type. Expected 17, found null",
        ),
        (
            d(&[("level", "snapshot".into()), ("atClusterTime", 5.into())]),
            14,
            "\"atClusterTime\" had the wrong type. Expected 17, found int",
        ),
        (d(&[("afterClusterTime", ts(0, 0))]), 72, "afterClusterTime cannot be a null timestamp"),
        (
            d(&[("level", "snapshot".into()), ("atClusterTime", Value::Null)]),
            14,
            "\"atClusterTime\" had the wrong type. Expected 17, found null",
        ),
        (d(&[("foo", true.into())]), 40415, "BSON field 'readConcern.foo' is an unknown field."),
        (
            d(&[("level", "snapshot".into()), ("atClusterTime", ts(0, 0))]),
            72,
            "atClusterTime cannot be a null timestamp",
        ),
        (
            d(&[("level", "local".into()), ("atClusterTime", ts(1, 1))]),
            72,
            "atClusterTime field can be set only if level is equal to snapshot",
        ),
        (
            d(&[("level", "available".into()), ("afterClusterTime", ts(1, 1))]),
            72,
            "afterClusterTime field can be set only if level is equal to majority, local, or snapshot",
        ),
        (
            d(&[("level", "linearizable".into()), ("afterClusterTime", ts(1, 1))]),
            72,
            "afterClusterTime field can be set only if level is equal to majority, local, or snapshot",
        ),
        (
            d(&[
                ("level", "snapshot".into()),
                ("afterClusterTime", ts(1, 1)),
                ("atClusterTime", ts(1, 1)),
            ]),
            72,
            "Specifying a timestamp for readConcern snapshot in a causally consistent session is not allowed. See https://docs.refdb.com/manual/core/read-isolation-consistency-recency/#causal-consistency",
        ),
        (
            d(&[
                ("afterClusterTime", ts(1, 1)),
                ("afterOpTime", Value::Document(d(&[("ts", ts(1, 1)), ("t", 1i64.into())]))),
            ]),
            72,
            "Can not specify both afterClusterTime and afterOpTime",
        ),
        (
            d(&[("afterOpTime", 5.into())]),
            10065,
            "invalid parameter: expected an object (afterOpTime)",
        ),
        (
            d(&[("afterOpTime", Value::Document(d(&[("x", 1.into())])))]),
            40414,
            "BSON field 'OpTimeBase.ts' is missing but a required field",
        ),
        (
            d(&[("provenance", "bogus".into())]),
            2,
            "Enumeration value 'bogus' for field 'readConcern.provenance' is not a valid value.",
        ),
        (
            d(&[("provenance", 5.into())]),
            14,
            "BSON field 'readConcern.provenance' is the wrong type 'int', expected type 'string'",
        ),
        (
            d(&[("provenance", "implicitDefault".into())]),
            72,
            "readConcern provenance must be unset or \"clientSupplied\"",
        ),
        (
            d(&[("$_allowTransactionTableSnapshot", 1.into())]),
            14,
            "BSON field 'readConcern.$_allowTransactionTableSnapshot' is the wrong type 'int', expected type 'bool'",
        ),
    ];
    for (rc, code, msg) in cases {
        assert_eq!(rc_err(Value::Document(rc.clone())), (code, msg.to_owned()), "{rc:?}");
    }
    assert_eq!(
        rc_err(5.into()),
        (
            14,
            "BSON field 'find.readConcern' is the wrong type 'int', expected type 'object'".into()
        )
    );
    // `null` values and the client's own provenance are as if absent; the internal flags pass.
    for rc in [
        Value::Null,
        Value::Document(d(&[("level", Value::Null), ("provenance", "clientSupplied".into())])),
        Value::Document(d(&[("$_waitLastStableRecoveryTimestamp", true.into())])),
        Value::Document(d(&[("afterClusterTime", ts(1, 1))])),
    ] {
        assert!(parse(&find_with("readConcern", rc.clone()), "find").is_ok(), "{rc:?}");
    }
}

#[test]
fn read_levels_map_to_the_range() {
    let read = |l: Option<Level>| ReadConcernArgs { level: l, ..ReadConcernArgs::default() };
    assert_eq!(read(None).to_range(), ReadConcern::Local);
    for (l, want) in [
        (Level::Local, ReadConcern::Local),
        (Level::Available, ReadConcern::Available),
        (Level::Majority, ReadConcern::Majority),
        (Level::Linearizable, ReadConcern::Linearizable),
        (Level::Snapshot, ReadConcern::Majority),
    ] {
        assert_eq!(read(Some(l)).to_range(), want);
    }
    assert_eq!(Level::Snapshot.name(), "snapshot");
}

#[test]
fn commands_take_only_their_read_levels() {
    let rc = |fields: &[(&str, Value)]| {
        Some(
            ReadConcernArgs::parse(&find_with("readConcern", Value::Document(d(fields))), "find")
                .expect("valid")
                .expect("given"),
        )
    };
    let majority = rc(&[("level", "majority".into())]);
    // `local`, or no level, goes with any command.
    for name in ["insert", "ping", "hello", "listCollections"] {
        assert!(check_support(name, rc(&[("level", "local".into())]).as_ref(), None).is_ok());
        assert!(check_support(name, rc(&[("afterClusterTime", ts(1, 1))]).as_ref(), None).is_ok());
    }
    for name in ["find", "aggregate", "distinct", "getMore", "killCursors", "count"] {
        assert!(check_support(name, majority.as_ref(), None).is_ok(), "{name}");
    }
    assert_eq!(
        err(check_support("insert", majority.as_ref(), None)),
        (72, "Command insert does not support { readConcern: { level: \"majority\", provenance: \"clientSupplied\" } } :: caused by :: read concern not supported".into())
    );
    let snapshot = rc(&[("level", "snapshot".into())]);
    assert!(check_support("find", snapshot.as_ref(), None).is_ok());
    assert_eq!(
        err(check_support("count", snapshot.as_ref(), None)),
        (72, "Command count does not support { readConcern: { level: \"snapshot\", provenance: \"clientSupplied\" } } :: caused by :: read concern snapshot not supported".into())
    );
    let full = rc(&[("level", "majority".into()), ("afterClusterTime", ts(5, 7))]);
    assert_eq!(
        err(check_support("findandmodify", full.as_ref(), None)).1,
        "Command findAndModify does not support { readConcern: { level: \"majority\", afterClusterTime: Timestamp(5, 7), provenance: \"clientSupplied\" } } :: caused by :: read concern not supported"
    );
    let op = Value::Document(d(&[("ts", ts(5, 7)), ("t", 1.into())]));
    let with_op = rc(&[("level", "snapshot".into()), ("afterOpTime", op)]);
    assert_eq!(
        err(check_support("ping", with_op.as_ref(), None)).1,
        "Command ping does not support { readConcern: { level: \"snapshot\", afterOpTime: { ts: Timestamp(5, 7), t: 1 }, provenance: \"clientSupplied\" } } :: caused by :: read concern not supported"
    );
    let at = rc(&[("level", "snapshot".into()), ("atClusterTime", ts(5, 7))]);
    assert!(
        err(check_support("ping", at.as_ref(), None)).1.contains("atClusterTime: Timestamp(5, 7)")
    );
}

#[test]
fn write_concern_fields_and_their_errors() {
    let cases: Vec<(Document, i32, &str)> = vec![
        (
            d(&[("w", (-1).into())]),
            9,
            "w has to be a non-negative number and not greater than 50; found: -1",
        ),
        (
            d(&[("w", 51.7.into())]),
            9,
            "w has to be a non-negative number and not greater than 50; found: 51",
        ),
        (
            d(&[("w", 1e20.into())]),
            9,
            "w has to be a non-negative number and not greater than 50; found: 9223372036854775807",
        ),
        (
            d(&[("w", Value::Decimal128("-2.5".parse::<Decimal128>().expect("decimal")))]),
            9,
            "w has to be a non-negative number and not greater than 50; found: -2",
        ),
        (d(&[("w", f64::NAN.into())]), 9, "w cannot be NaN"),
        (
            d(&[("w", (-1e20).into())]),
            9,
            "w has to be a non-negative number and not greater than 50; found: -9223372036854775808",
        ),
        (d(&[("w", true.into())]), 9, "w has to be a number, string, or object; found: bool"),
        (d(&[("w", Value::Document(d(&[])))]), 9, "tagged write concern requires tags"),
        (
            d(&[("w", Value::Document(d(&[("dc", "x".into())])))]),
            9,
            "tags must be a single level document with only number values; found: dc: \"x\"",
        ),
        (
            d(&[("j", "x".into())]),
            14,
            "BSON field 'WriteConcernOptions.j' is the wrong type 'string', expected types '[decimal, int, bool, long, double]'",
        ),
        (
            d(&[("fsync", "x".into())]),
            14,
            "BSON field 'WriteConcernOptions.fsync' is the wrong type 'string', expected types '[decimal, int, bool, long, double]'",
        ),
        (
            d(&[("w", 1.into()), ("fsync", true.into()), ("j", true.into())]),
            9,
            "fsync and j options cannot be used together",
        ),
        (
            d(&[("w", 1.into()), ("foo", 1.into())]),
            40415,
            "BSON field 'WriteConcernOptions.foo' is an unknown field.",
        ),
        (d(&[("wtimeout", 1e20.into())]), 9, "wtimeout must be a 32-bit integer"),
        (
            d(&[("provenance", "bogus".into())]),
            2,
            "Enumeration value 'bogus' for field 'WriteConcernOptions.provenance' is not a valid value.",
        ),
        (
            d(&[("provenance", "customDefault".into())]),
            72,
            "writeConcern provenance must be unset or \"clientSupplied\"",
        ),
    ];
    for (wc, code, msg) in cases {
        assert_eq!(wc_err(Value::Document(wc.clone())), (code, msg.to_owned()), "{wc:?}");
    }
    assert_eq!(
        wc_err(5.into()),
        (
            14,
            "BSON field 'insert.writeConcern' is the wrong type 'int', expected type 'object'"
                .into()
        )
    );
}

#[test]
fn write_concerns_read_as_refdb_reads_them() {
    let wc = |fields: &[(&str, Value)]| wc_of(Value::Document(d(fields)));
    assert_eq!(wc(&[("w", 1.5.into())]).w, Some(WArg::Nodes(1)));
    assert_eq!(wc(&[("w", 2i64.into())]).w, Some(WArg::Nodes(2)));
    assert_eq!(wc(&[("w", Value::Null)]).w, Some(WArg::Mode(String::new())));
    assert_eq!(wc(&[("w", "majority".into())]).w, Some(WArg::Majority));
    assert_eq!(
        wc(&[("w", Value::Document(d(&[("b", 2.into()), ("a", 1.5.into())])))]).w,
        Some(WArg::Tags(vec![("b".into(), 2), ("a".into(), 1)]))
    );
    // `wtimeout` of another type reads as 0; `null` fields and the ignored ones pass.
    assert_eq!(wc(&[("wtimeout", "abc".into())]).wtimeout, 0);
    assert_eq!(wc(&[("wtimeout", 7.9.into())]).wtimeout, 7);
    assert_eq!(wc(&[("wtimeout", f64::NAN.into())]).wtimeout, 0);
    let quiet = wc(&[("j", Value::Null), ("getLastError", 1.into()), ("wOpTime", 5.into())]);
    assert_eq!(quiet.sync, Sync::Unset);
    assert_eq!(wc(&[("j", 1.into())]).sync, Sync::Journal);
    assert_eq!(wc(&[("j", false.into())]).sync, Sync::None);
    assert_eq!(wc(&[("j", false.into()), ("fsync", true.into())]).sync, Sync::Fsync);
    assert_eq!(wc(&[("provenance", "clientSupplied".into())]).w, None);
    // The range's concern (D-034).
    assert_eq!(wc(&[("w", 0.into())]).to_range(), WriteConcern { w: W::Zero, j: false });
    assert_eq!(
        wc(&[("w", 0.into()), ("j", true.into())]).to_range(),
        WriteConcern { w: W::Zero, j: true }
    );
    assert_eq!(
        wc(&[("w", 3.into()), ("j", false.into())]).to_range(),
        WriteConcern { w: W::Nodes(3), j: false }
    );
    assert_eq!(wc(&[("w", "x".into())]).to_range(), WriteConcern { w: W::Majority, j: true });
    assert_eq!(default_write_concern().to_range(), WriteConcern { w: W::Majority, j: true });
}

#[test]
fn write_concern_errors_describe_the_concern() {
    let wc = |fields: &[(&str, Value)]| wc_of(Value::Document(d(fields)));
    let shown = |w: WriteConcernArgs| w.describe();
    assert_eq!(
        shown(wc(&[("w", 3.7.into()), ("j", 1.into())])),
        "{ w: 3, j: true, wtimeout: 0, provenance: \"clientSupplied\" }"
    );
    assert_eq!(
        shown(wc(&[("fsync", true.into()), ("w", 3.into())])),
        "{ w: 3, wtimeout: 0, fsync: true, provenance: \"clientSupplied\" }"
    );
    assert_eq!(
        shown(wc(&[("w", "x".into()), ("j", false.into()), ("wtimeout", 7.9.into())])),
        "{ w: \"x\", j: false, wtimeout: 7, provenance: \"clientSupplied\" }"
    );
    assert_eq!(
        shown(wc(&[("w", "majority".into()), ("j", false.into())])),
        "{ w: \"majority\", j: true, wtimeout: 0, provenance: \"clientSupplied\" }"
    );
    assert_eq!(
        shown(wc(&[("wtimeout", (-1).into())])),
        "{ w: \"majority\", wtimeout: -1, provenance: \"implicitDefault\" }"
    );
    assert_eq!(
        shown(wc(&[("w", Value::Document(d(&[("b", 2.into()), ("a", 1.into())])))])),
        "{ w: { b: 2, a: 1 }, wtimeout: 0, provenance: \"clientSupplied\" }"
    );
    // The errors a write carries in its reply.
    let after = |w: WriteConcernArgs| w.error_after_write().map(|e| (e.code.code, e.errmsg));
    assert_eq!(
        after(wc(&[("w", "x".into())])),
        Some((79, "No write concern mode named 'x' found in replica set configuration".into()))
    );
    assert_eq!(
        after(wc(&[("w", Value::Document(d(&[("dc", 1.into())])))])),
        Some((4, "No replica set tag key dc in config".into()))
    );
    // A negative `wtimeout` is met: the write answers once its replicas have it.
    for w in [
        wc(&[("wtimeout", (-1).into())]),
        wc(&[("w", 2.into()), ("wtimeout", (-1).into())]),
        wc(&[("w", 1.into()), ("wtimeout", (-1).into())]),
        wc(&[("w", "majority".into()), ("wtimeout", (-5).into())]),
    ] {
        assert_eq!(after(w), None);
    }
    let mode = wc(&[("w", "x".into()), ("wtimeout", (-1).into())]);
    let e = mode.error_after_write().expect("an unknown mode");
    assert_eq!(
        write_concern_error(&e, &mode).get("errmsg"),
        Some(&Value::from(
            "No write concern mode named 'x' found in replica set configuration; Error details: { writeConcern: { w: \"x\", wtimeout: -1, provenance: \"clientSupplied\" } }"
        ))
    );
}

#[test]
fn generic_arguments_are_checked_in_their_order() {
    let bogus = Value::Document(d(&[("level", "bogus".into())]));
    let foo = Value::Document(d(&[("foo", 1.into())]));
    let docs = Value::Array(vec![]);
    // `readConcern`, `writeConcern` and `maxTimeMS` in the order the command gives them.
    let a = d(&[
        ("insert", "c".into()),
        ("documents", docs.clone()),
        ("readConcern", bogus.clone()),
        ("writeConcern", foo.clone()),
    ]);
    assert_eq!(err(parse(&a, "insert")).0, 2);
    let b = d(&[
        ("insert", "c".into()),
        ("documents", docs.clone()),
        ("writeConcern", foo.clone()),
        ("readConcern", bogus.clone()),
    ]);
    assert_eq!(err(parse(&b, "insert")).0, 40415);
    let c = d(&[("find", "c".into()), ("maxTimeMS", (-1).into()), ("readConcern", bogus)]);
    assert_eq!(err(parse(&c, "find")).0, 2);
    assert!(err(parse(&c, "find")).1.starts_with("BSON field 'maxTimeMS'"));
    // The command's unknown fields, then whether it takes the write concern, then the level.
    let majority = Value::Document(d(&[("level", "majority".into())]));
    let w1 = Value::Document(d(&[("w", 1.into())]));
    let e = d(&[("insert", "c".into()), ("readConcern", majority.clone()), ("foo", 1.into())]);
    assert_eq!(
        err(parse(&e, "insert")),
        (40415, "BSON field 'insert.foo' is an unknown field.".into())
    );
    let f = d(&[("ping", 1.into()), ("readConcern", majority), ("writeConcern", w1.clone())]);
    assert_eq!(err(parse(&f, "ping")), (72, "Command does not support writeConcern".into()));
    for name in ["find", "count", "killCursors", "getMore", "hello", "endSessions"] {
        let g = d(&[(name, 1.into()), ("writeConcern", w1.clone())]);
        assert_eq!(err(parse(&g, name)).0, 72, "{name}");
    }
    for name in [
        "insert",
        "update",
        "delete",
        "findAndModify",
        "create",
        "drop",
        "dropDatabase",
        "collMod",
        "renameCollection",
        "bulkWrite",
    ] {
        let g = d(&[(name, 1.into()), ("writeConcern", w1.clone())]);
        assert!(parse(&g, name).is_ok(), "{name}");
    }
    let g = parse(
        &d(&[("find", "c".into()), ("maxTimeMS", 5.into()), ("writeConcern", Value::Null)]),
        "find",
    )
    .expect("valid");
    assert_eq!((g.max_time_ms, g.write), (Some(5), None));
}

#[test]
fn unknown_fields_go_first_except_in_commands_parsed_late() {
    // Measured on a `refrouter` 9.0.4: a command parsed with its schema refuses an unknown field
    // before it reads its generic arguments; `listCollections`, `collMod`, `dbStats`,
    // `collStats`, `getLog` and `lockInfo` read them, and check that they take them, first.
    let bogus = Value::Document(d(&[("level", "bogus".into())]));
    let snapshot = Value::Document(d(&[("level", "snapshot".into())]));
    let w1 = Value::Document(d(&[("w", 1.into())]));
    for name in ["insert", "find", "create", "getMore", "listDatabases", "ping", "refreshSessions"]
    {
        for (field, v) in
            [("readConcern", bogus.clone()), ("maxTimeMS", "x".into()), ("readConcern", 5.into())]
        {
            let cmd = d(&[(name, 1.into()), ("zzz", 1.into()), (field, v)]);
            assert_eq!(err(parse(&cmd, name)).0, 40415, "{name} {field}");
        }
    }
    for name in ["listCollections", "collMod", "dbStats", "collStats", "getLog", "lockInfo"] {
        let cmd = d(&[(name, 1.into()), ("zzz", 1.into()), ("readConcern", bogus.clone())]);
        assert_eq!(err(parse(&cmd, name)).0, 2, "{name}");
        let cmd = d(&[(name, 1.into()), ("zzz", 1.into()), ("maxTimeMS", "x".into())]);
        assert_eq!(err(parse(&cmd, name)).0, 14, "{name}");
        let cmd = d(&[(name, 1.into()), ("zzz", 1.into()), ("readConcern", snapshot.clone())]);
        assert_eq!(err(parse(&cmd, name)).0, 72, "{name}");
        let cmd = d(&[(name, 1.into()), ("zzz", 1.into())]);
        assert_eq!(err(parse(&cmd, name)).0, 40415, "{name}");
    }
    let lc = d(&[("listCollections", 1.into()), ("zzz", 1.into()), ("writeConcern", w1.clone())]);
    assert_eq!(
        err(parse(&lc, "listCollections")),
        (72, "Command does not support writeConcern".into())
    );
    let cm = d(&[("collMod", "c".into()), ("zzz", 1.into()), ("writeConcern", w1)]);
    assert_eq!(err(parse(&cm, "collMod")).0, 40415);
}

#[test]
fn strict_commands_reject_unknown_fields() {
    for (name, ok_field) in [
        ("find", "filter"),
        ("getMore", "collection"),
        ("killCursors", "cursors"),
        ("count", "query"),
        ("distinct", "key"),
        ("insert", "ordered"),
        ("update", "bypassDocumentValidation"),
        ("delete", "let"),
        ("findandmodify", "new"),
        ("bulkWrite", "nsInfo"),
        ("create", "validator"),
        ("drop", "collectionUUID"),
        ("renameCollection", "to"),
        ("collMod", "validationLevel"),
        ("listDatabases", "nameOnly"),
        ("listCollections", "nameOnly"),
        ("connectionStatus", "showPrivileges"),
        ("dbStats", "scale"),
        ("collStats", "scale"),
        ("ping", "comment"),
        ("buildInfo", "$readPreference"),
        ("dropDatabase", "lsid"),
    ] {
        let good = d(&[(name, 1.into()), (ok_field, 1.into())]);
        assert!(check_unknown_fields(&good, name).is_ok(), "{name}.{ok_field}");
        let bad = d(&[(name, 1.into()), ("zz", 1.into())]);
        let want = format!("BSON field '{}.zz' is an unknown field.", canonical(name));
        assert_eq!(err(check_unknown_fields(&bad, name)), (40415, want));
    }
    // The commands RefDb reads loosely take anything.
    for name in [
        "hello",
        "isMaster",
        "whatsmyuri",
        "serverStatus",
        "getParameter",
        "currentOp",
        "killOp",
        "listCommands",
    ] {
        assert!(check_unknown_fields(&d(&[(name, 1.into()), ("zz", 1.into())]), name).is_ok());
    }
    assert_eq!(canonical("ismaster"), "isMaster");
    assert!(is_generic("$clusterTime") && !is_generic("$_originalQuery"));
}
