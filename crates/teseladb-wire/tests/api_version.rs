//! The Stable API's parameters and what `apiStrict: true` refuses (006 FR-035), against what a
//! `refrouter` 9.0.4 answered on 05-11-2026 (scenario `api-strict`) and, for the API's test
//! commands, a `refnode` 9.0.4 with `enableTestCommands` the same day.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_wire::commands::lock;
use teseladb_wire::connection::Connection;
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

/// The answer of the node to `cmd` on database `db`, through the dispatch, where the API
/// parameters are checked.
fn send(m: &mut Mem, db: &str, cmd: Document) -> Document {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", db).to_bytes().unwrap());
    let mut conn = Connection::new(9);
    conn.receive(&bytes);
    let req = conn.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let r = dispatch_with(&mut conn, &req, &info(), Some((&mut *m, &shared)));
    m.shared = shared;
    match r {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

/// Command `cmd` with `apiVersion: "1"` and `apiStrict: true`.
fn strict(m: &mut Mem, cmd: &str) -> Document {
    let mut d = json(cmd);
    d.push("apiVersion", "1");
    d.push("apiStrict", true);
    send(m, "api", d)
}

/// The code and message of a reply.
fn failure(r: &Document) -> (i32, String) {
    match (field(r, "code"), field(r, "errmsg")) {
        (Value::Int32(c), Value::String(m)) => (*c, m.clone()),
        _ => panic!("{r:?}"),
    }
}

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn setup() -> Mem {
    let mut m = Mem::new();
    m.run("api", json(r#"{"insert":"c","documents":[{"_id":1,"x":1}]}"#));
    m
}

#[test]
fn only_version_1_exists_and_the_flags_need_a_version() {
    let mut m = setup();

    let v2 = send(&mut m, "api", json(r#"{"find":"c","apiVersion":"2"}"#));
    let int = send(&mut m, "api", json(r#"{"find":"c","apiVersion":1}"#));
    let alone = send(&mut m, "api", json(r#"{"find":"c","apiStrict":true}"#));
    let deprecation = send(&mut m, "api", json(r#"{"find":"c","apiDeprecationErrors":true}"#));
    let text = send(&mut m, "api", json(r#"{"find":"c","apiVersion":"1","apiStrict":"yes"}"#));
    let fine =
        send(&mut m, "api", json(r#"{"find":"c","apiVersion":"1","apiDeprecationErrors":true}"#));

    assert_eq!(failure(&v2), (322, "Cannot accept API version 2".to_owned()));
    assert_eq!(
        failure(&int),
        (
            14,
            "BSON field 'find.apiVersion' is the wrong type 'int', expected type 'string'"
                .to_owned()
        )
    );
    let without = "Provided apiStrict and/or apiDeprecationErrors without passing apiVersion";
    assert_eq!(failure(&alone), (4_886_600, without.to_owned()));
    assert_eq!(failure(&deprecation), (4_886_600, without.to_owned()));
    assert_eq!(
        failure(&text),
        (
            14,
            "BSON field 'find.apiStrict' is the wrong type 'string', expected type 'bool'"
                .to_owned()
        )
    );
    assert!(ok(&fine), "{fine:?}");
}

#[test]
fn a_command_outside_version_1_is_refused_only_when_strict() {
    let mut m = setup();

    let refused = strict(&mut m, r#"{"distinct":"c","key":"x"}"#);
    let loose = send(&mut m, "api", json(r#"{"distinct":"c","key":"x","apiVersion":"1"}"#));
    let unknown = strict(&mut m, r#"{"noSuchCommand":1}"#);
    let find = strict(&mut m, r#"{"find":"c"}"#);

    assert_eq!(
        failure(&refused),
        (
            323,
            "Provided apiStrict:true, but the command distinct is not in API Version 1. \
             Information on supported commands and migrations in API Version 1 can be found at \
             https://dochub.refdb.org/core/manual-versioned-api."
                .to_owned()
        )
    );
    assert!(ok(&loose) && ok(&find), "{loose:?} {find:?}");
    assert_eq!(failure(&unknown).0, 59);
}

#[test]
fn unstable_fields_are_refused_when_their_type_is_right() {
    let mut m = setup();

    let return_key = strict(&mut m, r#"{"find":"c","returnKey":true}"#);
    let min_typed = strict(&mut m, r#"{"find":"c","min":{"_id":0},"hint":{"_id":1}}"#);
    let min_wrong = strict(&mut m, r#"{"find":"c","min":1,"hint":{"_id":1}}"#);
    let size = strict(&mut m, r#"{"create":"capd","capped":true,"size":4096}"#);
    let sort = strict(
        &mut m,
        r#"{"update":"c","updates":[{"q":{},"u":{"$set":{"a":1}},"sort":{"_id":1}}]}"#,
    );
    let fields = strict(&mut m, r#"{"create":"e","encryptedFields":{"fields":[]}}"#);
    let disk = strict(&mut m, r#"{"find":"c","allowDiskUse":true}"#);

    let refused = |k: &str| (323, format!("BSON field '{k}' is not allowed with apiStrict:true."));
    assert_eq!(failure(&return_key), refused("returnKey"));
    assert_eq!(failure(&min_typed), refused("min"));
    assert_eq!(failure(&min_wrong).0, 14);
    assert_eq!(failure(&size), refused("size"));
    assert_eq!(failure(&sort), refused("sort"));
    assert_eq!(failure(&fields), refused("fields"));
    assert!(ok(&disk), "{disk:?}");
}

#[test]
fn a_text_index_is_refused_when_strict() {
    let mut m = setup();

    let r = strict(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"x":"text"},"name":"t"}]}"#);
    let sparse = strict(
        &mut m,
        r#"{"createIndexes":"c","indexes":[{"key":{"w":1},"name":"w","sparse":true}]}"#,
    );

    assert_eq!(
        failure(&r),
        (
            323,
            "Error in specification { key: { x: \"text\" }, name: \"t\" } :: caused by :: text \
             indexes cannot be created with apiStrict: true"
                .to_owned()
        )
    );
    assert!(ok(&sparse), "{sparse:?}");
}

#[test]
fn stages_outside_version_1_are_refused_also_inside_others() {
    let mut m = setup();

    let fill = r#"{"$fill":{"output":{"x":{"value":0}}}}"#;
    let top = strict(&mut m, &format!(r#"{{"aggregate":"c","pipeline":[{fill}],"cursor":{{}}}}"#));
    let lookup = strict(
        &mut m,
        &format!(
            r#"{{"aggregate":"c","pipeline":[{{"$lookup":{{"from":"c","pipeline":[{fill}],"as":"l"}}}}],"cursor":{{}}}}"#
        ),
    );
    let union = strict(
        &mut m,
        &format!(
            r#"{{"aggregate":"c","pipeline":[{{"$unionWith":{{"coll":"c","pipeline":[{fill}]}}}}],"cursor":{{}}}}"#
        ),
    );
    let facet = strict(
        &mut m,
        &format!(r#"{{"aggregate":"c","pipeline":[{{"$facet":{{"a":[{fill}]}}}}],"cursor":{{}}}}"#),
    );
    let count =
        strict(&mut m, r#"{"aggregate":"c","pipeline":[{"$collStats":{"count":{}}}],"cursor":{}}"#);
    let storage = strict(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$collStats":{"storageStats":{}}}],"cursor":{}}"#,
    );
    let plain = strict(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$match":{}},{"$project":{"_id":1}}],"cursor":{}}"#,
    );

    let not_allowed =
        (323, "$fill is not allowed with 'apiStrict: true' in API Version 1".to_owned());
    for r in [&top, &lookup, &union, &facet] {
        assert_eq!(failure(r), not_allowed);
    }
    assert!(ok(&count) && ok(&plain), "{count:?} {plain:?}");
    assert_eq!(
        failure(&storage),
        (
            323,
            "Unable to retrieve information for $collStats stage :: caused by :: only the 'count' \
             option to $collStats is supported in API Version 1"
                .to_owned()
        )
    );
}

#[test]
fn the_api_test_commands_answer_their_parameters_with_enable_test_commands() {
    let mut m = setup();
    let without = send(&mut m, "admin", doc(&[("testVersion2", 1.into())]));
    lock(&m.shared).failpoint = Some(Default::default());

    let plain = send(&mut m, "admin", doc(&[("testVersion2", 1.into())]));
    let v1 = send(&mut m, "admin", json(r#"{"testVersion2":1,"apiVersion":"1"}"#));
    let refused =
        send(&mut m, "admin", json(r#"{"testVersion2":1,"apiVersion":"1","apiStrict":true}"#));
    let deprecated = send(
        &mut m,
        "admin",
        json(r#"{"testDeprecation":1,"apiVersion":"1","apiDeprecationErrors":true}"#),
    );
    let removal =
        send(&mut m, "admin", json(r#"{"testRemoval":1,"apiVersion":"1","apiStrict":true}"#));
    let listed = send(&mut m, "admin", doc(&[("listCommands", 1.into())]));

    assert_eq!(failure(&without).0, 59);
    assert!(ok(&plain), "{plain:?}");
    assert_eq!(field(&v1, "apiVersion"), &Value::from("1"));
    assert_eq!(failure(&refused).0, 323);
    assert_eq!(
        failure(&deprecated),
        (
            324,
            "Provided apiDeprecationErrors:true, but the command testDeprecation is deprecated in \
             API Version 1."
                .to_owned()
        )
    );
    assert_eq!(field(&removal, "apiStrict"), &Value::Boolean(true));
    assert_eq!(
        field(&listed, "commands.testDeprecation.deprecatedApiVersions"),
        &Value::Array(vec![Value::from("1")])
    );
}

/// `collMod`'s `_removeLegacyTimeseriesBucketingParametersHaveChanged` is `stability: unstable`
/// in RefDb r9.0.4 `coll_mod.idl`: with `apiStrict` it is refused with 323 before the internal
/// privilege it needs is checked; without it, that privilege no user has refuses it with 13
/// (measured on the native `refrouter` 9.0.4 on 09-11-2026, scenario `api-strict`).
#[test]
fn the_internal_coll_mod_field_is_unstable_before_it_is_unauthorized() {
    let mut m = setup();
    let field = "_removeLegacyTimeseriesBucketingParametersHaveChanged";
    let cmd = format!(r#"{{"collMod":"c","{field}":true}}"#);

    let refused = strict(&mut m, &cmd);
    let loose = send(&mut m, "api", json(&cmd));

    assert_eq!(
        failure(&refused),
        (323, format!("BSON field '{field}' is not allowed with apiStrict:true."))
    );
    assert_eq!(failure(&loose).0, 13);
}
