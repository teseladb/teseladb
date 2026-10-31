//! The TLS 1.2 server handshake for the clients' port (RFC 5246, 7.3; RFC 8422; RFC 7627):
//! a full ECDHE handshake every time (no session resumption), with optional client certificates,
//! secure renegotiation announced and renegotiation refused.

use std::sync::Arc;

use crate::codec::{Reader, put_vec8, put_vec16};
use crate::config::{ClientAuth, ServerConfig, Version};
use crate::conn::{Connection, Handshake12, Role, State};
use crate::keyschedule::Secret;
use crate::kx::{Ephemeral, KEY_RANDOM_LEN};
use crate::msgs::{self, CertEntry, ClientHello, Extensions, ServerHello, hs};
use crate::record::Protection;
use crate::suites::NamedGroup;
use crate::tls12::{
    self, DOWNGRADE_SENTINEL, FALLBACK_SCSV, RENEGOTIATION_SCSV, SCHEMES, ServerKeyExchange,
    UNCOMPRESSED, ext,
};
use crate::{Alert, Error};

fn internal() -> Error {
    Error::Local(Alert::InternalError)
}

fn failure() -> Error {
    Error::Local(Alert::HandshakeFailure)
}

fn config(c: &Connection) -> Result<Arc<ServerConfig>, Error> {
    match &c.role {
        Role::Server(cfg) => Ok(cfg.clone()),
        Role::Client(_) => Err(internal()),
    }
}

/// An extension that must be empty, if present.
fn empty_extension(ch: &ClientHello, typ: u16) -> Result<bool, Error> {
    match ch.extensions.get(typ) {
        None => Ok(false),
        Some([]) => Ok(true),
        Some(_) => Err(Error::Local(Alert::DecodeError)),
    }
}

impl Connection {
    /// Answers a ClientHello that does not offer TLS 1.3.
    pub(crate) fn server12_client_hello(
        &mut self,
        cfg: &ServerConfig,
        ch: &ClientHello,
        msg: &[u8],
    ) -> Result<(), Error> {
        // supported_versions, when sent, overrides legacy_version (RFC 8446, 4.2.1).
        let offers_tls12 = match ch.extensions.get(msgs::ext::SUPPORTED_VERSIONS) {
            Some(v) => {
                let mut r = Reader::new(v);
                let list = r.vec8()?;
                r.finish()?;
                list.chunks(2).any(|c| c == msgs::TLS12.to_be_bytes())
            }
            None => ch.legacy_version >= msgs::TLS12,
        };
        if !offers_tls12 {
            return Err(Error::Local(Alert::ProtocolVersion));
        }
        // A client retrying with a lower version than it supports, while this side has TLS 1.3.
        let tls13 = cfg.max_version == Version::Tls13;
        if tls13 && ch.suites.contains(&FALLBACK_SCSV) {
            return Err(Error::Local(Alert::InappropriateFallback));
        }
        if !ch.compression.contains(&0) {
            return Err(Error::Local(Alert::IllegalParameter));
        }
        let secure_renegotiation = match ch.extensions.get(ext::RENEGOTIATION_INFO) {
            Some([0]) => true,
            Some(_) => return Err(failure()),
            None => ch.suites.contains(&RENEGOTIATION_SCSV),
        };
        let ems = empty_extension(ch, ext::EXTENDED_MASTER_SECRET)?;
        if cfg.require_extended_master_secret && !ems {
            return Err(failure());
        }
        let point_formats = match ch.extensions.get(ext::EC_POINT_FORMATS) {
            Some(data) => {
                let mut r = Reader::new(data);
                let list = r.vec8()?;
                r.finish()?;
                if !list.contains(&UNCOMPRESSED) {
                    return Err(Error::Local(Alert::IllegalParameter));
                }
                true
            }
            None => false,
        };
        // Without supported_groups the client takes any curve (RFC 8422, 4): P-256.
        let offered_groups = match ch.extensions.get(msgs::ext::SUPPORTED_GROUPS) {
            Some(g) => msgs::read_u16_list(g)?,
            None => vec![NamedGroup::Secp256r1.code()],
        };
        let group = cfg
            .groups
            .iter()
            .copied()
            .find(|g| offered_groups.contains(&g.code()))
            .ok_or(failure())?;
        let peer_schemes = msgs::read_u16_list(
            ch.extensions.get(msgs::ext::SIGNATURE_ALGORITHMS).ok_or(failure())?,
        )?;
        let key = &cfg.identity.key;
        let suite = cfg
            .tls12_suites
            .iter()
            .copied()
            .find(|s| ch.suites.contains(&s.code()) && s.fits(key))
            .ok_or(failure())?;
        let scheme = tls12::choose_scheme(key, &peer_schemes).ok_or(failure())?;
        if let Some(sni) = ch.extensions.get(msgs::ext::SERVER_NAME) {
            self.server_name = crate::server::parse_sni(sni)?;
        }
        self.alpn = crate::server::choose_alpn(cfg, ch.extensions.get(msgs::ext::ALPN))?;
        let staple = cfg
            .ocsp_staple
            .as_ref()
            .filter(|_| ch.extensions.get(msgs::ext::STATUS_REQUEST).is_some());
        self.transcript.extend_from_slice(msg);
        let mut server_random = self.random::<32>()?;
        if tls13 {
            server_random[24..].copy_from_slice(&DOWNGRADE_SENTINEL);
        }
        let eph = Ephemeral::generate(group, &self.random::<KEY_RANDOM_LEN>()?)?;
        let mut e = Extensions::default();
        if secure_renegotiation {
            e.push(ext::RENEGOTIATION_INFO, tls12::one_byte_list(&[]));
        }
        if ems {
            e.push(ext::EXTENDED_MASTER_SECRET, Vec::new());
        }
        if point_formats {
            e.push(ext::EC_POINT_FORMATS, tls12::one_byte_list(&[UNCOMPRESSED]));
        }
        if staple.is_some() {
            e.push(msgs::ext::STATUS_REQUEST, Vec::new());
        }
        if let Some(p) = &self.alpn {
            let mut list = Vec::new();
            put_vec8(&mut list, p);
            let mut v = Vec::new();
            put_vec16(&mut v, &list);
            e.push(msgs::ext::ALPN, v);
        }
        let sh = ServerHello {
            random: server_random,
            session_id: Vec::new(),
            suite: suite.code(),
            extensions: e,
        };
        self.send_handshake(&sh.encode())?;
        let chain: Vec<Vec<u8>> = cfg.identity.chain.iter().map(|c| c.der().to_vec()).collect();
        self.send_handshake(&tls12::certificate(&chain))?;
        if let Some(response) = staple {
            let status = crate::server::certificate_status(response);
            self.send_handshake(&msgs::frame(tls12::hs::CERTIFICATE_STATUS, &status))?;
        }
        let params = tls12::ecdh_params(group, &eph.public());
        let signed = [ch.random.as_slice(), server_random.as_slice(), params.as_slice()].concat();
        let salt = self.random::<64>()?;
        let sig = tls12::sign(key, scheme, &signed, &salt)?;
        self.send_handshake(&ServerKeyExchange::encode(&params, scheme, &sig))?;
        self.requested_cert = cfg.client_auth != ClientAuth::None;
        if self.requested_cert {
            self.send_handshake(&tls12::certificate_request(&SCHEMES))?;
        }
        self.send_handshake(&msgs::frame(tls12::hs::SERVER_HELLO_DONE, &[]))?;
        self.tls12 = Some(Box::new(Handshake12 {
            suite,
            client_random: ch.random,
            server_random,
            ems,
            master: None,
            ephemeral: Some(eph),
            server_share: None,
            cert_request: None,
            pending_read: None,
            pending_write: None,
        }));
        self.state = if self.requested_cert {
            State::Server12WaitCertificate
        } else {
            State::Server12WaitKeyExchange
        };
        Ok(())
    }

    /// The client's messages after ServerHelloDone.
    pub(crate) fn server12_message(
        &mut self,
        typ: u8,
        body: &[u8],
        msg: &[u8],
    ) -> Result<(), Error> {
        match (self.state, typ) {
            (State::Server12WaitCertificate, hs::CERTIFICATE) => {
                let chain = tls12::parse_certificate(body)?;
                if chain.is_empty() {
                    if config(self)?.client_auth == ClientAuth::Required {
                        return Err(failure());
                    }
                } else {
                    let entries: Vec<CertEntry> = chain
                        .into_iter()
                        .map(|der| CertEntry { der, extensions: Extensions::default() })
                        .collect();
                    self.validate_chain(&entries, teseladb_x509::Purpose::ClientAuth)?;
                }
                self.transcript.extend_from_slice(msg);
                self.state = State::Server12WaitKeyExchange;
                Ok(())
            }
            (State::Server12WaitKeyExchange, tls12::hs::CLIENT_KEY_EXCHANGE) => {
                self.server12_key_exchange(body, msg)
            }
            (State::Server12WaitCertVerify, hs::CERTIFICATE_VERIFY) => {
                let cv = msgs::CertificateVerify::parse(body)?;
                let leaf = self.peer_chain.first().ok_or(internal())?;
                tls12::verify(leaf, cv.scheme, &SCHEMES, &self.transcript, &cv.signature)?;
                self.transcript.extend_from_slice(msg);
                self.state = State::Server12WaitCcs;
                Ok(())
            }
            (State::Server12WaitFinished, hs::FINISHED) => self.server12_finished(body, msg),
            _ => Err(Error::Local(Alert::UnexpectedMessage)),
        }
    }

    fn server12_key_exchange(&mut self, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        let point = tls12::parse_client_key_exchange(body)?;
        self.transcript.extend_from_slice(msg);
        let session_hash = self.hs12()?.suite.hash().hash(&self.transcript);
        let h = self.hs12()?;
        let pre_master: Secret = h.ephemeral.take().ok_or(internal())?.agree(&point)?;
        let master = tls12::master_secret(
            h.suite.hash(),
            &pre_master.0,
            h.ems.then_some(session_hash.as_ref()),
            &h.client_random,
            &h.server_random,
        );
        let (client, server) =
            tls12::key_block(h.suite, &master.0, &h.client_random, &h.server_random);
        h.pending_read = Some(Protection::tls12(h.suite, &client)?);
        // This side's keys wait for its own change_cipher_spec, after the client's Finished.
        h.pending_write = Some(Protection::tls12(h.suite, &server)?);
        h.master = Some(master);
        self.state = if self.peer_chain.is_empty() {
            State::Server12WaitCcs
        } else {
            State::Server12WaitCertVerify
        };
        Ok(())
    }

    fn server12_finished(&mut self, body: &[u8], msg: &[u8]) -> Result<(), Error> {
        let h = self.tls12.as_deref().ok_or(internal())?;
        let master = h.master.as_ref().ok_or(internal())?;
        let expected = tls12::verify_data(h.suite, &master.0, true, &self.transcript);
        if !teseladb_crypto::ct::ct_eq(&expected, body) {
            return Err(Error::Local(Alert::DecryptError));
        }
        let mine = {
            let mut t = self.transcript.clone();
            t.extend_from_slice(msg);
            tls12::verify_data(h.suite, &master.0, false, &t)
        };
        self.transcript.extend_from_slice(msg);
        self.send_ccs();
        self.records.write = Some(self.hs12()?.pending_write.take().ok_or(internal())?);
        self.send_handshake(&msgs::frame(hs::FINISHED, &mine))?;
        self.finish12()
    }
}
