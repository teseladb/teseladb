//! The TLS 1.2 client handshake (RFC 5246, 7.3; RFC 8422; RFC 7627), for the outbound clients
//! whose `min_version` is TLS 1.2 when the server does not have TLS 1.3: a full ECDHE handshake,
//! with the client certificate when asked, secure renegotiation required and the downgrade
//! sentinel of RFC 8446, 4.1.3 checked.

use std::sync::Arc;

use crate::codec::Reader;
use crate::config::{ClientConfig, Version};
use crate::conn::{Connection, Handshake12, Role, State};
use crate::kx::{Ephemeral, KEY_RANDOM_LEN};
use crate::msgs::{self, CertEntry, ClientHello, Extensions, ServerHello, hs};
use crate::record::Protection;
use crate::suites::NamedGroup;
use crate::tls12::{self, Auth, DOWNGRADE_SENTINEL, SCHEMES, ServerKeyExchange, Tls12Suite, ext};
use crate::{Alert, Error};

fn internal() -> Error {
    Error::Local(Alert::InternalError)
}

fn illegal() -> Error {
    Error::Local(Alert::IllegalParameter)
}

fn config(c: &Connection) -> Result<Arc<ClientConfig>, Error> {
    match &c.role {
        Role::Client(cfg) => Ok(cfg.clone()),
        Role::Server(_) => Err(internal()),
    }
}

impl Connection {
    /// A ServerHello that chose TLS 1.2.
    pub(crate) fn client12_server_hello(
        &mut self,
        sh: &ServerHello,
        msg: &[u8],
    ) -> Result<(), Error> {
        let cfg = config(self)?;
        // A TLS 1.3 server that negotiates TLS 1.2 says so: a downgrade by an attacker.
        if cfg.max_version == Version::Tls13 && sh.random[24..] == DOWNGRADE_SENTINEL {
            return Err(illegal());
        }
        let suite = Tls12Suite::from_code(sh.suite)
            .filter(|s| cfg.tls12_suites.contains(s))
            .ok_or(illegal())?;
        // The session id this side sent is random and names no session: an echo would be a
        // resumption it never offered.
        if sh.session_id == self.session_id {
            return Err(illegal());
        }
        sh.extensions.only(&[
            ext::RENEGOTIATION_INFO,
            ext::EXTENDED_MASTER_SECRET,
            ext::EC_POINT_FORMATS,
            msgs::ext::ALPN,
            msgs::ext::SERVER_NAME,
            msgs::ext::STATUS_REQUEST,
        ])?;
        if let Some(s) = sh.extensions.get(msgs::ext::STATUS_REQUEST) {
            if !s.is_empty() || !cfg.request_ocsp {
                return Err(illegal());
            }
            self.ocsp_requested = true;
        }
        if sh.extensions.get(ext::RENEGOTIATION_INFO) != Some(&[0][..]) {
            return Err(Error::Local(Alert::HandshakeFailure));
        }
        let ems = match sh.extensions.get(ext::EXTENDED_MASTER_SECRET) {
            None => false,
            Some([]) => true,
            Some(_) => return Err(Error::Local(Alert::DecodeError)),
        };
        if cfg.require_extended_master_secret && !ems {
            return Err(Error::Local(Alert::HandshakeFailure));
        }
        if let Some(a) = sh.extensions.get(msgs::ext::ALPN) {
            let mut r = Reader::new(a);
            let mut list = Reader::new(r.vec16()?);
            r.finish()?;
            let proto = list.vec8()?.to_vec();
            list.finish()?;
            if !cfg.alpn.contains(&proto) {
                return Err(illegal());
            }
            self.alpn = Some(proto);
        }
        let client_random = match self.sent_hello.as_deref().map(msgs::unframe) {
            Some(Ok((_, b))) => ClientHello::parse(b)?.random,
            _ => return Err(internal()),
        };
        self.transcript.extend_from_slice(msg);
        self.tls12 = Some(Box::new(Handshake12 {
            suite,
            client_random,
            server_random: sh.random,
            ems,
            master: None,
            ephemeral: None,
            server_share: None,
            cert_request: None,
            pending_read: None,
            pending_write: None,
        }));
        self.state = State::Client12WaitCertificate;
        Ok(())
    }

    /// The server's messages after its ServerHello.
    pub(crate) fn client12_message(
        &mut self,
        typ: u8,
        body: &[u8],
        msg: &[u8],
    ) -> Result<(), Error> {
        match (self.state, typ) {
            (State::Client12WaitCertificate, hs::CERTIFICATE) => {
                let chain = tls12::parse_certificate(body)?;
                if chain.is_empty() {
                    return Err(Error::Local(Alert::DecodeError));
                }
                let entries: Vec<CertEntry> = chain
                    .into_iter()
                    .map(|der| CertEntry { der, extensions: Extensions::default() })
                    .collect();
                self.validate_chain(&entries, teseladb_x509::Purpose::ServerAuth)?;
                let rsa = matches!(
                    self.peer_chain.first().map(|c| &c.public_key),
                    Some(teseladb_x509::PublicKey::Rsa(_))
                );
                if rsa != (self.hs12()?.suite.auth() == Auth::Rsa) {
                    return Err(Error::Local(Alert::UnsupportedCertificate));
                }
                self.transcript.extend_from_slice(msg);
                self.state = State::Client12WaitKeyExchange;
                Ok(())
            }
            (State::Client12WaitKeyExchange, tls12::hs::CERTIFICATE_STATUS)
                if self.ocsp_requested =>
            {
                self.ocsp_requested = false;
                self.check_staple(body)?;
                self.transcript.extend_from_slice(msg);
                Ok(())
            }
            (State::Client12WaitKeyExchange, tls12::hs::SERVER_KEY_EXCHANGE) => {
                let ske = ServerKeyExchange::parse(body)?;
                let cfg = config(self)?;
                let group = NamedGroup::from_code(ske.group)
                    .filter(|g| cfg.groups.contains(g))
                    .ok_or(illegal())?;
                let h = self.tls12.as_deref().ok_or(internal())?;
                let signed =
                    [h.client_random.as_slice(), h.server_random.as_slice(), &ske.params].concat();
                let leaf = self.peer_chain.first().ok_or(internal())?;
                tls12::verify(leaf, ske.scheme, &SCHEMES, &signed, &ske.signature)?;
                self.hs12()?.server_share = Some((group, ske.public));
                self.transcript.extend_from_slice(msg);
                self.state = State::Client12WaitDone;
                Ok(())
            }
            (State::Client12WaitDone, hs::CERTIFICATE_REQUEST)
                if self.tls12.as_ref().is_some_and(|h| h.cert_request.is_none()) =>
            {
                let schemes = tls12::parse_certificate_request(body)?;
                self.hs12()?.cert_request = Some(schemes);
                self.transcript.extend_from_slice(msg);
                Ok(())
            }
            (State::Client12WaitDone, tls12::hs::SERVER_HELLO_DONE) => {
                if !body.is_empty() {
                    return Err(Error::Local(Alert::DecodeError));
                }
                self.transcript.extend_from_slice(msg);
                self.client12_flight()
            }
            (State::Client12WaitFinished, hs::FINISHED) => {
                let h = self.tls12.as_deref().ok_or(internal())?;
                let master = h.master.as_ref().ok_or(internal())?;
                let expected = tls12::verify_data(h.suite, &master.0, false, &self.transcript);
                if !teseladb_crypto::ct::ct_eq(&expected, body) {
                    return Err(Error::Local(Alert::DecryptError));
                }
                self.transcript.extend_from_slice(msg);
                self.finish12()
            }
            _ => Err(Error::Local(Alert::UnexpectedMessage)),
        }
    }

    /// Certificate, ClientKeyExchange, CertificateVerify, change_cipher_spec and Finished.
    fn client12_flight(&mut self) -> Result<(), Error> {
        let cfg = config(self)?;
        let request = self.hs12()?.cert_request.take();
        // Without a certificate, or without a scheme the key can use, an empty Certificate.
        let chosen = request.as_ref().and_then(|schemes| {
            cfg.identity
                .as_ref()
                .and_then(|id| tls12::choose_scheme(&id.key, schemes).map(|s| (id, s)))
        });
        if request.is_some() {
            let chain: Vec<Vec<u8>> = chosen
                .map(|(id, _)| id.chain.iter().map(|c| c.der().to_vec()).collect())
                .unwrap_or_default();
            self.send_handshake(&tls12::certificate(&chain))?;
        }
        let (group, server_public) = self.hs12()?.server_share.take().ok_or(internal())?;
        let eph = match self.ephemerals.iter().position(|k| k.group() == group) {
            Some(i) => self.ephemerals.swap_remove(i),
            None => Ephemeral::generate(group, &self.random::<KEY_RANDOM_LEN>()?)?,
        };
        self.ephemerals.clear();
        self.send_handshake(&tls12::client_key_exchange(&eph.public()))?;
        let pre_master = eph.agree(&server_public)?;
        let session_hash = self.hs12()?.suite.hash().hash(&self.transcript);
        let h = self.hs12()?;
        let master = tls12::master_secret(
            h.suite.hash(),
            &pre_master.0,
            h.ems.then_some(session_hash.as_ref()),
            &h.client_random,
            &h.server_random,
        );
        let (client, server) =
            tls12::key_block(h.suite, &master.0, &h.client_random, &h.server_random);
        h.pending_read = Some(Protection::tls12(h.suite, &server)?);
        let suite = h.suite;
        h.master = Some(master);
        if let Some((id, scheme)) = chosen {
            let salt = self.random::<64>()?;
            let sig = tls12::sign(&id.key, scheme, &self.transcript, &salt)?;
            let cv = msgs::CertificateVerify { scheme: scheme.code(), signature: sig };
            self.send_handshake(&cv.encode())?;
        }
        self.send_ccs();
        self.records.write = Some(Protection::tls12(suite, &client)?);
        let h = self.tls12.as_deref().ok_or(internal())?;
        let master = h.master.as_ref().ok_or(internal())?;
        let fin = tls12::verify_data(suite, &master.0, true, &self.transcript);
        self.send_handshake(&msgs::frame(hs::FINISHED, &fin))?;
        self.state = State::Client12WaitCcs;
        Ok(())
    }
}
