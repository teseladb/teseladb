//! The OCSF audit log (013 T042, FR-031) against the lines the reference enterprise server 9.0.4
//! wrote with `--auditSchema OCSF` (measured on 31-10-2026; time, connection and ports taken from
//! them).

use teseladb_auth::audit::{
    AuditType, Auditor, Client, Config, Destination, Endpoint, Event, Format, Options, ProcessInfo,
};
use teseladb_auth::audit_param::{self, IndexBuildState};
use teseladb_auth::users::{RoleName, UserName};
use teseladb_bson::{Document, Value};

const TS: i64 = 1_793_410_947_189;
/// The connection `8cf14412-020e-44fa-b5c6-4851d27645ef`.
const UUID: [u8; 16] = [
    0x8c, 0xf1, 0x44, 0x12, 0x02, 0x0e, 0x44, 0xfa, 0xb5, 0xc6, 0x48, 0x51, 0xd2, 0x76, 0x45, 0xef,
];

fn head(activity: i32, category: i32, class: i32, actor: &str) -> String {
    format!(
        r#"{{"activity_id":{activity},"category_uid":{category},"class_uid":{class},"time":{TS},"severity_id":1,"type_uid":{},"metadata":{{"correlation_uid":"8cf14412-020e-44fa-b5c6-4851d27645ef","product":"{product}","version":"1.0.0"}},"actor":{{"user":{actor}}}"#,
        class * 100 + activity,
        product = teseladb_names::OCSF_PRODUCT,
    )
}

const ROOT: &str = r#"{"type_id":1,"name":"admin.root","groups":[{"name":"admin.root"}]}"#;
const SOURCE: &str = r#""src_endpoint":{"ip":"127.0.0.1","port":49512},"direct_endpoint":{"ip":"127.0.0.1","port":49512}"#;
const ALL: &str = r#""dst_endpoint":{"ip":"127.0.0.1","port":27017},"src_endpoint":{"ip":"127.0.0.1","port":49512},"direct_endpoint":{"ip":"127.0.0.1","port":49512}"#;

fn root() -> Client {
    Client {
        uuid: UUID,
        local: Endpoint::parse("127.0.0.1:27017").unwrap(),
        remote: Endpoint::parse("127.0.0.1:49512").unwrap(),
        users: vec![UserName::new("root", "admin")],
        roles: vec![RoleName::new("root", "admin")],
    }
}

fn auditor() -> Auditor {
    let o = Options {
        destination: Some(Destination::File.name()),
        format: Some(Format::Json.name()),
        path: Some("/tmp/a.json"),
        schema: Some("OCSF"),
        ..Options::default()
    };
    let mut config = Config::from_options(&o).unwrap().unwrap();
    config.process =
        Some(ProcessInfo { pid: 9, os_name: "Ubuntu".into(), os_version: "22.04".into() });
    Auditor::new(config)
}

fn line(atype: AuditType, param: Document, result: i32, c: &Client) -> String {
    let e = Event { atype, ts_ms: TS, param, result };
    String::from_utf8(auditor().render(&e, c).unwrap()).unwrap().trim_end().to_owned()
}

#[test]
fn user_role_and_catalog_messages_match_the_reference() {
    let c = root();
    let ro = UserName::new("ro", "admin");
    let read_x = RoleName::new("read", "x");
    assert_eq!(
        line(
            AuditType::GrantRolesToUser,
            audit_param::user_roles(&ro, std::slice::from_ref(&read_x)),
            0,
            &c
        ),
        format!(
            r#"{},"user":{{"type_id":1,"name":"admin.ro"}},{SOURCE},"unmapped":{{"atype":"grantRolesToUser","roles":["x.read"]}}}}"#,
            head(7, 3, 3001, ROOT)
        )
    );
    assert_eq!(
        line(AuditType::DropUser, audit_param::drop_user(&ro), 0, &c),
        format!(
            r#"{},{SOURCE},"user":{{"type_id":1,"name":"admin.ro"}}}}"#,
            head(6, 3, 3001, ROOT)
        )
    );
    let r1 = RoleName::new("r1", "admin");
    assert_eq!(
        line(AuditType::DropRole, audit_param::drop_role(&r1), 0, &c),
        format!(
            r#"{},{SOURCE},"unmapped":{{"atype":"dropRole","role":"admin.r1"}}}}"#,
            head(6, 3, 3001, ROOT)
        )
    );
    assert_eq!(
        line(AuditType::RenameCollection, audit_param::rename_collection("t.c", "t.d"), 0, &c),
        format!(
            r#"{},{SOURCE},"entity":{{"name":"t.c","type":"rename_collection_source"}},"entity_result":{{"name":"t.d","type":"rename_collection_destination"}}}}"#,
            head(3, 3, 3004, ROOT)
        )
    );
    assert_eq!(
        line(AuditType::DropIndex, audit_param::drop_index("t.d", "x_1"), 0, &c),
        format!(
            r#"{},{SOURCE},"entity":{{"name":"x_1@t.d","type":"drop_index"}}}}"#,
            head(4, 3, 3004, ROOT)
        )
    );
    let spec = Document::new()
        .with("v", 2)
        .with("key", Document::new().with("_id", 1))
        .with("name", "_id_");
    assert_eq!(
        line(
            AuditType::CreateIndex,
            audit_param::create_index(
                "admin.system.version",
                "_id_",
                &spec,
                IndexBuildState::Started
            ),
            0,
            &c
        ),
        format!(
            r#"{},{SOURCE},"entity":{{"name":"_id_@admin.system.version","type":"create_index","data":{{"v":2,"key":{{"_id":1}},"name":"_id_"}}}},"comment":"IndexBuildState: IndexBuildStarted"}}"#,
            head(1, 3, 3004, ROOT)
        )
    );
    assert_eq!(
        line(AuditType::DropAllRolesFromDatabase, audit_param::database("admin"), 0, &c),
        format!(
            r#"{},{SOURCE},"unmapped":{{"atype":"dropAllRolesFromDatabase","db":"admin"}}}}"#,
            head(6, 3, 3001, ROOT)
        )
    );
}

#[test]
fn authentication_checks_and_process_messages_match_the_reference() {
    let c = root();
    assert_eq!(
        line(
            AuditType::Authenticate,
            audit_param::authenticate(&UserName::new("root", "admin"), "SCRAM-SHA-256"),
            0,
            &c
        ),
        format!(
            r#"{},{ALL},"user":{{"type_id":1,"name":"admin.root"}},"auth_protocol":"SCRAM-SHA-256","unmapped":{{"atype":"authenticate"}}}}"#,
            head(1, 3, 3002, ROOT)
        )
    );
    let args = Document::new().with("insert", "c").with("$db", "t");
    assert_eq!(
        line(AuditType::AuthCheck, audit_param::auth_check("insert", "t.c", Some(&args)), 13, &c),
        format!(
            r#"{},{ALL},"api":{{"operation":"insert","request":{{"uid":"t.c"}},"response":{{"code":13,"error":"Unauthorized"}}}},"unmapped":{{"args":{{"insert":"c","$db":"t"}}}}}}"#,
            head(1, 6, 6003, ROOT)
        )
    );
    assert_eq!(
        line(AuditType::AuthCheck, audit_param::auth_check("count", "t.c", None), 0, &c),
        format!(
            r#"{},{ALL},"api":{{"operation":"count","request":{{"uid":"t.c"}},"response":{{"code":0}}}}}}"#,
            head(0, 6, 6003, ROOT)
        )
    );
    assert_eq!(
        line(AuditType::ApplicationMessage, Document::new().with("msg", "msg"), 0, &c),
        format!(
            r#"{},"unmapped":{{"atype":"applicationMessage","msg":"msg"}}}}"#,
            head(99, 1, 1007, ROOT)
        )
    );
    let system = Client { uuid: UUID, ..Client::system(UUID) };
    assert_eq!(
        line(AuditType::Shutdown, audit_param::shutdown(), 0, &system),
        format!(
            r#"{},"type_id":0,"os":{{"name":"Ubuntu","version":"22.04","type_id":0}}}}"#,
            head(2, 1, 1007, r#"{"type_id":3}"#)
        )
    );
    let options =
        Document::new().with("security", Document::new().with("authorization", "enabled"));
    assert_eq!(
        line(AuditType::Startup, audit_param::startup(&options), 0, &system),
        format!(
            r#"{},"pid":9,"type_id":0,"os":{{"name":"Ubuntu","version":"22.04","type_id":0}},"status_id":1,"unmapped":{{"atype":"startup","startup_options":{{"security":{{"authorization":"enabled"}}}}}}}}"#,
            head(1, 1, 1007, r#"{"type_id":3}"#)
        )
    );
    let logout =
        audit_param::logout(audit_param::DISCONNECTED, &[UserName::new("root", "admin")], &[], TS);
    assert_eq!(
        line(AuditType::Logout, logout, 0, &c),
        format!(
            r#"{},"user":{{"type_id":3}},"message":"Reason: Client has disconnected"}}"#,
            head(2, 3, 3002, r#"{"type_id":3}"#)
        )
    );
    let asked = Document::new().with("requestedClusterServerParameters", "defaultMaxTimeMS");
    assert_eq!(
        line(AuditType::GetClusterParameter, asked, 0, &c),
        format!(
            r#"{},{ALL},"unmapped":{{"requestedClusterServerParameters":"defaultMaxTimeMS"}}}}"#,
            head(2, 6, 6003, ROOT)
        )
    );
    let configure = Document::new()
        .with("previous", Document::new())
        .with("config", Document::new().with("_id", "auditConfig"));
    assert_eq!(
        line(AuditType::AuditConfigure, configure, 0, &c),
        format!(
            r#"{},"unmapped":{{"atype":"auditConfigure","previous":{{}},"config":{{"_id":"auditConfig"}}}}}}"#,
            head(1, 5, 5002, ROOT)
        )
    );
    // Values inside documents are masked as in the reference server's schema (DV-054).
    // Values inside documents are masked as in the reference server's schema (DV-054).
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: UUID.to_vec() });
    let with_uuid = Document::new().with("ping", 1).with("lsid", lsid);
    assert!(
        line(
            AuditType::AuthCheck,
            audit_param::auth_check("ping", "admin", Some(&with_uuid)),
            0,
            &c
        )
        .contains(r####""lsid":{"id":"###"}"####)
    );
}

/// Every other action type TeselaDB writes, with the class and activity of the reference server's
/// messages (measured on 9.0.4) and its own fields.
#[test]
fn each_action_type_has_its_class_and_activity() {
    let c = root();
    let ro = UserName::new("ro", "admin");
    let r1 = RoleName::new("r1", "admin");
    let read_t = [RoleName::new("read", "t")];
    let privilege =
        Value::Document(Document::new().with("actions", Value::Array(vec!["find".into()])));
    let custom = Document::new().with("k", 1);
    let meta = Document::new().with("application", Document::new().with("name", "shell"));
    let cases: Vec<(AuditType, Document, &str, &str)> = vec![
        (
            AuditType::CreateUser,
            audit_param::create_user(&ro, &read_t),
            r#""activity_id":1,"category_uid":3,"class_uid":3001"#,
            r#""user":{"type_id":1,"name":"admin.ro","groups":[{"name":"t.read"}]},"unmapped":{"atype":"createUser","authenticationRestrictions":[]}"#,
        ),
        (
            AuditType::UpdateUser,
            audit_param::update_user(&ro, true, Some(&custom), None),
            r#""activity_id":3,"category_uid":3,"class_uid":3001"#,
            r#""unmapped":{"atype":"updateUser","customData":{"k":1},"authenticationRestrictions":[]}"#,
        ),
        (
            AuditType::RevokeRolesFromUser,
            audit_param::user_roles(&ro, &read_t),
            r#""activity_id":8"#,
            r#""roles":["t.read"]"#,
        ),
        (
            AuditType::CreateRole,
            audit_param::create_role(&r1, &read_t, std::slice::from_ref(&privilege)),
            r#""activity_id":1,"category_uid":3,"class_uid":3001"#,
            r#""unmapped":{"atype":"createRole","role":"admin.r1","roles":["t.read"],"privileges":[{"actions":["find"]}]}"#,
        ),
        (
            AuditType::UpdateRole,
            audit_param::update_role(&r1, Some(&read_t), None),
            r#""activity_id":99"#,
            r#""unmapped":{"atype":"updateRole","role":"admin.r1","roles":["t.read"]}"#,
        ),
        (
            AuditType::GrantRolesToRole,
            audit_param::role_roles(&r1, &read_t),
            r#""activity_id":7"#,
            r#""roles":["t.read"]"#,
        ),
        (
            AuditType::RevokeRolesFromRole,
            audit_param::role_roles(&r1, &read_t),
            r#""activity_id":8"#,
            r#""atype":"revokeRolesFromRole""#,
        ),
        (
            AuditType::GrantPrivilegesToRole,
            audit_param::role_privileges(&r1, std::slice::from_ref(&privilege)),
            r#""activity_id":7"#,
            r#""privileges":[{"actions":["find"]}]"#,
        ),
        (
            AuditType::RevokePrivilegesFromRole,
            audit_param::role_privileges(&r1, &[privilege]),
            r#""activity_id":8"#,
            r#""atype":"revokePrivilegesFromRole""#,
        ),
        (
            AuditType::DropAllUsersFromDatabase,
            audit_param::database("app"),
            r#""activity_id":6"#,
            r#""unmapped":{"atype":"dropAllUsersFromDatabase","allUsersFromDatabase":"app"}"#,
        ),
        (
            AuditType::CreateDatabase,
            audit_param::namespace("t"),
            r#""activity_id":1,"category_uid":3,"class_uid":3004"#,
            r#""entity":{"name":"t","type":"create_database"}"#,
        ),
        (
            AuditType::CreateCollection,
            audit_param::namespace("t.c"),
            r#""class_uid":3004"#,
            r#""type":"create_collection""#,
        ),
        (
            AuditType::DropCollection,
            audit_param::namespace("t.c"),
            r#""activity_id":4"#,
            r#""type":"drop_collection""#,
        ),
        (
            AuditType::DropDatabase,
            audit_param::namespace("t"),
            r#""activity_id":4"#,
            r#""type":"drop_database""#,
        ),
        (
            AuditType::ClientMetadata,
            audit_param::client_metadata(&Document::new(), &meta),
            r#""activity_id":99,"category_uid":4,"class_uid":4001"#,
            r#""unmapped":{"application":{"name":"shell"}}"#,
        ),
        (
            AuditType::SetClusterParameter,
            Document::new().with("x", 1),
            r#""activity_id":1,"category_uid":5,"class_uid":5002"#,
            r#""unmapped":{"atype":"setClusterParameter","x":1}"#,
        ),
        (
            AuditType::UpdateCachedClusterServerParameter,
            Document::new(),
            r#""class_uid":5002"#,
            r#""atype":"updateCachedClusterServerParameter""#,
        ),
        (
            AuditType::RotateLog,
            Document::new().with("y", 2),
            r#""activity_id":99,"category_uid":1,"class_uid":1007"#,
            r#""unmapped":{"atype":"rotateLog","y":2}"#,
        ),
    ];
    for (atype, param, head, body) in cases {
        let l = line(atype, param, 0, &c);
        assert!(l.contains(head), "{atype:?}: {l}");
        assert!(l.contains(body), "{atype:?}: {l}");
    }
    // A refused check with another code, a unix socket client, a client without a user.
    let unix = Client {
        remote: Endpoint::parse("/tmp/teseladb.sock").unwrap(),
        users: Vec::new(),
        roles: Vec::new(),
        ..root()
    };
    let l = line(AuditType::AuthCheck, audit_param::auth_check("find", "t.c", None), 18, &unix);
    assert!(
        l.contains(r#""src_endpoint":{"path":"/tmp/teseladb.sock"}"#)
            && l.contains(r#""error":"Location18""#),
        "{l}"
    );
    assert!(
        l.contains(r#""actor":{"user":{"type_id":1}}"#) && l.contains(r#""activity_id":2,"#),
        "{l}"
    );
    let o = Options { destination: Some("console"), schema: Some("OCSF"), ..Options::default() };
    let no_process = Config::from_options(&o).unwrap().unwrap();
    let e = Event { atype: AuditType::Shutdown, ts_ms: TS, param: Document::new(), result: 0 };
    let text =
        String::from_utf8(Auditor::new(no_process).render(&e, &Client::system(UUID)).unwrap())
            .unwrap();
    assert!(text.contains(r#""os":{"name":"","version":"","type_id":0}"#), "{text}");
}
