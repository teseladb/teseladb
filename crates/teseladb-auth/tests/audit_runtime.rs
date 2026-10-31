//! Runtime audit configuration and the audit schema (013 T042, FR-031), against RefDb
//! Enterprise 9.0.4 (measured on 31-10-2026): `auditLog.runtimeConfiguration` and its option
//! errors, `auditLog.schema`, a filter and `auditAuthorizationSuccess` changed while the node
//! runs, and `auditConfigure`, which every filter lets through.

use teseladb_auth::audit::{
    AuditError, AuditType, Auditor, Client, Config, Destination, Endpoint, Event, Options, Schema,
};
use teseladb_bson::{Document, Value};

const TS: i64 = 1_793_387_567_890;

fn client() -> Client {
    Client {
        uuid: [1; 16],
        local: Endpoint::parse("127.0.0.1:27017").unwrap(),
        remote: Endpoint::parse("127.0.0.1:35608").unwrap(),
        users: Vec::new(),
        roles: Vec::new(),
    }
}

fn console(extra: impl FnOnce(&mut Options<'static>)) -> Result<Option<Config>, AuditError> {
    let mut o = Options { destination: Some(Destination::Console.name()), ..Options::default() };
    extra(&mut o);
    Config::from_options(&o)
}

fn recorded(a: &Auditor, atype: AuditType) -> bool {
    let e = Event { atype, ts_ms: TS, param: Document::new(), result: 0 };
    a.render(&e, &client()).is_some()
}

#[test]
fn runtime_configuration_and_schema_options_follow_refdb() {
    let msg = |r: Result<Option<Config>, AuditError>| r.unwrap_err().0;
    // Without a destination, and with what the runtime configuration replaces.
    let none = Options { runtime_configuration: true, ..Options::default() };
    assert_eq!(
        Config::from_options(&none).unwrap_err().0,
        "auditLog.destination is required when auditLog.runtimeConfiguration is specified"
    );
    assert_eq!(
        msg(console(|o| {
            o.runtime_configuration = true;
            o.filter = Some("{atype: 'x'}");
        })),
        "auditLog.filter must not be configured when runtime audit configuration is enabled"
    );
    assert_eq!(
        msg(console(|o| {
            o.runtime_configuration = true;
            o.authorization_success_set = true;
        })),
        "setParameter.auditAuthorizationSuccess must not be configured when runtime audit configuration is enabled"
    );
    let on = console(|o| o.runtime_configuration = true).unwrap().unwrap();
    assert!(on.runtime && on.schema == Schema::Ref);
    // The schema: `ref` or `OCSF`, written exactly so.
    for bad in ["ocsf", "xyz"] {
        assert_eq!(
            msg(console(|o| o.schema = Some(bad))),
            "auditSchema must be one of 'ref' or 'OCSF'"
        );
    }
    assert_eq!(console(|o| o.schema = Some("OCSF")).unwrap().unwrap().schema, Schema::Ocsf);
    assert_eq!(console(|o| o.schema = Some("ref")).unwrap().unwrap().schema, Schema::Ref);
    let schema_alone = Options { schema: Some("OCSF"), ..Options::default() };
    assert_eq!(
        Config::from_options(&schema_alone).unwrap_err().0,
        "auditLog.destination is required when auditLog.schema is specified"
    );
    assert_eq!(Schema::Ocsf.name(), "OCSF");
}

#[test]
fn a_runtime_filter_replaces_the_old_one_and_lets_audit_configure_through() {
    let mut a = Auditor::new(console(|o| o.runtime_configuration = true).unwrap().unwrap());
    assert!(recorded(&a, AuditType::CreateCollection) && !a.records_auth_check(0));
    let filter = Document::new().with("atype", "applicationMessage");
    assert_eq!(a.runtime_version(), None);
    a.configure(&filter, true, (5, 1)).unwrap();
    assert_eq!(a.runtime_version(), Some((5, 1)));
    assert!(recorded(&a, AuditType::ApplicationMessage));
    assert!(!recorded(&a, AuditType::CreateCollection));
    // `auditConfigure` is always recorded (measured with a filter that leaves it out).
    assert!(recorded(&a, AuditType::AuditConfigure));
    assert!(a.records_auth_check(0));
    assert_eq!(a.config().filter_doc, Some(filter.clone()));
    // A filter RefDb cannot use as an audit filter leaves the old one.
    let bad = Document::new().with("$where", "1");
    assert_eq!(
        a.configure(&bad, false, (6, 1)).unwrap_err().0,
        "Failed to parse auditFilter :: caused by :: $where is not allowed in this context"
    );
    assert!(a.records_auth_check(0) && !recorded(&a, AuditType::CreateCollection));
    let unknown = Document::new().with("$foo", Value::Int32(1));
    assert!(
        a.configure(&unknown, false, (6, 1))
            .unwrap_err()
            .0
            .contains("unknown top level operator: $foo")
    );
    // An empty filter records everything again.
    a.configure(&Document::new(), false, (7, 1)).unwrap();
    assert!(recorded(&a, AuditType::CreateCollection) && !a.records_auth_check(0));
    assert_eq!(a.config().filter_doc, Some(Document::new()));
    assert_eq!(AuditType::from_name("auditConfigure"), Some(AuditType::AuditConfigure));
}
