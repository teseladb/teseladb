//! The server handshake (RFC 8446, 4 and 2): suite, group and signature selection,
//! `HelloRetryRequest`, session tickets and PSK resumption, and client authentication.

use std::sync::Arc;

use teseladb_crypto::aes::Aes;
use teseladb_crypto::modes::AesGcm;
use teseladb_x509::Certificate;

use crate::codec::{Reader, put_vec8, put_vec16, put_vec24};
use crate::config::{ClientAuth, ServerConfig, Version};
use crate::conn::{Connection, ENTROPY_LEN, Role, State};
use crate::keyschedule::{self, KeySchedule, Secret};
use crate::kx::{self, Ephemeral, KEY_RANDOM_LEN};
use crate::msgs::{self, ClientHello, Extensions, OfferedPsks, ServerHello, ext, hs};
use crate::suites::{CipherSuite, NamedGroup, SignatureScheme};
use crate::{Alert, Error};

const TICKET_FORMAT: u8 = 1;
const TICKET_AAD: &[u8] = b"teseladb-tls tls13 ticket";
const TICKET_NONCE_LEN: usize = 12;

fn illegal() -> Error {
    Error::Local(Alert::IllegalParameter)
}

fn internal() -> Error {
    Error::Local(Alert::InternalError)
}

fn config(c: &Connection) -> Result<Arc<ServerConfig>, Error> {
    match &c.role {
        Role::Server(cfg) => Ok(cfg.clone()),
        Role::Client(_) => Err(internal()),
    }
}

/// What a ticket carries.
struct TicketState {
    suite: CipherSuite,
    issued_ms: i64,
    lifetime: u32,
    psk: Secret,
    client_chain: Vec<Vec<u8>>,
}

fn ticket_aead(key: &Secret) -> Result<AesGcm, Error> {
    Ok(AesGcm::new(Aes::new(&key.0).ok().ok_or(internal())?))
}

fn open_ticket(key: &Secret, sealed: &[u8]) -> Option<TicketState> {
    if sealed.len() < TICKET_NONCE_LEN + 16 {
        return None;
    }
    let (nonce, ct) = sealed.split_at(TICKET_NONCE_LEN);
    let plain = Secret(ticket_aead(key).ok()?.open(nonce, TICKET_AAD, ct).ok()?);
    let mut r = Reader::new(&plain.0);
    if r.u8().ok()? != TICKET_FORMAT {
        return None;
    }
    let suite = CipherSuite::from_code(r.u16().ok()?)?;
    let issued_ms = i64::from_be_bytes(r.bytes(8).ok()?.try_into().ok()?);
    let lifetime = r.u32().ok()?;
    let psk = Secret(r.vec8().ok()?.to_vec());
    let mut client_chain = Vec::new();
    for _ in 0..r.u8().ok()? {
        client_chain.push(r.vec24().ok()?.to_vec());
    }
    r.finish().ok()?;
    Some(TicketState { suite, issued_ms, lifetime, psk, client_chain })
}

impl Connection {
    /// A server connection waiting for a `ClientHello`.
    #[must_use]
    pub fn server(
        config: Arc<ServerConfig>,
        now_ms: i64,
        entropy: &[u8; ENTROPY_LEN],
    ) -> Connection {
        Connection::new(Role::Server(config), State::ServerWaitClientHello, now_ms, entropy)
    }

    pub(crate) fn server_message(&mut self, typ: u8, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        if self.tls12.is_some() {
            return self.server12_message(typ, body, msg);
        }
        match (self.state, typ) {
            (State::ServerWaitClientHello, hs::CLIENT_HELLO) => self.server_client_hello(body, msg),
            (State::ServerWaitCertificate, hs::CERTIFICATE) => {
                let cm = msgs::CertificateMsg::parse(body)?;
                if !cm.context.is_empty() {
                    return Err(illegal());
                }
                let required = config(self)?.client_auth == ClientAuth::Required;
                if cm.entries.is_empty() {
                    if required {
                        return Err(Error::Local(Alert::CertificateRequired));
                    }
                    self.state = State::ServerWaitFinished;
                } else {
                    self.validate_chain(&cm.entries, teseladb_x509::Purpose::ClientAuth)?;
                    self.state = State::ServerWaitCertVerify;
                }
                self.transcript.extend_from_slice(msg);
                Ok(())
            }
            (State::ServerWaitCertVerify, hs::CERTIFICATE_VERIFY) => {
                self.check_cert_verify(body, &SignatureScheme::HANDSHAKE, false)?;
                self.transcript.extend_from_slice(msg);
                self.state = State::ServerWaitFinished;
                Ok(())
            }
            (State::ServerWaitFinished, hs::FINISHED) => self.server_finished(body, msg),
            _ => Err(Error::Local(Alert::UnexpectedMessage)),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the ClientHello is answered in the order of RFC 8446, section 4.1, one check after another"
    )]
    fn server_client_hello(&mut self, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        let cfg = config(self)?;
        let ch = ClientHello::parse(body)?;
        let tls13 = match ch.extensions.get(ext::SUPPORTED_VERSIONS) {
            Some(v) => {
                let mut r = Reader::new(v);
                let list = r.vec8()?;
                r.finish()?;
                list.chunks(2).any(|c| c == [0x03, 0x04])
            }
            None => false,
        } && cfg.max_version == Version::Tls13;
        if !tls13 {
            // TLS 1.2 where the port takes it (the clients'); earlier versions are refused.
            if cfg.min_version == Version::Tls12 && !self.retried {
                return self.server12_client_hello(&cfg, &ch, msg);
            }
            return Err(Error::Local(Alert::ProtocolVersion));
        }
        if ch.compression != [0] || (self.retried && ch.session_id != self.session_id) {
            return Err(illegal());
        }
        self.session_id.clone_from(&ch.session_id);
        let suite = cfg
            .suites
            .iter()
            .copied()
            .find(|s| ch.suites.contains(&s.code()))
            .ok_or(Error::Local(Alert::HandshakeFailure))?;
        if self.suite.is_some_and(|s| s != suite) {
            return Err(illegal());
        }
        self.suite = Some(suite);
        if let Some(sni) = ch.extensions.get(ext::SERVER_NAME) {
            self.server_name = parse_sni(sni)?;
        }
        self.ocsp_requested = ch.extensions.get(ext::STATUS_REQUEST).is_some();
        let shares = parse_key_shares(
            ch.extensions.get(ext::KEY_SHARE).ok_or(Error::Local(Alert::MissingExtension))?,
        )?;
        let offered_groups = msgs::read_u16_list(
            ch.extensions
                .get(ext::SUPPORTED_GROUPS)
                .ok_or(Error::Local(Alert::MissingExtension))?,
        )?;
        let chosen =
            cfg.groups.iter().copied().find(|g| shares.iter().any(|(c, _)| *c == g.code()));
        let Some(group) = chosen else {
            let retry = cfg.groups.iter().copied().find(|g| offered_groups.contains(&g.code()));
            return match (retry, self.retried) {
                (Some(g), false) => self.send_retry(g, msg),
                (Some(_), true) => Err(illegal()),
                (None, _) => Err(Error::Local(Alert::HandshakeFailure)),
            };
        };
        let peer_share = shares
            .iter()
            .find(|(c, _)| *c == group.code())
            .map(|(_, k)| k.clone())
            .ok_or(internal())?;
        let ticket = self.accept_psk(&cfg, &ch, msg, suite)?;
        let scheme = if ticket.is_some() {
            None
        } else {
            let peer = msgs::read_u16_list(
                ch.extensions
                    .get(ext::SIGNATURE_ALGORITHMS)
                    .ok_or(Error::Local(Alert::MissingExtension))?,
            )?;
            Some(
                kx::choose_scheme(&cfg.identity_for(self.server_name.as_deref()).key, &peer)
                    .ok_or(Error::Local(Alert::HandshakeFailure))?,
            )
        };
        self.alpn = choose_alpn(&cfg, ch.extensions.get(ext::ALPN))?;
        self.transcript.extend_from_slice(msg);
        let eph = Ephemeral::generate(group, &self.random::<KEY_RANDOM_LEN>()?)?;
        let secret = eph.agree(&peer_share)?;
        let mut e = Extensions::default();
        e.push(ext::SUPPORTED_VERSIONS, vec![0x03, 0x04]);
        e.push(ext::KEY_SHARE, msgs::key_share_entry(group.code(), &eph.public()));
        if ticket.is_some() {
            e.push(ext::PRE_SHARED_KEY, vec![0, 0]);
        }
        let sh = ServerHello {
            random: self.random::<32>()?,
            session_id: self.session_id.clone(),
            suite: suite.code(),
            extensions: e,
        };
        self.send_handshake(&sh.encode())?;
        if !self.session_id.is_empty() && !self.retried {
            self.send_ccs();
        }
        let mut ks = KeySchedule::new(suite.hash(), ticket.as_ref().map(|t| t.psk.0.as_slice()));
        ks.advance(Some(&secret.0));
        let th = self.transcript_hash()?;
        let c_hs = ks.derive(b"c hs traffic", &th);
        let s_hs = ks.derive(b"s hs traffic", &th);
        self.schedule = Some(ks);
        self.set_write(&s_hs.0)?;
        self.set_read(&c_hs.0)?;
        self.send_server_flight(&cfg, scheme, &s_hs)?;
        if let Some(t) = ticket {
            self.resumed = true;
            self.peer_chain =
                t.client_chain.iter().filter_map(|d| Certificate::from_der(d).ok()).collect();
        }
        self.hs_secrets = Some((c_hs, s_hs));
        self.state = if self.requested_cert {
            State::ServerWaitCertificate
        } else {
            State::ServerWaitFinished
        };
        Ok(())
    }

    fn send_server_flight(
        &mut self,
        cfg: &ServerConfig,
        scheme: Option<SignatureScheme>,
        s_hs: &Secret,
    ) -> Result<(), Error> {
        let mut ee = Extensions::default();
        if let Some(p) = &self.alpn {
            let mut list = Vec::new();
            put_vec8(&mut list, p);
            let mut v = Vec::new();
            put_vec16(&mut v, &list);
            ee.push(ext::ALPN, v);
        }
        let mut body = Vec::new();
        ee.write(&mut body);
        self.send_handshake(&msgs::frame(hs::ENCRYPTED_EXTENSIONS, &body))?;
        if let Some(scheme) = scheme {
            if cfg.client_auth != ClientAuth::None {
                let schemes: Vec<u16> =
                    SignatureScheme::HANDSHAKE.iter().map(|s| s.code()).collect();
                let mut e = Extensions::default();
                e.push(ext::SIGNATURE_ALGORITHMS, msgs::u16_list(&schemes));
                self.send_handshake(
                    &msgs::CertificateRequest { context: Vec::new(), extensions: e }.encode(),
                )?;
                self.requested_cert = true;
            }
            let mut entries: Vec<msgs::CertEntry> = cfg
                .identity_for(self.server_name.as_deref())
                .chain
                .iter()
                .map(|c| msgs::CertEntry {
                    der: c.der().to_vec(),
                    extensions: Extensions::default(),
                })
                .collect();
            // The OCSP response goes with the leaf, for a client that asked (RFC 8446, 4.4.2.1).
            if let (true, Some(staple), Some(leaf)) =
                (self.ocsp_requested, &cfg.ocsp_staple, entries.first_mut())
            {
                leaf.extensions.push(ext::STATUS_REQUEST, certificate_status(staple));
            }
            self.send_handshake(&msgs::CertificateMsg { context: Vec::new(), entries }.encode())?;
            let content = kx::verify_content(true, &self.transcript_hash()?);
            let salt = self.random::<64>()?;
            let sig = kx::sign(
                &cfg.identity_for(self.server_name.as_deref()).key,
                scheme,
                &content,
                &salt,
            )?;
            self.send_handshake(
                &msgs::CertificateVerify { scheme: scheme.code(), signature: sig }.encode(),
            )?;
        }
        let fin = self.finished_message(&s_hs.0)?;
        self.send_handshake(&fin)?;
        self.derive_app_secrets()?;
        let s_ap = self.app_secrets.as_ref().map(|(_, s)| s.0.clone()).ok_or(internal())?;
        self.set_write(&s_ap)
    }

    fn send_retry(&mut self, group: NamedGroup, msg: &[u8]) -> Result<(), Error> {
        self.retried = true;
        let hash = self.suite()?.hash();
        let ch1 = hash.hash(msg);
        self.transcript = msgs::frame(hs::MESSAGE_HASH, ch1.as_ref());
        let mut e = Extensions::default();
        e.push(ext::SUPPORTED_VERSIONS, vec![0x03, 0x04]);
        e.push(ext::KEY_SHARE, group.code().to_be_bytes().to_vec());
        let hrr = ServerHello {
            random: msgs::HRR_RANDOM,
            session_id: self.session_id.clone(),
            suite: self.suite()?.code(),
            extensions: e,
        };
        self.send_handshake(&hrr.encode())?;
        if !self.session_id.is_empty() {
            self.send_ccs();
        }
        Ok(())
    }

    /// The ticket of the first offered identity, if it decrypts, has not expired, goes with the
    /// suite's hash and carries a valid binder. A wrong binder fails the handshake.
    fn accept_psk(
        &self,
        cfg: &ServerConfig,
        ch: &ClientHello,
        msg: &[u8],
        suite: CipherSuite,
    ) -> Result<Option<TicketState>, Error> {
        let (Some(key), Some(data)) = (&cfg.ticket_key, ch.extensions.get(ext::PRE_SHARED_KEY))
        else {
            return Ok(None);
        };
        let modes = ch
            .extensions
            .get(ext::PSK_KEY_EXCHANGE_MODES)
            .ok_or(Error::Local(Alert::MissingExtension))?;
        let mut r = Reader::new(modes);
        let modes = r.vec8()?;
        r.finish()?;
        if !modes.contains(&msgs::PSK_DHE_KE) {
            return Ok(None);
        }
        let offered = OfferedPsks::parse(data)?;
        let (id, _) = &offered.identities[0];
        let Some(t) = open_ticket(key, id) else {
            return Ok(None);
        };
        if t.suite.hash() != suite.hash()
            || self.now_ms - t.issued_ms > i64::from(t.lifetime) * 1000
        {
            return Ok(None);
        }
        let h = suite.hash();
        let mut partial = self.transcript.clone();
        partial.extend_from_slice(&msg[..msg.len() - offered.binders_len()]);
        let ks = KeySchedule::new(h, Some(&t.psk.0));
        let expected =
            keyschedule::finished(h, &ks.resumption_binder_key().0, h.hash(&partial).as_ref());
        if !teseladb_crypto::ct::ct_eq(&expected, &offered.binders[0]) {
            return Err(Error::Local(Alert::DecryptError));
        }
        Ok(Some(t))
    }

    fn server_finished(&mut self, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        let (c_hs, _) = self.hs_secrets.take().ok_or(internal())?;
        self.check_finished(body, &c_hs.0)?;
        self.transcript.extend_from_slice(msg);
        let c_ap = self.app_secrets.as_ref().map(|(c, _)| c.0.clone()).ok_or(internal())?;
        self.set_read(&c_ap)?;
        self.derive_resumption()?;
        self.state = State::Connected;
        if let Some(key) = config(self)?.ticket_key.clone() {
            self.send_ticket(&key)?;
        }
        Ok(())
    }

    fn send_ticket(&mut self, key: &Secret) -> Result<(), Error> {
        let cfg = config(self)?;
        let suite = self.suite()?;
        let nonce = u64::from(self.tickets_issued).to_be_bytes();
        self.tickets_issued += 1;
        let master = self.resumption_master.as_ref().ok_or(internal())?;
        let psk = keyschedule::resumption_psk(suite.hash(), &master.0, &nonce);
        let lifetime = cfg.ticket_lifetime.min(crate::config::MAX_TICKET_LIFETIME);
        let mut plain = vec![TICKET_FORMAT];
        plain.extend_from_slice(&suite.code().to_be_bytes());
        plain.extend_from_slice(&self.now_ms.to_be_bytes());
        plain.extend_from_slice(&lifetime.to_be_bytes());
        put_vec8(&mut plain, &psk.0);
        let chain: Vec<&Certificate> = self.peer_chain.iter().take(usize::from(u8::MAX)).collect();
        plain.push(u8::try_from(chain.len()).unwrap_or(u8::MAX));
        for c in chain {
            put_vec24(&mut plain, c.der());
        }
        let plain = Secret(plain);
        let ticket_nonce = self.random::<TICKET_NONCE_LEN>()?;
        let mut ticket = ticket_nonce.to_vec();
        ticket.extend_from_slice(&ticket_aead(key)?.seal(&ticket_nonce, TICKET_AAD, &plain.0));
        let age_add = u32::from_be_bytes(self.random::<4>()?);
        let nst = msgs::NewSessionTicket {
            lifetime,
            age_add,
            nonce: nonce.to_vec(),
            ticket,
            extensions: Extensions::default(),
        };
        self.send_post_handshake(&nst.encode())
    }
}

/// The body of `CertificateStatus`: status type ocsp and the response.
pub(crate) fn certificate_status(response: &[u8]) -> Vec<u8> {
    let mut v = vec![crate::conn::OCSP_STATUS_TYPE];
    put_vec24(&mut v, response);
    v
}

pub(crate) fn parse_sni(data: &[u8]) -> Result<Option<String>, Error> {
    let mut r = Reader::new(data);
    let mut list = Reader::new(r.vec16()?);
    r.finish()?;
    let mut name = None;
    while !list.is_empty() {
        let typ = list.u8()?;
        let host = list.vec16()?;
        if typ == 0 && name.is_none() {
            if host.is_empty() || !host.is_ascii() {
                return Err(Error::Local(Alert::DecodeError));
            }
            name = Some(String::from_utf8_lossy(host).into_owned());
        }
    }
    Ok(name)
}

fn parse_key_shares(data: &[u8]) -> Result<Vec<(u16, Vec<u8>)>, Error> {
    let mut r = Reader::new(data);
    let mut list = Reader::new(r.vec16()?);
    r.finish()?;
    let mut out: Vec<(u16, Vec<u8>)> = Vec::new();
    while !list.is_empty() {
        let entry = msgs::read_key_share_entry(&mut list)?;
        if out.iter().any(|(g, _)| *g == entry.0) {
            return Err(illegal());
        }
        out.push(entry);
    }
    Ok(out)
}

pub(crate) fn choose_alpn(
    cfg: &ServerConfig,
    data: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, Error> {
    let (Some(data), false) = (data, cfg.alpn.is_empty()) else {
        return Ok(None);
    };
    let mut r = Reader::new(data);
    let mut list = Reader::new(r.vec16()?);
    r.finish()?;
    let mut offered = Vec::new();
    while !list.is_empty() {
        offered.push(list.vec8()?.to_vec());
    }
    cfg.alpn
        .iter()
        .find(|p| offered.contains(p))
        .cloned()
        .map(Some)
        .ok_or(Error::Local(Alert::NoApplicationProtocol))
}
