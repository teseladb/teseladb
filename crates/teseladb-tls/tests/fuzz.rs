//! Fuzzing of records and handshake messages (013 T008 and T041, FR-018, SC-003): the bytes of
//! real TLS 1.3 and TLS 1.2 handshakes (each side's flights, with tickets and client
//! certificates), mutated and fed in
//! random pieces to a fresh client or server, which must fail with an alert or wait for more,
//! and never panic.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-003), and `TESELADB_FUZZ_SEED` replays one
//! seed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb_crypto::ec::NamedCurve;
use teseladb_tls::keyschedule::Secret;
use teseladb_tls::suites::NamedGroup;
use teseladb_tls::{
    CertifiedKey, ClientAuth, ClientConfig, Connection, ServerConfig, Trust, Version,
};
use teseladb_types::Rng;
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Name, PrivateKey, oid};

const NOW_MS: i64 = 1_800_000_000_000;
const DEFAULT_ROUNDS: u64 = 600;
const HOST: &str = "fuzz.test";

struct Setup {
    client: Arc<ClientConfig>,
    server: Arc<ServerConfig>,
    /// Bytes each side sent in a full handshake: (to the server, to the client).
    to_server: Vec<Vec<u8>>,
    to_client: Vec<Vec<u8>>,
}

/// A full handshake to record: TLS 1.3, or TLS 1.2 when `tls12`.
fn setup(tls12: bool) -> Setup {
    let now = NOW_MS / 1000;
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[1; 40]).unwrap();
    let mut p = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        now - 60,
        now + 86_400,
    );
    p.ca = Some(None);
    let ca = generate::self_signed(&p, &ca_key).unwrap();
    let leaf = |seed: u8, name: &str| {
        let key = PrivateKey::ed25519(&[seed; 32]);
        let mut lp = CertificateParams::node(
            [seed; 16],
            Name::build(&[(oid::COMMON_NAME, name)]),
            now - 60,
            now + 86_400,
        );
        lp.alt_names = vec![GeneralName::Dns(name.into())];
        CertifiedKey {
            chain: vec![generate::issue(&lp, &key.public_key(), &ca, &ca_key).unwrap()],
            key,
        }
    };
    let server_id = leaf(2, HOST);
    let client_id = leaf(3, "client");
    let mut sc = ServerConfig::new(server_id);
    sc.client_auth = ClientAuth::Optional;
    sc.trust = Trust::anchors(vec![ca.clone()]);
    sc.ticket_key = Some(Secret(vec![4; 32]));
    sc.groups = vec![NamedGroup::Secp256r1, NamedGroup::X25519];
    let mut cc = ClientConfig::new(Trust::anchors(vec![ca]));
    cc.identity = Some(client_id);
    cc.groups = vec![NamedGroup::Secp384r1, NamedGroup::Secp256r1];
    if tls12 {
        sc.min_version = Version::Tls12;
        cc.min_version = Version::Tls12;
        cc.max_version = Version::Tls12;
    }
    let (client, server) = (Arc::new(cc), Arc::new(sc));
    let mut c = Connection::client(client.clone(), Some(HOST), NOW_MS, &[5; 48], None).unwrap();
    let mut s = Connection::server(server.clone(), NOW_MS, &[6; 48]);
    let (mut to_server, mut to_client) = (Vec::new(), Vec::new());
    for _ in 0..10 {
        let a = c.take_output();
        let b = s.take_output();
        if a.is_empty() && b.is_empty() {
            break;
        }
        if !a.is_empty() {
            s.feed(&a).unwrap();
            to_server.push(a);
        }
        if !b.is_empty() {
            c.feed(&b).unwrap();
            to_client.push(b);
        }
    }
    assert!(!c.is_handshaking() && !s.is_handshaking());
    Setup { client, server, to_server, to_client }
}

fn mutate(rng: &mut Rng, flights: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = flights.to_vec();
    let i = rng.range(0, out.len() as u64) as usize;
    let v = &mut out[i];
    for _ in 0..rng.range(1, 4) {
        if v.is_empty() {
            break;
        }
        let at = rng.range(0, v.len() as u64) as usize;
        match rng.range(0, 5) {
            0 => v[at] ^= 1 << rng.range(0, 8),
            1 => v.truncate(at),
            2 => v.insert(at, rng.next_u32() as u8),
            3 => v[at] = [0x00, 0x01, 0x16, 0x17, 0xff][rng.range(0, 5) as usize],
            _ => {
                v.remove(at);
            }
        }
    }
    out
}

fn feed_in_pieces(rng: &mut Rng, conn: &mut Connection, flights: &[Vec<u8>]) {
    for f in flights {
        let mut rest = f.as_slice();
        while !rest.is_empty() {
            let n = (rng.range(1, 400) as usize).min(rest.len());
            if conn.feed(&rest[..n]).is_err() {
                assert!(conn.is_failed());
                return;
            }
            rest = &rest[n..];
        }
    }
    let _ = conn.take_output();
    let _ = conn.read();
}

#[test]
fn connections_survive_mutated_peers() {
    run(&setup(false));
}

#[test]
fn tls12_connections_survive_mutated_peers() {
    run(&setup(true));
}

fn run(st: &Setup) {
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x7151_0508);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..20 {
            // A server reading a mutated client, and a client reading a mutated server. Both
            // start from the same randomness as the recorded handshake, so the unmutated parts
            // are what they expect.
            let mut s = Connection::server(st.server.clone(), NOW_MS, &[6; 48]);
            let flights = mutate(&mut rng, &st.to_server);
            feed_in_pieces(&mut rng, &mut s, &flights);
            let mut c =
                Connection::client(st.client.clone(), Some(HOST), NOW_MS, &[5; 48], None).unwrap();
            let _ = c.take_output();
            let flights = mutate(&mut rng, &st.to_client);
            feed_in_pieces(&mut rng, &mut c, &flights);
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
    eprintln!("fuzz: {rounds} rounds in {:?}", start.elapsed());
}
