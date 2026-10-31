//! The documents of `admin.system.users` and `admin.system.roles` as Group 0 keeps them
//! (013 T024, FR-013, FR-030): they read back to the same user or role, and a document that does
//! not have their form is refused field by field.

use teseladb_auth::privileges::Privilege;
use teseladb_auth::roles::Role;
use teseladb_auth::scram::{Credentials, Mechanism};
use teseladb_auth::users::{BadDocument, RoleName, User, UserName};
use teseladb_bson::{Document, Value};

fn user() -> User {
    let mut u = User::new(UserName::new("u", "test"));
    u.user_id = Some([7; 16]);
    for m in [Mechanism::ScramSha1, Mechanism::ScramSha256] {
        let c = Credentials::derive(m, b"pw", &vec![1; m.salt_len()], 4096);
        match m {
            Mechanism::ScramSha1 => u.scram_sha1 = Some(c),
            Mechanism::ScramSha256 => u.scram_sha256 = Some(c),
        }
    }
    u.roles = vec![RoleName::new("read", "test")];
    let source = Document::new().with("clientSource", vec![Value::from("127.0.0.1")]);
    u.restrictions = Some(vec![Value::Document(source)]);
    u.custom_data = Some(Document::new().with("x", 1));
    u
}

fn with(d: &Document, k: &str, v: Value) -> Document {
    let mut d = d.clone();
    d.insert(k, v);
    d
}

fn without(d: &Document, k: &str) -> Document {
    let mut d = d.clone();
    d.remove(k);
    d
}

#[test]
fn users_read_back_and_bad_fields_are_named() {
    let u = user();
    let d = u.to_document();
    assert_eq!(User::from_document(&d), Ok(u.clone()));
    let creds = match d.get("credentials") {
        Some(Value::Document(c)) => c.clone(),
        other => panic!("{other:?}"),
    };
    let sha1 = match creds.get("SCRAM-SHA-1") {
        Some(Value::Document(c)) => c.clone(),
        other => panic!("{other:?}"),
    };
    let bad_creds = |c: Document| with(&d, "credentials", Value::Document(c));
    let bad_sha1 = |c: Document| bad_creds(with(&creds, "SCRAM-SHA-1", Value::Document(c)));
    let role = |r: Value| with(&d, "roles", Value::Array(vec![r]));
    let one = |k: &str, v: Value| Value::Document(Document::new().with(k, v));
    let restriction = |r: Value| with(&d, "authenticationRestrictions", Value::Array(vec![r]));
    let cases: [(Document, i32, &str); 27] = [
        (without(&d, "user"), 11, "no user with a string 'user'"),
        (with(&d, "db", Value::from(1)), 11, "no user with a string 'db'"),
        (
            with(&d, "userId", Value::Binary { subtype: 4, bytes: vec![1] }),
            207,
            "uuid must be a 16-byte binary field with UUID (4) subtype",
        ),
        (
            without(&d, "credentials"),
            12,
            "Cannot extract credentials from user documents without a 'credentials' field",
        ),
        (
            with(&d, "credentials", Value::from(1)),
            10065,
            "invalid parameter: expected an object (credentials)",
        ),
        (
            bad_creds(Document::new()),
            12,
            "User documents must provide credentials for SCRAM-SHA-1 and/or SCRAM-SHA-256",
        ),
        (
            bad_creds(with(&creds, "SCRAM-SHA-1", Value::from(1))),
            10065,
            "invalid parameter: expected an object (SCRAM-SHA-1)",
        ),
        (
            bad_sha1(with(&sha1, "salt", Value::from("!!"))),
            50684,
            "credential document SCRAM-SHA-1 failed validation",
        ),
        (
            bad_sha1(with(&sha1, "salt", Value::from("AAAA"))),
            50684,
            "credential document SCRAM-SHA-1 failed validation",
        ),
        (bad_sha1(with(&sha1, "salt", Value::from(1))), 17502, "Missing SCRAM-SHA-1 salt"),
        (bad_sha1(without(&sha1, "serverKey")), 17503, "Missing SCRAM-SHA-1 serverKey"),
        (bad_sha1(without(&sha1, "storedKey")), 17504, "Missing SCRAM-SHA-1 storedKey"),
        (
            bad_sha1(with(&sha1, "iterationCount", Value::from(-1))),
            17501,
            "Invalid or missing SCRAM-SHA-1 iteration count",
        ),
        (
            bad_sha1(with(&sha1, "iterationCount", Value::from("1"))),
            17501,
            "Invalid or missing SCRAM-SHA-1 iteration count",
        ),
        (
            bad_sha1(without(&sha1, "iterationCount")),
            17501,
            "Invalid or missing SCRAM-SHA-1 iteration count",
        ),
        (
            with(&d, "authenticationRestrictions", Value::from(1)),
            12,
            "'authenticationRestrictions' field must be an array",
        ),
        (
            restriction(Value::from(1)),
            12,
            "restriction array sub-documents must be address restriction objects",
        ),
        (
            restriction(one("foo", Value::from(1))),
            2,
            "BSON field 'address restriction.foo' is an unknown field.",
        ),
        (
            restriction(one("clientSource", Value::from("x"))),
            2,
            "BSON field 'address restriction.clientSource' is the wrong type 'string', expected type 'array'",
        ),
        (without(&d, "roles"), 12, "User document needs 'roles' field to be an array"),
        (with(&d, "roles", Value::from(1)), 2, "Invalid roles field, expected array"),
        (role(Value::from(1)), 2, "RoleName must be either a string or an object"),
        (role(Value::from("read")), 2, "RoleName must contain a '.' separated database.role pair"),
        (
            role(Value::from("test.read")),
            12,
            "User document needs values in 'roles' array to be a sub-documents",
        ),
        (role(one("role", Value::from("r"))), 2, "RoleName must contain a field named: db"),
        (role(one("db", Value::from("t"))), 2, "RoleName must contain a field named: role"),
        (
            role(one("role", Value::from(1))),
            2,
            "RoleName must contain a string field named: role. But, has type int",
        ),
    ];
    for (doc, code, msg) in cases {
        assert_eq!(User::from_document(&doc), Err(BadDocument::new(code, msg)), "{doc:?}");
    }
    // What RefDb lets through: no `userId` or one of another form, any `customData`, numbers
    // of other types as the iteration count, fields it does not know, `{role, db}` with more.
    let mut lenient = without(&d, "userId");
    lenient = with(&lenient, "customData", Value::from("x"));
    lenient = with(&lenient, "extra", Value::from(1));
    let sha1 = with(&sha1, "iterationCount", Value::Double(4096.0));
    lenient = with(
        &lenient,
        "credentials",
        Value::Document(with(&creds, "SCRAM-SHA-1", Value::Document(sha1))),
    );
    let read = Document::new().with("role", "read").with("db", "test").with("x", 1);
    lenient = with(&lenient, "roles", Value::Array(vec![Value::Document(read)]));
    let back = User::from_document(&lenient).unwrap();
    assert_eq!(back.user_id, None);
    assert_eq!(back.custom_data, None);
    assert_eq!(back.roles, u.roles);
    assert!(back.to_document().get("userId").is_none());
    let odd_id = with(&d, "userId", Value::from("abc"));
    assert_eq!(User::from_document(&odd_id).unwrap().user_id, None);
}

#[test]
fn roles_read_back_and_bad_fields_are_named() {
    let p = Privilege::from_value(
        &Value::Document(
            Document::new()
                .with("resource", Document::new().with("db", "test").with("collection", "c"))
                .with("actions", vec![Value::from("find")]),
        ),
        "test",
    )
    .unwrap();
    let role = Role {
        name: RoleName::new("r", "test"),
        privileges: vec![p],
        roles: vec![RoleName::new("read", "test")],
        restrictions: Some(vec![Value::from(1)]),
    };
    let d = role.to_document();
    assert_eq!(Role::from_document(&d), Ok(role));
    let privilege = |p: Value| with(&d, "privileges", Value::Array(vec![p]));
    let actions = |a: Vec<Value>| {
        Value::Document(
            Document::new()
                .with("resource", Document::new().with("db", "test").with("collection", ""))
                .with("actions", a),
        )
    };
    let cases: [(Document, i32, &str); 10] = [
        (without(&d, "role"), 12, "no string 'role' in role"),
        (
            with(&d, "privileges", Value::from(1)),
            12,
            "Invalid 'privileges' field in role document 'r@test'",
        ),
        (privilege(Value::from(1)), 12, "Expected privilege document as object, got int"),
        (
            privilege(Value::Document(Document::new().with("actions", vec![Value::from("find")]))),
            40414,
            "BSON field 'resolveRoles.resource' is missing but a required field",
        ),
        (
            privilege(Value::Document(Document::new().with("foo", 1))),
            40414,
            "BSON field 'resolveRoles.resource' is missing but a required field",
        ),
        (
            privilege(actions(vec![Value::from("nosuchaction")])),
            2,
            "'actions' field of privilege resource must not be empty",
        ),
        (
            with(&d, "roles", Value::from(Document::new())),
            2,
            "Invalid 'roles' field in role document 'r@test', expected an array but found object",
        ),
        (
            with(&d, "roles", Value::Array(vec![Value::from(1)])),
            2,
            "RoleName must be either a string or an object",
        ),
        (
            with(&d, "roles", Value::Array(vec![Value::from("read")])),
            2,
            "RoleName must contain a '.' separated database.role pair",
        ),
        (
            with(&d, "authenticationRestrictions", Value::from(1)),
            12,
            "Invalid 'authenticationRestrictions' field in role document 'r@test'",
        ),
    ];
    for (doc, code, msg) in cases {
        assert_eq!(Role::from_document(&doc), Err(BadDocument::new(code, msg)), "{doc:?}");
    }
    // Missing `privileges` and `roles` are empty, a role named `db.role` is that role, and
    // unknown actions are left out (measured).
    let mut lenient = without(&without(&d, "privileges"), "roles");
    let bare = Role::from_document(&lenient).unwrap();
    assert!(bare.privileges.is_empty() && bare.roles.is_empty());
    lenient = with(&lenient, "roles", Value::Array(vec![Value::from("test.read")]));
    lenient = with(
        &lenient,
        "privileges",
        Value::Array(vec![actions(vec![Value::from("find"), Value::from("nosuch")])]),
    );
    let back = Role::from_document(&lenient).unwrap();
    assert_eq!(back.roles, vec![RoleName::new("read", "test")]);
    assert_eq!(back.privileges.len(), 1);
    assert!(Role::builtin(&RoleName::new("r", "test")).is_none());
    // System buckets of another database are outside a role of `test`.
    let other = Value::Document(
        Document::new()
            .with("resource", Document::new().with("db", "other").with("system_buckets", "b"))
            .with("actions", vec![Value::from("find")]),
    );
    assert!(Privilege::from_value(&other, "test").is_err());
}

#[test]
fn mechanisms_credentials_and_names() {
    let ext = User::new(UserName::new("CN=a", "$external"));
    assert_eq!(ext.mechanisms(), ["external"]);
    assert_eq!(ext.credentials_document(), Document::new().with("external", true));
    let mut one = user();
    one.scram_sha1 = None;
    assert_eq!(one.mechanisms(), ["SCRAM-SHA-256"]);
    let creds = one.credentials_document();
    assert_eq!(creds.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["SCRAM-SHA-256"]);
    assert!(teseladb_auth::roles::is_builtin_name("clusterAdmin"));
    let role =
        Role { restrictions: None, ..Role::builtin(&RoleName::new("read", "test")).unwrap() };
    assert!(role.to_document().get("authenticationRestrictions").is_none());
}

#[test]
fn a_conversation_names_its_mechanism() {
    use teseladb_auth::sasl::{Conversation, scram_user};
    let (first, _) = scram_user("test", b"n,,n=u,r=abcdefgh").unwrap();
    let (conv, _) =
        Conversation::start_scram(Mechanism::ScramSha1, &first, Some(user()), "s", true).unwrap();
    assert_eq!(conv.mechanism(), Mechanism::ScramSha1);
}
