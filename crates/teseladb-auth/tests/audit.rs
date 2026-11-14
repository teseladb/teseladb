//! The audit log (013 FR-016, T028) against what the reference enterprise server 9.0.4 wrote with
//! `--auditDestination file --auditFormat JSON` and `--auditFilter`, and against its errors for bad
//! options. The arguments of `authCheck` keep no document values (a DV of FR-016: the reference
//! server writes them even with `redactClientLogData`).

use teseladb_auth::audit::{
    AuditError, AuditType, Auditor, Client, Config, Destination, Endpoint, Event, Format, Options,
};
use teseladb_auth::audit_param::{self, IndexBuildState, PASSWORD_MASK, VALUE_MASK};
use teseladb_auth::users::{RoleName, UserName};
use teseladb_bson::{Document, RawDoc, Value};

/// 2026-10-30T19:12:47.890Z.
const TS: i64 = 1_793_387_567_890;
const UUID: [u8; 16] = [0x41; 16];
const UUID_JSON: &str = r#"{ "$binary" : "QUFBQUFBQUFBQUFBQUFBQQ==", "$type" : "04" }"#;

fn client(users: &[(&str, &str)], roles: &[(&str, &str)]) -> Client {
    Client {
        uuid: UUID,
        local: Endpoint::parse("127.0.0.1:27017").unwrap(),
        remote: Endpoint::parse("127.0.0.1:35608").unwrap(),
        users: users.iter().map(|(u, d)| UserName::new(*u, *d)).collect(),
        roles: roles.iter().map(|(r, d)| RoleName::new(*r, *d)).collect(),
    }
}

fn config(o: Options<'_>) -> Result<Option<Config>, AuditError> {
    Config::from_options(&o)
}

fn file_json(filter: Option<&str>) -> Auditor {
    let o = Options {
        destination: Some(Destination::File.name()),
        format: Some(Format::Json.name()),
        path: Some("/tmp/audit.json"),
        filter,
        ..Options::default()
    };
    Auditor::new(config(o).unwrap().unwrap())
}

fn line(a: &Auditor, atype: AuditType, param: Document, result: i32, c: &Client) -> String {
    let event = Event { atype, ts_ms: TS, param, result };
    String::from_utf8(a.render(&event, c).expect("recorded")).unwrap()
}

fn header(atype: &str) -> String {
    format!(
        concat!(
            r#"{{ "atype" : "{}", "ts" : {{ "$date" : "2026-10-30T19:12:47.890+00:00" }}, "#,
            r#""uuid" : {}, "local" : {{ "ip" : "127.0.0.1", "port" : 27017 }}, "#,
            r#""remote" : {{ "ip" : "127.0.0.1", "port" : 35608 }}, "#,
            r#""directRemote" : {{ "ip" : "127.0.0.1", "port" : 35608 }}, "#
        ),
        atype, UUID_JSON
    )
}

#[test]
fn messages_have_the_measured_shape() {
    let a = file_json(None);
    let root = client(&[("root", "admin")], &[("root", "admin")]);
    let who = r#""users" : [ { "user" : "root", "db" : "admin" } ], "roles" : [ { "role" : "root", "db" : "admin" } ], "#;
    let app = UserName::new("app", "ventas");
    let read = [RoleName::new("read", "ventas")];
    let cases = [
        (
            AuditType::CreateUser,
            audit_param::create_user(&app, &read),
            r#""param" : { "user" : "app", "db" : "ventas", "roles" : [ { "role" : "read", "db" : "ventas" } ] }"#,
        ),
        (
            AuditType::UpdateUser,
            audit_param::update_user(&app, true, Some(&Document::new().with("k", 1)), None),
            r#""param" : { "user" : "app", "db" : "ventas", "passwordChanged" : true, "customData" : { "k" : 1 } }"#,
        ),
        (
            AuditType::CreateIndex,
            audit_param::create_index(
                "ventas.pedidos",
                "a_1",
                &Document::new()
                    .with("v", 2)
                    .with("key", Document::new().with("a", 1))
                    .with("name", "a_1"),
                IndexBuildState::Started,
            ),
            r#""param" : { "ns" : "ventas.pedidos", "indexName" : "a_1", "indexSpec" : { "v" : 2, "key" : { "a" : 1 }, "name" : "a_1" }, "indexBuildState" : "IndexBuildStarted" }"#,
        ),
        (
            AuditType::RenameCollection,
            audit_param::rename_collection("ventas.pedidos", "ventas.p2"),
            r#""param" : { "old" : "ventas.pedidos", "new" : "ventas.p2" }"#,
        ),
        (
            AuditType::DropIndex,
            audit_param::drop_index("ventas.pedidos", "a_1"),
            r#""param" : { "ns" : "ventas.pedidos", "indexName" : "a_1" }"#,
        ),
        (
            AuditType::CreateRole,
            audit_param::create_role(
                &RoleName::new("r1", "ventas"),
                &[],
                &[Value::Document(
                    Document::new()
                        .with(
                            "resource",
                            Document::new().with("db", "ventas").with("collection", "p2"),
                        )
                        .with("actions", Value::Array(vec!["find".into()])),
                )],
            ),
            r#""param" : { "role" : "r1", "db" : "ventas", "roles" : [], "privileges" : [ { "resource" : { "db" : "ventas", "collection" : "p2" }, "actions" : [ "find" ] } ] }"#,
        ),
        (
            AuditType::GrantRolesToRole,
            audit_param::role_roles(&RoleName::new("r1", "ventas"), &read),
            r#""param" : { "role" : "r1", "db" : "ventas", "roles" : [ { "role" : "read", "db" : "ventas" } ] }"#,
        ),
        (
            AuditType::DropAllUsersFromDatabase,
            audit_param::database("ventas"),
            r#""param" : { "db" : "ventas" }"#,
        ),
        (
            AuditType::DropDatabase,
            audit_param::namespace("ventas"),
            r#""param" : { "ns" : "ventas" }"#,
        ),
    ];
    for (atype, param, expected) in cases {
        let got = line(&a, atype, param, 0, &root);
        assert_eq!(got, format!("{}{who}{expected}, \"result\" : 0 }}\n", header(atype.name())));
    }
}

#[test]
fn failed_logins_and_logouts_name_who_tried() {
    let a = file_json(None);
    let nobody = client(&[], &[]);
    let got = line(
        &a,
        AuditType::Authenticate,
        audit_param::authenticate(&UserName::new("nobody", "admin"), "SCRAM-SHA-1"),
        11,
        &nobody,
    );
    let expected = concat!(
        r#""users" : [], "roles" : [], "#,
        r#""param" : { "user" : "nobody", "db" : "admin", "mechanism" : "SCRAM-SHA-1" }, "result" : 11 }"#
    );
    assert_eq!(got, format!("{}{expected}\n", header("authenticate")));
    let got = line(
        &a,
        AuditType::Logout,
        audit_param::logout(audit_param::DISCONNECTED, &[UserName::new("root", "admin")], &[], TS),
        0,
        &nobody,
    );
    assert!(got.contains(concat!(
        r#""param" : { "reason" : "Client has disconnected", "initialUsers" : [ { "user" : "root", "db" : "admin" } ], "#,
        r#""updatedUsers" : [], "loginTime" : { "$date" : "2026-10-30T19:12:47.890+00:00" } }"#
    )));
}

#[test]
fn denied_commands_keep_no_password_and_no_document_value() {
    let a = file_json(None);
    let ro = client(&[("ro", "ventas")], &[("read", "ventas")]);
    let insert = Document::new()
        .with("insert", "p")
        .with(
            "documents",
            Value::Array(vec![Value::Document(
                Document::new().with("secret", "valor-documento-123"),
            )]),
        )
        .with("ordered", true)
        .with("lsid", Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] }))
        .with("$db", "ventas");
    let ns = audit_param::auth_check_ns("ventas", &insert);
    let got = line(
        &a,
        AuditType::AuthCheck,
        audit_param::auth_check("insert", &ns, Some(&insert)),
        13,
        &ro,
    );
    assert!(got.contains(concat!(
        r#""param" : { "command" : "insert", "ns" : "ventas.p", "args" : { "insert" : "p", "#,
        r####""documents" : [ { "secret" : "###" } ], "ordered" : true, "lsid" : { "id" : "###" }, "####,
        r#""$db" : "ventas" } }, "result" : 13 }"#
    )));
    assert!(!got.contains("valor-documento-123"));
    // `createUser` with a password, as the reference server masks it; the namespace is `db.<user>`.
    let create = Document::new()
        .with("createUser", "x")
        .with("pwd", "pw-secret-999")
        .with("roles", Value::Array(Vec::new()))
        .with("$db", "ventas");
    let ns = audit_param::auth_check_ns("ventas", &create);
    let got = line(
        &a,
        AuditType::AuthCheck,
        audit_param::auth_check("createUser", &ns, Some(&create)),
        13,
        &ro,
    );
    assert!(got.contains(&format!(
        r#""args" : {{ "createUser" : "x", "pwd" : "{PASSWORD_MASK}", "roles" : [], "$db" : "ventas" }}"#
    )));
    assert!(!got.contains("pw-secret-999"));
    assert_eq!(VALUE_MASK, "###");
    // Not authenticated: no arguments and the database as namespace (measured).
    let shutdown = Document::new().with("shutdown", 1).with("$db", "admin");
    assert_eq!(audit_param::auth_check_ns("admin", &shutdown), "admin");
    let got = line(
        &a,
        AuditType::AuthCheck,
        audit_param::auth_check("createUser", "ventas", None),
        13,
        &client(&[], &[]),
    );
    assert!(
        got.contains(r#""param" : { "command" : "createUser", "ns" : "ventas" }, "result" : 13 }"#)
    );
}

#[test]
fn the_filter_is_a_query_on_the_message() {
    // The filter measured on 9.0.4: only these action types, and no `getParameter`.
    let a = file_json(Some(
        r#"{ atype: { $in: ["authCheck","authenticate","createUser","updateUser"] }, "param.command": { $nin: ["getParameter","aggregate"] } }"#,
    ));
    let c = client(&[], &[]);
    let render =
        |atype, param: Document| a.render(&Event { atype, ts_ms: TS, param, result: 13 }, &c);
    assert!(
        render(AuditType::AuthCheck, audit_param::auth_check("insert", "ventas.p", None)).is_some()
    );
    assert!(
        render(AuditType::AuthCheck, audit_param::auth_check("getParameter", "admin", None))
            .is_none()
    );
    assert!(render(AuditType::CreateCollection, audit_param::namespace("ventas.p")).is_none());
    assert!(
        render(
            AuditType::Authenticate,
            audit_param::authenticate(&UserName::new("u", "d"), "PLAIN")
        )
        .is_some()
    );
    // Strict Extended JSON, and an array as the document of its positions.
    let strict = file_json(Some(r#"{"result": {"$numberInt": "13"}}"#));
    assert!(
        strict
            .render(
                &Event {
                    atype: AuditType::Shutdown,
                    ts_ms: TS,
                    param: Document::new(),
                    result: 13
                },
                &c
            )
            .is_some()
    );
    assert!(
        strict
            .render(
                &Event { atype: AuditType::Shutdown, ts_ms: TS, param: Document::new(), result: 0 },
                &c
            )
            .is_none()
    );
    let array = file_json(Some("[1]"));
    assert!(
        array
            .render(
                &Event { atype: AuditType::Shutdown, ts_ms: TS, param: Document::new(), result: 0 },
                &c
            )
            .is_none()
    );
    assert_eq!(array.config().filter_doc, Some(Document::new().with("0", 1)));
}

#[test]
fn bad_options_stop_with_the_reference_servers_messages() {
    let o = |destination, format, path, filter| Options {
        destination,
        format,
        path,
        filter,
        ..Options::default()
    };
    let cases: [(Options<'_>, &str); 12] = [
        (
            o(Some("stdout"), None, None, None),
            "auditDestination must be one of 'console', 'syslog', or 'file'",
        ),
        (o(Some("Console"), None, None, None), "invalid auditLog destination"),
        (
            o(None, None, Some("/tmp/a.json"), None),
            "auditLog.destination is required when auditLog.path is specified",
        ),
        (
            o(None, Some("JSON"), None, None),
            "auditLog.destination is required when auditLog.format is specified",
        ),
        (
            o(None, None, None, Some("{}")),
            "auditLog.destination is required when auditLog.filter is specified",
        ),
        (
            o(Some("file"), None, None, None),
            "auditLog.format must be specified when auditLog.destination is to a file",
        ),
        (
            o(Some("file"), None, Some("/tmp/a"), None),
            "auditLog.format must be specified when auditLog.destination is to a file",
        ),
        (
            o(Some("file"), Some("JSON"), None, None),
            "auditLog.path must be specified when auditLog.destination is to a file",
        ),
        (
            o(Some("console"), Some("BSON"), None, None),
            "auditLog.format and auditLog.path are only allowed when auditLog.destination is 'file'",
        ),
        (
            o(Some("syslog"), None, Some("/tmp/a"), None),
            "auditLog.format and auditLog.path are only allowed when auditLog.destination is 'file'",
        ),
        (
            o(Some("file"), Some("XML"), Some("/tmp/a"), None),
            "auditFormat must be one of 'BSON' or 'JSON'",
        ),
        (o(Some("file"), Some("bson"), Some("/tmp/a"), None), "Invalid value for auditLog.format"),
    ];
    for (opts, msg) in cases {
        assert_eq!(config(opts).unwrap_err(), AuditError(msg.to_owned()), "{msg}");
    }
    let filter = |f| config(o(Some("console"), None, None, Some(f))).unwrap_err().0;
    assert_eq!(
        filter("{atype:{$bogus:1}}"),
        "Failed to parse auditFilter :: caused by :: unknown operator: $bogus"
    );
    assert!(filter("{$where:\"1\"}").starts_with("Failed to parse auditFilter :: caused by :: "));
    assert!(filter("notjson").starts_with("Bad auditFilter: "));
    assert!(filter("{\"a\":1}x").starts_with("Bad auditFilter: "));
    assert!(config(Options::default()).unwrap().is_none());
    assert_eq!(AuditError("x".into()).to_string(), "x");
}

#[test]
fn destinations_and_formats_shape_the_bytes() {
    let event =
        Event { atype: AuditType::Shutdown, ts_ms: TS, param: audit_param::shutdown(), result: 0 };
    let system = Client::system(UUID);
    let console = Auditor::new(
        config(Options { destination: Some("console"), ..Options::default() }).unwrap().unwrap(),
    );
    assert_eq!(console.config().format, Format::Json);
    let text = String::from_utf8(console.render(&event, &system).unwrap()).unwrap();
    assert_eq!(
        text,
        format!(
            concat!(
                r#"{{ "atype" : "shutdown", "ts" : {{ "$date" : "2026-10-30T19:12:47.890+00:00" }}, "uuid" : {}, "#,
                r#""local" : {{ "isSystemUser" : true }}, "remote" : {{ "isSystemUser" : true }}, "#,
                r#""users" : [], "roles" : [], "param" : {{}}, "result" : 0 }}"#,
                "\n"
            ),
            UUID_JSON
        )
    );
    let syslog = Auditor::new(
        config(Options { destination: Some("syslog"), ..Options::default() }).unwrap().unwrap(),
    );
    assert_eq!(syslog.render(&event, &system).unwrap(), text.trim_end().as_bytes());
    let bson = Auditor::new(
        config(Options {
            destination: Some("file"),
            format: Some("BSON"),
            path: Some("/a"),
            filter: None,
            ..Options::default()
        })
        .unwrap()
        .unwrap(),
    );
    let bytes = bson.render(&event, &system).unwrap();
    let raw = RawDoc::from_bytes(&bytes).unwrap();
    assert_eq!(Document::from(raw), event.to_document(&system));
    assert_eq!(bson.config().path.as_deref(), Some("/a"));
    assert_eq!(bson.config().destination, Destination::File);
}

#[test]
fn authorization_successes_follow_the_parameter() {
    let mut a = file_json(None);
    assert!(a.records_auth_check(13));
    assert!(!a.records_auth_check(0));
    a.set_authorization_success(true);
    assert!(a.records_auth_check(0));
    assert!(a.config().authorization_success);
}

#[test]
fn endpoints_and_types_read_back() {
    assert_eq!(
        Endpoint::parse("[::1]:27017"),
        Some(Endpoint::Address { ip: "::1".into(), port: 27017 })
    );
    assert_eq!(
        Endpoint::parse("/tmp/teseladb-27017.sock"),
        Some(Endpoint::Unix("/tmp/teseladb-27017.sock".into()))
    );
    for bad in ["127.0.0.1", "h:x", ":1", "[]:1"] {
        assert_eq!(Endpoint::parse(bad), None, "{bad}");
    }
    assert_eq!(Endpoint::Unix("/s".into()).to_document(), Document::new().with("unix", "/s"));
    for t in AuditType::ALL {
        assert_eq!(AuditType::from_name(t.name()), Some(*t));
    }
    assert_eq!(AuditType::from_name("nope"), None);
    assert_eq!(IndexBuildState::Succeeded.name(), "IndexBuildSucceeded");
    let u = UserName::new("u", "d");
    let r = [RoleName::new("r", "d")];
    let p = [Value::from(1)];
    assert_eq!(audit_param::drop_user(&u), Document::new().with("user", "u").with("db", "d"));
    assert!(audit_param::user_roles(&u, &r).get("roles").is_some());
    assert!(audit_param::update_user(&u, false, None, Some(&r)).get("roles").is_some());
    let role = RoleName::new("x", "d");
    assert_eq!(audit_param::drop_role(&role), Document::new().with("role", "x").with("db", "d"));
    assert!(audit_param::update_role(&role, Some(&r), Some(&p)).get("privileges").is_some());
    assert!(audit_param::role_privileges(&role, &p).get("privileges").is_some());
    let meta = audit_param::client_metadata(&Document::new().with("ip", "h"), &Document::new());
    assert!(meta.get("localEndpoint").is_some());
    assert!(audit_param::startup(&Document::new()).get("startupOptions").is_some());
}

#[test]
fn filters_read_like_the_reference_reads_them() {
    let doc = |f: &str| file_json(Some(f)).config().filter_doc.clone().unwrap();
    assert_eq!(
        doc(r#"{ atype : 'auth"Check', 'param.db': "it's", n: true, $or: [{a: 1}, {b: 'x\'y'}] }"#),
        Document::new().with("atype", "auth\"Check").with("param.db", "it's").with("n", true).with(
            "$or",
            Value::Array(vec![
                Value::Document(Document::new().with("a", 1)),
                Value::Document(Document::new().with("b", "x'y")),
            ]),
        )
    );
    assert_eq!(doc(r#"{"a\"b": "c\\d"}"#), Document::new().with("a\"b", "c\\d"));
    let err = |f: &str| {
        config(Options { destination: Some("console"), filter: Some(f), ..Options::default() })
            .unwrap_err()
            .0
    };
    assert!(err("{a: 'open").starts_with("Bad auditFilter: "));
    assert!(err("{a: 1, : 2}").starts_with("Bad auditFilter: "));
    assert!(err("1").starts_with("Bad auditFilter: "));
}

#[test]
fn keys_at_the_end_of_the_text_and_values_that_look_like_keys() {
    let doc = |f: &str| file_json(Some(f)).config().filter_doc.clone().unwrap();
    assert_eq!(
        doc("{param.db: 1, a: [1, true]}"),
        Document::new()
            .with("param.db", 1)
            .with("a", Value::Array(vec![Value::Int32(1), Value::Boolean(true)]))
    );
    let err = |f: &str| {
        config(Options { destination: Some("console"), filter: Some(f), ..Options::default() })
            .unwrap_err()
            .0
    };
    assert!(err("{abc").starts_with("Bad auditFilter: "));
    assert!(err("{abc  ").starts_with("Bad auditFilter: "));
    assert!(err("\"text\"").starts_with("Bad auditFilter: "));
    assert_eq!(audit_param::auth_check_ns("db", &Document::new().with("find", "")), "db");
    let role = RoleName::new("x", "d");
    assert_eq!(
        audit_param::update_role(&role, None, None),
        Document::new().with("role", "x").with("db", "d")
    );
}
