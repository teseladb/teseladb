//! 007 T063 (FR-036, SC-011): a tenant's switches of the HTTP port, its cap of API keys and its
//! `maxTimeMS`. With `httpEnabled` off the REST API answers `401` to the tenant's users, and with
//! `mcpEnabled` off `/mcp` does; `createApiKey` refuses with 13 the key past `maxApiKeys`, whoever
//! asks for it, and any key when both parts are off; the service user is held by neither. A
//! command of the tenant's users runs for at most the tenant's `maxTimeMS`, the `getMore` of a
//! plain cursor too, save the `getMore` of a change stream or a tailable cursor and the end of a
//! transaction.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::{Document, Value};
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;

const ROOT_PASSWORD: &str = "root-pw-31";
const PASSWORD: &str = "acme-pw-123";
const TENANT: &str = "acme";
/// The `maxTimeMS` of the tenant in the test of the cap.
const TENANT_MAX_TIME_MS: i64 = 150;
/// The turns of the loop the `$where` of the test of the cap runs for each document: the
/// interpreter's clock stands still within a call, so the time is spent in turns.
const TURNS: i64 = 1_000_000;
/// The documents of the test of the cap.
const SLOW_DOCS: i32 = 20;
/// A `maxTimeMS` a command gives, far past the tenant's.
const LARGE_MAX_TIME_MS: i64 = 600_000;
const UNAUTHORIZED: i32 = 13;
const MAX_TIME_MS_EXPIRED: i32 = 50;

/// The `root` user of a credentials file, as a first start creates it (013 FR-004).
fn root_user() -> Vec<u8> {
    let thread = format!("{:?}", std::thread::current().id())
        .replace(|c: char| !c.is_ascii_alphanumeric(), "");
    let dir =
        std::env::temp_dir().join(format!("teseladb-switches-{}-{thread}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(teseladb_server::pki::ROOT_CREDENTIALS);
    std::fs::write(&file, format!("user=root\npassword={ROOT_PASSWORD}\n")).unwrap();
    let mut random = |b: &mut [u8]| {
        b.fill(7);
        Ok(())
    };
    let doc = teseladb_server::pki::root_user(&dir, &mut random).unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    doc
}

/// A node that asks for authentication, with the HTTP port and the administration port.
fn start() -> Node<MemDisk> {
    let base = config(Vec::new());
    let client = teseladb_server::client_listener::ClientConfig {
        auth: true,
        ..base.client.clone().unwrap()
    };
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let cfg = NodeConfig {
        http: Some(http),
        client: Some(client),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        root_user: Some(root_user()),
        ..base
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

fn admin(node: &Node<MemDisk>, args: &[&str]) -> String {
    let at = node.admin_addr().unwrap().to_string();
    let argv: Vec<String> =
        ["admin", "--node", &at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    assert_eq!(code, 0, "{args:?}: {}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap()
}

/// `tenant limits acme` with `opts`.
fn limits(node: &Node<MemDisk>, opts: &[&str]) {
    let args: Vec<&str> = ["tenant", "limits", TENANT].iter().chain(opts).copied().collect();
    admin(node, &args);
}

/// Logs `c` in as `user` of `admin` with SCRAM-SHA-256: the last answer.
fn login(c: &mut Client, user: &str, password: &str) -> Document {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(user, password, false).unwrap();
    let (mut s, first) = ClientSession::start(mech, user, &prepared, "switch-nonce-0123");
    let bin = |b: Vec<u8>| Value::Binary { subtype: 0, bytes: b };
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", mech.name())
        .with("payload", bin(first))
        .with("options", Document::new().with("skipEmptyExchange", true));
    let r = c.run("admin", start);
    if !ok(&r) {
        return r;
    }
    let Some(Value::Binary { bytes, .. }) = r.get("payload") else { panic!("{r:?}") };
    let fin = s.respond(bytes).unwrap();
    let next =
        Document::new().with("saslContinue", 1).with("conversationId", 1).with("payload", bin(fin));
    c.run("admin", next)
}

fn code(r: &Document) -> Option<i32> {
    match r.get("code") {
        Some(Value::Int32(c)) => Some(*c),
        _ => None,
    }
}

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

/// The status of `GET /v1/databases` with `key`.
fn rest(at: SocketAddr, key: &str) -> u16 {
    let req =
        Request::new("GET", "/v1/databases").header("Authorization", &format!("Bearer {key}"));
    send(at, req).status
}

/// The status of a `tools/list` of `/mcp` with `key`; a `401` says how to authenticate.
fn mcp(at: SocketAddr, key: &str) -> u16 {
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
    let req = Request::new("POST", "/mcp")
        .header("Authorization", &format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .body(list.as_bytes().to_vec());
    let r = send(at, req);
    if r.status == 401 {
        assert!(r.header("www-authenticate").is_some(), "{r:?}");
    }
    r.status
}

/// `createApiKey` of `user`, run by `c`; with `tenant`, in the tenant with `$tenant`.
fn create_key(c: &mut Client, user: &str, tenant: bool) -> Document {
    let mut cmd = Document::new().with("createApiKey", user).with("db", "admin");
    if tenant {
        cmd.push("$tenant", TENANT);
    }
    c.run("admin", cmd)
}

fn key_of(r: &Document) -> String {
    match r.get("key") {
        Some(Value::String(k)) => k.clone(),
        _ => panic!("no key in {r:?}"),
    }
}

fn create_user(root: &mut Client, user: &str) {
    let create = Document::new()
        .with("createUser", user)
        .with("pwd", PASSWORD)
        .with("roles", Value::Array(vec![Value::from("root")]))
        .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
        .with("$tenant", TENANT);
    let r = root.run("admin", create);
    assert!(ok(&r), "{r:?}");
}

/// `want` from `f` within [`READY`]: the limits apply on the node when Group 0 publishes them.
fn becomes<T: PartialEq>(what: &str, want: &T, mut f: impl FnMut() -> T) {
    until(what, || (f() == *want).then_some(()));
}

/// A node with tenant `acme`, its users `ann` and `svc`, and `root` signed in.
fn tenant_node() -> (Node<MemDisk>, Client) {
    let node = start();
    admin(&node, &["tenant", "create", TENANT]);
    let mut root = Client::new(&node);
    assert!(ok(&login(&mut root, "root", ROOT_PASSWORD)));
    create_user(&mut root, "ann");
    create_user(&mut root, "svc");
    (node, root)
}

#[test]
fn the_switches_close_each_part_of_the_http_port_and_the_service_user_passes() {
    let (node, mut root) = tenant_node();
    let at = node.http_addrs()[0];
    limits(&node, &["--service-user", "svc"]);
    let ann = key_of(&create_key(&mut root, "ann", true));
    let svc = key_of(&create_key(&mut root, "svc", true));
    becomes("ann's key on REST", &200, || rest(at, &ann));
    assert_eq!(mcp(at, &ann), 200);
    // REST off: 401 on `/v1` to Ann, `/mcp` still takes her; the service user passes both.
    limits(&node, &["--http", "off"]);
    becomes("REST off", &401, || rest(at, &ann));
    assert_eq!(mcp(at, &ann), 200);
    assert_eq!((rest(at, &svc), mcp(at, &svc)), (200, 200));
    // MCP off as well: no part takes her, and she gets no new key; the service user does.
    limits(&node, &["--mcp", "off"]);
    becomes("MCP off", &401, || mcp(at, &ann));
    assert_eq!(rest(at, &ann), 401);
    assert_eq!((rest(at, &svc), mcp(at, &svc)), (200, 200));
    let mut c = Client::new(&node);
    assert!(ok(&login(&mut c, "ann", PASSWORD)));
    assert_eq!(code(&create_key(&mut c, "ann", false)), Some(UNAUTHORIZED));
    assert_eq!(code(&create_key(&mut root, "ann", true)), Some(UNAUTHORIZED));
    assert!(ok(&create_key(&mut root, "svc", true)));
    // Back on, only `/v1`: REST takes her again and MCP still refuses her.
    limits(&node, &["--http", "on"]);
    becomes("REST on", &200, || rest(at, &ann));
    assert_eq!(mcp(at, &ann), 401);
    // The default tenant is held by no switch of a tenant.
    let own = key_of(&create_key(&mut root, "root", false));
    assert_eq!((rest(at, &own), mcp(at, &own)), (200, 200));
    drop(node.stop());
}

/// The body of `r` as text.
fn text(r: &Response) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

/// A cursor of a tenant's user through REST (019 contract `rest-api.md`): its `next` link gives the next batch, by the
/// names the tenant gives its databases, and closing it works the same.
#[test]
fn a_tenant_users_rest_cursor_follows_its_next_link() {
    let (node, mut root) = tenant_node();
    let at = node.http_addrs()[0];
    let docs =
        Value::Array((0..5).map(|i| Value::Document(Document::new().with("_id", i))).collect());
    let insert =
        Document::new().with("insert", "items").with("documents", docs).with("$tenant", TENANT);
    assert!(ok(&root.run("shop", insert)));
    let key = key_of(&create_key(&mut root, "ann", true));
    let bearer = format!("Bearer {key}");
    let find = Request::new("POST", "/v1/databases/shop/collections/items/find")
        .header("Authorization", &bearer)
        .header("Content-Type", "application/json")
        .body(br#"{"filter":{},"sort":{"_id":1},"batchSize":2}"#.to_vec());
    let first = send(at, find);
    assert_eq!(first.status, 200, "{}", text(&first));
    let body = text(&first);
    let next =
        body.split("\"next\":\"").nth(1).and_then(|r| r.split('"').next()).unwrap().to_owned();
    let more = send(at, Request::new("GET", &next).header("Authorization", &bearer));
    assert_eq!(more.status, 200, "{}", text(&more));
    assert!(text(&more).contains("\"nextBatch\":[{\"_id\":2},{\"_id\":3}]"), "{}", text(&more));
    let cursor = next.split('?').next().unwrap().to_owned();
    let closed = send(at, Request::new("DELETE", &cursor).header("Authorization", &bearer));
    assert_eq!(closed.status, 200, "{}", text(&closed));
    drop(node.stop());
}

#[test]
fn the_key_past_max_api_keys_is_refused_whoever_asks_and_the_service_user_has_no_cap() {
    let (node, mut root) = tenant_node();
    limits(&node, &["--max-api-keys", "2", "--service-user", "svc"]);
    let mut ann = Client::new(&node);
    assert!(ok(&login(&mut ann, "ann", PASSWORD)));
    // Once the node has the cap, the third key is refused.
    becomes("the cap", &Some(UNAUTHORIZED), || code(&create_key(&mut ann, "ann", false)));
    let listed = ann.run("admin", Document::new().with("listApiKeys", 1));
    let Some(Value::Array(keys)) = listed.get("keys") else { panic!("{listed:?}") };
    assert_eq!(keys.len(), 2, "{listed:?}");
    // The default tenant with `$tenant` is refused as well.
    assert_eq!(code(&create_key(&mut root, "ann", true)), Some(UNAUTHORIZED));
    // The service user's keys count for no cap, and take no place of Ann's.
    for _ in 0..3 {
        assert!(ok(&create_key(&mut root, "svc", true)));
    }
    assert_eq!(code(&create_key(&mut root, "ann", true)), Some(UNAUTHORIZED));
    // A higher cap lets one more through.
    limits(&node, &["--max-api-keys", "3"]);
    becomes("the higher cap", &true, || ok(&create_key(&mut ann, "ann", false)));
    assert_eq!(code(&create_key(&mut ann, "ann", false)), Some(UNAUTHORIZED));
    drop(node.stop());
}

/// The `$where` that runs [`TURNS`] turns for each document.
fn slow_filter() -> String {
    format!("function() {{ for (var i = 0; i < {TURNS}; i++) {{}} return true; }}")
}

/// A `find` of `c` whose filter runs [`TURNS`] turns for each document.
fn slow(c: &mut Client, max: Option<i64>) -> Document {
    let mut find = Document::new()
        .with("find", "c")
        .with("filter", Document::new().with("$where", slow_filter()));
    if let Some(m) = max {
        find.push("maxTimeMS", m);
    }
    c.run("app", find)
}

fn ms(n: i64) -> Duration {
    Duration::from_millis(u64::try_from(n).unwrap())
}

#[test]
fn the_tenant_max_time_ms_caps_its_commands_but_not_a_change_stream_or_a_commit() {
    let (node, mut root) = tenant_node();
    let mut ann = Client::new(&node);
    assert!(ok(&login(&mut ann, "ann", PASSWORD)));
    let insert = Document::new().with("insert", "c").with("documents", docs(0..SLOW_DOCS));
    assert!(ok(&ann.run("app", insert)));
    // Without a cap the slow command ends, well past the cap.
    let at = Instant::now();
    let r = slow(&mut ann, None);
    let uncapped = at.elapsed();
    assert!(ok(&r), "{r:?}");
    // A plain cursor opened before the cap, with one document of its slow filter read.
    let one = Document::new()
        .with("find", "c")
        .with("filter", Document::new().with("$where", slow_filter()))
        .with("batchSize", 1);
    let r = ann.run("app", one);
    let Some(Value::Document(cursor)) = r.get("cursor") else { panic!("{r:?}") };
    let Some(Value::Int64(plain)) = cursor.get("id") else { panic!("{cursor:?}") };
    let plain = *plain;
    let cap = TENANT_MAX_TIME_MS.to_string();
    limits(&node, &["--max-time-ms", &cap]);
    // A command that gives none receives the tenant's, and a larger one is cut to it.
    becomes("the cap", &Some(MAX_TIME_MS_EXPIRED), || code(&slow(&mut ann, None)));
    let at = Instant::now();
    assert_eq!(code(&slow(&mut ann, Some(LARGE_MAX_TIME_MS))), Some(MAX_TIME_MS_EXPIRED));
    assert!(at.elapsed() < uncapped, "{:?} against {uncapped:?}", at.elapsed());
    // The `getMore` of a cursor that does not wait for data is held by the cap too.
    let more = Document::new().with("getMore", plain).with("collection", "c");
    assert_eq!(code(&ann.run("app", more)), Some(MAX_TIME_MS_EXPIRED));
    // The default tenant is held by none.
    assert!(ok(&slow(&mut root, None)));
    // The `getMore` of a change stream waits for its own `maxTimeMS`, past the cap.
    let stage = Document::new().with("$changeStream", Document::new());
    let open = Document::new()
        .with("aggregate", "c")
        .with("pipeline", Value::Array(vec![Value::Document(stage)]))
        .with("cursor", Document::new());
    let r = ann.run("app", open);
    assert!(ok(&r), "{r:?}");
    let Some(Value::Document(cursor)) = r.get("cursor") else { panic!("{r:?}") };
    let Some(Value::Int64(id)) = cursor.get("id") else { panic!("{cursor:?}") };
    assert!(batch(&r, "firstBatch").is_empty(), "{r:?}");
    let wait = TENANT_MAX_TIME_MS * 4;
    let more = Document::new().with("getMore", *id).with("collection", "c").with("maxTimeMS", wait);
    let at = Instant::now();
    let r = ann.run("app", more);
    assert!(ok(&r), "{r:?}");
    assert!(at.elapsed() >= ms(wait / 2), "{:?}", at.elapsed());
    // Nor the `getMore` of a tailable cursor that waits for data.
    let capped = Document::new().with("create", "log").with("capped", true).with("size", 4096);
    assert!(ok(&ann.run("app", capped)));
    let insert = Document::new().with("insert", "log").with("documents", docs(0..1));
    assert!(ok(&ann.run("app", insert)));
    let tail = Document::new()
        .with("find", "log")
        .with("tailable", true)
        .with("awaitData", true)
        .with("batchSize", 1);
    let r = ann.run("app", tail);
    let Some(Value::Document(cursor)) = r.get("cursor") else { panic!("{r:?}") };
    let Some(Value::Int64(id)) = cursor.get("id") else { panic!("{cursor:?}") };
    let more =
        Document::new().with("getMore", *id).with("collection", "log").with("maxTimeMS", wait);
    let at = Instant::now();
    let r = ann.run("app", more);
    assert!(ok(&r), "{r:?}");
    assert!(at.elapsed() >= ms(wait / 2), "{:?}", at.elapsed());
    // Committing a transaction is held by no cap either. Its write is: on a busy machine it may
    // pass the cap, which ends the transaction with a write error, and the transaction starts
    // again with the next number, as a driver does.
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![3; 16] });
    let txn = |cmd: Document, number: i64| {
        cmd.with("lsid", lsid.clone())
            .with("txnNumber", Value::Int64(number))
            .with("autocommit", false)
    };
    let mut number = 0;
    let number = until("a transaction whose write fits in the cap", || {
        number += 1;
        let insert = Document::new().with("insert", "c").with("documents", docs(100..101));
        let r = ann.run("app", txn(insert, number).with("startTransaction", true));
        assert!(ok(&r), "{r:?}");
        r.get("writeErrors").is_none().then_some(number)
    });
    let commit = txn(Document::new().with("commitTransaction", 1), number)
        .with("maxTimeMS", LARGE_MAX_TIME_MS);
    let r = ann.run("admin", commit);
    assert!(ok(&r), "{r:?}");
    drop(node.stop());
}
