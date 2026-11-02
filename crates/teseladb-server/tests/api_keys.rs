//! API keys and Bearer credentials of the HTTP API on three nodes (019 T020, T021, T023; FR-015 to
//! FR-017, SC-005): `teseladb admin apikey create` gives the first key, which manages users and
//! keys over `/v1`; a key carries the roles of its user it was given and no more; and a revoked
//! key is refused by every node in less than a second.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;
use teseladb_server::audit_log::{AuditLog, AuditOpts};
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};

const READY: Duration = Duration::from_secs(30);

fn config(join: Vec<String>) -> NodeConfig {
    NodeConfig {
        cores: Some(2),
        join,
        client: Some(ClientConfig {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            ..ClientConfig::new(Vec::new())
        }),
        ..NodeConfig::default()
    }
}

/// `f` until it gives a value, for at most [`READY`].
fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + READY;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "{what} did not happen");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// SC-005.
const REVOCATION_BUDGET: Duration = Duration::from_secs(1);

fn send(addr: SocketAddr, req: Request) -> Response {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(&req.header("Host", "h").encode().unwrap()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            return r;
        }
    }
}

/// `method target` with `key` as the Bearer credential: status and body.
fn call(addr: SocketAddr, key: &str, method: &str, target: &str, body: &str) -> (u16, Document) {
    let req = Request::new(method, target)
        .header("Authorization", &format!("Bearer {key}"))
        .body(body.as_bytes().to_vec());
    let r = send(addr, req);
    let doc = extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default();
    (r.status, doc)
}

/// The `root` user of a credentials file, as a first start creates it (013 FR-004).
fn root_user() -> Vec<u8> {
    let thread = format!("{:?}", std::thread::current().id())
        .replace(|c: char| !c.is_ascii_alphanumeric(), "");
    let dir =
        std::env::temp_dir().join(format!("teseladb-apikeys-{}-{thread}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(teseladb_server::pki::ROOT_CREDENTIALS);
    std::fs::write(&file, "user=root\npassword=root-pw-31\n").unwrap();
    let mut random = |b: &mut [u8]| {
        b.fill(7);
        Ok(())
    };
    let doc = teseladb_server::pki::root_user(&dir, &mut random).unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    doc
}

fn start(join: Vec<String>) -> Node<MemDisk> {
    start_with(join, None)
}

/// A node, with its audit log in JSON at `audit`.
fn start_with(join: Vec<String>, audit: Option<&std::path::Path>) -> Node<MemDisk> {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let base = config(join.clone());
    let mut client = ClientConfig { auth: true, ..base.client.clone().unwrap() };
    if let Some(path) = audit {
        let opts = AuditOpts {
            destination: Some("file".into()),
            format: Some("JSON".into()),
            path: Some(path.to_str().unwrap().into()),
            ..AuditOpts::default()
        };
        client.audit = AuditLog::open(&opts).unwrap();
    }
    let cfg = NodeConfig {
        http: Some(http),
        client: Some(client),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        root_user: join.is_empty().then(root_user),
        ..base
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

/// `teseladb admin --node <admin port> <args>`: exit code and output.
fn admin(node: &Node<MemDisk>, args: &[&str]) -> (i32, String, String) {
    let at = node.admin_addr().unwrap().to_string();
    let none = std::env::temp_dir().join("teseladb-apikeys-no-pki");
    let mut all = vec!["--node".to_owned(), at, "--dbpath".to_owned()];
    all.push(none.to_str().unwrap().to_owned());
    all.extend(args.iter().map(|a| (*a).to_owned()));
    let o = teseladb_server::admin::parse(&all).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::admin::run(&o, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn text(d: &Document, f: &str) -> String {
    match d.get(f) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{f}: {other:?} in {d:?}"),
    }
}

#[test]
fn keys_carry_their_roles_and_a_revoked_key_is_refused_everywhere_within_a_second() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2).map(|_| start(seed.clone())).collect();
    let at: Vec<SocketAddr> =
        std::iter::once(&first).chain(others.iter()).map(|n| n.http_addrs()[0]).collect();
    // The first key comes from the administration port, printed once.
    let (code, out, err) = admin(&first, &["apikey", "create", "--user", "root", "--db", "admin"]);
    assert_eq!(code, 0, "{err}");
    let root = out.trim().to_owned();
    assert!(teseladb_auth::apikey::is_key(&root), "{out}");
    // Every node takes it; a key that does not exist, or a malformed one, is refused.
    for a in &at {
        let (st, d) = until("the key on every node", || {
            let r = call(*a, &root, "GET", "/v1/databases", "");
            (r.0 == 200).then_some(r)
        });
        assert_eq!(st, 200, "{d:?}");
    }
    let unknown = teseladb_auth::apikey::encode_key(&[9; 32]);
    let req =
        Request::new("GET", "/v1/databases").header("Authorization", &format!("Bearer {unknown}"));
    let r = send(at[0], req);
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").is_some_and(|c| c.contains("invalid_token")), "{r:?}");
    assert_eq!(call(at[0], "tdb_short", "GET", "/v1/databases", "").0, 401);
    // The root key creates a user, and a key of that user with one of its two roles.
    let create = r#"{"createUser":"ann","pwd":"ann-pw-31","roles":[{"role":"read","db":"app"},{"role":"readWrite","db":"app"}]}"#;
    let (st, d) = call(at[1], &root, "POST", "/v1/databases/admin/command", create);
    assert_eq!(st, 200, "{d:?}");
    let ask =
        r#"{"user":"ann","db":"admin","roles":[{"role":"read","db":"app"}],"expiresAfterDays":7}"#;
    let (st, d) = call(at[1], &root, "POST", "/v1/apiKeys", ask);
    assert_eq!(st, 200, "{d:?}");
    let ann = text(&d, "key");
    let key_id = text(&d, "keyId");
    assert_eq!(text(&d, "prefix"), ann[..12]);
    // A role the user does not hold, and a life past the maximum, are refused.
    let bad = r#"{"user":"ann","db":"admin","roles":[{"role":"root","db":"admin"}]}"#;
    assert_eq!(call(at[1], &root, "POST", "/v1/apiKeys", bad).0, 400);
    let long = r#"{"user":"ann","db":"admin","expiresAfterDays":400}"#;
    assert_eq!(call(at[1], &root, "POST", "/v1/apiKeys", long).0, 400);
    let nobody = r#"{"user":"nobody","db":"admin"}"#;
    assert_eq!(call(at[1], &root, "POST", "/v1/apiKeys", nobody).0, 404);
    // Ann's key reads `app` and cannot write it (the key's roles, FR-017).
    let find = "/v1/databases/app/collections/c/find";
    let insert = "/v1/databases/app/collections/c/insertOne";
    assert_eq!(call(at[2], &root, "POST", insert, r#"{"document":{"_id":1}}"#).0, 200);
    let (st, d) = call(at[2], &ann, "POST", find, "{}");
    assert_eq!(st, 200, "{d:?}");
    let (st, d) = call(at[2], &ann, "POST", insert, r#"{"document":{"_id":2}}"#);
    assert_eq!((st, d.get("code")), (403, Some(&Value::Int32(13))), "{d:?}");
    // Ann sees her own keys only, without the key or its hash; she cannot create one with a
    // role her key does not carry, and the list of the root key shows both.
    let (st, d) = call(at[0], &ann, "GET", "/v1/apiKeys", "");
    assert_eq!(st, 200, "{d:?}");
    let Some(Value::Array(keys)) = d.get("keys") else { panic!("{d:?}") };
    assert_eq!(keys.len(), 1, "{d:?}");
    let shown = format!("{d:?}");
    assert!(!shown.contains(&ann) && !shown.contains(&teseladb_auth::apikey::hash_hex(&ann)));
    let widen = r#"{"user":"ann","db":"admin","roles":[{"role":"readWrite","db":"app"}]}"#;
    assert_eq!(call(at[0], &ann, "POST", "/v1/apiKeys", widen).0, 400);
    let (_, d) = call(at[0], &root, "GET", "/v1/apiKeys", "");
    let Some(Value::Array(keys)) = d.get("keys") else { panic!("{d:?}") };
    assert_eq!(keys.len(), 2, "{d:?}");
    let (code, out, _) = admin(&others[0], &["apikey", "list", "--user", "ann"]);
    assert_eq!(code, 0);
    assert!(out.starts_with(&key_id) && !out.contains(&ann), "{out}");
    // Taking the role from the user takes it from the key too.
    let revoke_role = r#"{"revokeRolesFromUser":"ann","roles":[{"role":"read","db":"app"}]}"#;
    assert_eq!(call(at[0], &root, "POST", "/v1/databases/admin/command", revoke_role).0, 200);
    assert_eq!(call(at[2], &ann, "POST", find, "{}").0, 403);
    let grant = r#"{"grantRolesToUser":"ann","roles":[{"role":"read","db":"app"}]}"#;
    assert_eq!(call(at[0], &root, "POST", "/v1/databases/admin/command", grant).0, 200);
    assert_eq!(call(at[2], &ann, "POST", find, "{}").0, 200);
    // SC-005: once revoked, no node takes the key; the revocation answers in under a second.
    for a in &at {
        assert_eq!(call(*a, &ann, "POST", find, "{}").0, 200);
    }
    let started = Instant::now();
    let (st, d) = call(at[1], &root, "DELETE", &format!("/v1/apiKeys/{key_id}"), "");
    assert_eq!(st, 200, "{d:?}");
    let mut refused = Vec::new();
    for a in &at {
        refused.push(call(*a, &ann, "POST", find, "{}").0);
    }
    let took = started.elapsed();
    assert_eq!(refused, [401, 401, 401]);
    assert!(took < REVOCATION_BUDGET, "{took:?}");
    assert_eq!(call(at[0], &root, "DELETE", &format!("/v1/apiKeys/{key_id}"), "").0, 404);
    let (code, _, err) = admin(&first, &["apikey", "revoke", &key_id]);
    assert_eq!(code, 1, "{err}");
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}

/// 019 T022, FR-018: each REST and MCP request goes to the audit log as `httpRequest` with its
/// transport, user, route or tool, status and the key's `keyId`; neither the key, a token nor a
/// body is written there.
#[test]
fn the_audit_log_records_each_request_without_bodies_keys_or_tokens() {
    let dir = std::env::temp_dir().join(format!("teseladb-apikeys-audit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("audit.json");
    let node = start_with(Vec::new(), Some(&path));
    let at = node.http_addrs()[0];
    let (code, out, err) = admin(&node, &["apikey", "create", "--user", "root", "--db", "admin"]);
    assert_eq!(code, 0, "{err}");
    let key = out.trim().to_owned();
    let secret = "s3cret-value-77";
    let body = format!(r#"{{"document":{{"_id":1,"note":"{secret}"}}}}"#);
    let insert = "/v1/databases/app/collections/c/insertOne";
    let (st, d) = until("the key", || {
        let r = call(at, &key, "POST", insert, &body);
        (r.0 == 200).then_some(r)
    });
    assert_eq!(st, 200, "{d:?}");
    let tool = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"find","arguments":{{"database":"app","collection":"c","filter":{{"note":"{secret}"}}}},"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}}}}"#
    );
    let req = Request::new("POST", "/mcp")
        .header("Authorization", &format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .body(tool.into_bytes());
    let r = send(at, req);
    assert_eq!(r.status, 200, "{r:?}");
    let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.c2ln";
    assert_eq!(call(at, jwt, "GET", "/v1/databases", "").0, 401);
    let text = until("the events", || {
        let t = std::fs::read_to_string(&path).ok()?;
        (t.matches(r#""atype" : "httpRequest""#).count() >= 3).then_some(t)
    });
    let rest = r#""transport" : "rest", "method" : "POST", "route" : "/v1/databases/{db}/collections/{coll}/insertOne", "status" : 200, "keyId" : ""#;
    assert!(text.contains(rest), "{text}");
    let mcp = r#""transport" : "mcp", "method" : "POST", "route" : "tools/call", "tool" : "find", "status" : 200"#;
    assert!(text.contains(mcp), "{text}");
    assert!(text.contains(r#""route" : "/v1/databases", "status" : 401"#), "{text}");
    assert!(text.contains(r#""user" : "root""#), "{text}");
    for leaked in [key.as_str(), &key[4..], jwt, secret, &teseladb_auth::apikey::hash_hex(&key)] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
    }
    drop(node.stop());
    let _ = std::fs::remove_dir_all(&dir);
}
