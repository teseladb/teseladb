//! Reads of `aggregate` whose leading `$match` may go through the planner, and the namespace and
//! argument checks of the stages of the node itself, against what a `refnode` 9.0.4 answered on
//! 01-11-2026.

mod common;

use common::{Mem, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, db: &str, cmd: &str) -> Document {
    m.run(db, json(cmd))
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

fn loaded() -> Mem {
    let mut m = Mem::new();
    run(
        &mut m,
        "r",
        r#"{"insert":"c","documents":[{"_id":1,"a":1,"b":"x"},{"_id":2,"a":2,"b":"y"},{"_id":3,"a":3,"b":"x"}]}"#,
    );
    run(&mut m, "r", r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    m
}

/// A leading `$match` on a view, on a collection that does not exist and one that matches
/// nothing.
#[test]
fn a_leading_match_reads_views_missing_collections_and_empty_matches() {
    let mut m = loaded();
    run(&mut m, "r", r#"{"create":"v","viewOn":"c","pipeline":[{"$match":{"b":"x"}}]}"#);
    let r = run(
        &mut m,
        "r",
        r#"{"aggregate":"v","pipeline":[{"$match":{"a":{"$gt":1}}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), vec![json(r#"{"_id":3,"a":3,"b":"x"}"#)], "{r:?}");
    let r = run(&mut m, "r", r#"{"aggregate":"nope","pipeline":[{"$match":{"a":1}}],"cursor":{}}"#);
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
    let r = run(
        &mut m,
        "r",
        r#"{"aggregate":"c","pipeline":[{"$match":{"a":{"$in":[]}}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
}

/// `$currentOp` runs only on `admin` with `{aggregate: 1}`.
#[test]
fn current_op_runs_only_on_admin_without_a_collection() {
    let mut m = loaded();
    let msg = "$currentOp must be run against the 'admin' database with {aggregate: 1}";
    for (db, target) in [("r", "1"), ("admin", r#""c""#)] {
        let r = run(
            &mut m,
            db,
            &format!(
                r#"{{"aggregate":{target},"pipeline":[{{"$currentOp":{{}}}}],"cursor":{{}}}}"#
            ),
        );
        assert_eq!(field(&r, "code"), &Value::Int32(73), "{db}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg));
    }
}

/// The users of `$listLocalSessions` are documents with a user and a database as strings.
#[test]
fn list_local_sessions_checks_its_users() {
    let mut m = loaded();
    let cases = [
        (
            r#"[{"user":"x","db":"admin"},{"user":1,"db":"a"}]"#,
            "BSON field '$listLocalSessions.users.user' is the wrong type 'int', expected type 'string'",
        ),
        (
            "[5]",
            "BSON field '$listLocalSessions.users.0' is the wrong type 'int', expected type 'object'",
        ),
    ];
    for (users, msg) in cases {
        let r = run(
            &mut m,
            "admin",
            &format!(
                r#"{{"aggregate":1,"pipeline":[{{"$listLocalSessions":{{"users":{users}}}}}],"cursor":{{}}}}"#
            ),
        );
        assert_eq!(field(&r, "code"), &Value::Int32(14), "{users}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg));
    }
    let r = run(
        &mut m,
        "admin",
        r#"{"aggregate":1,"pipeline":[{"$listLocalSessions":{"users":[{"user":"x","db":"admin"}]}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
}
