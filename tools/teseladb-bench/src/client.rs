//! The bench's client of the wire protocol (002 FR-019): `OP_MSG` messages built and read with
//! `teseladb-wire`, so that TeselaDB and RefDb are measured with the same client and not with a
//! different driver on each side. `--target refdb://host:port` names the server; without it the
//! scenario starts a development cluster of three nodes in this process (`teseladb dev`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::cli::DevOpts;
use teseladb_server::node::Node;
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};

/// Longest a request waits for its answer.
const TIMEOUT: Duration = Duration::from_secs(30);
/// How often [`Target::wait_ranges`] looks at the routing table.
const RANGE_POLL: Duration = Duration::from_millis(10);
/// Nodes of the cluster started in this process without `--target`.
const LOCAL_NODES: u16 = 3;

/// Where a scenario sends its requests.
pub enum Target {
    /// A server by address.
    Remote(SocketAddr),
    /// A development cluster in this process, stopped when this is dropped.
    Local(Vec<Node<MemDisk>>),
}

impl Target {
    /// From `--target` (`refdb://host:port[/...]`), or a local cluster without it.
    pub fn open(uri: Option<&str>) -> Result<Target, String> {
        let Some(uri) = uri else { return Target::local(LOCAL_NODES, 1) };
        let rest = uri
            .strip_prefix("refdb://")
            .ok_or(format!("--target is refdb://host:port, not {uri}"))?;
        let host = rest.split(['/', '?', ',']).next().unwrap_or_default();
        let addr = host
            .to_socket_addrs()
            .map_err(|e| format!("{host}: {e}"))?
            .next()
            .ok_or(format!("{host}: no address"))?;
        Ok(Target::Remote(addr))
    }

    /// A cluster of `nodes` nodes of `cores` cores in this process.
    pub fn local(nodes: u16, cores: u16) -> Result<Target, String> {
        let opts = DevOpts { nodes, cores, port: 0, test_commands: false };
        match teseladb_server::dev::start(&opts, None) {
            Ok(nodes) => Ok(Target::Local(nodes)),
            Err(e) => Err(format!("local cluster: {e:?}")),
        }
    }

    /// Address of the server (the first node of a local cluster).
    pub fn addr(&self) -> Result<SocketAddr, String> {
        match self {
            Target::Remote(a) => Ok(*a),
            Target::Local(nodes) => nodes
                .first()
                .and_then(|n| n.client_addrs().first().copied())
                .ok_or("the local cluster has no clients' port".into()),
        }
    }
}

impl Target {
    /// The administration port of a local cluster's first node (`None` for a remote server).
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        match self {
            Target::Remote(_) => None,
            Target::Local(nodes) => nodes.first().and_then(Node::admin_addr),
        }
    }

    /// The ranges a local cluster's first node routes (`None` for a remote server).
    pub fn ranges(&self) -> Option<Vec<u64>> {
        match self {
            Target::Remote(_) => None,
            Target::Local(nodes) => {
                nodes.first().map(|n| n.ranges().iter().map(|e| e.group.0).collect())
            }
        }
    }

    /// Waits until a local cluster's first node routes at least `n` ranges.
    pub fn wait_ranges(&self, n: usize) -> Result<(), String> {
        let until = Instant::now() + TIMEOUT;
        while self.ranges().is_some_and(|r| r.len() < n) {
            if Instant::now() >= until {
                return Err(format!("the cluster did not route {n} ranges within {TIMEOUT:?}"));
            }
            std::thread::sleep(RANGE_POLL);
        }
        Ok(())
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Target::Local(nodes) = self {
            for n in nodes.drain(..) {
                drop(n.stop());
            }
        }
    }
}

/// One connection.
pub struct Client {
    stream: TcpStream,
    next_id: i32,
}

impl Client {
    /// Connects to `addr`.
    pub fn connect(addr: SocketAddr) -> Result<Client, String> {
        let stream =
            TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("{addr}: {e}"))?;
        stream.set_nodelay(true).map_err(text)?;
        stream.set_read_timeout(Some(TIMEOUT)).map_err(text)?;
        Ok(Client { stream, next_id: 0 })
    }

    /// Runs `command` on database `db` and returns the reply's bytes; a reply with `ok` other
    /// than 1, or with write errors, is an error with its message.
    pub fn command(&mut self, db: &str, command: Document) -> Result<Vec<u8>, String> {
        let reply = self.send(db, command)?;
        check(&reply)?;
        Ok(reply)
    }

    /// Runs `command` on database `db` and returns the reply's bytes, whatever it says.
    pub fn send(&mut self, db: &str, mut command: Document) -> Result<Vec<u8>, String> {
        command.insert("$db", Value::String(db.to_owned()));
        let body = command.to_bytes().map_err(debug)?;
        self.next_id = self.next_id.wrapping_add(1);
        let msg = OpMsg::encode(self.next_id, 0, 0, &body);
        self.stream.write_all(&msg).map_err(text)?;
        let mut whole = vec![0u8; HEADER_LEN];
        self.stream.read_exact(&mut whole).map_err(text)?;
        let length =
            usize::try_from(Header::parse(&whole).length).map_err(|_| "negative length")?;
        whole.resize(length.max(HEADER_LEN), 0);
        self.stream.read_exact(&mut whole[HEADER_LEN..]).map_err(text)?;
        Ok(OpMsg::parse(&whole).map_err(debug)?.body.ok_or("a reply without a body")?)
    }
}

/// An error for a reply that is not a success.
pub fn check(reply: &[u8]) -> Result<(), String> {
    let doc = RawDoc::from_bytes(reply).map_err(debug)?;
    let ok = match doc.get("ok") {
        Some(RawValue::Double(v)) => v == 1.0,
        Some(RawValue::Int32(v)) => v == 1,
        Some(RawValue::Int64(v)) => v == 1,
        _ => false,
    };
    let message = |d: RawDoc<'_>| match d.get("errmsg") {
        Some(RawValue::String(s)) => s.to_owned(),
        _ => "no message".to_owned(),
    };
    if !ok {
        return Err(message(doc));
    }
    if let Some(RawValue::Array(errors)) = doc.get("writeErrors")
        && let Some(RawValue::Document(first)) = errors.iter().next()
    {
        return Err(message(first));
    }
    Ok(())
}

/// An error as its message.
fn text(e: std::io::Error) -> String {
    e.to_string()
}

/// An error of the BSON or wire crates as its debug form.
fn debug(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}

/// A document from pairs.
pub fn doc(pairs: Vec<(&str, Value)>) -> Document {
    let mut d = Document::new();
    for (k, v) in pairs {
        d.insert(k, v);
    }
    d
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn bytes(d: Document) -> Vec<u8> {
        d.to_bytes().expect("bson")
    }

    #[test]
    fn replies_are_checked_for_ok_and_write_errors() {
        assert_eq!(check(&bytes(doc(vec![("ok", Value::Double(1.0))]))), Ok(()));
        assert_eq!(check(&bytes(doc(vec![("ok", Value::Int32(1))]))), Ok(()));
        assert_eq!(check(&bytes(doc(vec![("ok", Value::Int64(1))]))), Ok(()));
        let failed = doc(vec![("ok", Value::Double(0.0)), ("errmsg", Value::String("bad".into()))]);
        assert_eq!(check(&bytes(failed)), Err("bad".into()));
        assert_eq!(
            check(&bytes(doc(vec![("ok", Value::String("1".into()))]))),
            Err("no message".into())
        );
        let we = doc(vec![("index", Value::Int32(0)), ("errmsg", Value::String("dup".into()))]);
        let written = doc(vec![
            ("ok", Value::Int32(1)),
            ("writeErrors", Value::Array(vec![Value::Document(we)])),
        ]);
        assert_eq!(check(&bytes(written)), Err("dup".into()));
        let empty = doc(vec![("ok", Value::Int32(1)), ("writeErrors", Value::Array(vec![]))]);
        assert_eq!(check(&bytes(empty)), Ok(()));
        assert!(check(&[1, 2, 3]).is_err());
    }

    #[test]
    fn targets_are_refdb_uris() {
        assert!(
            Target::open(Some("http://x")).err().expect("error").contains("refdb://host:port")
        );
        let t = Target::open(Some("refdb://127.0.0.1:27017/db?x=1")).expect("target");
        assert_eq!(t.addr().expect("addr").port(), 27017);
        assert!(Client::connect(SocketAddr::from(([127, 0, 0, 1], 1))).is_err());
        assert_eq!(text(std::io::Error::other("x")), "x");
    }
}
