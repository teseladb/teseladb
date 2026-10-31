//! TLS over a blocking socket, for the connections that live on their own thread: the
//! administration port and `teseladb admin` (013 FR-017) and the LDAP client (013 FR-025).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use teseladb_tls::{ClientConfig, Connection, ENTROPY_LEN, ServerConfig};
use teseladb_x509::Certificate;

/// Bytes read from the socket at a time.
const CHUNK: usize = 16 << 10;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn entropy() -> Result<[u8; ENTROPY_LEN], String> {
    let mut e = [0u8; ENTROPY_LEN];
    teseladb_sys::fill_random(&mut e).map_err(crate::error_text)?;
    Ok(e)
}

/// A TLS connection whose handshake has finished.
pub struct TlsStream {
    sock: TcpStream,
    conn: Connection,
}

impl std::fmt::Debug for TlsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsStream").field("sock", &self.sock).finish_non_exhaustive()
    }
}

impl TlsStream {
    /// Connects as a client to `host` (the name its certificate has to hold) over `sock`.
    pub fn client(
        sock: TcpStream,
        cfg: Arc<ClientConfig>,
        host: &str,
    ) -> Result<TlsStream, String> {
        let conn = Connection::client(cfg, Some(host), now_ms(), &entropy()?, None)
            .map_err(crate::error_debug)?;
        TlsStream::handshake(sock, conn, &[])
    }

    /// Accepts a client over `sock`, of which `first` was already read.
    pub fn server(
        sock: TcpStream,
        cfg: Arc<ServerConfig>,
        first: &[u8],
    ) -> Result<TlsStream, String> {
        let conn = Connection::server(cfg, now_ms(), &entropy()?);
        TlsStream::handshake(sock, conn, first)
    }

    fn handshake(sock: TcpStream, conn: Connection, first: &[u8]) -> Result<TlsStream, String> {
        let mut s = TlsStream { sock, conn };
        if !first.is_empty() {
            s.conn.feed(first).map_err(|e| s.fail(&e))?;
        }
        loop {
            s.flush()?;
            if !s.conn.is_handshaking() {
                return Ok(s);
            }
            if !s.read_raw()? {
                return Err("the connection closed during the TLS handshake".into());
            }
        }
    }

    /// Sends the alert the connection holds after `e` and names the error.
    fn fail(&mut self, e: &teseladb_tls::Error) -> String {
        let _ = self.flush();
        format!("TLS: {e:?}")
    }

    fn flush(&mut self) -> Result<(), String> {
        let out = self.conn.take_output();
        if !out.is_empty() {
            self.sock.write_all(&out).map_err(crate::error_text)?;
        }
        Ok(())
    }

    /// Feeds what the socket has to the connection; `false` when the peer closed the socket.
    fn read_raw(&mut self) -> Result<bool, String> {
        let mut buf = vec![0u8; CHUNK];
        let n = self.sock.read(&mut buf).map_err(crate::error_text)?;
        if n == 0 {
            return Ok(false);
        }
        let fed = self.conn.feed(&buf[..n]);
        fed.map_err(|e| self.fail(&e))?;
        Ok(true)
    }

    /// Sends `bytes`.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        let written = self.conn.write(bytes);
        written.map_err(|e| self.fail(&e))?;
        self.flush()
    }

    /// The next bytes from the peer, at least one; empty when the peer closed.
    pub fn recv(&mut self) -> Result<Vec<u8>, String> {
        loop {
            let data = self.conn.read();
            if !data.is_empty() {
                return Ok(data);
            }
            if self.conn.peer_closed() || !self.read_raw()? {
                return Ok(Vec::new());
            }
            self.flush()?;
        }
    }

    /// Everything the peer sends until it closes.
    pub fn recv_to_end(&mut self) -> Result<Vec<u8>, String> {
        let mut all = Vec::new();
        loop {
            let data = self.recv()?;
            if data.is_empty() {
                return Ok(all);
            }
            all.extend_from_slice(&data);
        }
    }

    /// Sends `close_notify`.
    pub fn close(&mut self) {
        if self.conn.close().is_ok() {
            let _ = self.flush();
        }
    }

    /// The peer's certificate chain, leaf first, if it sent one.
    pub fn peer_certificates(&self) -> &[Certificate] {
        self.conn.peer_certificates()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use teseladb_tls::{CertifiedKey, Trust};
    use teseladb_x509::ext::GeneralName;

    fn pki(name: &str) -> crate::pki::Pki {
        let dir =
            std::env::temp_dir().join(format!("teseladb-tls-stream-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
        let params =
            crate::pki::PkiParams { now, node_names: vec![GeneralName::Dns("localhost".into())] };
        crate::pki::load_or_create(&dir, &params, &mut |b| teseladb_sys::fill_random(b)).unwrap()
    }

    /// A server that echoes what it reads back over TLS until the client closes.
    fn echo(config: Arc<ServerConfig>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(sock) = sock else { return };
                let config = config.clone();
                std::thread::spawn(move || {
                    let Ok(mut s) = TlsStream::server(sock, config, &[]) else { return };
                    while let Ok(data) = s.recv() {
                        if data.is_empty() || s.send(&data).is_err() {
                            break;
                        }
                    }
                    s.close();
                    s.close();
                });
            }
        });
        port
    }

    #[test]
    fn a_client_and_a_server_talk_and_close() {
        let p = pki("talk");
        let identity = CertifiedKey { chain: p.node_chain.clone(), key: p.node_key.clone() };
        let port = echo(Arc::new(ServerConfig::new(identity)));
        let trust = Arc::new(ClientConfig::new(Trust::anchors(vec![p.ca.clone()])));
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut c = TlsStream::client(sock, trust.clone(), "localhost").unwrap();
        assert!(format!("{c:?}").contains("TlsStream"));
        assert!(c.peer_certificates().len() == 1);
        c.send(b"hello").unwrap();
        assert_eq!(c.recv().unwrap(), b"hello");
        c.close();
        assert!(c.send(b"after close").is_err());
        assert_eq!(c.recv_to_end().unwrap(), Vec::<u8>::new());
        // A server name the certificate does not hold, and an untrusted server.
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(TlsStream::client(sock, trust, "other.test").is_err());
        let none = Arc::new(ClientConfig::new(Trust::anchors(Vec::new())));
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(TlsStream::client(sock, none, "localhost").unwrap_err().starts_with("TLS: "));
    }

    #[test]
    fn a_server_refuses_what_is_not_a_handshake() {
        let p = pki("refuse");
        let identity = CertifiedKey { chain: p.node_chain.clone(), key: p.node_key.clone() };
        let config = Arc::new(ServerConfig::new(identity));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Bytes already read that are not TLS, and a client that closes during the handshake.
        let client = std::thread::spawn(move || TcpStream::connect(addr).unwrap());
        let (sock, _) = listener.accept().unwrap();
        let _keep = client.join().unwrap();
        assert!(TlsStream::server(sock, config.clone(), b"GET / HTTP/1.1\r\n\r\n").is_err());
        let client = std::thread::spawn(move || drop(TcpStream::connect(addr).unwrap()));
        let (sock, _) = listener.accept().unwrap();
        client.join().unwrap();
        let e = TlsStream::server(sock, config, &[]).unwrap_err();
        assert!(e.contains("closed during the TLS handshake"), "{e}");
    }
}
