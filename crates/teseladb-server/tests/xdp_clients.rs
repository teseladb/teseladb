//! The clients' port on AF_XDP (012 FR-033, `--net-xdp`) on a real Linux kernel: a `veth` pair
//! with the port's end here and the clients' end in a namespace of its own, so that every
//! segment crosses the pair and the XDP program. Plain and TLS connections, a node's commands,
//! and the kernel's TCP when the interface cannot take AF_XDP. `scripts/test-linux.sh --test
//! xdp_clients teseladb-server` runs it with `CAP_NET_ADMIN` and `CAP_BPF`.

#[cfg(target_os = "linux")]
mod common;
#[cfg(target_os = "linux")]
#[path = "common/port_engine.rs"]
mod port_engine;

#[cfg(target_os = "linux")]
mod linux {
    use super::{common, port_engine};

    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::time::Duration;

    use teseladb_bson::{Document, RawDoc, Value};
    use teseladb_crypto::ec::NamedCurve;
    use teseladb_io::engine::disk::DiskPlace;
    use teseladb_server::client_listener::{ClientConfig, NodeState};
    use teseladb_server::node::{Node, NodeConfig};
    use teseladb_server::tls_port::{TlsMode, TlsPort};
    use teseladb_sys::netns::{self, NetNs};
    use teseladb_tls::{CertifiedKey, Connection, ENTROPY_LEN, ServerConfig, Trust};
    use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
    use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};
    use teseladb_x509::ext::GeneralName;
    use teseladb_x509::generate::{self, CertificateParams};
    use teseladb_x509::{Certificate, Name, PrivateKey, oid};

    use common::{READY, batch, docs, ok};

    struct Fixed;

    impl NodeState for Fixed {
        fn info(&self) -> NodeInfo {
            NodeInfo {
                topology: Topology { process_id: [3; 12], counter: 0 },
                cluster_time: ClusterTime { time: 5, increment: 1 },
                now_ms: 1,
                keys: None,
                replica_set: None,
            }
        }
    }

    /// A `veth` pair `<prefix>0` here with `10.<net>.0.1` and `<prefix>1` in `ns` with
    /// `10.<net>.0.2`; the address of the port's end.
    fn pair(prefix: &str, net: u8, ns: &NetNs) -> (String, Ipv4Addr) {
        let (here, there) = (format!("{prefix}0"), format!("{prefix}1"));
        let (server, client) = (Ipv4Addr::new(10, net, 0, 1), Ipv4Addr::new(10, net, 0, 2));
        netns::create_veth(&here, &there, 1).unwrap();
        netns::move_to(&there, ns).unwrap();
        netns::add_address(&here, server, 24).unwrap();
        netns::set_up(&here).unwrap();
        ns.run(move || {
            netns::add_address(&there, client, 24).unwrap();
            netns::set_up(&there).unwrap();
            // The frames of the pair carry their own checksums, as a NIC's do.
            netns::disable_tx_checksum(&there).unwrap();
        })
        .unwrap();
        (here, server)
    }

    fn message(id: i32, db: &str, cmd: Document) -> Vec<u8> {
        let cmd = cmd.with("$db", db);
        OpMsg::encode(id, 0, 0, &cmd.to_bytes().unwrap())
    }

    fn read_reply(s: &mut TcpStream) -> Document {
        let mut h = [0u8; HEADER_LEN];
        s.read_exact(&mut h).unwrap();
        let header = Header::parse(&h);
        let mut rest = vec![0u8; header.length as usize - HEADER_LEN];
        s.read_exact(&mut rest).unwrap();
        Document::from(RawDoc::from_bytes(&rest[5..]).unwrap())
    }

    /// Runs `cmds` on `db` through a connection to `to` from the namespace: their replies.
    fn over(ns: &NetNs, to: SocketAddr, db: &'static str, cmds: Vec<Document>) -> Vec<Document> {
        ns.run(move || {
            let mut s = TcpStream::connect_timeout(&to, Duration::from_secs(5)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            cmds.into_iter()
                .enumerate()
                .map(|(i, c)| {
                    s.write_all(&message(i as i32 + 1, db, c)).unwrap();
                    read_reply(&mut s)
                })
                .collect()
        })
        .unwrap()
    }

    fn ping() -> Document {
        Document::new().with("ping", 1)
    }

    fn a_plain_connection_crosses_the_pair_through_the_port_on_af_xdp() {
        let ns = NetNs::new().unwrap();
        let (iface, ip) = pair("xa", 81, &ns);
        let cfg = ClientConfig {
            addrs: vec![SocketAddr::from((ip, 0))],
            xdp: Some(iface.clone()),
            ..ClientConfig::new(Vec::new())
        };
        let l = port_engine::spawn_port(&cfg, Arc::new(Fixed)).unwrap();
        let status = l.xdp().unwrap().unwrap();
        assert!(status.starts_with(&format!("{iface}: 1 queues, XDP in ")), "{status}");
        let replies =
            over(&ns, l.addrs()[0], "admin", vec![ping(), Document::new().with("hello", 1)]);
        assert!(replies.iter().all(ok), "{replies:?}");
        common::until("the port's end of the connection", || {
            (l.open_connections() == 0).then_some(())
        });
        l.stop();
    }

    fn now() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn pki() -> (Certificate, CertifiedKey) {
        let t = now();
        let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[1; 40]).unwrap();
        let mut p = CertificateParams::node(
            [1; 16],
            Name::build(&[(oid::COMMON_NAME, "CA")]),
            t - 60,
            t + 86_400,
        );
        p.ca = Some(None);
        let ca = generate::self_signed(&p, &ca_key).unwrap();
        let key = PrivateKey::ed25519(&[2; 32]);
        let mut lp = CertificateParams::node(
            [2; 16],
            Name::build(&[(oid::COMMON_NAME, "server")]),
            t - 60,
            t + 86_400,
        );
        lp.alt_names = vec![GeneralName::Dns("localhost".into())];
        let chain = vec![generate::issue(&lp, &key.public_key(), &ca, &ca_key).unwrap()];
        (ca, CertifiedKey { chain, key })
    }

    fn a_tls_connection_goes_through_the_port_on_af_xdp_too() {
        let ns = NetNs::new().unwrap();
        let (iface, ip) = pair("xt", 82, &ns);
        let (ca, server) = pki();
        let cfg = ClientConfig {
            addrs: vec![SocketAddr::from((ip, 0))],
            xdp: Some(iface),
            tls: TlsPort::new(TlsMode::RequireTls, Some(ServerConfig::new(server))),
            ..ClientConfig::new(Vec::new())
        };
        let l = port_engine::spawn_port(&cfg, Arc::new(Fixed)).unwrap();
        assert!(l.xdp().unwrap().is_ok());
        let to = l.addrs()[0];
        let reply = ns
            .run(move || {
                let cc = teseladb_tls::ClientConfig::new(Trust::anchors(vec![ca]));
                let mut c = Connection::client(
                    Arc::new(cc),
                    Some("localhost"),
                    now() * 1000,
                    &[7; ENTROPY_LEN],
                    None,
                )
                .unwrap();
                let mut s = TcpStream::connect_timeout(&to, Duration::from_secs(5)).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                let (mut buf, mut sent, mut reply) = (vec![0u8; 65536], false, Vec::new());
                loop {
                    let out = c.take_output();
                    if !out.is_empty() {
                        s.write_all(&out).unwrap();
                    }
                    if !c.is_handshaking() && !sent {
                        c.write(&message(1, "admin", ping())).unwrap();
                        sent = true;
                        continue;
                    }
                    reply.extend_from_slice(&c.read());
                    if reply.len() > HEADER_LEN
                        && i32::from_le_bytes(reply[..4].try_into().unwrap()) as usize
                            == reply.len()
                    {
                        return Document::from(
                            RawDoc::from_bytes(&reply[HEADER_LEN + 5..]).unwrap(),
                        );
                    }
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0, "the port closed the connection");
                    c.feed(&buf[..n]).unwrap();
                }
            })
            .unwrap();
        assert!(ok(&reply), "{reply:?}");
        l.stop();
    }

    fn a_node_serves_its_commands_on_af_xdp() {
        let ns = NetNs::new().unwrap();
        let (iface, ip) = pair("xn", 83, &ns);
        let base = common::config(Vec::new());
        let client = ClientConfig {
            addrs: vec![SocketAddr::from((ip, 0))],
            xdp: Some(iface),
            ..base.client.clone().unwrap()
        };
        let node =
            Node::start(NodeConfig { client: Some(client), ..base }, DiskPlace::Memory, None)
                .unwrap();
        assert!(node.wait_ready(READY));
        let to = node.client_addrs()[0];
        let cmds = vec![
            Document::new().with("insert", "x").with("documents", docs(0..300)),
            Document::new().with("find", "x").with("batchSize", 300),
        ];
        let r = over(&ns, to, "xdp", cmds);
        assert!(ok(&r[0]), "{:?}", r[0]);
        assert_eq!(batch(&r[1], "firstBatch").len(), 300);
        // From this namespace the address is local: the kernel's listener answers, with the same data.
        let mut local = common::Client::new(&node);
        let count = local.run("xdp", Document::new().with("count", "x"));
        assert_eq!(count.get("n"), Some(&Value::Int32(300)), "{count:?}");
        assert_eq!(
            batch(&r[1], "firstBatch")[299],
            Value::Document(Document::new().with("_id", 299).with("v", 2990))
        );
        node.stop();
    }

    fn an_interface_that_cannot_take_af_xdp_leaves_the_clients_to_the_kernel() {
        let cfg = ClientConfig {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            xdp: Some("no-such-if".into()),
            ..ClientConfig::new(Vec::new())
        };
        let l = port_engine::spawn_port(&cfg, Arc::new(Fixed)).unwrap();
        let why = l.xdp().unwrap().unwrap_err();
        assert!(why.starts_with("no-such-if: the interface"), "{why}");
        let mut s = TcpStream::connect(l.addrs()[0]).unwrap();
        s.write_all(&message(1, "admin", ping())).unwrap();
        assert!(ok(&read_reply(&mut s)));
        l.stop();
        // Without `--net-xdp` there is nothing to say.
        let plain = port_engine::spawn_port(
            &ClientConfig {
                addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
                ..ClientConfig::new(Vec::new())
            },
            Arc::new(Fixed),
        )
        .unwrap();
        assert!(plain.xdp().is_none());
        plain.stop();
    }

    /// The tests in order: the node first, with its own engine, stopped when the test ends
    /// (D4: one engine per process), then the ports on the engine the process starts for them.
    pub(super) fn run() {
        a_node_serves_its_commands_on_af_xdp();
        a_plain_connection_crosses_the_pair_through_the_port_on_af_xdp();
        a_tls_connection_goes_through_the_port_on_af_xdp_too();
        an_interface_that_cannot_take_af_xdp_leaves_the_clients_to_the_kernel();
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
}
