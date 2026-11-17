// Each test binary that includes this harness uses only part of it.
#![allow(dead_code)]
//! A native TCP connection between the stack (over the fake device of `cov_net_wire.rs`) and
//! a peer simulated by the test, which injects segments and reads the stack's. The including
//! file declares the wire harness as `mod wire` before this module.

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::net::ip::Ipv4Traits;
use teseladb::engine::net::net::HwFeatures;
use teseladb::engine::net::tcp::{Connection, Listener, Tcp};

use crate::wire::*;

pub type Conn = Connection<Ipv4Traits>;

/// The port the stack listens on.
pub const LPORT: u16 = 80;
/// The peer's port.
pub const PPORT: u16 = 40000;
/// The peer's initial sequence number.
pub const PEER_ISS: u32 = 1000;

/// The errno an I/O error carried by `e` holds.
pub fn errno_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error())
}

/// A connection between the stack and the simulated peer.
pub struct Session {
    pub st: IpStack,
    pub conn: Conn,
    pub _listener: Option<Listener<Ipv4Traits>>,
    /// The stack's port.
    pub lport: u16,
    /// The peer's port.
    pub pport: u16,
    /// The window the peer advertises.
    pub window: u16,
    /// The peer's next sequence number.
    pub pseq: u32,
    /// What the peer acknowledges of the stack's sequence space.
    pub sseq: u32,
    /// The stack's SYN (active open) or SYN-ACK (passive open).
    pub syn: TcpView,
}

impl Session {
    /// A passive open on a default stack, the peer's SYN carrying `opts`.
    pub fn passive(opts: &[u8]) -> Session {
        Self::passive_on(IpStack::new(HwFeatures::default()), opts, 60000)
    }

    /// A passive open on `st`: listen, receive the peer's SYN, answer, accept.
    pub fn passive_on(st: IpStack, opts: &[u8], window: u16) -> Session {
        let tcp = st.tcp();
        // SAFETY: the layer is live and no reference into it is live.
        let mut listener = unsafe { (*tcp).listen(LPORT, 8) };
        assert_eq!(listener.port(), LPORT);
        let accepted = listener.accept();
        st.wire.inject(&tcp_frame(
            &TcpSpec::new(PPORT, LPORT, PEER_ISS, 0, SYN).window(window).options(opts),
            b"",
        ));
        let synack = st.wire.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_TCP);
        assert_eq!(synack.dst_mac(), PEER_MAC);
        // With transmit checksum offload the device completes the checksum.
        assert!(st.ipv4().hw_features().tx_csum_l4_offload || synack.l4_csum_ok());
        let synack = synack.tcp();
        assert!(synack.has(SYN | ACK), "{synack:?}");
        assert_eq!(synack.ack, PEER_ISS + 1);
        assert_eq!((synack.sport, synack.dport), (LPORT, PPORT));
        let iss = synack.seq;
        st.wire.inject(&tcp_frame(
            &TcpSpec::new(PPORT, LPORT, PEER_ISS + 1, iss.wrapping_add(1), ACK).window(window),
            b"",
        ));
        let conn = wait(accepted).expect("the connection is accepted");
        Session {
            st,
            conn,
            _listener: Some(listener),
            lport: LPORT,
            pport: PPORT,
            window,
            pseq: PEER_ISS + 1,
            sseq: iss.wrapping_add(1),
            syn: synack,
        }
    }

    /// An active open from `st` (which knows the peer's MAC) to the peer's `pport`; the
    /// peer's SYN-ACK carries `opts`.
    pub fn active_on(st: IpStack, pport: u16, opts: &[u8], window: u16) -> Session {
        // SAFETY: the layer is live and no reference into it is live.
        let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(PEER_IP, pport)) };
        let connected = conn.connected();
        let syn = st.wire.next_tcp();
        assert!(syn.has(SYN) && !syn.has(ACK), "{syn:?}");
        st.wire.inject(&tcp_frame(
            &TcpSpec::new(pport, syn.sport, PEER_ISS, syn.seq.wrapping_add(1), SYN | ACK)
                .window(window)
                .options(opts),
            b"",
        ));
        wait(connected).expect("the connection is established");
        let ack = st.wire.next_tcp_where(|t| t.has(ACK));
        assert_eq!(ack.ack, PEER_ISS + 1);
        assert_eq!(ack.seq, syn.seq.wrapping_add(1));
        Session {
            st,
            conn,
            _listener: None,
            lport: syn.sport,
            pport,
            window,
            pseq: PEER_ISS + 1,
            sseq: syn.seq.wrapping_add(1),
            syn,
        }
    }

    /// A segment from the peer with the current numbers.
    pub fn spec(&self, flags: u8) -> TcpSpec {
        TcpSpec::new(self.pport, self.lport, self.pseq, self.sseq, flags).window(self.window)
    }

    pub fn inject(&self, spec: &TcpSpec, payload: &[u8]) {
        self.st.wire.inject(&tcp_frame(spec, payload));
    }

    /// Sends `payload` with `flags` from the peer and advances the peer's sequence number.
    pub fn peer_send(&mut self, flags: u8, payload: &[u8]) {
        let spec = self.spec(flags);
        self.inject(&spec, payload);
        let ctl = u32::from(flags & (SYN | FIN) != 0);
        self.pseq = self.pseq.wrapping_add(payload.len() as u32 + ctl);
    }

    /// The peer acknowledges the stack's sequence space up to `ack`.
    pub fn peer_ack(&mut self, ack: u32) {
        self.sseq = ack;
        let spec = self.spec(ACK);
        self.inject(&spec, b"");
    }

    /// The next segment from the stack acknowledging `ack`.
    pub fn expect_ack(&self, ack: u32) -> TcpView {
        self.st.wire.next_tcp_where(|t| t.has(ACK) && t.ack == ack)
    }

    /// The next segment from the stack that carries data.
    pub fn expect_data(&self) -> TcpView {
        self.st.wire.next_tcp_where(|t| !t.payload.is_empty())
    }

    /// Reads what the connection received.
    pub fn read(&mut self) -> Vec<u8> {
        packet_bytes(&self.conn.read())
    }
}

/// `n` bytes of a repeating pattern.
pub fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}
