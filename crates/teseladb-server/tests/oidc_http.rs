//! OIDC access tokens on the HTTP port end to end (019 T020, T031; FR-015, FR-027, FR-028), with
//! a test identity provider of its own: an HTTPS server with the discovery document and the
//! JWKS, under a CA the node trusts. A token issued for the audience of REFDB-OIDC opens the
//! REST API; a token for `/mcp` carries the resource's URI as audience (RFC 8707), and each is
//! refused where the other is taken. The metadata of `/mcp` name the provider and the resource.
//! The resource's URI never comes from the `Host` the client sends: it is `mcpOptions.resourceUri`
//! or, without it, the address the HTTP port listens on; a node listening only on wildcard
//! addresses takes no token on `/mcp` until `mcpOptions.resourceUri` is set.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use teseladb_auth::sasl::SaslMechanism;
use teseladb_bson::{Document, Value as Bson, extjson};
use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::rsa::RsaPrivateKey;
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_json::{Value, parse};
use teseladb_raftlog::MemDisk;
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::https::Https;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_server::oidc_client::{Oidc, QUIESCE};
use teseladb_server::tls_stream::TlsStream;
use teseladb_tls::{CertifiedKey, ServerConfig, Trust};
use teseladb_x509::PrivateKey;
use teseladb_x509::ext::GeneralName;

const READY: Duration = Duration::from_secs(30);
/// The audience of REFDB-OIDC, for the REST API.
const AUDIENCE: &str = "teseladb-rest";
/// The `Host` the tests send, which the node must not take as the URI of `/mcp`.
const HOST: &str = "db.test:27080";
const KEY_ID: &str = "k1";

fn key() -> RsaPrivateKey {
    let pem = include_str!("../../teseladb-auth/tests/fixtures/oidc/rsa2048.pem");
    match PrivateKey::from_pem(pem, None).unwrap() {
        PrivateKey::Rsa(k, _) => k,
        _ => panic!("not RSA"),
    }
}

fn b64url(bytes: &[u8]) -> String {
    teseladb_x509::pem::base64_encode(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

fn jwks() -> String {
    let k = key();
    let p = k.public_key();
    format!(
        r#"{{"keys": [{{"kty": "RSA", "kid": "{KEY_ID}", "use": "sig", "alg": "RS256", "n": "{}", "e": "{}"}}]}}"#,
        b64url(&p.modulus()),
        b64url(&p.exponent())
    )
}

/// An RS256 access token of `issuer` for `audience`, of user `sub` in the groups `groups`.
fn token(issuer: &str, audience: &str, sub: &str, groups: &str) -> String {
    let exp =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    let header = format!(r#"{{"alg":"RS256","typ":"JWT","kid":"{KEY_ID}"}}"#);
    let claims = format!(
        r#"{{"iss":"{issuer}","aud":"{audience}","sub":"{sub}","exp":{exp},"groups":{groups}}}"#
    );
    let signed = format!("{}.{}", b64url(header.as_bytes()), b64url(claims.as_bytes()));
    let sig = key().sign_pkcs1v15(HashAlgorithm::Sha256, signed.as_bytes()).unwrap();
    format!("{signed}.{}", b64url(&sig))
}

/// The test provider: HTTPS on a port of its own with the certificate `config`, serving the
/// discovery document and the JWKS. Its issuer.
fn provider(config: Arc<ServerConfig>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { return };
            let config = config.clone();
            std::thread::spawn(move || serve(sock, &config, port));
        }
    });
    format!("https://localhost:{port}")
}

fn serve(sock: TcpStream, config: &Arc<ServerConfig>, port: u16) {
    let Ok(mut s) = TlsStream::server(sock, config.clone(), &[]) else { return };
    let mut got = Vec::new();
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.recv() {
            Ok(d) if !d.is_empty() => got.extend_from_slice(&d),
            _ => return,
        }
    }
    let head = String::from_utf8_lossy(&got).into_owned();
    let body = if head.starts_with("GET /.well-known/openid-configuration ") {
        format!(
            r#"{{"issuer": "https://localhost:{port}", "jwks_uri": "https://localhost:{port}/jwks"}}"#
        )
    } else if head.starts_with("GET /jwks ") {
        jwks()
    } else {
        let _ = s.send(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    let answer = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let _ = s.send(answer.as_bytes());
    s.close();
}

/// A CA with a certificate for `localhost` signed by it, in a temporary folder that is removed.
fn pki() -> teseladb_server::pki::Pki {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("teseladb-oidc-http-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
    let params = teseladb_server::pki::PkiParams {
        now,
        node_names: vec![GeneralName::Dns("localhost".into())],
    };
    let pki =
        teseladb_server::pki::load_or_create(&dir, &params, &mut |b| teseladb_sys::fill_random(b))
            .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    pki
}

/// The `root` user of a credentials file, as a first start creates it (013 FR-004).
fn root_user() -> Vec<u8> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("teseladb-oidc-root-{}-{n}", std::process::id()));
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

/// A node that requires credentials, with REFDB-OIDC and the provider of `issuer`, which it
/// reaches trusting `ca`, and its HTTP port on `http`.
fn start(
    issuer: &str,
    ca: teseladb_x509::Certificate,
    http: SocketAddr,
    tls: Option<teseladb_server::tls_port::TlsPort>,
) -> Node<MemDisk> {
    let json = format!(
        r#"[{{"issuer": "{issuer}", "audience": "{AUDIENCE}", "authNamePrefix": "idp", "clientId": "cid", "requestScopes": ["db.read"], "useAuthorizationClaim": true, "authorizationClaim": "groups", "supportsHumanFlows": false}}]"#
    );
    let https = Https::new(Trust::anchors(vec![ca]), Duration::from_secs(10));
    let oidc = Oidc::with_https(&json, https, QUIESCE).unwrap();
    let client = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        auth: true,
        oidc: Some(Arc::new(oidc)),
        mechanisms: vec![SaslMechanism::ScramSha256, SaslMechanism::Oidc],
        ..ClientConfig::new(Vec::new())
    };
    let http = HttpPortConfig { addrs: vec![http], tls, settings: Settings::default() };
    let cfg = NodeConfig {
        cores: Some(2),
        client: Some(client),
        http: Some(http),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        root_user: Some(root_user()),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

fn send(at: SocketAddr, req: Request) -> Response {
    let mut s = TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(&req.header("Host", HOST).encode().unwrap()).unwrap();
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

/// `method target` with Bearer `credential`: status and body.
fn call(
    at: SocketAddr,
    credential: &str,
    method: &str,
    target: &str,
    body: &str,
) -> (u16, Document) {
    let req = Request::new(method, target)
        .header("Authorization", &format!("Bearer {credential}"))
        .body(body.as_bytes().to_vec());
    let r = send(at, req);
    (r.status, extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default())
}

/// A stateless `tools/call` of `find` on `app.c` with Bearer `credential`.
fn mcp_request(at: SocketAddr, node: u64, credential: &str) -> Response {
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}"#;
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{{meta},"name":"find","arguments":{{"connectionId":"{node}.stateless","database":"app","collection":"c","filter":{{}}}}}}}}"#
    );
    let req = Request::new("POST", "/mcp")
        .header("Authorization", &format!("Bearer {credential}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.into_bytes());
    send(at, req)
}

/// [`mcp_request`]: status and answer.
fn mcp_find(at: SocketAddr, node: u64, credential: &str) -> (u16, Value) {
    let r = mcp_request(at, node, credential);
    (r.status, parse(&r.body).unwrap_or(Value::Null))
}

fn found(v: &Value) -> Option<i64> {
    v.get("result")?.get("structuredContent")?.get("queryResultsCount")?.as_i64()
}

/// The first key of `root`, from the administration port.
fn root_key(node: &Node<MemDisk>) -> String {
    let at = node.admin_addr().unwrap().to_string();
    let none = std::env::temp_dir().join("teseladb-oidc-no-pki");
    let args = ["--node", &at, "--dbpath", none.to_str().unwrap(), "apikey", "create"];
    let mut all: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    all.extend(["--user", "root", "--db", "admin"].map(str::to_owned));
    let o = teseladb_server::admin::parse(&all).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::admin::run(&o, &mut out, &mut err);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap().trim().to_owned()
}

/// The provider's test PKI, the provider and a node with its HTTP port on `http`: the node, the
/// address its HTTP port answers on over loopback, and the issuer.
fn setup(http: SocketAddr) -> (Node<MemDisk>, SocketAddr, String) {
    let (node, at, issuer, _) = setup_with(http, false);
    (node, at, issuer)
}

/// [`setup`], with TLS on the HTTP port when `tls`: also the PKI.
fn setup_with(
    http: SocketAddr,
    tls: bool,
) -> (Node<MemDisk>, SocketAddr, String, teseladb_server::pki::Pki) {
    let pki = pki();
    let config = Arc::new(ServerConfig::new(CertifiedKey {
        chain: pki.node_chain.clone(),
        key: pki.node_key.clone(),
    }));
    let issuer = provider(config);
    // Client certificates of the cluster's CA asked for and optional, as with `--tlsCAFile` and
    // `--tlsAllowConnectionsWithoutCertificates`: an API key, a token or a certificate log in.
    let port_tls = tls.then(|| {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("teseladb-oidc-ca-{}-{n}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let ca_file = dir.join("ca.pem");
        std::fs::write(&ca_file, pki.ca.to_pem()).unwrap();
        let opts = teseladb_server::cli::TlsOpts {
            ca_file: Some(ca_file.clone()),
            allow_without_certificates: true,
            ..teseladb_server::cli::TlsOpts::default()
        };
        let port = teseladb_server::tls_port::build(&opts, &pki).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        port
    });
    let node = start(&issuer, pki.ca.clone(), http, port_tls);
    let port = node.http_addrs()[0].port();
    (node, SocketAddr::from(([127, 0, 0, 1], port)), issuer, pki)
}

/// The metadata of `/mcp`: status and document.
fn metadata(at: SocketAddr) -> (u16, Document) {
    let r = send(at, Request::new("GET", "/.well-known/oauth-protected-resource/mcp"));
    (r.status, extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default())
}

/// [`mcp_request`]: status and `WWW-Authenticate`.
fn mcp_challenge(at: SocketAddr, node: u64, credential: &str) -> (u16, String) {
    let r = mcp_request(at, node, credential);
    let challenge = r
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("WWW-Authenticate"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    (r.status, challenge)
}

#[test]
fn tokens_of_the_provider_open_rest_and_mcp_each_with_its_own_audience() {
    let (node, at, issuer) = setup(SocketAddr::from(([127, 0, 0, 1], 0)));
    let root = root_key(&node);
    // The provider's group `readers` reads `app`, through the role its claim names.
    let role =
        r#"{"createRole":"idp/readers","privileges":[],"roles":[{"role":"read","db":"app"}]}"#;
    let (st, d) = call(at, &root, "POST", "/v1/databases/admin/command", role);
    assert_eq!(st, 200, "{d:?}");
    let docs = r#"{"documents":[{"_id":1},{"_id":2}]}"#;
    let (st, d) = call(at, &root, "POST", "/v1/databases/app/collections/c/insertMany", docs);
    assert_eq!(st, 200, "{d:?}");

    // The metadata of `/mcp`, public, name the provider and the resource: the address the
    // HTTP port listens on, not the `Host` the client sent.
    let (st, meta) = metadata(at);
    assert_eq!(st, 200, "{meta:?}");
    let resource = format!("https://{at}/mcp");
    assert_eq!(meta.get("resource"), Some(&Bson::from(resource.as_str())));
    let servers = Bson::Array(vec![Bson::from(issuer.as_str())]);
    assert_eq!(meta.get("authorization_servers"), Some(&servers));
    let scopes = Bson::Array(vec![Bson::from("db.read")]);
    assert_eq!(meta.get("scopes_supported"), Some(&scopes));

    // A token for the audience of REFDB-OIDC reads through REST, and not through MCP.
    let rest_token = token(&issuer, AUDIENCE, "alice", r#"["readers"]"#);
    let find = r#"{"filter":{}}"#;
    let (st, d) = call(at, &rest_token, "POST", "/v1/databases/app/collections/c/find", find);
    assert_eq!(st, 200, "{d:?}");
    let (st, _) = mcp_find(at, node.node_id().0, &rest_token);
    assert_eq!(st, 401);
    // A token for the URI of `/mcp` reads through MCP, and not through REST.
    let mcp_token = token(&issuer, &resource, "alice", r#"["readers"]"#);
    let (st, v) = mcp_find(at, node.node_id().0, &mcp_token);
    assert_eq!((st, found(&v)), (200, Some(2)), "{v:?}");
    let (st, _) = call(at, &mcp_token, "POST", "/v1/databases/app/collections/c/find", find);
    assert_eq!(st, 401);
    // A token issued for the resource the client names in `Host` is someone else's: refused.
    let host_token = token(&issuer, &format!("https://{HOST}/mcp"), "alice", r#"["readers"]"#);
    let (st, challenge) = mcp_challenge(at, node.node_id().0, &host_token);
    assert_eq!(st, 401);
    let url = format!("https://{at}/.well-known/oauth-protected-resource/mcp");
    assert!(challenge.contains(&format!("resource_metadata=\"{url}\"")), "{challenge}");
    // The roles come from the token's groups: without `readers`, nothing is read.
    let (st, d) = call(
        at,
        &token(&issuer, AUDIENCE, "bob", "[]"),
        "POST",
        "/v1/databases/app/collections/c/find",
        find,
    );
    assert_eq!(st, 403, "{d:?}");
    // A token signed by another key, or of an issuer the node does not know, is refused.
    let forged = format!("{}x", &rest_token[..rest_token.len() - 1]);
    assert_eq!(call(at, &forged, "GET", "/v1/databases", "").0, 401);
    let stranger = token("https://other.test", AUDIENCE, "alice", "[]");
    assert_eq!(call(at, &stranger, "GET", "/v1/databases", "").0, 401);
    // Another scheme than Bearer is refused.
    let basic = Request::new("GET", "/v1/databases").header("Authorization", "Basic cm9vdDp4");
    assert_eq!(send(at, basic).status, 401);
    // A call with progress opens its stream with an API key, and is refused without a credential
    // the node takes.
    let progress = |credential: &str| {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","progressToken":"p"}},"name":"find","arguments":{{"connectionId":"{}.stateless","database":"app","collection":"c"}}}}}}"#,
            node.node_id().0
        );
        let req = Request::new("POST", "/mcp")
            .header("Authorization", &format!("Bearer {credential}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body.into_bytes());
        send(at, req)
    };
    let r = progress(&root);
    assert_eq!(r.status, 200);
    assert!(String::from_utf8_lossy(&r.body).contains("notifications/progress"));
    assert_eq!(progress(&rest_token).status, 401);
    drop(node.stop());
}

/// A node whose HTTP port listens only on a wildcard address does not know the URI clients reach
/// `/mcp` by: it takes no OIDC token there, says why, and publishes no metadata, until
/// `mcpOptions.resourceUri` is set. API keys keep working.
#[test]
fn without_a_known_uri_mcp_takes_no_token_until_the_resource_is_set() {
    let (node, at, issuer) = setup(SocketAddr::from(([0, 0, 0, 0], 0)));
    let root = root_key(&node);
    let id = node.node_id().0;
    let host_token = token(&issuer, &format!("https://{HOST}/mcp"), "alice", r#"["readers"]"#);
    let (st, challenge) = mcp_challenge(at, id, &host_token);
    assert_eq!(st, 401);
    assert!(challenge.contains("error=\"invalid_token\""), "{challenge}");
    assert!(challenge.contains("mcpOptions.resourceUri"), "{challenge}");
    let (st, d) = metadata(at);
    assert_eq!(st, 404, "{d:?}");
    assert!(format!("{d:?}").contains("mcpOptions.resourceUri"), "{d:?}");
    assert_eq!(mcp_find(at, id, &root).0, 200);
    // The stream of a session needs a credential too.
    let stream = Request::new("GET", "/mcp").header("Mcp-Session-Id", "s");
    assert_eq!(send(at, stream).status, 401);

    // With `mcpOptions.resourceUri`, tokens for it are taken, whatever `Host` the client sends.
    let fixed = "https://db.example.com/mcp";
    let set = format!(r#"{{"setClusterParameter":{{"mcpOptions":{{"resourceUri":"{fixed}"}}}}}}"#);
    let (st, d) = call(at, &root, "POST", "/v1/databases/admin/command", &set);
    assert_eq!(st, 200, "{d:?}");
    let role =
        r#"{"createRole":"idp/readers","privileges":[],"roles":[{"role":"read","db":"app"}]}"#;
    let (st, d) = call(at, &root, "POST", "/v1/databases/admin/command", role);
    assert_eq!(st, 200, "{d:?}");
    let fixed_token = token(&issuer, fixed, "alice", r#"["readers"]"#);
    let end = std::time::Instant::now() + READY;
    while mcp_find(at, id, &fixed_token).0 != 200 {
        assert!(std::time::Instant::now() < end, "the fixed resource was never taken");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (st, challenge) = mcp_challenge(at, id, &host_token);
    assert_eq!(st, 401);
    let url = "https://db.example.com/.well-known/oauth-protected-resource/mcp";
    assert!(challenge.contains(&format!("resource_metadata=\"{url}\"")), "{challenge}");
    let (st, meta) = metadata(at);
    assert_eq!(st, 200);
    assert_eq!(meta.get("resource"), Some(&Bson::from(fixed)));
    drop(node.stop());
}

/// 019 FR-020: `teseladb mcp --uri https://…` reaches a node whose HTTP port has TLS, trusting
/// the CA of `--tlsCAFile`; progress comes back as the events of a stream, one line each. Without
/// `--tlsCAFile`, with a file that is not a CA, or with another scheme, it stops with a message.
#[test]
fn mcp_over_stdio_reaches_a_node_over_tls() {
    use teseladb_server::mcp_stdio::{McpOpts, run};
    let (node, at, _, pki) = setup_with(SocketAddr::from(([127, 0, 0, 1], 0)), true);
    let key = root_key(&node);
    let dir = std::env::temp_dir().join(format!("teseladb-mcp-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca = dir.join("ca.pem");
    std::fs::write(&ca, pki.ca.to_pem()).unwrap();
    let uri = format!("https://localhost:{}", at.port());
    let o = McpOpts { uri, ca_file: Some(ca), certificate_key_file: None };
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{"progressToken":"p"},"name":"list-databases","arguments":{}}}"#;
    let input = format!("{init}\n{call}\n");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&o, &mut input.as_bytes(), &mut out, &mut err, Some(key.clone()));
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    let text = String::from_utf8(out).unwrap();
    assert!(text.lines().count() >= 2 && text.contains(r#""id":2"#), "{text}");
    let fails = |o: &McpOpts| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(o, &mut input.as_bytes(), &mut out, &mut err, Some(key.clone()));
        (code, String::from_utf8_lossy(&err).into_owned())
    };
    let none = McpOpts { ca_file: None, ..o.clone() };
    assert_eq!(fails(&none), (1, "teseladb mcp: an https:// URI needs --tlsCAFile\n".to_owned()));
    let missing = McpOpts { ca_file: Some(dir.join("nope.pem")), ..o.clone() };
    let (code, msg) = fails(&missing);
    assert!(code == 1 && msg.contains("--tlsCAFile"), "{msg}");
    let not_ca = dir.join("not.pem");
    std::fs::write(&not_ca, "not a certificate").unwrap();
    let (code, msg) = fails(&McpOpts { ca_file: Some(not_ca), ..o.clone() });
    assert!(code == 1 && msg.contains("--tlsCAFile"), "{msg}");
    let (code, msg) = fails(&McpOpts { uri: "ftp://h".to_owned(), ..o.clone() });
    assert!(code == 1 && msg.contains("https:// or http://"), "{msg}");
    drop(node.stop());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A certificate of the CA the port trusts, for the subject `CN=<cn>`.
fn client_identity(pki: &teseladb_server::pki::Pki, cn: &str, seed: u8) -> CertifiedKey {
    use teseladb_x509::generate::{self, CertificateParams};
    use teseladb_x509::{Name, oid};
    let key = PrivateKey::generate_ec(teseladb_crypto::ec::NamedCurve::P256, &[seed; 40]).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
    let name = Name::build(&[(oid::COMMON_NAME, cn)]);
    let p = CertificateParams::node([seed; 16], name, now - 60, now + 86_400);
    let cert = generate::issue(&p, &key.public_key(), &pki.ca, &pki.ca_key).unwrap();
    CertifiedKey { chain: vec![cert], key }
}

/// `method target` over TLS to `localhost`, showing `identity` and sending Bearer `credential`
/// when given, and the fields of `extra`: status and body.
fn call_tls(
    at: SocketAddr,
    pki: &teseladb_server::pki::Pki,
    identity: Option<CertifiedKey>,
    credential: Option<&str>,
    (method, target, body): (&str, &str, &str),
    extra: &[(&str, &str)],
) -> (u16, Document) {
    let mut config = teseladb_tls::ClientConfig::new(Trust::anchors(vec![pki.ca.clone()]));
    config.identity = identity;
    let mut req = Request::new(method, target)
        .header("Host", HOST)
        .header("Connection", "close")
        .body(body.as_bytes().to_vec());
    if let Some(c) = credential {
        req = req.header("Authorization", &format!("Bearer {c}"));
    }
    for (k, v) in extra {
        req = req.header(k, v);
    }
    let sock = TcpStream::connect(at).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut s = TlsStream::client(sock, Arc::new(config), "localhost").unwrap();
    s.send(&req.encode().unwrap()).unwrap();
    let bytes = s.recv_to_end().unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let r = match reader.feed(&bytes).unwrap() {
        Some(r) => r,
        None => reader.finish().unwrap(),
    };
    (r.status, extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default())
}

/// 019 FR-015 with 013 FR-007: on an HTTP port with TLS, a request without `Authorization` that
/// shows a client certificate of a trusted CA logs in as the REFDB-X509 user of `$external`
/// named after its subject, for REST and for `teseladb mcp --tlsCertificateKeyFile`; a
/// certificate whose subject is no user, and a request with neither, get `401`.
#[test]
fn a_client_certificate_logs_in_as_its_x509_user_on_the_http_port() {
    use teseladb_server::mcp_stdio::{McpOpts, run};
    let (node, at, _, pki) = setup_with(SocketAddr::from(([127, 0, 0, 1], 0)), true);
    let root = root_key(&node);
    let user = r#"{"createUser":"CN=mcp-agent","roles":[{"role":"readWrite","db":"app"}]}"#;
    let create = ("POST", "/v1/databases/$external/command", user);
    let (st, d) = call_tls(at, &pki, None, Some(&root), create, &[]);
    assert_eq!(st, 200, "{d:?}");
    let agent = client_identity(&pki, "mcp-agent", 21);
    let nobody = client_identity(&pki, "nobody", 22);
    let insert =
        ("POST", "/v1/databases/app/collections/c/insertMany", r#"{"documents":[{"_id":1}]}"#);
    // With an Idempotency-Key, which belongs to the certificate's user: the retry gets the same
    // answer, and inserts nothing more.
    let key = [("Idempotency-Key", "x509-insert-1")];
    let (st, d) = call_tls(at, &pki, Some(agent.clone()), None, insert, &key);
    assert_eq!(st, 200, "{d:?}");
    let (st, again) = call_tls(at, &pki, Some(agent.clone()), None, insert, &key);
    assert_eq!((st, &again), (200, &d));
    let find = ("POST", "/v1/databases/app/collections/c/find", r#"{"filter":{}}"#);
    assert_eq!(call_tls(at, &pki, Some(nobody.clone()), None, find, &[]).0, 401);
    assert_eq!(call_tls(at, &pki, None, None, find, &[]).0, 401);

    // `teseladb mcp` with the certificate and no API key.
    let dir = std::env::temp_dir().join(format!("teseladb-mcp-x509-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca = dir.join("ca.pem");
    std::fs::write(&ca, pki.ca.to_pem()).unwrap();
    let pem_of = |name: &str, id: &CertifiedKey| {
        let file = dir.join(name);
        let key = teseladb_x509::pem::encode("PRIVATE KEY", &id.key.to_pkcs8());
        std::fs::write(&file, format!("{}{key}", id.chain[0].to_pem())).unwrap();
        file
    };
    let o = McpOpts {
        uri: format!("https://localhost:{}", at.port()),
        ca_file: Some(ca),
        certificate_key_file: Some(pem_of("agent.pem", &agent)),
    };
    // A stateless call, as `mcp_request` sends it.
    let id = node.node_id().0;
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}"#;
    let input = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{{meta},"name":"find","arguments":{{"connectionId":"{id}.stateless","database":"app","collection":"c","filter":{{}}}}}}}}"#
    ) + "\n";
    let relay = |o: &McpOpts| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(o, &mut input.as_bytes(), &mut out, &mut err, None);
        let out = String::from_utf8(out).unwrap();
        (code, out, String::from_utf8_lossy(&err).into_owned())
    };
    let (code, out, err) = relay(&o);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(r#""queryResultsCount":1"#), "{out}");
    let refused = McpOpts { certificate_key_file: Some(pem_of("nobody.pem", &nobody)), ..o };
    let (code, _, err) = relay(&refused);
    assert_eq!((code, err.as_str()), (1, "teseladb mcp: the node refused the credential (401)\n"));
    drop(node.stop());
    std::fs::remove_dir_all(&dir).unwrap();
}
