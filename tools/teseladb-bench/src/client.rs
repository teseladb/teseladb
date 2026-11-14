//! The bench's client of the wire protocol (002 FR-019): `OP_MSG` messages built and read with
//! `teseladb-wire`, so that TeselaDB and the reference server are measured with the same client and
//! not with a different driver on each side. `--target host:port` names the
//! server; without it the scenario starts a development cluster of three nodes in this process
//! (`teseladb dev`).

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
    /// From `--target` (`host:port`), or a local cluster without it.
    pub fn open(uri: Option<&str>) -> Result<Target, String> {
        let Some(uri) = uri else { return Target::local(LOCAL_NODES, 1) };
        if uri.contains(['/', '?', ',']) {
            return Err(format!("--target is host:port, not {uri}"));
        }
        let host = uri;
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
        let what = format!("routing {n} ranges");
        poll_until(&what, TIMEOUT, RANGE_POLL, || self.ranges().is_none_or(|r| r.len() >= n))
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
        let body = OpMsg::parse(&whole).map_err(debug)?.body.ok_or("a reply without a body")?;
        // A reply that read whole keeps its bytes; any other is written back without limits, as
        // the node already kept it within its own.
        let unlimited = teseladb_bson::Limits { max_size: usize::MAX, max_depth: usize::MAX };
        body.to_bytes_with(unlimited).map_err(debug)
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

/// The reference server's `NotWritablePrimary`: the node has no leader to take the command yet.
const NOT_WRITABLE_PRIMARY: i32 = 10107;

/// The code of a failed reply, or of its first write error.
fn error_code(reply: &[u8]) -> Result<Option<i32>, String> {
    let doc = RawDoc::from_bytes(reply).map_err(debug)?;
    if let Some(RawValue::Int32(c)) = doc.get("code") {
        return Ok(Some(c));
    }
    if let Some(RawValue::Array(errors)) = doc.get("writeErrors")
        && let Some(RawValue::Document(first)) = errors.iter().next()
        && let Some(RawValue::Int32(c)) = first.get("code")
    {
        return Ok(Some(c));
    }
    Ok(None)
}

impl Client {
    /// Runs `command` until it succeeds, while it fails with `NotWritablePrimary`, at most
    /// [`TIMEOUT`], as a driver retries: a local cluster that just started may still be electing
    /// its leaders. For the setup of a scenario, outside its timing.
    pub fn until_ready(&mut self, db: &str, command: Document) -> Result<(), String> {
        self.until_ready_within(db, command, TIMEOUT)
    }

    /// [`Client::until_ready`] for at most `timeout`; past it, the last reply's error.
    fn until_ready_within(
        &mut self,
        db: &str,
        command: Document,
        timeout: Duration,
    ) -> Result<(), String> {
        let mut last = Vec::new();
        // Settled or not, the last reply says how the command went.
        retry(timeout, RANGE_POLL, || {
            last = self.send(db, command.clone())?;
            Ok((error_code(&last)? != Some(NOT_WRITABLE_PRIMARY)).then_some(()))
        })?;
        check(&last)
    }
}

/// Calls `done` every `every` until it holds, at most `timeout`: past it, an error saying that
/// `what` did not happen.
pub fn poll_until(
    what: &str,
    timeout: Duration,
    every: Duration,
    mut done: impl FnMut() -> bool,
) -> Result<(), String> {
    let polled = retry(timeout, every, || Ok(done().then_some(())))?;
    polled.map(drop).ok_or(format!("{what} did not happen within {timeout:?}"))
}

/// Calls `step` every `every` until it gives a value, at most `timeout` (it runs at least once):
/// the value and how many calls came before it, or `None` past the timeout. An error of `step`
/// ends the wait.
pub fn retry<T>(
    timeout: Duration,
    every: Duration,
    mut step: impl FnMut() -> Result<Option<T>, String>,
) -> Result<Option<(T, u32)>, String> {
    let until = Instant::now() + timeout;
    let mut before = 0u32;
    loop {
        if let Some(v) = step()? {
            return Ok(Some((v, before)));
        }
        if Instant::now() >= until {
            return Ok(None);
        }
        before += 1;
        std::thread::sleep(every);
    }
}

/// An error as its message.
pub fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// An error as its message after `what`, the step that failed.
pub fn context<E: std::fmt::Display>(what: impl std::fmt::Display) -> impl FnOnce(E) -> String {
    move |e| format!("{what}: {e}")
}

/// The error of a thread of `what` that panicked.
pub fn panicked<P>(what: &str) -> impl FnOnce(P) -> String + '_ {
    move |_| format!("{what} panicked")
}

/// An error of the BSON or wire crates as its debug form.
pub fn debug(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}

/// `command` asking the durability every comparison uses (002 FR-038): written on a majority and
/// flushed to its journal or log, `{w: "majority", j: true}`.
pub fn durable(mut command: Document) -> Document {
    let wc = doc(vec![("w", Value::String("majority".into())), ("j", Value::Boolean(true))]);
    command.insert("writeConcern", Value::Document(wc));
    command
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
    fn retry_waits_for_a_value_until_its_timeout_or_an_error() {
        assert_eq!(retry(TIMEOUT, Duration::ZERO, || Ok(Some(7))), Ok(Some((7, 0))));
        let mut calls = 0;
        let third = retry(TIMEOUT, Duration::ZERO, || {
            calls += 1;
            Ok((calls == 3).then_some("done"))
        });
        assert_eq!(third, Ok(Some(("done", 2))));
        assert_eq!(retry(Duration::ZERO, Duration::ZERO, || Ok(None::<()>)), Ok(None));
        let failed = retry(TIMEOUT, Duration::ZERO, || Err::<Option<()>, _>("broken".into()));
        assert_eq!(failed, Err("broken".to_string()));
    }

    /// A server on the loopback that answers each command with the next of `replies`.
    fn answering(replies: Vec<Document>) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            for reply in replies {
                let mut whole = vec![0u8; HEADER_LEN];
                s.read_exact(&mut whole).unwrap();
                let header = Header::parse(&whole);
                whole.resize(usize::try_from(header.length).unwrap(), 0);
                s.read_exact(&mut whole[HEADER_LEN..]).unwrap();
                let bytes = reply.to_bytes().unwrap();
                s.write_all(&OpMsg::encode(1, header.request_id, 0, &bytes)).unwrap();
            }
        });
        addr
    }

    #[test]
    fn until_ready_repeats_a_command_while_the_primary_is_not_writable() {
        let not_writable = || {
            Document::new()
                .with("ok", 0.0)
                .with("errmsg", "not primary")
                .with("code", NOT_WRITABLE_PRIMARY)
        };
        let ok = Document::new().with("ok", 1.0);
        let mut c = Client::connect(answering(vec![not_writable(), ok])).unwrap();
        assert_eq!(c.until_ready("db", Document::new().with("ping", 1)), Ok(()));
        // Past the timeout the last refusal is the error.
        let mut c = Client::connect(answering(vec![not_writable()])).unwrap();
        let err = c.until_ready_within("db", Document::new().with("ping", 1), Duration::ZERO);
        assert!(err.unwrap_err().contains("not primary"));
    }

    #[test]
    fn errors_read_as_their_message_with_the_step_that_failed() {
        assert_eq!(text("broken"), "broken");
        assert_eq!(debug("broken"), "\"broken\"");
        assert_eq!(context("loading")("broken"), "loading: broken");
        assert_eq!(panicked("the writer")(()), "the writer panicked");
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
    fn the_code_of_a_reply_is_its_own_or_that_of_its_first_write_error() {
        let coded = |c: i32| doc(vec![("code", Value::Int32(c))]);
        assert_eq!(error_code(&bytes(coded(NOT_WRITABLE_PRIMARY))), Ok(Some(NOT_WRITABLE_PRIMARY)));
        let written = |errors: Vec<Value>| bytes(doc(vec![("writeErrors", Value::Array(errors))]));
        assert_eq!(error_code(&written(vec![Value::Document(coded(11000))])), Ok(Some(11000)));
        assert_eq!(error_code(&written(vec![Value::Document(Document::new())])), Ok(None));
        assert_eq!(error_code(&written(vec![Value::Int32(1)])), Ok(None));
        assert_eq!(error_code(&written(vec![])), Ok(None));
        assert_eq!(error_code(&bytes(doc(vec![("ok", Value::Int32(1))]))), Ok(None));
        assert!(error_code(&[1, 2, 3]).is_err());
    }

    #[test]
    fn polling_ends_when_the_condition_holds_or_the_time_is_up() {
        let mut calls = 0;
        let three = poll_until("three calls", TIMEOUT, Duration::ZERO, || {
            calls += 1;
            calls == 3
        });
        assert_eq!((three, calls), (Ok(()), 3));
        let never = poll_until("nothing", Duration::ZERO, Duration::ZERO, || false);
        assert_eq!(never, Err("nothing did not happen within 0ns".into()));
    }

    #[test]
    fn targets_are_host_and_port() {
        assert!(Target::open(Some("http://x")).err().expect("error").contains("is host:port"));
        let t = Target::open(Some("127.0.0.1:27017")).expect("target");
        assert_eq!(t.addr().expect("addr").port(), 27017);
        assert!(Client::connect(SocketAddr::from(([127, 0, 0, 1], 1))).is_err());
        assert_eq!(text(std::io::Error::other("x")), "x");
    }
}
