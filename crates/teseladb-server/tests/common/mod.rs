//! What the client-port tests share: a node's configuration and a client that sends commands
//! as `OP_MSG` and reads the replies.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use teseladb_server::client_listener::ClientConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_raftlog::Disk;

pub const READY: Duration = Duration::from_secs(30);

pub fn config(join: Vec<String>) -> NodeConfig {
    NodeConfig {
        cores: Some(2),
        join,
        client: Some(ClientConfig {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            ..ClientConfig::new(Vec::new())
        }),
        ..NodeConfig::default()
    }
}

pub struct Client {
    pub s: TcpStream,
    next: i32,
}

impl Client {
    pub fn new<D: Disk + Send + 'static>(node: &Node<D>) -> Client {
        let s = TcpStream::connect(node.client_addrs()[0]).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
        Client { s, next: 1 }
    }

    pub fn run(&mut self, db: &str, cmd: Document) -> Document {
        self.send(db, cmd);
        self.receive()
    }

    /// Sends `cmd` without reading its reply.
    pub fn send(&mut self, db: &str, cmd: Document) {
        let mut cmd = cmd;
        cmd.push("$db", db);
        self.next += 1;
        let msg = OpMsg::encode(self.next, 0, 0, &cmd.to_bytes().unwrap());
        self.s.write_all(&msg).unwrap();
    }

    /// Reads the next reply.
    pub fn receive(&mut self) -> Document {
        let mut h = [0u8; HEADER_LEN];
        self.s.read_exact(&mut h).unwrap();
        let header = Header::parse(&h);
        let mut rest = vec![0u8; header.length as usize - HEADER_LEN];
        self.s.read_exact(&mut rest).unwrap();
        Document::from(RawDoc::from_bytes(&rest[5..]).unwrap())
    }
}

pub fn ok(d: &Document) -> bool {
    d.get("ok") == Some(&Value::Double(1.0))
}

pub fn batch(d: &Document, field: &str) -> Vec<Value> {
    let Some(Value::Document(c)) = d.get("cursor") else { panic!("no cursor in {d:?}") };
    match c.get(field) {
        Some(Value::Array(a)) => a.clone(),
        other => panic!("{other:?}"),
    }
}

pub fn docs(range: std::ops::Range<i32>) -> Value {
    Value::Array(
        range.map(|i| Value::Document(Document::new().with("_id", i).with("v", i * 10))).collect(),
    )
}

pub fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + READY;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
