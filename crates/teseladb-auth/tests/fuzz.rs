//! Fuzzing of the SCRAM and SASLprep parsers (013 T010, FR-018, SC-003): the messages of real
//! SCRAM-SHA-1 and SCRAM-SHA-256 conversations, mutated (bit flips, cuts, insertions, spliced
//! bytes) and fed to the server's and the client's side, which must answer or fail and never
//! panic; a mutated client-final message never logs in. The same for PLAIN messages, the LDAP
//! server's answers to a login (a mutated refusal never logs in), LDAP filter texts, and a KMIP
//! server's answers (013 FR-028), measured from a real server.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-003), and `TESELADB_FUZZ_SEED` replays one
//! seed.

use std::time::{Duration, Instant};

use teseladb_auth::kmip::{Answer, Request};
use teseladb_auth::ldap::{self, Login, Mapping};
use teseladb_auth::ldap_ber::{self, tag};
use teseladb_auth::ldap_filter;
use teseladb_auth::plain;
use teseladb_auth::sasl::scram_user;
use teseladb_auth::saslprep::saslprep;
use teseladb_auth::scram::{ClientFirst, ClientSession, Credentials, Mechanism, ServerSession};
use teseladb_types::Rng;

const DEFAULT_ROUNDS: u64 = 20_000;
const SERVER_NONCE: &str = "c2VydmVyLW5vbmNl";
const ITERATIONS: u32 = 4096;

/// One conversation's messages and what each side needs to read them again.
struct Flight {
    mech: Mechanism,
    creds: Credentials,
    prepared: Vec<u8>,
    first: Vec<u8>,
    server_first: Vec<u8>,
    client_final: Vec<u8>,
    server_final: Vec<u8>,
}

fn flight(mech: Mechanism, user: &str, password: &str) -> Flight {
    let prepared = mech.prepare_password(user, password, mech == Mechanism::ScramSha1).unwrap();
    let creds = Credentials::derive(mech, &prepared, b"0123456789abcdef", ITERATIONS);
    let (mut client, first) = ClientSession::start(mech, user, &prepared, "client-nonce");
    let parsed = ClientFirst::parse(&first).unwrap();
    let (mut server, server_first) =
        ServerSession::start(mech, &parsed, creds.clone(), SERVER_NONCE);
    let client_final = client.respond(&server_first).unwrap();
    let server_final = server.finish(&client_final).unwrap();
    client.verify(&server_final).unwrap();
    Flight { mech, creds, prepared, first, server_first, client_final, server_final }
}

fn flights() -> Vec<Flight> {
    vec![
        flight(Mechanism::ScramSha256, "user", "pencil"),
        flight(Mechanism::ScramSha1, "user=,name", "pencil"),
        flight(Mechanism::ScramSha256, "I\u{00AD}X", "\u{2168}\u{00AA}"),
    ]
}

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
                let bytes = [b',', b'=', b'r', b'p', 0xff, 0xc3, 0, b'n'];
                out.insert(i, bytes[rng.range(0, bytes.len() as u64) as usize]);
            }
            3 => {
                let other = seeds[rng.range(0, seeds.len() as u64) as usize];
                let cut = rng.range(0, other.len() as u64 + 1) as usize;
                let at = rng.range(0, len + 1) as usize;
                out.splice(at..at, other[..cut].iter().copied());
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

fn feed(rng: &mut Rng, f: &Flight) {
    let messages: [&[u8]; 4] = [&f.first, &f.server_first, &f.client_final, &f.server_final];
    let input = mutate(rng, &messages);
    // The server's first step and the user it names.
    if let Ok(first) = ClientFirst::parse(&input) {
        let _ = ServerSession::start(f.mech, &first, f.creds.clone(), SERVER_NONCE);
    }
    let _ = scram_user("admin", &input);
    // The server's second step after the real first message: a changed proof never logs in.
    let parsed = ClientFirst::parse(&f.first).unwrap();
    let (mut server, _) = ServerSession::start(f.mech, &parsed, f.creds.clone(), SERVER_NONCE);
    let result = server.finish(&input);
    assert!(
        result.is_err() || input == f.client_final,
        "a mutated proof logged in: {:?} for {:?}",
        String::from_utf8_lossy(&input),
        String::from_utf8_lossy(&f.client_final)
    );
    // The client's steps.
    let (mut client, _) = ClientSession::start(f.mech, "user", &f.prepared, "client-nonce");
    if client.respond(&input).is_ok() {
        let _ = client.verify(&mutate(rng, &messages));
    }
    // SASLprep of whatever text the bytes hold, for a query and for a stored string.
    let text = String::from_utf8_lossy(&input);
    let _ = saslprep(&text, false);
    let _ = saslprep(&text, true);
    ldap_and_plain(rng);
    kmip_answers(rng);
}

/// The LDAP answers of a login that binds directly: success, refusal, a search's entry and end.
fn ldap_answers() -> [Vec<u8>; 3] {
    let result = |t: u8, code: u8| {
        let mut r = ldap_ber::element(tag::ENUMERATED, &[code]);
        ldap_ber::put(&mut r, tag::OCTET_STRING, b"");
        ldap_ber::put(&mut r, tag::OCTET_STRING, b"msg");
        let mut m = ldap_ber::element(tag::INTEGER, &[1]);
        m.extend_from_slice(&ldap_ber::element(t, &r));
        ldap_ber::element(tag::SEQUENCE, &m)
    };
    [result(0x61, 0), result(0x61, 49), result(0x65, 0)]
}

fn ldap_and_plain(rng: &mut Rng) {
    let answers = ldap_answers();
    let seeds: [&[u8]; 6] = [
        &answers[0],
        &answers[1],
        &answers[2],
        b"\0jane\0janepw",
        b"(&(uid=a*b)(|(cn=x)(!(sn~=y))))",
        b"ou=people,dc=x??one?(uid={0})",
    ];
    let input = mutate(rng, &seeds);
    let _ = plain::parse(&input);
    let text = String::from_utf8_lossy(&input);
    let _ = ldap_filter::encode(&text);
    let _ = ldap_filter::Query::parse(&text);
    let _ = ldap::read_message(&input);
    let mapping = Mapping::default();
    let (mut login, _) = Login::start(&mapping, "uid=jane", b"pw", None).unwrap();
    let answer = login.receive(&input);
    // Only a success answer logs in: a mutated refusal never does.
    if matches!(answer, Ok(ldap::Step::Done { .. })) {
        assert!(
            matches!(ldap::read_message(&input), Ok(Some((m, _))) if matches!(m.op, ldap::Op::BindResponse(ref r) if r.code == 0)),
            "a refused bind logged in"
        );
    }
}

fn kmip_fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/kmip/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).unwrap();
    let text = text.trim();
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

/// A KMIP server's answers, mutated: each request reads them or fails, and a key is only ever
/// 32 bytes.
fn kmip_answers(rng: &mut Rng) {
    static ANSWERS: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    let answers = ANSWERS.get_or_init(|| {
        ["create-response", "get-response", "activate-response", "state-response"]
            .iter()
            .map(|n| kmip_fixture(n))
            .collect()
    });
    let seeds: Vec<&[u8]> = answers.iter().map(Vec::as_slice).collect();
    let input = mutate(rng, &seeds);
    let id = "1".to_owned();
    for req in [
        Request::CreateKey,
        Request::Get(id.clone()),
        Request::Activate(id.clone()),
        Request::State(id),
    ] {
        if let Ok(Answer::Key(k)) = req.answer(&input) {
            assert_eq!(k.len(), 32, "a key of another size");
        }
    }
}

#[test]
fn scram_and_saslprep_survive_mutated_messages() {
    let flights = flights();
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_013a);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for f in &flights {
            for _ in 0..30 {
                feed(&mut rng, f);
                rounds += 1;
            }
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
