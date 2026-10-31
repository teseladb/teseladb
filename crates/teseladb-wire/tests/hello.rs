//! The handshake as a `refrouter` answers it (006 T008, FR-003, FR-004; contract
//! `hello-response.md`): fields and their order, `isMaster`, compression, client metadata, the
//! awaitable `hello`, `loadBalanced`, and the errors a `refrouter` gives (measured against RefDb
//! 7.0's `refrouter`; the version fields are 9.0's).

use teseladb_bson::{Document, Value};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{
    Action, ClusterTime, NodeInfo, Topology, dispatch, hello_after_wait,
};
use teseladb_wire::error;
use teseladb_wire::msg::{HEADER_LEN, Header, opcode};
use teseladb_wire::{MAX_WIRE_VERSION, MIN_WIRE_VERSION};

const PID: [u8; 12] = [0x6a, 0xe4, 0x6a, 0x38, 0x4b, 0x94, 0xcd, 0xb3, 0x52, 0xa3, 0x8f, 0x7c];

fn node() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: PID, counter: 0 },
        cluster_time: ClusterTime { time: 1_793_354_317, increment: 2 },
        now_ms: 1_793_354_317_123,
        keys: None,
    }
}

fn request(conn: &mut Connection, cmd: Document) -> Request {
    let body = cmd.to_bytes().unwrap();
    let mut m = Vec::new();
    let len = (HEADER_LEN + 5 + body.len()) as i32;
    Header { length: len, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }.write(&mut m);
    m.extend_from_slice(&[0, 0, 0, 0, 0]);
    m.extend_from_slice(&body);
    conn.receive(&m);
    conn.next_request().unwrap().unwrap()
}

fn run(conn: &mut Connection, cmd: Document) -> Action {
    let req = request(conn, cmd);
    dispatch(conn, &req, &node())
}

fn reply(conn: &mut Connection, cmd: Document) -> Document {
    match run(conn, cmd) {
        Action::Reply(d) => d,
        other => panic!("not a reply: {other:?}"),
    }
}

fn hello() -> Document {
    Document::new().with("hello", 1).with("$db", "admin")
}

fn keys(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

fn error_of(d: &Document) -> (i32, String) {
    match (d.get("code"), d.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("not an error: {d:?}"),
    }
}

#[test]
fn hello_fields_in_refrouter_order() {
    let mut c = Connection::new(17);
    let d = reply(&mut c, hello());
    assert_eq!(
        keys(&d),
        [
            "isWritablePrimary",
            "msg",
            "topologyVersion",
            "maxBsonObjectSize",
            "maxMessageSizeBytes",
            "maxWriteBatchSize",
            "localTime",
            "logicalSessionTimeoutMinutes",
            "connectionId",
            "maxWireVersion",
            "minWireVersion",
            "ok",
            "$clusterTime",
            "operationTime",
        ]
    );
    assert_eq!(d.get("isWritablePrimary"), Some(&Value::Boolean(true)));
    assert_eq!(d.get("msg"), Some(&Value::from("isdbgrid")));
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    assert_eq!(d.get("topologyVersion"), Some(&Value::Document(tv)));
    assert_eq!(d.get("maxBsonObjectSize"), Some(&Value::Int32(16_777_216)));
    assert_eq!(d.get("maxMessageSizeBytes"), Some(&Value::Int32(48_000_000)));
    assert_eq!(d.get("maxWriteBatchSize"), Some(&Value::Int32(100_000)));
    assert_eq!(d.get("localTime"), Some(&Value::DateTime(1_793_354_317_123)));
    assert_eq!(d.get("logicalSessionTimeoutMinutes"), Some(&Value::Int32(30)));
    assert_eq!(d.get("connectionId"), Some(&Value::Int32(17)));
    assert_eq!(d.get("maxWireVersion"), Some(&Value::Int32(MAX_WIRE_VERSION)));
    assert_eq!(d.get("minWireVersion"), Some(&Value::Int32(MIN_WIRE_VERSION)));
    assert_eq!(MAX_WIRE_VERSION, 29);
    assert_eq!(d.get("ok"), Some(&Value::Double(1.0)));
    let ts = Value::Timestamp { time: 1_793_354_317, increment: 2 };
    assert_eq!(d.get("operationTime"), Some(&ts));
    let Some(Value::Document(ct)) = d.get("$clusterTime") else { panic!("no $clusterTime") };
    assert_eq!(ct.get("clusterTime"), Some(&ts));
    let sig = Document::new()
        .with("hash", Value::Binary { subtype: 0, bytes: vec![0; 20] })
        .with("keyId", 0i64);
    assert_eq!(ct.get("signature"), Some(&Value::Document(sig)));
}

#[test]
fn is_master_and_hello_ok() {
    for name in ["isMaster", "ismaster"] {
        let mut c = Connection::new(1);
        let d =
            reply(&mut c, Document::new().with(name, 1).with("helloOk", true).with("$db", "admin"));
        assert_eq!(keys(&d)[..4], ["ismaster", "msg", "topologyVersion", "helloOk"]);
        assert_eq!(d.get("ismaster"), Some(&Value::Boolean(true)));
    }
    // `hello` with `helloOk` echoes it too; other databases answer the same.
    let mut c = Connection::new(1);
    let d =
        reply(&mut c, Document::new().with("hello", 0).with("helloOk", true).with("$db", "test"));
    assert_eq!(d.get("helloOk"), Some(&Value::Boolean(true)));
    // The name is case-sensitive.
    let d = reply(&mut c, Document::new().with("HELLO", 1).with("$db", "admin"));
    assert_eq!(error_of(&d), (59, "no such cmd: HELLO".into()));
}

#[test]
fn compression_is_negotiated_per_connection() {
    let mut c = Connection::new(1);
    let list: Vec<Value> =
        ["zstd", "lz4", "snappy", "zlib", "noop", "zstd"].map(Value::from).to_vec();
    let d = reply(&mut c, hello().with("compression", list));
    assert_eq!(keys(&d)[0], "compression");
    let want: Vec<Value> = ["zstd", "snappy", "zlib"].map(Value::from).to_vec();
    assert_eq!(d.get("compression"), Some(&Value::Array(want.clone())));
    // Later replies on the connection keep the list.
    let d = reply(&mut c, Document::new().with("isMaster", 1).with("$db", "admin"));
    assert_eq!(d.get("compression"), Some(&Value::Array(want)));
    // An empty list, or none offered, leaves the field out.
    let mut c = Connection::new(1);
    let d = reply(&mut c, hello().with("compression", Vec::<Value>::new()));
    assert_eq!(d.get("compression"), None);
    // A string instead of an array.
    let d = reply(&mut c, hello().with("compression", "zlib"));
    assert_eq!(
        error_of(&d),
        (
            14,
            "BSON field 'hello.compression' is the wrong type 'string', expected type 'array'"
                .into()
        )
    );
}

#[test]
fn client_metadata_rules() {
    let os = Document::new().with("type", "Linux");
    let driver = Document::new().with("name", "x").with("version", "1");
    let client = Document::new().with("driver", driver.clone()).with("os", os.clone());
    let mut c = Connection::new(1);
    assert_eq!(
        reply(&mut c, hello().with("client", client.clone())).get("ok"),
        Some(&Value::Double(1.0))
    );
    let d = reply(&mut c, hello().with("client", client.clone()));
    assert_eq!(
        error_of(&d),
        (186, "The client metadata document may only be sent in the first hello".into())
    );
    let cases = [
        (Value::Int32(5), 14, "The client metadata document must be a document"),
        (
            Value::Document(Document::new().with("os", os.clone())),
            183,
            "Missing required sub-document 'driver' in the client metadata document",
        ),
        (
            Value::Document(Document::new().with("driver", driver.clone())),
            183,
            "Missing required sub-document 'os' in the client metadata document",
        ),
        (
            Value::Document(client.clone().with("pad", "a".repeat(600))),
            185,
            "The client metadata document must be less then or equal to 512 bytes",
        ),
        (
            Value::Document(
                client.clone().with("application", Document::new().with("name", "a".repeat(200))),
            ),
            184,
            "The 'application.name' field must be less then or equal to 128 bytes in the client metadata document",
        ),
    ];
    for (value, code, msg) in cases {
        let mut c = Connection::new(1);
        let d = reply(&mut c, hello().with("client", value));
        assert_eq!(error_of(&d), (code, msg.into()));
    }
}

#[test]
fn awaitable_hello() {
    let tv = |counter: Value| {
        Document::new().with("processId", Value::ObjectId(PID)).with("counter", counter)
    };
    let mut c = Connection::new(1);
    // Same process and counter: wait up to maxAwaitTimeMS.
    let a = run(
        &mut c,
        hello().with("topologyVersion", tv(Value::Int64(0))).with("maxAwaitTimeMS", 500),
    );
    assert!(matches!(a, Action::Await { max_await_ms: 500, exhaust: false, .. }));
    // Another process: answer now.
    let other = Document::new().with("processId", Value::ObjectId([1; 12])).with("counter", 0i64);
    let a = run(&mut c, hello().with("topologyVersion", other).with("maxAwaitTimeMS", 500));
    assert!(matches!(a, Action::Reply(_)));
    // Errors, as a refrouter gives them.
    let cases: Vec<(Document, i32, &str)> = vec![
        (
            hello().with("maxAwaitTimeMS", 5),
            51760,
            "A request with 'maxAwaitTimeMS' must include a 'topologyVersion'",
        ),
        (
            hello().with("topologyVersion", tv(Value::Int64(0))),
            51760,
            "A request with a 'topologyVersion' must include 'maxAwaitTimeMS'",
        ),
        (
            hello().with("topologyVersion", tv(Value::Int64(-1))).with("maxAwaitTimeMS", 5),
            51758,
            "topologyVersion must have a non-negative counter",
        ),
        (
            hello().with("topologyVersion", tv(Value::Int64(5))).with("maxAwaitTimeMS", 5),
            51761,
            "Received a topology version with counter: 5 which is greater than the refrouter topology version counter: 0",
        ),
        (
            hello().with("topologyVersion", tv(Value::Int64(0))).with("maxAwaitTimeMS", -1),
            2,
            "BSON field 'maxAwaitTimeMS' value must be >= 0, actual value '-1'",
        ),
        (
            hello().with("topologyVersion", tv(Value::Int32(0))).with("maxAwaitTimeMS", 5),
            14,
            "BSON field 'hello.topologyVersion.counter' is the wrong type 'int', expected type 'long'",
        ),
        (
            hello()
                .with(
                    "topologyVersion",
                    Document::new().with("processId", "x").with("counter", 0i64),
                )
                .with("maxAwaitTimeMS", 5),
            14,
            "BSON field 'hello.topologyVersion.processId' is the wrong type 'string', expected type 'objectId'",
        ),
        (
            hello()
                .with("topologyVersion", tv(Value::Int64(0)).with("z", 1))
                .with("maxAwaitTimeMS", 5),
            40415,
            "BSON field 'hello.topologyVersion.z' is an unknown field.",
        ),
        (
            hello().with("topologyVersion", 3).with("maxAwaitTimeMS", 5),
            14,
            "BSON field 'hello.topologyVersion' is the wrong type 'int', expected type 'object'",
        ),
        (
            hello().with("topologyVersion", Document::new()).with("maxAwaitTimeMS", 5),
            40414,
            "BSON field 'hello.topologyVersion.counter' is missing but a required field",
        ),
        (
            hello().with("topologyVersion", tv(Value::Int64(0))).with("maxAwaitTimeMS", "x"),
            14,
            "BSON field 'hello.maxAwaitTimeMS' is the wrong type 'string', expected types '[long, int, double, decimal]'",
        ),
    ];
    for (cmd, code, msg) in cases {
        let Action::Reply(d) = run(&mut c, cmd) else { panic!("{code} should fail") };
        assert_eq!(error_of(&d), (code, msg.into()));
    }
}

#[test]
fn exhaust_and_the_reply_after_waiting() {
    let mut c = Connection::new(1);
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let cmd = hello().with("topologyVersion", tv).with("maxAwaitTimeMS", 300);
    let body = cmd.to_bytes().unwrap();
    let mut m = Vec::new();
    let len = (HEADER_LEN + 5 + body.len()) as i32;
    Header { length: len, request_id: 11, response_to: 0, op_code: opcode::OP_MSG }.write(&mut m);
    m.extend_from_slice(&(1u32 << 16).to_le_bytes());
    m.push(0);
    m.extend_from_slice(&body);
    c.receive(&m);
    let req = c.next_request().unwrap().unwrap();
    let Action::Await { max_await_ms: 500, exhaust: true, hello } = dispatch(&mut c, &req, &node())
    else {
        panic!("an exhaust hello waits")
    };
    let d = hello_after_wait(&c, &hello, &node());
    assert_eq!(d.get("isWritablePrimary"), Some(&Value::Boolean(true)));
}

#[test]
fn load_balanced_adds_service_id() {
    let mut c = Connection::new(1);
    let d = reply(&mut c, hello().with("loadBalanced", true));
    let k = keys(&d);
    let at = k.iter().position(|&x| x == "serviceId").expect("serviceId");
    assert_eq!(k[at - 1], "minWireVersion");
    assert_eq!(d.get("serviceId"), Some(&Value::ObjectId(PID)));
}

#[test]
fn op_query_answers() {
    let mut c = Connection::new(1);
    let q = |ns: &str, cmd: Document| {
        let mut b = 0i32.to_le_bytes().to_vec();
        b.extend_from_slice(ns.as_bytes());
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        b.extend_from_slice(&cmd.to_bytes().unwrap());
        let mut m = Vec::new();
        Header {
            length: (HEADER_LEN + b.len()) as i32,
            request_id: 2,
            response_to: 0,
            op_code: opcode::OP_QUERY,
        }
        .write(&mut m);
        m.extend_from_slice(&b);
        m
    };
    c.receive(&q("admin.$cmd", Document::new().with("isMaster", 1)));
    let req = c.next_request().unwrap().unwrap();
    let Action::Reply(d) = dispatch(&mut c, &req, &node()) else { panic!() };
    assert_eq!(d.get("ismaster"), Some(&Value::Boolean(true)));
    c.receive(&q("admin.$cmd", Document::new().with("ping", 1)));
    let req = c.next_request().unwrap().unwrap();
    let Action::Reply(d) = dispatch(&mut c, &req, &node()) else { panic!() };
    assert_eq!(
        error_of(&d),
        (352, "Unsupported OP_QUERY command: ping. The client driver may require an upgrade. For more details see https://dochub.refdb.org/core/legacy-opcode-removal".into())
    );
    c.receive(&q("test.items", Document::new().with("a", 1)));
    let req = c.next_request().unwrap().unwrap();
    let Action::Reply(d) = dispatch(&mut c, &req, &node()) else { panic!() };
    assert_eq!(keys(&d), ["$err", "code", "ok"]);
    assert_eq!(d.get("code"), Some(&Value::Int32(error::OP_QUERY_REMOVED.code)));
    let bytes = c.reply(&req, &d, false);
    assert_eq!(
        i32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        teseladb_wire::query::QUERY_FAILURE
    );
}

#[test]
fn database_and_unknown_commands() {
    let mut c = Connection::new(1);
    let d = reply(&mut c, Document::new().with("ping", 1).with("$db", "x"));
    assert_eq!(keys(&d), ["ok", "$clusterTime", "operationTime"]);
    let d = reply(&mut c, Document::new().with("ping", 1));
    assert_eq!(
        error_of(&d),
        (40414, "BSON field 'ping.$db' is missing but a required field".into())
    );
    let d = reply(&mut c, Document::new().with("ping", 1).with("$db", 1));
    assert_eq!(
        error_of(&d),
        (14, "BSON field 'ping.$db' is the wrong type 'int', expected type 'string'".into())
    );
    let d = reply(&mut c, Document::new().with("ping", 1).with("$db", ""));
    assert_eq!(error_of(&d), (73, "Invalid database name: ''".into()));
    let d = reply(&mut c, Document::new().with("nosuch", 1).with("$db", "admin"));
    assert_eq!(error_of(&d), (59, "no such cmd: nosuch".into()));
    // A `refrouter` 9.0.4 has no `reIndex` (009 FR-020, measured).
    let d = reply(&mut c, Document::new().with("reIndex", "c").with("$db", "t"));
    assert_eq!(error_of(&d), (59, "no such cmd: reIndex".into()));
    assert_eq!(keys(&d), ["ok", "errmsg", "code", "codeName", "$clusterTime", "operationTime"]);
    let d = reply(&mut c, Document::new().with("$db", "admin"));
    assert_eq!(error_of(&d).0, 59);
}

#[test]
fn negotiation_keeps_only_enabled_compressors() {
    use teseladb_wire::compressed::Compressor;
    let mut c = Connection::with_compressors(1, &[Compressor::Zlib, Compressor::Snappy]);
    let list: Vec<Value> = ["zstd", "zlib", "snappy"].map(Value::from).to_vec();
    let d = reply(&mut c, hello().with("compression", list));
    let want: Vec<Value> = ["zlib", "snappy"].map(Value::from).to_vec();
    assert_eq!(d.get("compression"), Some(&Value::Array(want)));
}

#[test]
fn field_types_as_refrouter_checks_them() {
    let tv = |d: Document| hello().with("topologyVersion", d).with("maxAwaitTimeMS", 5);
    let cases: Vec<(Document, i32, &str)> = vec![
        (
            tv(Document::new().with("processId", Value::ObjectId(PID))),
            40414,
            "BSON field 'hello.topologyVersion.counter' is missing but a required field",
        ),
        (
            hello().with("compression", vec![Value::Int32(1)]),
            14,
            "BSON field 'hello.compression.0' is the wrong type 'int', expected type 'string'",
        ),
        (
            hello().with("helloOk", "x"),
            14,
            "BSON field 'hello.helloOk' is the wrong type 'string', expected types '[bool, long, double, decimal, int]'",
        ),
        (
            hello().with("loadBalanced", "x"),
            14,
            "BSON field 'hello.loadBalanced' is the wrong type 'string', expected type 'bool'",
        ),
        (
            hello().with("saslSupportedMechs", 1),
            14,
            "BSON field 'hello.saslSupportedMechs' is the wrong type 'int', expected types '[string, object]'",
        ),
        (
            tv(Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64))
                .with("maxAwaitTimeMS", -1.5),
            2,
            "BSON field 'maxAwaitTimeMS' value must be >= 0, actual value '-1.5'",
        ),
    ];
    for (cmd, code, msg) in cases {
        let mut c = Connection::new(1);
        let d = reply(&mut c, cmd);
        assert_eq!(error_of(&d), (code, msg.into()));
    }
    // Accepted: a user for saslSupportedMechs (a `refrouter` 9.0 lists its mechanisms for any
    // user), a short
    // application name, `helloOk: 1`, a fractional maxAwaitTimeMS.
    let mut c = Connection::new(1);
    let client = Document::new()
        .with("application", Document::new().with("name", "app"))
        .with("driver", Document::new().with("name", "x").with("version", "1"))
        .with("os", Document::new().with("type", "Linux"));
    let d = reply(
        &mut c,
        hello().with("saslSupportedMechs", "admin.u").with("client", client).with("helloOk", 1),
    );
    let mechs = vec![Value::from("SCRAM-SHA-1"), Value::from("SCRAM-SHA-256")];
    assert_eq!(d.get("saslSupportedMechs"), Some(&Value::Array(mechs)));
    assert_eq!(d.get("helloOk"), Some(&Value::Boolean(true)));
    let mut c = Connection::new(1);
    let d = reply(&mut c, hello().with("saslSupportedMechs", "nodot"));
    assert_eq!(
        error_of(&d),
        (2, "UserName must contain a '.' separated database.user pair".into())
    );
    let pid_tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let a = run(&mut c, hello().with("topologyVersion", pid_tv).with("maxAwaitTimeMS", 5.5));
    assert!(matches!(a, Action::Await { max_await_ms: 500, .. }));
    // A driver part that is not a document.
    let d = reply(
        &mut Connection::new(1),
        hello().with("client", Document::new().with("driver", "x").with("os", Document::new())),
    );
    assert_eq!(
        error_of(&d),
        (
            14,
            "The 'driver' field is required to be a BSON document in the client metadata document"
                .into()
        )
    );
    let d = reply(
        &mut Connection::new(1),
        hello().with(
            "client",
            Document::new()
                .with("driver", Document::new().with("version", "1"))
                .with("os", Document::new()),
        ),
    );
    assert_eq!(
        error_of(&d),
        (183, "Missing required field 'driver.name' in the client metadata document".into())
    );
}

#[test]
fn older_counter_answers_at_once() {
    let mut c = Connection::new(1);
    let body = hello()
        .with(
            "topologyVersion",
            Document::new().with("processId", Value::ObjectId(PID)).with("counter", 1i64),
        )
        .with("maxAwaitTimeMS", 500);
    let req = request(&mut c, body);
    let mut info = node();
    info.topology.counter = 3;
    assert!(matches!(dispatch(&mut c, &req, &info), Action::Reply(_)));
}
