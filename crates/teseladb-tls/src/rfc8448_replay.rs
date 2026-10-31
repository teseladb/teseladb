//! TeselaDB's client against the server side of the RFC 8448 traces (013 T008, FR-003). The
//! client starts from the trace's ClientHello and ephemeral key, takes in the server's records
//! byte for byte and must answer with exactly the trace's records: the Finished (which checks
//! the transcript, the key schedule, the server's RSA-PSS CertificateVerify and the record
//! protection), the application data and close_notify.

use std::sync::Arc;

use teseladb_x509::Certificate;

use crate::config::{ClientConfig, Trust};
use crate::conn::{Connection, Role, State};
use crate::kx::Ephemeral;
use crate::msgs::{self, ClientHello, hs};

struct Value {
    section: u8,
    side: String,
    step: String,
    label: String,
    bytes: Vec<u8>,
}

fn load(section: u8) -> Vec<Value> {
    let text = include_str!("../tests/data/rfc8448.tsv");
    text.lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let bytes = (0..f[4].len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&f[4][i..i + 2], 16).unwrap())
                .collect();
            Value {
                section: f[0].parse().unwrap(),
                side: f[1].into(),
                step: f[2].into(),
                label: f[3].into(),
                bytes,
            }
        })
        .filter(|v| v.section == section)
        .collect()
}

fn find(values: &[Value], side: &str, step: &str, label: &str) -> Vec<u8> {
    values
        .iter()
        .find(|v| v.side == side && v.step.starts_with(step) && v.label == label)
        .unwrap()
        .bytes
        .clone()
}

/// The records of the trace after the first ClientHello, in order, with their side.
fn records(values: &[Value]) -> Vec<(String, Vec<u8>)> {
    values
        .iter()
        .filter(|v| v.label == "complete record")
        .skip(1)
        .map(|v| (v.side.clone(), v.bytes.clone()))
        .collect()
}

#[test]
fn the_client_reproduces_the_simple_1rtt_trace() {
    let v = load(3);
    let hello = find(&v, "client", "construct a ClientHello", "ClientHello");
    let cert_msg = find(&v, "server", "construct a Certificate handshake", "Certificate");
    let (_, body) = msgs::unframe(&cert_msg).unwrap();
    let leaf =
        Certificate::from_der(&msgs::CertificateMsg::parse(body).unwrap().entries[0].der).unwrap();
    let now_ms = (leaf.not_before + 86_400) * 1000;
    let cfg = Arc::new(ClientConfig::new(Trust::anchors(vec![leaf])));
    let mut c = Connection::new(Role::Client(cfg), State::ClientWaitServerHello, now_ms, &[0; 48]);
    let key: [u8; 32] =
        find(&v, "client", "create an ephemeral", "private key").try_into().unwrap();
    c.ephemerals = vec![Ephemeral::X25519(key)];
    let (typ, ch_body) = msgs::unframe(&hello).unwrap();
    assert_eq!(typ, hs::CLIENT_HELLO);
    let ch = ClientHello::parse(ch_body).unwrap();
    c.session_id = ch.session_id.clone();
    c.offered_extensions = ch.extensions.0.iter().map(|(t, _)| *t).collect();
    c.transcript = hello.clone();
    c.sent_hello = Some(hello);
    let mut checked = 0;
    for (side, record) in records(&v) {
        if side == "server" {
            c.feed(&record).unwrap();
            continue;
        }
        // The client's next record: its Finished comes out of the feed; data and close_notify
        // are written with the trace's payloads.
        let mut out = c.take_output();
        if out.is_empty() {
            match record[0] {
                23 if record.len() == 24 => c.close().unwrap(),
                _ => {
                    let payload = find(&v, "client", "send application_data", "payload");
                    c.write(&payload).unwrap();
                }
            }
            out = c.take_output();
        }
        assert_eq!(out, record, "client record {checked}");
        checked += 1;
    }
    assert_eq!(checked, 3);
    assert_eq!(c.read(), find(&v, "server", "send application_data", "payload"));
    assert!(c.peer_closed());
    assert_eq!(c.take_tickets().len(), 1);
}
