//! The audit log configured while the node runs (013 T042, FR-031), against RefDb Enterprise
//! 9.0.4 with `--auditRuntimeConfiguration true` (measured on 31-10-2026): the cluster parameter
//! `auditConfig` with its forms and errors, the events of the cluster parameters,
//! `logApplicationMessage`, and another node that picks up the new filter.

mod common;

use std::sync::{Arc, Mutex, RwLock};

use common::{Mem, doc, field};
use teseladb_auth::audit::{Auditor, Config, Options};
use teseladb_auth::users::{EXTERNAL_DB, RoleName, User, UserName};
use teseladb_bson::{Document, Value};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_wire::commands::audit::{Audit, Sink};
use teseladb_wire::commands::auth::AUTHENTICATE;
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const ADMIN: &str = "admin";
const CERT_USER: &str = "CN=client,OU=KernelUser,O=RefDb,L=New York City,ST=New York,C=US";
const AUDIT_CONFIG: &str = "auditConfig";
const AAS: &str = "auditAuthorizationSuccess";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Sink for Capture {
    fn write(&mut self, bytes: &[u8]) {
        self.0.lock().unwrap().extend_from_slice(bytes);
    }
}

impl Capture {
    /// `atype` and `param` of each line written since the last call.
    fn events(&self) -> Vec<String> {
        let text = String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap();
        text.lines()
            .map(|l| {
                let atype = &l[13..l.find("\", \"ts\"").unwrap()];
                let p = &l[l.find("\"param\" : ").unwrap() + 10..l.rfind(", \"result\"").unwrap()];
                format!("{atype} {p}")
            })
            .collect()
    }
}

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    }
}

/// The node's shared state with an audit log to the console, configured with `options`.
fn sessions(options: Options<'_>) -> (Mutex<NodeSessions>, Capture) {
    let mut sessions = NodeSessions::new(Timeouts::default());
    let capture = Capture::default();
    if options.destination.is_some() {
        let auditor = Auditor::new(Config::from_options(&options).unwrap().unwrap());
        let audit = Audit::new(Arc::new(RwLock::new(auditor)), Box::new(capture.clone()));
        sessions.audit = Some(audit);
    }
    (Mutex::new(sessions), capture)
}

fn runtime() -> Options<'static> {
    Options { destination: Some("console"), runtime_configuration: true, ..Options::default() }
}

fn backend() -> Mem {
    let mut m = Mem::new();
    let mut cert = User::new(UserName::new(CERT_USER, EXTERNAL_DB));
    cert.roles = vec![RoleName::new("root", ADMIN)];
    let op = SecurityOp::named(
        Principal::User,
        EXTERNAL_DB,
        CERT_USER,
        Expect::Absent,
        Some(cert.to_document().to_bytes().unwrap()),
    );
    m.security.apply(&[op]).unwrap();
    m
}

fn request(cmd: &Document) -> Request {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut c = Connection::new(1);
    c.receive(&bytes);
    c.next_request().unwrap().unwrap()
}

fn run(m: &mut Mem, shared: &Mutex<NodeSessions>, c: &mut Connection, cmd: Document) -> Document {
    let a = dispatch_with(c, &request(&cmd.with("$db", ADMIN)), &info(), Some((m, shared)));
    match a {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

/// A connection logged in as root with its certificate.
fn root(m: &mut Mem, shared: &Mutex<NodeSessions>) -> Connection {
    let mut c = Connection::new(9);
    c.auth.required = true;
    c.auth.peer_user = Some(CERT_USER.to_owned());
    c.auth.client_addr = Some("127.0.0.1:35608".into());
    c.auth.server_addr = Some("127.0.0.1:27017".into());
    let bytes = OpMsg::encode(
        1,
        0,
        0,
        &doc(&[
            (AUTHENTICATE, 1.into()),
            ("mechanism", "REFDB-X509".into()),
            ("$db", EXTERNAL_DB.into()),
        ])
        .to_bytes()
        .unwrap(),
    );
    let mut r = Connection::new(1);
    r.receive(&bytes);
    let req = r.next_request().unwrap().unwrap();
    assert!(matches!(dispatch_with(&mut c, &req, &info(), Some((m, shared))), Action::Reply(_)));
    c
}

fn set(config: Document) -> Document {
    doc(&[(
        "setClusterParameter",
        Value::Document(doc(&[(AUDIT_CONFIG, Value::Document(config))])),
    )])
}

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("{r:?}"),
    }
}

#[test]
fn audit_config_has_refdbs_forms_and_errors() {
    let mut m = backend();
    let (shared, cap) = sessions(runtime());
    let mut c = root(&mut m, &shared);
    cap.events();
    // Never set: the default, under its own name.
    let r = run(&mut m, &shared, &mut c, doc(&[("getClusterParameter", AUDIT_CONFIG.into())]));
    let p = field(&r, "clusterParameters");
    let Value::Array(list) = p else { panic!("{r:?}") };
    let Value::Document(first) = &list[0] else { panic!() };
    assert_eq!(first.get("_id"), Some(&Value::from(AUDIT_CONFIG)));
    assert_eq!(field(first, "auditConfig.filter"), &Value::Document(Document::new()));
    assert_eq!(field(first, "auditConfig.auditAuthorizationSuccess"), &Value::Boolean(false));
    assert_eq!(
        field(first, "auditConfig.clusterParameterTime"),
        &Value::Timestamp { time: 0, increment: 0 }
    );
    assert_eq!(
        cap.events(),
        [r#"getClusterParameter { "requestedClusterServerParameters" : "auditConfig" }"#]
    );
    let missing = "is missing but a required field";
    let cases: Vec<(Document, i32, String)> = vec![
        (doc(&[("filter", Value::Document(Document::new()))]), 40414, format!("BSON field 'auditConfigDocument.auditAuthorizationSuccess' {missing}")),
        (doc(&[(AAS, true.into())]), 40414, format!("BSON field 'auditConfigDocument.filter' {missing}")),
        (doc(&[(AAS, true.into()), ("bogus", 1.into())]), 40414, format!("BSON field 'auditConfigDocument.filter' {missing}")),
        (doc(&[("filter", 5.into())]), 14, "BSON field 'auditConfigDocument.filter' is the wrong type 'int', expected type 'object'".into()),
        (doc(&[(AAS, "yes".into())]), 14, "BSON field 'auditConfigDocument.auditAuthorizationSuccess' is the wrong type 'string', expected type 'bool'".into()),
        (doc(&[(AAS, true.into()), ("filter", Value::Document(doc(&[("$where", "1".into())])))]), 2, "Failed to parse auditFilter :: caused by :: $where is not allowed in this context".into()),
        (doc(&[(AAS, true.into()), ("filter", Value::Document(Document::new())), ("clusterParameterTime", Value::Timestamp { time: 1, increment: 1 })]), 40413, "BSON field 'auditConfigDocument.clusterParameterTime' is a duplicate field".into()),
        (doc(&[(AAS, true.into()), ("filter", Value::Document(Document::new())), ("_id", "x".into())]), 2, "Can't have multiple _id fields in one document".into()),
    ];
    for (config, code, msg) in cases {
        let r = run(&mut m, &shared, &mut c, set(config.clone()));
        assert_eq!(error(&r), (code, msg), "{config:?}");
    }
    assert!(cap.events().is_empty(), "a refused change records nothing");
    // A change: the parameter events with the value before and the one given, then
    // auditConfigure, which the new filter would leave out.
    let filter = doc(&[("atype", "applicationMessage".into())]);
    let given = doc(&[
        ("filter", Value::Document(filter.clone())),
        (AAS, false.into()),
        ("bogus", 1.into()),
    ]);
    m.now = 1_793_411_000_000;
    assert!(ok(&run(&mut m, &shared, &mut c, set(given))));
    let events = cap.events();
    let before = r#"{ "_id" : "auditConfig", "auditConfig" : { "filter" : {}, "auditAuthorizationSuccess" : false, "clusterParameterTime" : { "$timestamp" : { "t" : 0, "i" : 0 } } } }"#;
    let after = r#"{ "_id" : "auditConfig", "clusterParameterTime" : { "$timestamp" : { "t" : 1793411000, "i" : 1 } }, "filter" : { "atype" : "applicationMessage" }, "auditAuthorizationSuccess" : false, "bogus" : 1 }"#;
    assert_eq!(
        events[0],
        format!(
            r#"setClusterParameter {{ "originalClusterServerParameter" : {before}, "updatedClusterServerParameter" : {after} }}"#
        )
    );
    assert_eq!(
        events[1],
        format!(
            r#"updateCachedClusterServerParameter {{ "originalClusterServerParameter" : {before}, "updatedClusterServerParameter" : {after} }}"#
        )
    );
    assert_eq!(
        events[2],
        r#"auditConfigure { "previous" : { "clusterParameterTime" : { "$timestamp" : { "t" : 0, "i" : 0 } }, "filter" : {}, "auditAuthorizationSuccess" : false }, "config" : { "_id" : "auditConfig", "filter" : { "atype" : "applicationMessage" }, "auditAuthorizationSuccess" : false, "clusterParameterTime" : { "$timestamp" : { "t" : 1793411000, "i" : 1 } } } }"#
    );
    assert_eq!(events.len(), 3);
    // The new filter applies at once: only application messages.
    let msg = |text: Value| doc(&[("logApplicationMessage", text)]);
    assert!(ok(&run(&mut m, &shared, &mut c, msg("hello audit".into()))));
    assert!(ok(&run(&mut m, &shared, &mut c, doc(&[("getClusterParameter", "*".into())]))));
    assert_eq!(cap.events(), [r#"applicationMessage { "msg" : "hello audit" }"#]);
    let r = run(&mut m, &shared, &mut c, msg(5.into()));
    assert_eq!(error(&r), (14, "BSON field 'logApplicationMessage.logApplicationMessage' is the wrong type 'int', expected type 'string'".into()));
    let r = run(&mut m, &shared, &mut c, msg(Value::Null));
    assert_eq!(
        error(&r),
        (40414, format!("BSON field 'logApplicationMessage.logApplicationMessage' {missing}"))
    );
}

#[test]
fn the_parameter_needs_the_audit_log_and_its_runtime_configuration() {
    let config = doc(&[("filter", Value::Document(Document::new())), (AAS, true.into())]);
    let mut m = backend();
    let (shared, _) = sessions(Options { destination: Some("console"), ..Options::default() });
    let mut c = root(&mut m, &shared);
    let r = run(&mut m, &shared, &mut c, set(config.clone()));
    assert_eq!(error(&r), (345, "Runtime audit configuration has not been enabled".into()));
    let (shared, _) = sessions(Options::default());
    let mut c = root(&mut m, &shared);
    let r = run(&mut m, &shared, &mut c, set(config));
    assert_eq!(error(&r), (344, "Auditing is not enabled".into()));
    // Without an audit log the message is taken and goes nowhere; getClusterParameter answers.
    assert!(ok(&run(&mut m, &shared, &mut c, doc(&[("logApplicationMessage", "x".into())]))));
    assert!(ok(&m.run(ADMIN, doc(&[("logApplicationMessage", "y".into())]))));
    assert!(ok(&run(
        &mut m,
        &shared,
        &mut c,
        doc(&[("getClusterParameter", AUDIT_CONFIG.into())])
    )));
}

#[test]
fn other_parameters_record_their_events_and_other_nodes_pick_up_the_filter() {
    let mut m = backend();
    let (one, cap1) = sessions(runtime());
    let (two, cap2) = sessions(runtime());
    let mut c1 = root(&mut m, &one);
    let mut c2 = root(&mut m, &two);
    cap1.events();
    cap2.events();
    // defaultMaxTimeMS: the value before in its stored form, the one given as it came.
    m.now = 1_793_411_920_000;
    let change = doc(&[(
        "setClusterParameter",
        Value::Document(doc(&[(
            "defaultMaxTimeMS",
            Value::Document(doc(&[("readOperations", 5.into())])),
        )])),
    )]);
    assert!(ok(&run(&mut m, &one, &mut c1, change)));
    let before = r#"{ "_id" : "defaultMaxTimeMS", "clusterParameterTime" : { "$timestamp" : { "t" : 0, "i" : 0 } }, "readOperations" : { "$numberLong" : "0" } }"#;
    let after = r#"{ "_id" : "defaultMaxTimeMS", "clusterParameterTime" : { "$timestamp" : { "t" : 1793411920, "i" : 1 } }, "readOperations" : 5 }"#;
    let events = cap1.events();
    assert_eq!(
        events[0],
        format!(
            r#"setClusterParameter {{ "originalClusterServerParameter" : {before}, "updatedClusterServerParameter" : {after} }}"#
        )
    );
    assert!(events[1].starts_with("updateCachedClusterServerParameter "));
    // An unknown name is recorded as asked, before the error.
    let r = run(&mut m, &one, &mut c1, doc(&[("getClusterParameter", "nope".into())]));
    assert!(!ok(&r));
    assert_eq!(
        cap1.events(),
        [r#"getClusterParameter { "requestedClusterServerParameters" : "nope" }"#]
    );
    // Node one changes the filter; node two applies it within a second, with its own events.
    let filter = doc(&[("atype", "applicationMessage".into())]);
    let config = doc(&[("filter", Value::Document(filter)), (AAS, false.into())]);
    assert!(ok(&run(&mut m, &one, &mut c1, set(config))));
    cap1.events();
    m.now += 1_500;
    assert!(ok(&run(&mut m, &two, &mut c2, doc(&[("logApplicationMessage", "on two".into())]))));
    let events = cap2.events();
    assert!(events[0].starts_with("updateCachedClusterServerParameter "), "{events:?}");
    assert!(events[1].starts_with("auditConfigure "), "{events:?}");
    assert_eq!(events[2], r#"applicationMessage { "msg" : "on two" }"#);
    assert_eq!(events.len(), 3, "{events:?}");
    // A second later both nodes find the configuration they already apply: nothing more.
    m.now += 1_500;
    for (shared, c, cap) in [(&two, &mut c2, &cap2), (&one, &mut c1, &cap1)] {
        assert!(ok(&run(&mut m, shared, c, doc(&[("logApplicationMessage", "again".into())]))));
        assert_eq!(cap.events(), [r#"applicationMessage { "msg" : "again" }"#]);
    }
}

#[test]
fn audit_authorization_success_is_a_server_parameter() {
    let get = || doc(&[("getParameter", 1.into()), (AAS, 1.into())]);
    let set = |v: Value| doc(&[("setParameter", 1.into()), (AAS, v)]);
    // With an audit log and no runtime configuration: read, changed, refused as a string.
    let mut m = backend();
    let (shared, _) = sessions(Options { destination: Some("console"), ..Options::default() });
    let mut c = root(&mut m, &shared);
    assert_eq!(field(&run(&mut m, &shared, &mut c, get()), AAS), &Value::Boolean(false));
    let r = run(&mut m, &shared, &mut c, set(true.into()));
    assert_eq!(field(&r, "was"), &Value::Boolean(false));
    assert_eq!(field(&run(&mut m, &shared, &mut c, get()), AAS), &Value::Boolean(true));
    let r = run(&mut m, &shared, &mut c, set(0.into()));
    assert!(ok(&r) && field(&r, "was") == &Value::Boolean(true));
    let r = run(&mut m, &shared, &mut c, set("true".into()));
    assert_eq!(error(&r), (2, "auditAuthorizationSuccess expects bool, got string".into()));
    // With the runtime configuration it comes from auditConfig.
    let (shared, _) = sessions(runtime());
    let mut c = root(&mut m, &shared);
    let r = run(&mut m, &shared, &mut c, set(true.into()));
    assert_eq!(
        error(&r),
        (2, "auditAuthorizationSuccess may not be changed via set parameter when runtime audit configuration is enabled".into())
    );
    // Without an audit log the parameter still exists.
    let (shared, _) = sessions(Options::default());
    let mut c = root(&mut m, &shared);
    assert!(ok(&run(&mut m, &shared, &mut c, set(true.into()))));
    assert_eq!(field(&run(&mut m, &shared, &mut c, get()), AAS), &Value::Boolean(true));
}

/// The OCSP parameters, with the defaults a `refnode` 9.0.4 shows (measured; 013 FR-031).
#[test]
fn the_ocsp_parameters_read_as_refnode_shows_them() {
    let mut m = backend();
    let (shared, _) = sessions(Options::default());
    let mut c = root(&mut m, &shared);
    let r = run(&mut m, &shared, &mut c, doc(&[("getParameter", "*".into())]));
    assert_eq!(field(&r, "ocspEnabled"), &Value::Boolean(true));
    assert_eq!(field(&r, "ocspStaplingRefreshPeriodSecs"), &Value::Int64(-1));
    assert_eq!(field(&r, "ocspValidationRefreshPeriodSecs"), &Value::Int64(-1));
    assert_eq!(field(&r, "tlsOCSPStaplingTimeoutSecs"), &Value::Int32(-1));
    assert_eq!(field(&r, "tlsOCSPVerifyTimeoutSecs"), &Value::Int32(4));
    let r = run(
        &mut m,
        &shared,
        &mut c,
        doc(&[("setParameter", 1.into()), ("ocspEnabled", false.into())]),
    );
    assert_eq!(error(&r).0, 20);
}

/// A login outside a command (the HTTP API's credentials, 019 FR-015) gives back what the login
/// gave; one that logs no one in and had no attempt in progress writes no `authenticate` event.
#[test]
fn a_login_outside_a_command_that_logs_no_one_in_writes_no_event() {
    let (shared, capture) = sessions(runtime());
    let mut m = backend();
    let mut c = Connection::new(3);
    c.auth.required = true;
    let r = teseladb_wire::dispatch::login(&mut c, &mut m, &shared, |_, _| Ok(7));
    assert_eq!(r.unwrap(), 7);
    let refused = teseladb_wire::error::CommandError::new(
        teseladb_wire::error::AUTHENTICATION_FAILED,
        "Authentication failed.",
    );
    let r: Result<(), _> =
        teseladb_wire::dispatch::login(&mut c, &mut m, &shared, |_, _| Err(refused.clone()));
    assert_eq!(r.unwrap_err(), refused);
    assert!(capture.events().is_empty());
    assert!(c.auth.user.is_none());
}
