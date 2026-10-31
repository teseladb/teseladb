//! The connection: the sans-I/O driver shared by client and server. Bytes from the socket go in
//! with `feed`, bytes for the socket come out of `take_output`, application data goes through
//! `write` and `read`.

use std::sync::Arc;

use teseladb_crypto::rand::HmacDrbg;
use teseladb_x509::{Certificate, VerifyOptions, verify_chain};

use crate::config::{ClientConfig, ServerConfig, Version};
use crate::keyschedule::{KeySchedule, Secret, TrafficKeys, next_traffic_secret};
use crate::kx::Ephemeral;
use crate::msgs::{self, hs};
use crate::record::{ContentType, HandshakeBuffer, Protection, RecordLayer};
use crate::suites::{CipherSuite, NamedGroup, SignatureScheme};
use crate::tls12::Tls12Suite;
use crate::{Alert, Error};

/// `status_type` `ocsp` of `status_request` and `CertificateStatus` (RFC 6066, 8).
pub(crate) const OCSP_STATUS_TYPE: u8 = 1;

/// Entropy a connection is seeded with.
pub const ENTROPY_LEN: usize = 48;
const ALERT_LEVEL_WARNING: u8 = 1;
const ALERT_LEVEL_FATAL: u8 = 2;
const KEY_UPDATE_NOT_REQUESTED: u8 = 0;
const KEY_UPDATE_REQUESTED: u8 = 1;

/// A session ticket kept by a client to resume with the same server.
#[derive(Clone, Debug)]
pub struct Ticket {
    /// The server name it was issued for.
    pub server_name: Option<String>,
    /// The suite of the session; a resumption offers a suite with the same hash.
    pub suite: CipherSuite,
    /// The resumption PSK.
    pub psk: Secret,
    /// The opaque ticket.
    pub ticket: Vec<u8>,
    /// `ticket_age_add`.
    pub age_add: u32,
    /// Lifetime, seconds.
    pub lifetime: u32,
    /// When it was received, milliseconds since the epoch.
    pub received_ms: i64,
    /// The server's chain as validated in the original handshake.
    pub server_chain: Vec<Certificate>,
}

/// Where the handshake stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    ClientWaitServerHello,
    ClientWaitEncryptedExtensions,
    ClientWaitCertOrRequest,
    ClientWaitCertificate,
    ClientWaitCertVerify,
    ClientWaitFinished,
    ServerWaitClientHello,
    ServerWaitCertificate,
    ServerWaitCertVerify,
    ServerWaitFinished,
    Client12WaitCertificate,
    Client12WaitKeyExchange,
    Client12WaitDone,
    Client12WaitCcs,
    Client12WaitFinished,
    Server12WaitCertificate,
    Server12WaitKeyExchange,
    Server12WaitCertVerify,
    Server12WaitCcs,
    Server12WaitFinished,
    Connected,
    Failed,
}

/// What a TLS 1.2 handshake keeps between messages.
pub(crate) struct Handshake12 {
    pub(crate) suite: Tls12Suite,
    pub(crate) client_random: [u8; 32],
    pub(crate) server_random: [u8; 32],
    /// Whether both sides use the extended master secret.
    pub(crate) ems: bool,
    pub(crate) master: Option<Secret>,
    /// This side's ephemeral key.
    pub(crate) ephemeral: Option<Ephemeral>,
    /// Client: the group and point of the ServerKeyExchange.
    pub(crate) server_share: Option<(NamedGroup, Vec<u8>)>,
    /// Client: the schemes of a CertificateRequest.
    pub(crate) cert_request: Option<Vec<u16>>,
    /// The peer's keys, installed by its change_cipher_spec.
    pub(crate) pending_read: Option<Protection>,
    /// This side's keys, installed when it sends its change_cipher_spec.
    pub(crate) pending_write: Option<Protection>,
}

/// Which side this is.
#[derive(Clone, Debug)]
pub(crate) enum Role {
    Client(Arc<ClientConfig>),
    Server(Arc<ServerConfig>),
}

/// One TLS connection.
pub struct Connection {
    pub(crate) role: Role,
    pub(crate) state: State,
    pub(crate) records: RecordLayer,
    pub(crate) hs_buf: HandshakeBuffer,
    out: Vec<u8>,
    plaintext: Vec<u8>,
    rng: HmacDrbg,
    pub(crate) now_ms: i64,
    pub(crate) transcript: Vec<u8>,
    pub(crate) suite: Option<CipherSuite>,
    pub(crate) schedule: Option<KeySchedule>,
    /// Handshake traffic secrets (client, server) until the handshake ends.
    pub(crate) hs_secrets: Option<(Secret, Secret)>,
    /// Application traffic secrets (client, server).
    pub(crate) app_secrets: Option<(Secret, Secret)>,
    pub(crate) resumption_master: Option<Secret>,
    pub(crate) ephemerals: Vec<Ephemeral>,
    pub(crate) server_name: Option<String>,
    pub(crate) peer_chain: Vec<Certificate>,
    pub(crate) alpn: Option<Vec<u8>>,
    pub(crate) resumed: bool,
    pub(crate) retried: bool,
    pub(crate) session_id: Vec<u8>,
    /// Client: the first ClientHello sent.
    pub(crate) sent_hello: Option<Vec<u8>>,
    /// Client: the extensions offered, the PSK offered and the CertificateRequest received.
    pub(crate) offered_extensions: Vec<u16>,
    pub(crate) offered_ticket: Option<Ticket>,
    pub(crate) cert_request: Option<msgs::CertificateRequest>,
    pub(crate) tickets: Vec<Ticket>,
    /// Server: whether a client certificate was asked for, and the tickets issued so far.
    pub(crate) requested_cert: bool,
    pub(crate) tickets_issued: u32,
    /// The state of a TLS 1.2 handshake; it stays after the handshake to mark the version.
    pub(crate) tls12: Option<Box<Handshake12>>,
    /// Server: the client asked for the OCSP response. Client (TLS 1.2): the server will send it.
    pub(crate) ocsp_requested: bool,
    peer_closed: bool,
    sent_close: bool,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("state", &self.state)
            .field("suite", &self.suite)
            .finish()
    }
}

impl Connection {
    pub(crate) fn new(
        role: Role,
        state: State,
        now_ms: i64,
        entropy: &[u8; ENTROPY_LEN],
    ) -> Connection {
        Connection {
            role,
            state,
            records: RecordLayer::default(),
            hs_buf: HandshakeBuffer::default(),
            out: Vec::new(),
            plaintext: Vec::new(),
            rng: HmacDrbg::new(entropy, &now_ms.to_be_bytes(), b"teseladb tls"),
            now_ms,
            transcript: Vec::new(),
            suite: None,
            schedule: None,
            hs_secrets: None,
            app_secrets: None,
            resumption_master: None,
            ephemerals: Vec::new(),
            server_name: None,
            peer_chain: Vec::new(),
            alpn: None,
            resumed: false,
            retried: false,
            session_id: Vec::new(),
            sent_hello: None,
            offered_extensions: Vec::new(),
            offered_ticket: None,
            cert_request: None,
            tickets: Vec::new(),
            requested_cert: false,
            tickets_issued: 0,
            tls12: None,
            ocsp_requested: false,
            peer_closed: false,
            sent_close: false,
        }
    }

    /// Feeds bytes received from the peer and runs the protocol as far as they allow. On error
    /// the alert to send is already in the output and the connection is dead.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.state == State::Failed {
            return Err(Error::Closed);
        }
        self.records.push_input(data);
        let r = self.process();
        if let Err(e) = r {
            self.fail(e);
        }
        r
    }

    fn fail(&mut self, e: Error) {
        if let Some(alert) = e.alert() {
            // Best effort: the connection is dead whatever happens to the alert.
            let _ = self.send_alert(ALERT_LEVEL_FATAL, alert);
        }
        self.state = State::Failed;
    }

    fn process(&mut self) -> Result<(), Error> {
        while let Some(rec) = self.records.next_record()? {
            if rec.typ != ContentType::Handshake && !self.hs_buf.is_empty() {
                return Err(Error::Local(Alert::UnexpectedMessage));
            }
            match rec.typ {
                ContentType::ChangeCipherSpec => self.change_cipher_spec(&rec.data)?,
                ContentType::Alert => self.handle_alert(&rec.data)?,
                ContentType::ApplicationData => {
                    if self.state != State::Connected || self.peer_closed {
                        return Err(Error::Local(Alert::UnexpectedMessage));
                    }
                    self.plaintext.extend_from_slice(&rec.data);
                }
                ContentType::Handshake => {
                    self.hs_buf.push(&rec.data);
                    while let Some(msg) = self.hs_buf.next_message()? {
                        self.handle_message(&msg)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// TLS 1.3's compatibility record is dropped while the handshake runs; TLS 1.2's installs
    /// the peer's keys where the handshake expects it.
    fn change_cipher_spec(&mut self, data: &[u8]) -> Result<(), Error> {
        let unexpected = Error::Local(Alert::UnexpectedMessage);
        if data != [1] || self.state == State::Connected {
            return Err(unexpected);
        }
        let Some(hs) = self.tls12.as_mut() else { return Ok(()) };
        let next = match self.state {
            State::Client12WaitCcs => State::Client12WaitFinished,
            State::Server12WaitCcs => State::Server12WaitFinished,
            _ => return Err(unexpected),
        };
        if !self.hs_buf.is_empty() {
            return Err(unexpected);
        }
        self.records.read = Some(hs.pending_read.take().ok_or(Error::Local(Alert::InternalError))?);
        self.state = next;
        Ok(())
    }

    fn handle_alert(&mut self, data: &[u8]) -> Result<(), Error> {
        let [level, code] = data else {
            return Err(Error::Local(Alert::DecodeError));
        };
        let alert = Alert::from_code(*code);
        match alert {
            Alert::CloseNotify => {
                self.peer_closed = true;
                Ok(())
            }
            Alert::UserCanceled | Alert::NoRenegotiation if *level == ALERT_LEVEL_WARNING => Ok(()),
            _ => {
                self.state = State::Failed;
                Err(Error::Peer(alert))
            }
        }
    }

    fn handle_message(&mut self, msg: &[u8]) -> Result<(), Error> {
        let (typ, body) = msgs::unframe(msg)?;
        match (self.state, &self.role) {
            (State::Connected, _) => self.handle_post_handshake(typ, body),
            (_, Role::Client(_)) => self.client_message(typ, body, msg),
            (_, Role::Server(_)) => self.server_message(typ, body, msg),
        }
    }

    pub(crate) fn handle_post_handshake(&mut self, typ: u8, body: &[u8]) -> Result<(), Error> {
        if self.tls12.is_some() {
            // A renegotiation (a new ClientHello, or a HelloRequest to the client) is refused
            // with a warning, and the connection goes on (RFC 5746, 4.2 and 4.4).
            return match (typ, &self.role) {
                (hs::CLIENT_HELLO, Role::Server(_))
                | (crate::tls12::hs::HELLO_REQUEST, Role::Client(_)) => {
                    self.send_alert(ALERT_LEVEL_WARNING, Alert::NoRenegotiation)
                }
                _ => Err(Error::Local(Alert::UnexpectedMessage)),
            };
        }
        match (typ, &self.role) {
            (hs::KEY_UPDATE, _) => {
                let request = match body {
                    [KEY_UPDATE_NOT_REQUESTED] => false,
                    [KEY_UPDATE_REQUESTED] => true,
                    _ => return Err(Error::Local(Alert::IllegalParameter)),
                };
                self.update_read_key()?;
                if request {
                    self.key_update(false)?;
                }
                Ok(())
            }
            (hs::NEW_SESSION_TICKET, Role::Client(_)) => self.client_new_ticket(body),
            _ => Err(Error::Local(Alert::UnexpectedMessage)),
        }
    }

    fn is_client(&self) -> bool {
        matches!(self.role, Role::Client(_))
    }

    fn update_read_key(&mut self) -> Result<(), Error> {
        if !self.hs_buf.is_empty() {
            return Err(Error::Local(Alert::UnexpectedMessage));
        }
        let suite = self.suite()?;
        let client = self.is_client();
        let (c, s) = self.app_secrets.as_mut().ok_or(Error::Local(Alert::InternalError))?;
        let secret = if client { s } else { c };
        *secret = next_traffic_secret(suite.hash(), &secret.0);
        self.records.read = Some(Protection::new(suite, &TrafficKeys::derive(suite, &secret.0))?);
        Ok(())
    }

    /// Sends a KeyUpdate and moves to the next write key; with `request`, asks the peer to do
    /// the same. TLS 1.2 has no KeyUpdate: `Error::Closed`, as before the handshake.
    pub fn key_update(&mut self, request: bool) -> Result<(), Error> {
        if self.state != State::Connected || self.tls12.is_some() {
            return Err(Error::Closed);
        }
        let flag = if request { KEY_UPDATE_REQUESTED } else { KEY_UPDATE_NOT_REQUESTED };
        let msg = msgs::frame(hs::KEY_UPDATE, &[flag]);
        self.records.write(ContentType::Handshake, &msg, &mut self.out)?;
        let suite = self.suite()?;
        let client = self.is_client();
        let (c, s) = self.app_secrets.as_mut().ok_or(Error::Local(Alert::InternalError))?;
        let secret = if client { c } else { s };
        *secret = next_traffic_secret(suite.hash(), &secret.0);
        self.records.write = Some(Protection::new(suite, &TrafficKeys::derive(suite, &secret.0))?);
        Ok(())
    }

    pub(crate) fn send_alert(&mut self, level: u8, alert: Alert) -> Result<(), Error> {
        self.records.write(ContentType::Alert, &[level, alert.code()], &mut self.out)
    }

    /// Bytes to send to the peer.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Whether bytes wait to be sent.
    pub fn wants_write(&self) -> bool {
        !self.out.is_empty()
    }

    /// Application data received.
    pub fn read(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.plaintext)
    }

    /// Sends application data.
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.state != State::Connected || self.sent_close {
            return Err(Error::Closed);
        }
        self.records.write(ContentType::ApplicationData, data, &mut self.out)
    }

    /// Sends close_notify; nothing more can be written.
    pub fn close(&mut self) -> Result<(), Error> {
        if self.sent_close || self.state == State::Failed {
            return Ok(());
        }
        self.sent_close = true;
        self.send_alert(ALERT_LEVEL_WARNING, Alert::CloseNotify)
    }

    /// Whether the handshake is still running.
    pub fn is_handshaking(&self) -> bool {
        !matches!(self.state, State::Connected | State::Failed)
    }

    /// Whether the peer sent close_notify.
    pub fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// Whether the connection failed.
    pub fn is_failed(&self) -> bool {
        self.state == State::Failed
    }

    /// The peer's validated chain, leaf first (empty if it sent none).
    pub fn peer_certificates(&self) -> &[Certificate] {
        &self.peer_chain
    }

    /// The negotiated ALPN protocol.
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.alpn.as_deref()
    }

    /// The server name: the one the client asked for (server side) or connects to (client).
    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    /// The negotiated suite.
    pub fn suite(&self) -> Result<CipherSuite, Error> {
        self.suite.ok_or(Error::Local(Alert::InternalError))
    }

    pub(crate) fn hs12(&mut self) -> Result<&mut Handshake12, Error> {
        self.tls12.as_deref_mut().ok_or(Error::Local(Alert::InternalError))
    }

    /// Ends a TLS 1.2 handshake: the secrets go, the suite stays.
    pub(crate) fn finish12(&mut self) -> Result<(), Error> {
        let hs = self.hs12()?;
        hs.master = None;
        hs.ephemeral = None;
        hs.server_share = None;
        self.state = State::Connected;
        Ok(())
    }

    /// The negotiated version, once the hellos are exchanged.
    pub fn version(&self) -> Option<Version> {
        match (&self.tls12, self.suite) {
            (Some(_), _) => Some(Version::Tls12),
            (None, Some(_)) => Some(Version::Tls13),
            (None, None) => None,
        }
    }

    /// The negotiated TLS 1.2 suite.
    pub fn tls12_suite(&self) -> Option<Tls12Suite> {
        self.tls12.as_ref().map(|h| h.suite)
    }

    /// Whether the session was resumed with a ticket.
    pub fn is_resumed(&self) -> bool {
        self.resumed
    }

    /// Session tickets received (client side).
    pub fn take_tickets(&mut self) -> Vec<Ticket> {
        std::mem::take(&mut self.tickets)
    }

    pub(crate) fn random<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut out = [0u8; N];
        self.rng.generate(&mut out, &[]).ok().ok_or(Error::Local(Alert::InternalError))?;
        Ok(out)
    }

    pub(crate) fn transcript_hash(&self) -> Result<Vec<u8>, Error> {
        Ok(self.suite()?.hash().hash(&self.transcript).as_ref().to_vec())
    }

    /// Appends a handshake message to the transcript and writes it.
    pub(crate) fn send_handshake(&mut self, msg: &[u8]) -> Result<(), Error> {
        self.transcript.extend_from_slice(msg);
        self.records.write(ContentType::Handshake, msg, &mut self.out)
    }

    /// Writes a handshake message that is not part of the transcript (after the handshake).
    pub(crate) fn send_post_handshake(&mut self, msg: &[u8]) -> Result<(), Error> {
        self.records.write(ContentType::Handshake, msg, &mut self.out)
    }

    /// Writes the compatibility change_cipher_spec record.
    pub(crate) fn send_ccs(&mut self) {
        self.out.extend_from_slice(&[20, 3, 3, 0, 1, 1]);
    }

    pub(crate) fn set_read(&mut self, secret: &[u8]) -> Result<(), Error> {
        if !self.hs_buf.is_empty() {
            return Err(Error::Local(Alert::UnexpectedMessage));
        }
        let suite = self.suite()?;
        self.records.read = Some(Protection::new(suite, &TrafficKeys::derive(suite, secret))?);
        Ok(())
    }

    pub(crate) fn set_write(&mut self, secret: &[u8]) -> Result<(), Error> {
        let suite = self.suite()?;
        self.records.write = Some(Protection::new(suite, &TrafficKeys::derive(suite, secret))?);
        Ok(())
    }

    /// Validates a peer chain from a Certificate message and keeps it.
    pub(crate) fn validate_chain(
        &mut self,
        entries: &[msgs::CertEntry],
        purpose: teseladb_x509::Purpose,
    ) -> Result<(), Error> {
        let certs: Vec<Certificate> = entries
            .iter()
            .map(|e| Certificate::from_der(&e.der))
            .collect::<Result<_, _>>()
            .map_err(|_| Error::Local(Alert::BadCertificate))?;
        let trust = match &self.role {
            Role::Client(c) => &c.trust,
            Role::Server(s) => &s.trust,
        };
        let opts = VerifyOptions {
            purpose,
            crls: &trust.crls,
            crl_check: trust.crl_check,
            ..VerifyOptions::new(self.now_ms.div_euclid(1000))
        };
        let (leaf, rest) = certs.split_first().ok_or(Error::Local(Alert::DecodeError))?;
        let mut anchors = trust.anchors.clone();
        if let Some(pin) = &trust.pinned {
            let pinned = rest.iter().filter(|c| teseladb_crypto::sha256(c.der()) == *pin);
            anchors.extend(pinned.cloned());
        }
        verify_chain(leaf, rest, &anchors, &opts).map_err(Error::Certificate)?;
        if let (Role::Client(_), Some(name)) = (&self.role, &self.server_name) {
            teseladb_x509::verify_hostname(leaf, name).map_err(Error::Certificate)?;
        }
        self.peer_chain = certs;
        Ok(())
    }

    /// Checks an OCSP response the server stapled (`CertificateStatus`: status type ocsp and the
    /// response) for the validated leaf: a revoked certificate fails the handshake, an answer that
    /// does not check is `bad_certificate_status_response`, `good` and `unknown` go on.
    pub(crate) fn check_staple(&self, status: &[u8]) -> Result<(), Error> {
        use teseladb_x509::ocsp::{self, CertId, Status};
        let bad = Error::Local(Alert::BadCertificateStatusResponse);
        let mut r = crate::codec::Reader::new(status);
        if r.u8()? != OCSP_STATUS_TYPE {
            return Err(bad);
        }
        let response = r.vec24()?;
        r.finish()?;
        let leaf = self.peer_chain.first().ok_or(Error::Local(Alert::InternalError))?;
        let trust = match &self.role {
            Role::Client(c) => &c.trust,
            Role::Server(s) => &s.trust,
        };
        let issuer = self
            .peer_chain
            .get(1)
            .or_else(|| trust.anchors.iter().find(|a| leaf.verify_signed_by(&a.public_key).is_ok()))
            .ok_or(bad)?;
        let id = CertId::new(leaf, issuer).map_err(|_| bad)?;
        match ocsp::check(response, &id, issuer, self.now_ms.div_euclid(1000)) {
            Ok(c) if matches!(c.status, Status::Revoked(_)) => {
                Err(Error::Certificate(teseladb_x509::Error::Revoked))
            }
            Ok(_) => Ok(()),
            Err(_) => Err(bad),
        }
    }

    /// Checks a CertificateVerify from the peer.
    pub(crate) fn check_cert_verify(
        &mut self,
        body: &[u8],
        offered: &[SignatureScheme],
        server: bool,
    ) -> Result<(), Error> {
        let cv = msgs::CertificateVerify::parse(body)?;
        let content = crate::kx::verify_content(server, &self.transcript_hash()?);
        let leaf = self.peer_chain.first().ok_or(Error::Local(Alert::InternalError))?;
        crate::kx::verify(leaf, cv.scheme, offered, &content, &cv.signature)
    }

    /// Checks a Finished from the peer against the base key.
    pub(crate) fn check_finished(&self, body: &[u8], base: &[u8]) -> Result<(), Error> {
        let expected =
            crate::keyschedule::finished(self.suite()?.hash(), base, &self.transcript_hash()?);
        if teseladb_crypto::ct::ct_eq(&expected, body) {
            Ok(())
        } else {
            Err(Error::Local(Alert::DecryptError))
        }
    }

    /// Builds this side's Finished.
    pub(crate) fn finished_message(&self, base: &[u8]) -> Result<Vec<u8>, Error> {
        let verify =
            crate::keyschedule::finished(self.suite()?.hash(), base, &self.transcript_hash()?);
        Ok(msgs::frame(hs::FINISHED, &verify))
    }

    /// Derives the application secrets from the transcript through the server Finished.
    pub(crate) fn derive_app_secrets(&mut self) -> Result<(), Error> {
        let th = self.transcript_hash()?;
        let ks = self.schedule.as_mut().ok_or(Error::Local(Alert::InternalError))?;
        ks.advance(None);
        let c = ks.derive(b"c ap traffic", &th);
        let s = ks.derive(b"s ap traffic", &th);
        self.app_secrets = Some((c, s));
        Ok(())
    }

    /// Derives the resumption master secret from the transcript through the client Finished.
    pub(crate) fn derive_resumption(&mut self) -> Result<(), Error> {
        let th = self.transcript_hash()?;
        let ks = self.schedule.as_ref().ok_or(Error::Local(Alert::InternalError))?;
        self.resumption_master = Some(ks.derive(b"res master", &th));
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::config::Trust;

    fn client() -> Connection {
        let cfg = Arc::new(ClientConfig::new(Trust::anchors(Vec::new())));
        Connection::new(Role::Client(cfg), State::ClientWaitServerHello, 0, &[1; ENTROPY_LEN])
    }

    #[test]
    fn records_out_of_place_fail_the_connection() {
        // Application data before the handshake ends.
        let mut c = client();
        assert_eq!(c.feed(&[23, 3, 3, 0, 1, 0]), Err(Error::Local(Alert::UnexpectedMessage)));
        assert!(c.is_failed());
        assert_eq!(c.take_output(), vec![21, 3, 3, 0, 2, 2, 10]);
        assert_eq!(c.feed(&[]), Err(Error::Closed));
        // A change_cipher_spec with the wrong byte, an alert of the wrong size, a peer alert,
        // a warning user_canceled and close_notify.
        let mut c = client();
        assert_eq!(c.feed(&[20, 3, 3, 0, 1, 2]), Err(Error::Local(Alert::UnexpectedMessage)));
        let mut c = client();
        assert_eq!(c.feed(&[20, 3, 3, 0, 1, 1]), Ok(()));
        assert_eq!(c.feed(&[21, 3, 3, 0, 1, 2]), Err(Error::Local(Alert::DecodeError)));
        let mut c = client();
        assert_eq!(c.feed(&[21, 3, 3, 0, 2, 1, 90, 21, 3, 3, 0, 2, 1, 0]), Ok(()));
        assert!(c.peer_closed());
        assert_eq!(c.feed(&[21, 3, 3, 0, 2, 2, 40]), Err(Error::Peer(Alert::HandshakeFailure)));
        assert!(c.take_output().is_empty());
        // A non-handshake record in the middle of a handshake message.
        let mut c = client();
        assert_eq!(
            c.feed(&[22, 3, 3, 0, 1, 2, 21, 3, 3, 0, 2, 1, 0]),
            Err(Error::Local(Alert::UnexpectedMessage))
        );
        // Writing, updating keys and closing before the handshake.
        let mut c = client();
        assert_eq!(c.write(b"x"), Err(Error::Closed));
        assert_eq!(c.key_update(true), Err(Error::Closed));
        assert!(c.is_handshaking());
        assert!(!c.wants_write());
        assert_eq!(c.close(), Ok(()));
        assert!(c.wants_write());
        assert_eq!(c.close(), Ok(()));
        assert!(format!("{c:?}").contains("ClientWaitServerHello"));
        assert_eq!(c.suite(), Err(Error::Local(Alert::InternalError)));
        assert!(
            c.peer_certificates().is_empty()
                && c.alpn_protocol().is_none()
                && c.server_name().is_none()
        );
        assert!(!c.is_resumed());
        assert!(c.take_tickets().is_empty() && c.read().is_empty());
    }
}
