//! TLS over a blocking socket for the servers the tests stand up on threads of their own (an LDAP,
//! KMIP or HTTPS server the node's clients talk to). The node itself has no blocking sockets
//! (023 FR-024); only the tests do.

#![allow(dead_code)]

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
    teseladb_sys::fill_random(&mut e).map_err(|e| e.to_string())?;
    Ok(e)
}

/// A TLS connection whose handshake has finished.
pub struct TlsStream {
    sock: TcpStream,
    conn: Connection,
}

impl TlsStream {
    /// Connects as a client to `host` (the name its certificate has to hold) over `sock`.
    pub fn client(sock: TcpStream, cfg: Arc<ClientConfig>, host: &str) -> Result<TlsStream, String> {
        let conn = Connection::client(cfg, Some(host), now_ms(), &entropy()?, None)
            .map_err(|e| format!("{e:?}"))?;
        TlsStream::handshake(sock, conn, &[])
    }

    /// Accepts a client over `sock`, of which `first` was already read.
    pub fn server(sock: TcpStream, cfg: Arc<ServerConfig>, first: &[u8]) -> Result<TlsStream, String> {
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

    fn fail(&mut self, e: &teseladb_tls::Error) -> String {
        let _ = self.flush();
        format!("TLS: {e:?}")
    }

    fn flush(&mut self) -> Result<(), String> {
        let out = self.conn.take_output();
        if !out.is_empty() {
            self.sock.write_all(&out).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Feeds what the socket has to the connection; `false` when the peer closed the socket.
    fn read_raw(&mut self) -> Result<bool, String> {
        let mut buf = vec![0u8; CHUNK];
        let n = self.sock.read(&mut buf).map_err(|e| e.to_string())?;
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

/// A connection of a test server, with TLS or without.
pub enum Peer {
    /// In clear.
    Plain(TcpStream),
    /// Under TLS.
    Tls(Box<TlsStream>),
}

impl Peer {
    /// Sends `bytes`.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        match self {
            Peer::Plain(s) => s.write_all(bytes).map_err(|e| e.to_string()),
            Peer::Tls(t) => t.send(bytes),
        }
    }

    /// The next bytes; empty when the peer closed.
    pub fn recv(&mut self) -> Result<Vec<u8>, String> {
        match self {
            Peer::Plain(s) => {
                let mut buf = vec![0u8; CHUNK];
                let n = s.read(&mut buf).map_err(|e| e.to_string())?;
                buf.truncate(n);
                Ok(buf)
            }
            Peer::Tls(t) => t.recv(),
        }
    }
}
