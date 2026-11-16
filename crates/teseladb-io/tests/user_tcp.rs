//! The TCP stack in user space of the clients' port on AF_XDP (012 FR-033): a client written
//! here, with its own encoder of frames, talks to the stack through the frames it sends and the
//! ones the stack gives back.
#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, SocketAddr};

use teseladb_io::user_tcp::{Arrival, Event, RECEIVE_BUFFER, State, TcpStack, flow_of};

const PORT: u16 = 27017;
const CLIENT_PORT: u16 = 40000;
const SERVER_IP: [u8; 4] = [10, 7, 0, 1];
const CLIENT_IP: [u8; 4] = [10, 7, 0, 2];
const SERVER_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
const CLIENT_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
const MTU: usize = 1500;
const MSS: usize = MTU - 40;
const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;
const SECOND: u64 = 1_000_000;

fn sum16(mut sum: u32, bytes: &[u8]) -> u32 {
    for c in bytes.chunks(2) {
        let hi = u32::from(c[0]) << 8;
        sum += hi | c.get(1).map_or(0, |&b| u32::from(b));
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// What the client sends.
#[derive(Clone, Default)]
struct Seg {
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    options: Vec<u8>,
    payload: Vec<u8>,
    dst_port: Option<u16>,
    dst_ip: Option<[u8; 4]>,
}

fn frame(s: &Seg) -> Vec<u8> {
    let dst_ip = s.dst_ip.unwrap_or(SERVER_IP);
    let mut tcp = Vec::new();
    tcp.extend_from_slice(&CLIENT_PORT.to_be_bytes());
    tcp.extend_from_slice(&s.dst_port.unwrap_or(PORT).to_be_bytes());
    tcp.extend_from_slice(&s.seq.to_be_bytes());
    tcp.extend_from_slice(&s.ack.to_be_bytes());
    tcp.push((((20 + s.options.len()) / 4) as u8) << 4);
    tcp.push(s.flags);
    tcp.extend_from_slice(&s.window.to_be_bytes());
    tcp.extend_from_slice(&[0, 0, 0, 0]);
    tcp.extend_from_slice(&s.options);
    tcp.extend_from_slice(&s.payload);
    let mut pseudo = sum16(0, &CLIENT_IP);
    pseudo = sum16(pseudo, &dst_ip);
    pseudo += 6 + tcp.len() as u32;
    let sum = fold(sum16(pseudo, &tcp));
    tcp[16..18].copy_from_slice(&sum.to_be_bytes());
    let mut ip = vec![0x45, 0];
    ip.extend_from_slice(&((20 + tcp.len()) as u16).to_be_bytes());
    ip.extend_from_slice(&[0, 1, 0x40, 0, 64, 6, 0, 0]);
    ip.extend_from_slice(&CLIENT_IP);
    ip.extend_from_slice(&dst_ip);
    let sum = fold(sum16(0, &ip));
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    let mut f = Vec::new();
    f.extend_from_slice(&SERVER_MAC);
    f.extend_from_slice(&CLIENT_MAC);
    f.extend_from_slice(&0x0800u16.to_be_bytes());
    f.extend_from_slice(&ip);
    f.extend_from_slice(&tcp);
    f
}

/// What the stack sent, read back.
#[derive(Debug, Clone)]
struct Got {
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    mss: Option<u16>,
    wscale: Option<u8>,
    payload: Vec<u8>,
}

fn read_back(f: &[u8]) -> Got {
    assert_eq!(&f[0..6], &CLIENT_MAC, "to the client's MAC");
    assert_eq!(&f[6..12], &SERVER_MAC);
    let ip = &f[14..];
    assert_eq!(fold(sum16(0, &ip[..20])), 0, "IPv4 checksum");
    assert_eq!((&ip[12..16], &ip[16..20]), (&SERVER_IP[..], &CLIENT_IP[..]));
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    let tcp = &ip[20..total];
    let mut pseudo = sum16(0, &SERVER_IP);
    pseudo = sum16(pseudo, &CLIENT_IP);
    pseudo += 6 + tcp.len() as u32;
    assert_eq!(fold(sum16(pseudo, tcp)), 0, "TCP checksum");
    assert_eq!(u16::from_be_bytes([tcp[0], tcp[1]]), PORT);
    assert_eq!(u16::from_be_bytes([tcp[2], tcp[3]]), CLIENT_PORT);
    let off = usize::from(tcp[12] >> 4) * 4;
    let (mut mss, mut wscale) = (None, None);
    let mut o = &tcp[20..off];
    while let Some(&k) = o.first() {
        match k {
            0 => break,
            1 => o = &o[1..],
            _ => {
                let l = usize::from(o[1]);
                if k == 2 {
                    mss = Some(u16::from_be_bytes([o[2], o[3]]));
                }
                if k == 3 {
                    wscale = Some(o[2]);
                }
                o = &o[l..];
            }
        }
    }
    let be = |at: usize| u32::from_be_bytes([tcp[at], tcp[at + 1], tcp[at + 2], tcp[at + 3]]);
    Got {
        seq: be(4),
        ack: be(8),
        flags: tcp[13],
        window: u16::from_be_bytes([tcp[14], tcp[15]]),
        mss,
        wscale,
        payload: tcp[off..].to_vec(),
    }
}

/// Gives `frame` to `stack`, which takes a frame of no connection as its own: the only stack of
/// the port.
fn feed(stack: &mut TcpStack, frame: &[u8], now: u64) {
    if stack.receive(frame, now) == Arrival::Stranger {
        stack.adopt(frame, now);
    }
}

fn out(stack: &mut TcpStack, now: u64) -> Vec<Got> {
    let mut frames = Vec::new();
    stack.transmit(now, &mut |f| {
        frames.push(read_back(f));
        true
    });
    frames
}

fn events(stack: &mut TcpStack) -> Vec<Event> {
    std::iter::from_fn(|| stack.event()).collect()
}

fn stack() -> TcpStack {
    TcpStack::new(PORT, None, MTU, 16, 42)
}

const CLIENT_ISN: u32 = 0xffff_fff0;

/// An open connection: its token and the server's next sequence number.
fn open(stack: &mut TcpStack, window: u16) -> (u64, u32) {
    let syn = Seg {
        seq: CLIENT_ISN,
        flags: SYN,
        window,
        options: vec![2, 4, 5, 0xb4, 1, 3, 3, 3],
        ..Seg::default()
    };
    feed(stack, &frame(&syn), 0);
    let synack = out(stack, 0);
    assert_eq!(synack.len(), 1);
    let sa = &synack[0];
    assert_eq!(sa.flags, SYN | ACK);
    assert_eq!(sa.ack, CLIENT_ISN.wrapping_add(1));
    assert_eq!(sa.mss, Some(MSS as u16));
    assert_eq!(sa.wscale, Some(6));
    let ack = Seg {
        seq: CLIENT_ISN.wrapping_add(1),
        ack: sa.seq.wrapping_add(1),
        flags: ACK,
        window,
        ..Seg::default()
    };
    feed(stack, &frame(&ack), 1_000);
    let ev = events(stack);
    let Some(Event::Accepted { token, peer, local }) = ev.first().copied() else {
        panic!("{ev:?}")
    };
    assert_eq!(peer, SocketAddr::from((Ipv4Addr::from(CLIENT_IP), CLIENT_PORT)));
    assert_eq!(local, SocketAddr::from((Ipv4Addr::from(SERVER_IP), PORT)));
    assert_eq!(stack.state(token), Some(State::Established));
    (token, sa.seq.wrapping_add(1))
}

fn data(seq: u32, ack: u32, bytes: &[u8]) -> Seg {
    Seg { seq, ack, flags: ACK, window: 1000, payload: bytes.to_vec(), ..Seg::default() }
}

const C1: u32 = CLIENT_ISN.wrapping_add(1);

#[test]
fn a_handshake_opens_a_connection_and_data_flows_both_ways() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    feed(&mut s, &frame(&data(C1, sseq, b"ping")), 2_000);
    assert_eq!(events(&mut s), vec![Event::Readable(token)]);
    assert_eq!(s.read(token), b"ping");
    let acks = out(&mut s, 2_000);
    assert_eq!(acks.len(), 1);
    assert_eq!((acks[0].ack, acks[0].flags), (C1.wrapping_add(4), ACK));
    // Our window is scaled by 6.
    assert_eq!(usize::from(acks[0].window) << 6, RECEIVE_BUFFER);
    // The reply: the client's window of 1000 << 3 bytes takes all of it, in segments of the MSS.
    let reply = vec![7u8; 5000];
    assert!(s.send(token, &reply));
    let segs = out(&mut s, 3_000);
    assert_eq!(segs.iter().map(|g| g.payload.len()).collect::<Vec<_>>(), [1460, 1460, 1460, 620]);
    assert_eq!(segs[1].seq, sseq.wrapping_add(1460));
    assert_eq!(s.queued(token), 5000);
    let ack = Seg {
        seq: C1.wrapping_add(4),
        ack: sseq.wrapping_add(5000),
        flags: ACK,
        window: 1000,
        ..Seg::default()
    };
    feed(&mut s, &frame(&ack), 4_000);
    assert_eq!(events(&mut s), vec![Event::Sent(token)]);
    assert_eq!(s.queued(token), 0);
    assert!(s.next_deadline().is_none(), "nothing to retransmit");
}

#[test]
fn a_lost_segment_goes_again_on_the_timer_with_a_doubled_timeout() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    s.send(token, b"reply");
    let first = out(&mut s, 10_000);
    assert_eq!(first[0].payload, b"reply");
    let due = s.next_deadline().unwrap();
    s.tick(due - 1);
    assert!(out(&mut s, due - 1).is_empty());
    s.tick(due);
    let again = out(&mut s, due);
    assert_eq!((again[0].seq, &again[0].payload[..]), (sseq, &b"reply"[..]));
    let next = s.next_deadline().unwrap();
    assert!(next - due >= 2 * (due - 10_000) - 1, "{} after {}", next - due, due - 10_000);
}

#[test]
fn three_duplicate_acknowledgements_send_the_first_segment_again() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    s.send(token, &vec![1u8; 4 * 1460]);
    assert_eq!(out(&mut s, 10_000).len(), 4);
    let dup = Seg { seq: C1, ack: sseq, flags: ACK, window: 1000, ..Seg::default() };
    for _ in 0..2 {
        feed(&mut s, &frame(&dup), 11_000);
        assert!(out(&mut s, 11_000).is_empty());
    }
    feed(&mut s, &frame(&dup), 11_000);
    let again = out(&mut s, 11_000);
    assert_eq!((again[0].seq, again[0].payload.len()), (sseq, 1460));
}

#[test]
fn segments_that_arrive_early_wait_for_the_gap() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    feed(&mut s, &frame(&data(C1.wrapping_add(3), sseq, b"def")), 2_000);
    assert!(events(&mut s).is_empty());
    // A duplicate acknowledgement asks for the gap.
    assert_eq!(out(&mut s, 2_000)[0].ack, C1);
    feed(&mut s, &frame(&data(C1, sseq, b"abc")), 3_000);
    assert_eq!(events(&mut s), vec![Event::Readable(token)]);
    assert_eq!(s.read(token), b"abcdef");
    // Again the first bytes and some new: only the new are taken.
    feed(&mut s, &frame(&data(C1.wrapping_add(4), sseq, b"efgh")), 4_000);
    assert_eq!(s.read(token), b"gh");
    // All old: an acknowledgement.
    feed(&mut s, &frame(&data(C1, sseq, b"ab")), 5_000);
    assert!(s.read(token).is_empty());
    assert_eq!(out(&mut s, 5_000)[0].ack, C1.wrapping_add(8));
}

#[test]
fn the_peer_closes_and_the_connection_ends_after_its_fin_is_acknowledged() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    let fin = Seg { seq: C1, ack: sseq, flags: FIN | ACK, window: 1000, ..Seg::default() };
    feed(&mut s, &frame(&fin), 2_000);
    assert_eq!(events(&mut s), vec![Event::Closed(token)]);
    assert!(!s.send(token, b"late"), "the application's connection ended");
    let ours = out(&mut s, 2_000);
    assert_eq!(ours.len(), 1);
    assert_eq!((ours[0].flags, ours[0].ack), (FIN | ACK, C1.wrapping_add(1)));
    assert_eq!(s.state(token), Some(State::LastAck));
    let last = Seg {
        seq: C1.wrapping_add(1),
        ack: sseq.wrapping_add(1),
        flags: ACK,
        window: 1000,
        ..Seg::default()
    };
    feed(&mut s, &frame(&last), 3_000);
    assert_eq!(s.connections(), 0);
    assert!(events(&mut s).is_empty(), "one Closed only");
}

#[test]
fn the_application_closes_after_its_data_and_time_wait_ends_the_connection() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    s.send(token, b"bye");
    s.close(token);
    assert_eq!(events(&mut s), vec![Event::Closed(token)]);
    let segs = out(&mut s, 2_000);
    assert_eq!(segs.len(), 1, "data and FIN together");
    assert_eq!((segs[0].flags & FIN, &segs[0].payload[..]), (FIN, &b"bye"[..]));
    let ack =
        Seg { seq: C1, ack: sseq.wrapping_add(4), flags: ACK, window: 1000, ..Seg::default() };
    feed(&mut s, &frame(&ack), 3_000);
    assert_eq!(s.state(token), Some(State::FinWait2));
    let fin = Seg {
        seq: C1,
        ack: sseq.wrapping_add(4),
        flags: FIN | ACK,
        window: 1000,
        ..Seg::default()
    };
    feed(&mut s, &frame(&fin), 4_000);
    assert_eq!(s.state(token), Some(State::TimeWait));
    assert_eq!(out(&mut s, 4_000)[0].ack, C1.wrapping_add(1));
    s.tick(4_000 + 60 * SECOND);
    assert_eq!(s.connections(), 0);
    assert!(events(&mut s).is_empty());
}

#[test]
fn simultaneous_close_goes_through_closing_to_time_wait() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    s.close(token);
    let fin = out(&mut s, 2_000);
    assert_eq!(fin[0].flags, FIN | ACK);
    let theirs = Seg { seq: C1, ack: sseq, flags: FIN | ACK, window: 1000, ..Seg::default() };
    feed(&mut s, &frame(&theirs), 3_000);
    assert_eq!(s.state(token), Some(State::Closing));
    let ack = Seg {
        seq: C1.wrapping_add(1),
        ack: sseq.wrapping_add(1),
        flags: ACK,
        window: 1000,
        ..Seg::default()
    };
    feed(&mut s, &frame(&ack), 4_000);
    assert_eq!(s.state(token), Some(State::TimeWait));
}

#[test]
fn only_a_reset_at_the_exact_number_resets_and_others_get_an_ack() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    let near = Seg { seq: C1.wrapping_add(10), flags: RST, ..Seg::default() };
    feed(&mut s, &frame(&near), 2_000);
    assert_eq!(s.state(token), Some(State::Established));
    assert_eq!(out(&mut s, 2_000)[0].flags, ACK, "challenge ACK");
    // A SYN inside the window gets one too.
    let syn = Seg { seq: C1.wrapping_add(5), flags: SYN, ..Seg::default() };
    feed(&mut s, &frame(&syn), 2_500);
    assert_eq!(out(&mut s, 2_500)[0].flags, ACK);
    let exact = Seg { seq: C1, ack: sseq, flags: RST, ..Seg::default() };
    feed(&mut s, &frame(&exact), 3_000);
    assert_eq!(events(&mut s), vec![Event::Closed(token)]);
    assert_eq!(s.connections(), 0);
}

#[test]
fn a_segment_of_no_connection_is_reset_and_other_ports_are_left_alone() {
    let mut s = stack();
    let stray = Seg { seq: 5, ack: 99, flags: ACK, payload: b"x".to_vec(), ..Seg::default() };
    feed(&mut s, &frame(&stray), 0);
    let r = out(&mut s, 0);
    assert_eq!((r[0].flags, r[0].seq), (RST, 99));
    let bare = Seg { seq: 5, flags: 0, payload: b"xy".to_vec(), ..Seg::default() };
    feed(&mut s, &frame(&bare), 0);
    let r = out(&mut s, 0);
    assert_eq!((r[0].flags, r[0].ack), (RST | ACK, 7));
    // A reset gets nothing back; another port or address is the kernel's.
    feed(&mut s, &frame(&Seg { flags: RST, ..Seg::default() }), 0);
    feed(&mut s, &frame(&Seg { flags: SYN, dst_port: Some(80), ..Seg::default() }), 0);
    let mut only = TcpStack::new(PORT, Some(Ipv4Addr::from(SERVER_IP)), MTU, 4, 1);
    feed(&mut only, &frame(&Seg { flags: SYN, dst_ip: Some([10, 7, 0, 9]), ..Seg::default() }), 0);
    assert!(out(&mut s, 0).is_empty());
    assert!(out(&mut only, 0).is_empty());
    assert_eq!((s.connections(), only.connections()), (0, 0));
    // A frame with a broken checksum is dropped.
    let mut bad = frame(&Seg { flags: SYN, ..Seg::default() });
    let last = bad.len() - 1;
    bad[last - 20] ^= 0xff;
    feed(&mut s, &bad, 0);
    assert_eq!(s.connections(), 0);
}

#[test]
fn a_closed_window_holds_the_data_and_a_probe_opens_it() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 0);
    s.send(token, b"waiting");
    assert!(out(&mut s, 2_000).is_empty());
    let due = s.next_deadline().unwrap();
    s.tick(due);
    let probe = out(&mut s, due);
    assert_eq!((probe[0].seq, probe[0].payload.len()), (sseq, 1));
    let open_again =
        Seg { seq: C1, ack: sseq.wrapping_add(1), flags: ACK, window: 1000, ..Seg::default() };
    feed(&mut s, &frame(&open_again), due + 1);
    let rest = out(&mut s, due + 1);
    assert_eq!(rest[0].payload, b"aiting");
}

#[test]
fn a_full_receive_buffer_closes_the_window_until_the_application_reads() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    let chunk = vec![3u8; MSS];
    let mut seq = C1;
    let mut last = None;
    for i in 0..(RECEIVE_BUFFER / MSS + 2) {
        feed(&mut s, &frame(&data(seq, sseq, &chunk)), 2_000 + i as u64);
        last = out(&mut s, 2_000 + i as u64).pop().or(last);
        seq = seq.wrapping_add(MSS as u32);
    }
    let last = last.unwrap();
    assert_eq!(last.window, 0, "the buffer is full");
    let got = s.read(token);
    assert_eq!(got.len(), RECEIVE_BUFFER);
    let update = out(&mut s, 9_000);
    assert_eq!(usize::from(update[0].window) << 6, RECEIVE_BUFFER, "the window opens again");
}

#[test]
fn a_repeated_syn_gets_the_syn_ack_again_and_unanswered_ones_are_forgotten() {
    let mut s = stack();
    let syn = Seg { seq: 100, flags: SYN, window: 1000, ..Seg::default() };
    feed(&mut s, &frame(&syn), 0);
    let first = out(&mut s, 0);
    feed(&mut s, &frame(&syn), 500);
    let again = out(&mut s, 500);
    assert_eq!((again[0].seq, again[0].flags), (first[0].seq, SYN | ACK));
    // Without options the peer's MSS is 536 and nothing is scaled.
    assert_eq!(again[0].wscale, None);
    let mut now = 500;
    for _ in 0..10 {
        let Some(d) = s.next_deadline() else { break };
        now = d;
        s.tick(now);
        out(&mut s, now);
    }
    assert_eq!(s.connections(), 0, "given up at {now}");
    assert!(events(&mut s).is_empty(), "never accepted, never closed");
}

#[test]
fn a_full_port_and_a_stopped_one_take_no_new_connection() {
    let mut s = TcpStack::new(PORT, None, MTU, 1, 7);
    open(&mut s, 1000);
    let other = Seg { seq: 1, flags: SYN, window: 1000, dst_port: None, ..Seg::default() };
    // Another client port is another connection: same port here, so it is a SYN of an open one.
    let mut s2 = stack();
    s2.stop_accepting();
    feed(&mut s2, &frame(&other), 0);
    assert!(out(&mut s2, 0).is_empty());
    assert_eq!(s2.connections(), 0);
    assert_eq!(s.open(), 1);
}

#[test]
fn an_aborted_connection_sends_a_reset() {
    let mut s = stack();
    let (token, sseq) = open(&mut s, 1000);
    s.abort(token);
    assert_eq!(events(&mut s), vec![Event::Closed(token)]);
    let r = out(&mut s, 2_000);
    assert_eq!((r[0].flags, r[0].seq), (RST | ACK, sseq));
    assert_eq!(s.connections(), 0);
}

#[test]
fn a_connection_whose_peer_never_answers_is_given_up() {
    let mut s = stack();
    let (token, _) = open(&mut s, 1000);
    s.send(token, b"lost");
    out(&mut s, 2_000);
    while let Some(d) = s.next_deadline() {
        s.tick(d);
        out(&mut s, d);
    }
    assert_eq!(events(&mut s), vec![Event::Closed(token)]);
    assert_eq!(s.connections(), 0);
}

#[test]
fn a_transmit_refused_by_a_full_queue_goes_on_at_the_next_call() {
    let mut s = stack();
    let (token, _) = open(&mut s, 1000);
    s.send(token, &vec![9u8; 3 * 1460]);
    let mut first = Vec::new();
    s.transmit(2_000, &mut |f| {
        if first.is_empty() {
            first.push(read_back(f));
            true
        } else {
            false
        }
    });
    let rest = out(&mut s, 2_001);
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].seq, first[0].seq.wrapping_add(1460));
    assert_eq!(s.frame_size(), 14 + MTU);
}

#[test]
fn a_stack_hands_back_what_is_not_its_own_and_names_the_flows_it_takes_and_forgets() {
    let mut s = stack();
    let syn = frame(&Seg { seq: 9, flags: SYN, window: 1000, ..Seg::default() });
    // Left to the caller until it adopts the frame.
    assert_eq!(s.receive(&syn, 0), Arrival::Stranger);
    assert_eq!(s.connections(), 0);
    let flow = flow_of(&syn).unwrap();
    s.adopt(&syn, 0);
    assert_eq!(s.take_flows(), (vec![flow], vec![]));
    assert_eq!(s.receive(&syn, 1), Arrival::Taken);
    assert_eq!(
        s.receive(&frame(&Seg { flags: SYN, dst_port: Some(80), ..Seg::default() }), 1),
        Arrival::Ignored
    );
    // The flow is the same for every segment of the connection.
    let ack = frame(&Seg { seq: 10, ack: 77, flags: ACK, window: 1000, ..Seg::default() });
    assert_eq!(flow_of(&ack), Some(flow));
    let token = 1;
    s.abort(token);
    assert_eq!(s.take_flows(), (vec![], vec![flow]));
}
