//! The client handshake (RFC 8446, 4 and 2): ClientHello with a key share and an optional
//! ticket, HelloRetryRequest, server authentication, client authentication when asked, and
//! session tickets.

use std::sync::Arc;

use crate::codec::{Reader, put_vec8, put_vec16};
use crate::config::{ClientConfig, Version};
use crate::conn::{Connection, ENTROPY_LEN, Role, State, Ticket};
use crate::keyschedule::{self, KeySchedule};
use crate::kx::{self, Ephemeral, KEY_RANDOM_LEN};
use crate::msgs::{self, ClientHello, Extensions, OfferedPsks, ServerHello, ext, hs};
use crate::suites::{CipherSuite, NamedGroup, SignatureScheme};
use crate::tls12;
use crate::{Alert, Error};

const MAX_TICKET_LIFETIME_MS: i64 = crate::config::MAX_TICKET_LIFETIME as i64 * 1000;

fn illegal() -> Error {
    Error::Local(Alert::IllegalParameter)
}

fn internal() -> Error {
    Error::Local(Alert::InternalError)
}

fn config(c: &Connection) -> Result<Arc<ClientConfig>, Error> {
    match &c.role {
        Role::Client(cfg) => Ok(cfg.clone()),
        Role::Server(_) => Err(internal()),
    }
}

impl Connection {
    /// A client connection to `server_name` (a host name, sent as SNI and checked against the
    /// certificate, or an IP address, checked only), offering `resume` if it was issued for the
    /// same name and is still valid. The ClientHello is already in the output.
    pub fn client(
        config: Arc<ClientConfig>,
        server_name: Option<&str>,
        now_ms: i64,
        entropy: &[u8; ENTROPY_LEN],
        resume: Option<Ticket>,
    ) -> Result<Connection, Error> {
        let mut c = Connection::new(
            Role::Client(config.clone()),
            State::ClientWaitServerHello,
            now_ms,
            entropy,
        );
        c.server_name = server_name.map(str::to_string);
        c.session_id = c.random::<32>()?.to_vec();
        let group = *config.groups.first().ok_or(internal())?;
        let key_random = c.random::<KEY_RANDOM_LEN>()?;
        c.ephemerals.push(Ephemeral::generate(group, &key_random)?);
        c.offered_ticket = resume.filter(|t| {
            t.server_name == c.server_name
                && now_ms - t.received_ms < i64::from(t.lifetime) * 1000
                && config.suites.iter().any(|s| s.hash() == t.suite.hash())
        });
        let random = c.random::<32>()?;
        let hello = c.build_hello(random, None)?;
        c.send_handshake(&hello)?;
        c.sent_hello = Some(hello);
        Ok(c)
    }

    fn build_hello(&mut self, random: [u8; 32], cookie: Option<&[u8]>) -> Result<Vec<u8>, Error> {
        let cfg = config(self)?;
        let mut e = Extensions::default();
        if let Some(name) =
            self.server_name.as_deref().filter(|n| teseladb_x509::ip::parse(n).is_none())
        {
            let mut entry = vec![0u8];
            put_vec16(&mut entry, name.as_bytes());
            let mut list = Vec::new();
            put_vec16(&mut list, &entry);
            e.push(ext::SERVER_NAME, list);
        }
        let groups: Vec<u16> = cfg.groups.iter().map(|g| g.code()).collect();
        e.push(ext::SUPPORTED_GROUPS, msgs::u16_list(&groups));
        // TLS 1.2 adds RSASSA-PKCS1-v1_5, which TLS 1.3 never takes in CertificateVerify.
        let offered: &[SignatureScheme] = match cfg.min_version {
            Version::Tls12 => &tls12::SCHEMES,
            Version::Tls13 => &SignatureScheme::HANDSHAKE,
        };
        let schemes: Vec<u16> = offered.iter().map(|s| s.code()).collect();
        e.push(ext::SIGNATURE_ALGORITHMS, msgs::u16_list(&schemes));
        if cfg.request_ocsp {
            // OCSP, with no responder list and no request extensions.
            e.push(ext::STATUS_REQUEST, vec![crate::conn::OCSP_STATUS_TYPE, 0, 0, 0, 0]);
        }
        if cfg.min_version == Version::Tls12 {
            e.push(tls12::ext::EC_POINT_FORMATS, tls12::one_byte_list(&[tls12::UNCOMPRESSED]));
            e.push(tls12::ext::EXTENDED_MASTER_SECRET, Vec::new());
        }
        let tls13 = cfg.max_version == Version::Tls13;
        if tls13 {
            self.push_tls13_extensions(&mut e, cookie);
        }
        if !cfg.alpn.is_empty() {
            let mut list = Vec::new();
            for p in &cfg.alpn {
                put_vec8(&mut list, p);
            }
            let mut v = Vec::new();
            put_vec16(&mut v, &list);
            e.push(ext::ALPN, v);
        }
        let ticket = self
            .offered_ticket
            .clone()
            .filter(|t| tls13 && self.suite.is_none_or(|s| s.hash() == t.suite.hash()));
        Ok(self.finish_hello(&cfg, random, e, ticket))
    }

    /// supported_versions, key_share, psk_key_exchange_modes and the cookie of a retry.
    fn push_tls13_extensions(&self, e: &mut Extensions, cookie: Option<&[u8]>) {
        let cfg = match &self.role {
            Role::Client(cfg) => cfg.clone(),
            Role::Server(_) => return,
        };
        let versions: &[u8] = match cfg.min_version {
            Version::Tls12 => &[0x03, 0x04, 0x03, 0x03],
            Version::Tls13 => &[0x03, 0x04],
        };
        let mut v = Vec::new();
        put_vec8(&mut v, versions);
        e.push(ext::SUPPORTED_VERSIONS, v);
        let shares: Vec<u8> = self
            .ephemerals
            .iter()
            .flat_map(|k| msgs::key_share_entry(k.group().code(), &k.public()))
            .collect();
        let mut ks = Vec::new();
        put_vec16(&mut ks, &shares);
        e.push(ext::KEY_SHARE, ks);
        e.push(ext::PSK_KEY_EXCHANGE_MODES, vec![1, msgs::PSK_DHE_KE]);
        if let Some(c) = cookie {
            let mut v = Vec::new();
            put_vec16(&mut v, c);
            e.push(ext::COOKIE, v);
        }
    }

    /// The ticket's pre_shared_key (last), the suites and the binder.
    fn finish_hello(
        &mut self,
        cfg: &ClientConfig,
        random: [u8; 32],
        mut e: Extensions,
        ticket: Option<Ticket>,
    ) -> Vec<u8> {
        if let Some(t) = &ticket {
            let age = (self.now_ms - t.received_ms).max(0) as u32;
            let psks = OfferedPsks {
                identities: vec![(t.ticket.clone(), age.wrapping_add(t.age_add))],
                binders: vec![vec![0; t.suite.hash().output_len()]],
            };
            e.push(ext::PRE_SHARED_KEY, psks.encode());
        }
        self.offered_ticket = ticket;
        self.offered_extensions = e.0.iter().map(|(t, _)| *t).collect();
        let hello = ClientHello {
            legacy_version: msgs::TLS12,
            random,
            session_id: self.session_id.clone(),
            suites: cfg
                .suites
                .iter()
                .filter(|_| cfg.max_version == Version::Tls13)
                .map(|s| s.code())
                .chain(match cfg.min_version {
                    Version::Tls12 => cfg
                        .tls12_suites
                        .iter()
                        .map(|s| s.code())
                        .chain([tls12::RENEGOTIATION_SCSV])
                        .collect(),
                    Version::Tls13 => Vec::new(),
                })
                .collect(),
            compression: vec![0],
            extensions: e,
        };
        let mut msg = hello.encode();
        if let Some(t) = &self.offered_ticket {
            let h = t.suite.hash();
            let binder_len = 2 + 1 + h.output_len();
            let mut partial = self.transcript.clone();
            partial.extend_from_slice(&msg[..msg.len() - binder_len]);
            let ks = KeySchedule::new(h, Some(&t.psk.0));
            let binder =
                keyschedule::finished(h, &ks.resumption_binder_key().0, h.hash(&partial).as_ref());
            let n = msg.len();
            msg[n - binder.len()..].copy_from_slice(&binder);
        }
        msg
    }

    pub(crate) fn client_message(&mut self, typ: u8, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        if self.tls12.is_some() {
            return self.client12_message(typ, body, msg);
        }
        match (self.state, typ) {
            (State::ClientWaitServerHello, hs::SERVER_HELLO) => self.client_server_hello(body, msg),
            (State::ClientWaitEncryptedExtensions, hs::ENCRYPTED_EXTENSIONS) => {
                self.client_ee(body, msg)
            }
            (State::ClientWaitCertOrRequest, hs::CERTIFICATE_REQUEST) => {
                let cr = msgs::CertificateRequest::parse(body)?;
                if !cr.context.is_empty() || cr.extensions.get(ext::SIGNATURE_ALGORITHMS).is_none()
                {
                    return Err(illegal());
                }
                self.cert_request = Some(cr);
                self.transcript.extend_from_slice(msg);
                self.state = State::ClientWaitCertificate;
                Ok(())
            }
            (State::ClientWaitCertOrRequest | State::ClientWaitCertificate, hs::CERTIFICATE) => {
                let cm = msgs::CertificateMsg::parse(body)?;
                if !cm.context.is_empty() || cm.entries.is_empty() {
                    return Err(Error::Local(Alert::DecodeError));
                }
                self.validate_chain(&cm.entries, teseladb_x509::Purpose::ServerAuth)?;
                if let Some(status) = cm.entries[0].extensions.get(ext::STATUS_REQUEST) {
                    if !self.offered_extensions.contains(&ext::STATUS_REQUEST) {
                        return Err(Error::Local(Alert::UnsupportedExtension));
                    }
                    self.check_staple(status)?;
                }
                self.transcript.extend_from_slice(msg);
                self.state = State::ClientWaitCertVerify;
                Ok(())
            }
            (State::ClientWaitCertVerify, hs::CERTIFICATE_VERIFY) => {
                self.check_cert_verify(body, &SignatureScheme::HANDSHAKE, true)?;
                self.transcript.extend_from_slice(msg);
                self.state = State::ClientWaitFinished;
                Ok(())
            }
            (State::ClientWaitFinished, hs::FINISHED) => self.client_finished(body, msg),
            _ => Err(Error::Local(Alert::UnexpectedMessage)),
        }
    }

    fn client_server_hello(&mut self, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        let sh = ServerHello::parse(body)?;
        let cfg = config(self)?;
        let version = sh.extensions.get(ext::SUPPORTED_VERSIONS);
        if version != Some(&[0x03, 0x04][..]) {
            // A TLS 1.2 server, where this side takes it; earlier versions are refused.
            let tls12 = version.is_none() && body.get(..2) == Some(&msgs::TLS12.to_be_bytes()[..]);
            if tls12 && cfg.min_version == Version::Tls12 && !self.retried {
                return self.client12_server_hello(&sh, msg);
            }
            return Err(Error::Local(Alert::ProtocolVersion));
        }
        let suite =
            CipherSuite::from_code(sh.suite).filter(|s| cfg.suites.contains(s)).ok_or(illegal())?;
        if sh.session_id != self.session_id || self.suite.is_some_and(|s| s != suite) {
            return Err(illegal());
        }
        self.suite = Some(suite);
        if sh.is_retry() {
            return self.client_retry(&sh, msg);
        }
        sh.extensions.only(&[ext::SUPPORTED_VERSIONS, ext::KEY_SHARE, ext::PRE_SHARED_KEY])?;
        let mut r = Reader::new(
            sh.extensions.get(ext::KEY_SHARE).ok_or(Error::Local(Alert::MissingExtension))?,
        );
        let (group, key) = msgs::read_key_share_entry(&mut r)?;
        r.finish()?;
        let shared = self
            .ephemerals
            .iter()
            .find(|k| k.group().code() == group)
            .ok_or(illegal())?
            .agree(&key)?;
        let psk = match sh.extensions.get(ext::PRE_SHARED_KEY) {
            Some(sel) => {
                let t = self.offered_ticket.as_ref().filter(|t| t.suite.hash() == suite.hash());
                match (sel, t) {
                    ([0, 0], Some(t)) => Some(t.psk.clone()),
                    _ => return Err(illegal()),
                }
            }
            None => None,
        };
        self.resumed = psk.is_some();
        if let (true, Some(t)) = (self.resumed, &self.offered_ticket) {
            self.peer_chain = t.server_chain.clone();
        }
        self.transcript.extend_from_slice(msg);
        let mut ks = KeySchedule::new(suite.hash(), psk.as_ref().map(|p| p.0.as_slice()));
        ks.advance(Some(&shared.0));
        let th = self.transcript_hash()?;
        let c = ks.derive(b"c hs traffic", &th);
        let s = ks.derive(b"s hs traffic", &th);
        self.schedule = Some(ks);
        self.set_read(&s.0)?;
        // From here on alerts and the client's flight go under the handshake keys.
        self.set_write(&c.0)?;
        self.hs_secrets = Some((c, s));
        self.ephemerals.clear();
        self.state = State::ClientWaitEncryptedExtensions;
        Ok(())
    }

    fn client_retry(&mut self, hrr: &ServerHello, msg: &[u8]) -> Result<(), Error> {
        if self.retried {
            return Err(Error::Local(Alert::UnexpectedMessage));
        }
        self.retried = true;
        hrr.extensions.only(&[ext::SUPPORTED_VERSIONS, ext::KEY_SHARE, ext::COOKIE])?;
        let cfg = config(self)?;
        let group = match hrr.extensions.get(ext::KEY_SHARE) {
            Some([a, b]) => NamedGroup::from_code(u16::from_be_bytes([*a, *b])),
            _ => None,
        }
        .filter(|g| cfg.groups.contains(g) && !self.ephemerals.iter().any(|k| k.group() == *g))
        .ok_or(illegal())?;
        let cookie = match hrr.extensions.get(ext::COOKIE) {
            Some(c) => {
                let mut r = Reader::new(c);
                let v = r.vec16()?.to_vec();
                r.finish()?;
                Some(v)
            }
            None => None,
        };
        // The transcript restarts with the hash of the first ClientHello (RFC 8446, 4.4.1).
        let hash = self.suite()?.hash();
        let ch1 = hash.hash(&self.transcript);
        self.transcript = msgs::frame(hs::MESSAGE_HASH, ch1.as_ref());
        self.transcript.extend_from_slice(msg);
        self.ephemerals = vec![Ephemeral::generate(group, &self.random::<KEY_RANDOM_LEN>()?)?];
        let random = match self.sent_hello.as_deref().map(msgs::unframe) {
            Some(Ok((_, b))) => ClientHello::parse(b)?.random,
            _ => return Err(internal()),
        };
        let hello = self.build_hello(random, cookie.as_deref())?;
        self.send_handshake(&hello)?;
        Ok(())
    }

    fn client_ee(&mut self, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        let mut r = Reader::new(body);
        let e = Extensions::read(&mut r)?;
        r.finish()?;
        let allowed: Vec<u16> = [ext::SERVER_NAME, ext::SUPPORTED_GROUPS, ext::ALPN]
            .into_iter()
            .filter(|t| self.offered_extensions.contains(t))
            .chain(
                self.offered_extensions
                    .iter()
                    .copied()
                    .filter(|t| !KNOWN_CLIENT_EXTENSIONS.contains(t)),
            )
            .collect();
        e.only(&allowed)?;
        if let Some(a) = e.get(ext::ALPN) {
            let mut r = Reader::new(a);
            let mut list = Reader::new(r.vec16()?);
            r.finish()?;
            let proto = list.vec8()?.to_vec();
            list.finish()?;
            if !config(self)?.alpn.contains(&proto) {
                return Err(illegal());
            }
            self.alpn = Some(proto);
        }
        self.transcript.extend_from_slice(msg);
        self.state =
            if self.resumed { State::ClientWaitFinished } else { State::ClientWaitCertOrRequest };
        Ok(())
    }

    fn client_finished(&mut self, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        let (c_hs, s_hs) = self.hs_secrets.take().ok_or(internal())?;
        self.check_finished(body, &s_hs.0)?;
        self.transcript.extend_from_slice(msg);
        self.derive_app_secrets()?;
        if let Some(cr) = self.cert_request.take() {
            self.client_send_certificate(&cr)?;
        }
        let fin = self.finished_message(&c_hs.0)?;
        self.send_handshake(&fin)?;
        let (c_ap, s_ap) =
            self.app_secrets.as_ref().map(|(c, s)| (c.0.clone(), s.0.clone())).ok_or(internal())?;
        self.set_write(&c_ap)?;
        self.set_read(&s_ap)?;
        self.derive_resumption()?;
        self.state = State::Connected;
        Ok(())
    }

    fn client_send_certificate(&mut self, cr: &msgs::CertificateRequest) -> Result<(), Error> {
        let cfg = config(self)?;
        let schemes =
            msgs::read_u16_list(cr.extensions.get(ext::SIGNATURE_ALGORITHMS).ok_or(illegal())?)?;
        // Without a certificate, or without a scheme the key can use, an empty Certificate.
        let chosen = cfg
            .identity
            .as_ref()
            .and_then(|id| kx::choose_scheme(&id.key, &schemes).map(|s| (id, s)));
        let entries = chosen
            .map(|(id, _)| {
                id.chain
                    .iter()
                    .map(|c| msgs::CertEntry {
                        der: c.der().to_vec(),
                        extensions: Extensions::default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cm = msgs::CertificateMsg { context: cr.context.clone(), entries };
        self.send_handshake(&cm.encode())?;
        if let Some((id, scheme)) = chosen {
            let content = kx::verify_content(false, &self.transcript_hash()?);
            let salt = self.random::<64>()?;
            let sig = kx::sign(&id.key, scheme, &content, &salt)?;
            let cv = msgs::CertificateVerify { scheme: scheme.code(), signature: sig };
            self.send_handshake(&cv.encode())?;
        }
        Ok(())
    }

    pub(crate) fn client_new_ticket(&mut self, body: &[u8]) -> Result<(), Error> {
        let nst = msgs::NewSessionTicket::parse(body)?;
        if i64::from(nst.lifetime) * 1000 > MAX_TICKET_LIFETIME_MS {
            return Err(illegal());
        }
        let suite = self.suite()?;
        let master = self.resumption_master.as_ref().ok_or(internal())?;
        if nst.lifetime > 0 {
            self.tickets.push(Ticket {
                server_name: self.server_name.clone(),
                suite,
                psk: keyschedule::resumption_psk(suite.hash(), &master.0, &nst.nonce),
                ticket: nst.ticket,
                age_add: nst.age_add,
                lifetime: nst.lifetime,
                received_ms: self.now_ms,
                server_chain: self.peer_chain.clone(),
            });
        }
        Ok(())
    }
}

/// Extensions this client may send; any other one in an offered hello (the replay of a recorded
/// hello in tests) may also come back in EncryptedExtensions.
const KNOWN_CLIENT_EXTENSIONS: [u16; 13] = [
    ext::STATUS_REQUEST,
    tls12::ext::EC_POINT_FORMATS,
    tls12::ext::EXTENDED_MASTER_SECRET,
    ext::SERVER_NAME,
    ext::SUPPORTED_GROUPS,
    ext::SIGNATURE_ALGORITHMS,
    ext::ALPN,
    ext::PRE_SHARED_KEY,
    ext::EARLY_DATA,
    ext::SUPPORTED_VERSIONS,
    ext::COOKIE,
    ext::PSK_KEY_EXCHANGE_MODES,
    ext::KEY_SHARE,
];
