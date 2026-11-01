//! Fuzzing of the texts and documents the security settings read (013 T010, FR-018, SC-003):
//! key files, `userToDNMapping` and the LDAP query templates (`ldap://` URLs without the scheme),
//! the audit options and their filter, the ends of a connection, the address blocks of the
//! authentication restrictions, the `awsSTSUrl`, and the user, role and privilege documents of
//! `admin.system.users` and `admin.system.roles`. Valid inputs are mutated (bit flips, cuts,
//! insertions, spliced bytes) and fed to the readers, which must answer or fail and never panic.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-003), and `TESELADB_FUZZ_SEED` replays one
//! seed.

use std::time::{Duration, Instant};

use teseladb_auth::actions::ActionType;
use teseladb_auth::audit::{Config, Destination, Endpoint, Format, Options, Schema};
use teseladb_auth::audit_crypt::Compression;
use teseladb_auth::ldap::{AuthzTemplate, Mapping};
use teseladb_auth::privileges::Privilege;
use teseladb_auth::roles::Role;
use teseladb_auth::scram::{Credentials, MIN_ITERATIONS, Mechanism};
use teseladb_auth::users::{RoleName, User, UserName, parse_roles};
use teseladb_auth::{aws, keyfile, restrictions, shell_json};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_types::Rng;

const DEFAULT_ROUNDS: u64 = 200_000;

/// Bytes a mutation inserts: the delimiters of YAML, JSON, LDAP URLs, addresses and BSON.
const INSERTED: [u8; 14] =
    [b'-', b'\'', b'"', b'{', b'[', b':', b'?', b'(', b'/', b'.', b'\\', 0, 0x7f, 0xff];

/// The texts the settings give, one of each kind (all valid).
const TEXTS: [&str; 13] = [
    "abcdefghijklmnopqrstuvwxyz0123456789+/==",
    "- 'abcdefghijklmnopqrstuvwxyz'\n- \"0123456789abcdef0123\"\n",
    r#"[{match: "(.+)@ENG", substitution: "cn={0},dc=x"}, {'match': '(.+)', ldapQuery: 'dc=x??one?(uid={0})'}]"#,
    "ou=groups,dc=x??sub?(&(objectClass=group)(member={USER}))",
    "{PROVIDED_USER}?memberOf?base",
    "{atype: {$in: ['authenticate', 'createUser']}, 'param.db': 'admin'}",
    "{$or: [{users: {$elemMatch: {user: 'u', db: 'test'}}}, {roles: []}]}",
    "127.0.0.1:27017",
    "[::1]:27017",
    "/tmp/refdb-27017.sock",
    "10.0.0.0/8",
    "fe80::1:2:3/64",
    "https://sts.amazonaws.com:443/path",
];

fn mutate(rng: &mut Rng, seeds: &[&[u8]]) -> Vec<u8> {
    let mut out = seeds[rng.range(0, seeds.len() as u64) as usize].to_vec();
    for _ in 0..=rng.range(0, 4) {
        let len = out.len() as u64;
        match rng.range(0, 5) {
            0 if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] ^= 1 << rng.range(0, 8);
            }
            1 if len > 0 => out.truncate(rng.range(0, len) as usize),
            2 => {
                let i = rng.range(0, len + 1) as usize;
                out.insert(i, INSERTED[rng.range(0, INSERTED.len() as u64) as usize]);
            }
            3 => {
                let other = seeds[rng.range(0, seeds.len() as u64) as usize];
                let from = rng.range(0, other.len() as u64 + 1) as usize;
                let cut = rng.range(from as u64, other.len() as u64 + 1) as usize;
                let at = rng.range(0, len + 1) as usize;
                out.splice(at..at, other[from..cut].iter().copied());
            }
            _ if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] = rng.range(0, 256) as u8;
            }
            _ => {}
        }
    }
    out
}

/// One text of the settings, mutated, through every reader of text.
fn texts(rng: &mut Rng) {
    let seeds: Vec<&[u8]> = TEXTS.iter().map(|t| t.as_bytes()).collect();
    let input = mutate(rng, &seeds);
    let text = String::from_utf8_lossy(&input);
    let text = text.as_ref();
    let _ = keyfile::parse(text);
    let _ = UserName::from_dotted(text);
    let _ = RoleName::from_value(&Value::from(text));
    let _ = shell_json::quote_keys(text);
    if let Ok(m) = Mapping::from_config(text) {
        let _ = m.map("alice@ENG");
        let _ = m.map(text);
    }
    if let Ok(t) = AuthzTemplate::parse(text) {
        let _ = t.query("cn=alice,dc=x", text);
    }
    let _ = Endpoint::parse(text);
    let _ = restrictions::address(text);
    let _ = restrictions::cidr(text);
    let _ = restrictions::host(text);
    let block = Document::new()
        .with("clientSource", vec![Value::from(text)])
        .with("serverAddress", vec![Value::from(text)]);
    let _ = restrictions::list_allows(&[Value::Document(block)], Some(text), Some(text));
    let _ = aws::sts_host(text);
    audit_options(rng, text);
}

/// The audit options with the mutated text in one of them and valid values in the others.
fn audit_options(rng: &mut Rng, text: &str) {
    let pick = |rng: &mut Rng, valid: &'static str| -> Option<&str> {
        match rng.range(0, 3) {
            0 => None,
            1 => Some(valid),
            _ => Some(text),
        }
    };
    let o = Options {
        destination: pick(rng, Destination::File.name()),
        format: pick(rng, Format::Json.name()),
        path: pick(rng, "/tmp/audit.json"),
        filter: pick(rng, TEXTS[5]),
        runtime_configuration: rng.range(0, 4) == 0,
        authorization_success_set: rng.range(0, 2) == 0,
        schema: pick(rng, Schema::Ocsf.name()),
        local_key_file: None,
        compression_mode: pick(rng, Compression::Zstd.name()),
        encryption_key_uid: pick(rng, "1"),
        kmip_server_set: rng.range(0, 2) == 0,
    };
    let _ = Config::from_options(&o);
}

fn user_document() -> Document {
    let mut u = User::new(UserName::new("u", "test"));
    u.user_id = Some([7; 16]);
    u.scram_sha256 =
        Some(Credentials::derive(Mechanism::ScramSha256, b"pw", &[1; 28], MIN_ITERATIONS));
    u.roles = vec![RoleName::new("read", "test"), RoleName::new("dbAdmin", "admin")];
    let source = Document::new()
        .with("clientSource", vec![Value::from("127.0.0.1"), Value::from("10.0.0.0/8")])
        .with("serverAddress", vec![Value::from("::1")]);
    u.restrictions = Some(vec![Value::Document(source)]);
    u.custom_data = Some(Document::new().with("x", 1));
    u.to_document()
}

fn role_document() -> Document {
    let resource = Document::new().with("db", "test").with("collection", "c");
    let privilege = Value::Document(Document::new().with("resource", resource).with(
        "actions",
        vec![Value::from(ActionType::Find.name()), Value::from(ActionType::Insert.name())],
    ));
    let cluster = Value::Document(
        Document::new()
            .with("resource", Document::new().with("cluster", true))
            .with("actions", vec![Value::from(ActionType::ServerStatus.name())]),
    );
    let privileges = vec![
        Privilege::from_value(&privilege, "test").unwrap(),
        Privilege::from_value(&cluster, "admin").unwrap(),
    ];
    Role {
        name: RoleName::new("r", "test"),
        privileges,
        roles: vec![RoleName::new("read", "test")],
        restrictions: Some(vec![Value::Document(
            Document::new().with("clientSource", vec![Value::from("127.0.0.1")]),
        )]),
    }
    .to_document()
}

/// A user or role document, mutated as BSON, through the readers of documents.
fn documents(rng: &mut Rng, seeds: &[&[u8]]) {
    let input = mutate(rng, seeds);
    let Ok(raw) = RawDoc::from_bytes(&input) else { return };
    let doc = Document::from(raw);
    let _ = User::from_document(&doc);
    let _ = Role::from_document(&doc);
    let _ = parse_roles(&doc);
    if let Some(Value::Array(list)) = doc.get("privileges") {
        for p in list {
            let _ = Privilege::from_value(p, "test");
        }
    }
    for field in ["authenticationRestrictions", "restrictions"] {
        if let Some(Value::Array(list)) = doc.get(field) {
            let _ = restrictions::list_allows(list, Some("127.0.0.1:5000"), Some("[::1]:27017"));
        }
    }
}

#[test]
fn settings_and_documents_survive_mutated_inputs() {
    let docs = [user_document().to_bytes().unwrap(), role_document().to_bytes().unwrap()];
    let doc_seeds: Vec<&[u8]> = docs.iter().map(Vec::as_slice).collect();
    // The seeds are valid: the mutations start from inputs every reader accepts.
    assert!(keyfile::parse(TEXTS[0]).is_ok() && keyfile::parse(TEXTS[1]).is_ok());
    assert!(Mapping::from_config(TEXTS[2]).is_ok());
    assert!(AuthzTemplate::parse(TEXTS[3]).is_ok() && AuthzTemplate::parse(TEXTS[4]).is_ok());
    assert!(TEXTS[7..10].iter().all(|t| Endpoint::parse(t).is_some()));
    assert!(TEXTS[10..12].iter().all(|t| restrictions::cidr(t).is_some()));
    assert!(aws::sts_host(TEXTS[12]).is_ok());
    let audit = |filter| Options {
        destination: Some(Destination::File.name()),
        format: Some(Format::Json.name()),
        path: Some("/tmp/audit.json"),
        filter: Some(filter),
        ..Options::default()
    };
    assert!(TEXTS[5..7].iter().all(|f| matches!(Config::from_options(&audit(f)), Ok(Some(_)))));
    assert!(User::from_document(&user_document()).is_ok());
    assert!(Role::from_document(&role_document()).is_ok());
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_013c);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..100 {
            texts(&mut rng);
            documents(&mut rng, &doc_seeds);
            rounds += 1;
        }
        if replay.is_some() {
            break;
        }
        match secs {
            Some(s) if start.elapsed() < Duration::from_secs(s) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < DEFAULT_ROUNDS => seed += 1,
            None => break,
        }
    }
    eprintln!("fuzz: {rounds} inputs in {:?}", start.elapsed());
}
