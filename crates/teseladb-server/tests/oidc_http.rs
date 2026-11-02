//! OIDC access tokens on the HTTP port end to end (019 T020, T031; FR-015, FR-027, FR-028), with
//! a test identity provider of its own: an HTTPS server with the discovery document and the
//! JWKS, under a CA the node trusts. A token issued for the audience of REFDB-OIDC opens the
//! REST API; a token for `/mcp` carries the resource's URI as audience (RFC 8707), and each is
//! refused where the other is taken. The metadata of `/mcp` name the provider and the resource,
//! and `mcpOptions.resourceUri` fixes the resource whatever `Host` the client sends.

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
/// The `Host` the tests send, so the URI of `/mcp` the node derives from it.
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
    let dir = std::env::temp_dir().join(format!("teseladb-oidc-http-{}", std::process::id()));
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
    let dir = std::env::temp_dir().join(format!("teseladb-oidc-root-{}", std::process::id()));
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
/// reaches trusting `ca`.
fn start(issuer: &str, ca: teseladb_x509::Certificate) -> Node<MemDisk> {
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
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
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

/// A stateless `tools/call` of `find` on `app.c` with Bearer `credential`: status and answer.
fn mcp_find(at: SocketAddr, node: u64, credential: &str) -> (u16, Value) {
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}"#;
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{{meta},"name":"find","arguments":{{"connectionId":"{node}.stateless","database":"app","collection":"c","filter":{{}}}}}}}}"#
    );
    let req = Request::new("POST", "/mcp")
        .header("Authorization", &format!("Bearer {credential}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.into_bytes());
    let r = send(at, req);
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

#[test]
fn tokens_of_the_provider_open_rest_and_mcp_each_with_its_own_audience() {
    let pki = pki();
    let config = Arc::new(ServerConfig::new(CertifiedKey {
        chain: pki.node_chain.clone(),
        key: pki.node_key.clone(),
    }));
    let issuer = provider(config);
    let node = start(&issuer, pki.ca.clone());
    let at = node.http_addrs()[0];
    let root = root_key(&node);
    // The provider's group `readers` reads `app`, through the role its claim names.
    let role =
        r#"{"createRole":"idp/readers","privileges":[],"roles":[{"role":"read","db":"app"}]}"#;
    let (st, d) = call(at, &root, "POST", "/v1/databases/admin/command", role);
    assert_eq!(st, 200, "{d:?}");
    let docs = r#"{"documents":[{"_id":1},{"_id":2}]}"#;
    let (st, d) = call(at, &root, "POST", "/v1/databases/app/collections/c/insertMany", docs);
    assert_eq!(st, 200, "{d:?}");

    // The metadata of `/mcp`, public, name the provider and the resource.
    let r = send(at, Request::new("GET", "/.well-known/oauth-protected-resource/mcp"));
    assert_eq!(r.status, 200);
    let meta = extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap();
    let resource = format!("https://{HOST}/mcp");
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

    // With `mcpOptions.resourceUri`, the resource no longer follows `Host`.
    let fixed = "https://db.example.com/mcp";
    let set = format!(r#"{{"setClusterParameter":{{"mcpOptions":{{"resourceUri":"{fixed}"}}}}}}"#);
    let (st, d) = call(at, &root, "POST", "/v1/databases/admin/command", &set);
    assert_eq!(st, 200, "{d:?}");
    let fixed_token = token(&issuer, fixed, "alice", r#"["readers"]"#);
    let end = std::time::Instant::now() + READY;
    while mcp_find(at, node.node_id().0, &fixed_token).0 != 200 {
        assert!(std::time::Instant::now() < end, "the fixed resource was never taken");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(mcp_find(at, node.node_id().0, &mcp_token).0, 401);
    let r = send(at, Request::new("GET", "/.well-known/oauth-protected-resource/mcp"));
    let meta = extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap();
    assert_eq!(meta.get("resource"), Some(&Bson::from(fixed)));
    drop(node.stop());
}
