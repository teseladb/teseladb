//! Tests of the TCP option parser and writer: malformed option lists from the wire are
//! parsed within their bounds, and well-formed ones are read and written correctly.

#![cfg(target_os = "linux")]

use teseladb::engine::net::tcp::{TcpHdr, TcpOption};

const EOL: u8 = 0;
const NOP: u8 = 1;
const MSS: u8 = 2;
const WS: u8 = 3;
const SACK: u8 = 4;
const TS: u8 = 8;
const UNKNOWN: u8 = 30;

/// What a parse leaves in the option state, comparable.
#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    mss_received: bool,
    win_scale_received: bool,
    sack_received: bool,
    remote_mss: u16,
    remote_win_scale: u8,
    local_win_scale: u8,
}

fn parse(opts: &[u8]) -> Parsed {
    let mut o = TcpOption::default();
    o.parse(opts);
    Parsed {
        mss_received: o._mss_received,
        win_scale_received: o._win_scale_received,
        sack_received: o._sack_received,
        remote_mss: o._remote_mss,
        remote_win_scale: o._remote_win_scale,
        local_win_scale: o._local_win_scale,
    }
}

fn nothing() -> Parsed {
    parse(&[])
}

/// Parses `opts` as the head of a buffer whose following bytes hold a valid MSS of 1460 and
/// window scale of 9, so that any read past the end of the option list would show up in the
/// result; also parses the same bytes from a heap copy of exactly their length.
fn parse_bounded(opts: &[u8]) -> Parsed {
    let mut buf = opts.to_vec();
    buf.extend_from_slice(&[MSS, 4, 0x05, 0xb4, WS, 3, 9, NOP]);
    let in_bigger = parse(&buf[..opts.len()]);
    // A heap block of exactly the list's length: a read past it is out of the allocation.
    let exact_buf: Box<[u8]> = opts.into();
    let exact = parse(&exact_buf);
    assert_eq!(in_bigger, exact, "bytes past the option list changed the parse of {opts:?}");
    exact
}

#[test]
fn empty_list_leaves_defaults() {
    let p = parse_bounded(&[]);
    assert!(!p.mss_received && !p.win_scale_received && !p.sack_received);
    assert_eq!(p.remote_mss, 536);
}

#[test]
fn kind_in_last_byte_is_ignored() {
    assert_eq!(parse_bounded(&[MSS]), nothing());
    assert_eq!(parse_bounded(&[NOP, NOP, WS]), nothing());
    assert_eq!(parse_bounded(&[NOP, UNKNOWN]), nothing());
    // A valid option before it is kept.
    let p = parse_bounded(&[MSS, 4, 0x02, 0x18, SACK]);
    assert!(p.mss_received);
    assert_eq!(p.remote_mss, 0x0218);
    assert!(!p.sack_received);
}

#[test]
fn len_zero_or_one_stops_the_parse() {
    for kind in [MSS, WS, SACK, TS, UNKNOWN] {
        for len in [0u8, 1] {
            // The MSS after the malformed option is not reached: the rest is ignored.
            let p = parse_bounded(&[kind, len, MSS, 4, 0x05, 0xb4]);
            assert_eq!(p, nothing(), "kind {kind} len {len}");
        }
    }
}

#[test]
fn len_past_the_end_stops_the_parse() {
    assert_eq!(parse_bounded(&[MSS, 4, 0x05]), nothing());
    assert_eq!(parse_bounded(&[WS, 3]), nothing());
    assert_eq!(parse_bounded(&[SACK, 3]), nothing());
    assert_eq!(parse_bounded(&[UNKNOWN, 255, 0, 0]), nothing());
    assert_eq!(parse_bounded(&[TS, 10, 1, 2, 3, 4, 5, 6, 7]), nothing());
    // A valid option before it is kept.
    let p = parse_bounded(&[WS, 3, 4, MSS, 4, 0x05]);
    assert!(p.win_scale_received && !p.mss_received);
    assert_eq!(p.remote_win_scale, 4);
}

#[test]
fn known_option_with_wrong_len_is_skipped_unread() {
    // An MSS of len 3 is skipped by its len; the window scale after it is read.
    let p = parse_bounded(&[MSS, 3, 0x05, WS, 3, 6]);
    assert!(!p.mss_received);
    assert_eq!(p.remote_mss, 536);
    assert!(p.win_scale_received);
    assert_eq!(p.remote_win_scale, 6);

    // A window scale of len 2 (no shift byte) is skipped; the MSS after it is read.
    let p = parse_bounded(&[WS, 2, MSS, 4, 0x05, 0xb4]);
    assert!(!p.win_scale_received);
    assert_eq!(p.local_win_scale, 0);
    assert!(p.mss_received);
    assert_eq!(p.remote_mss, 1460);

    // Longer than their fixed size too.
    let p = parse_bounded(&[MSS, 6, 0x05, 0xb4, 0, 0, SACK, 3, 0, NOP]);
    assert_eq!(p, nothing());
}

#[test]
fn eol_ends_the_list() {
    assert_eq!(parse_bounded(&[EOL, MSS, 4, 0x05, 0xb4]), nothing());
    let p = parse_bounded(&[SACK, 2, EOL, WS, 3, 7]);
    assert!(p.sack_received && !p.win_scale_received);
}

#[test]
fn valid_syn_options_are_read() {
    // The options of a typical SYN: MSS, SACK permitted, timestamps, NOP, window scale.
    let opts = [MSS, 4, 0x05, 0xb4, SACK, 2, TS, 10, 0, 0, 0, 1, 0, 0, 0, 0, NOP, WS, 3, 7];
    let p = parse_bounded(&opts);
    assert_eq!(
        p,
        Parsed {
            mss_received: true,
            win_scale_received: true,
            sack_received: true,
            remote_mss: 1460,
            remote_win_scale: 7,
            local_win_scale: 7,
        }
    );
    // An option we do not understand is skipped by its len.
    let p = parse_bounded(&[UNKNOWN, 5, 1, 2, 3, MSS, 4, 0x23, 0x28]);
    assert!(p.mss_received);
    assert_eq!(p.remote_mss, 9000);
}

#[test]
fn arbitrary_lists_never_read_past_their_end() {
    // Every list of up to 3 bytes over the interesting values, then pseudo-random lists up to
    // the longest a header carries.
    let values = [EOL, NOP, MSS, WS, SACK, TS, UNKNOWN, 2, 3, 4, 10, 40, 255];
    for &a in &values {
        parse_bounded(&[a]);
        for &b in &values {
            parse_bounded(&[a, b]);
            for &c in &values {
                parse_bounded(&[a, b, c]);
            }
        }
    }
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as u8
    };
    for _ in 0..20_000 {
        let n = next() as usize % (TcpOption::MAX_LEN + 1);
        let opts: Vec<u8> = (0..n).map(|_| values[next() as usize % values.len()]).collect();
        parse_bounded(&opts);
    }
}

fn syn(ack: bool) -> TcpHdr {
    let mut h = TcpHdr::default();
    h.set_f_syn(true);
    h.set_f_ack(ack);
    h
}

#[test]
fn fill_writes_the_syn_options_and_they_parse_back() {
    let o = TcpOption { _local_mss: 1460, _local_win_scale: 7, ..TcpOption::default() };
    let th = syn(false);
    let size = o.get_size(th.f_syn(), th.f_ack());
    assert_eq!(size, 8);
    let mut out = vec![0xaa; size as usize];
    assert_eq!(o.fill(&mut out, &th), size);
    assert_eq!(out, [MSS, 4, 0x05, 0xb4, WS, 3, 7, EOL]);

    let p = parse_bounded(&out);
    assert!(p.mss_received && p.win_scale_received);
    assert_eq!((p.remote_mss, p.remote_win_scale), (1460, 7));
}

#[test]
fn fill_pads_a_lone_mss_with_nops() {
    // A SYN-ACK answering a peer that sent an MSS but no window scale.
    let o = TcpOption { _mss_received: true, _local_mss: 536, ..TcpOption::default() };
    let th = syn(true);
    let size = o.get_size(th.f_syn(), th.f_ack());
    let mut out = vec![0xaa; size as usize];
    assert_eq!(o.fill(&mut out, &th), size);
    assert_eq!(out, [MSS, 4, 0x02, 0x18, NOP, NOP, NOP, EOL]);
}

#[test]
fn fill_writes_nothing_without_syn() {
    let o = TcpOption { _mss_received: true, _win_scale_received: true, ..TcpOption::default() };
    let th = TcpHdr::default();
    assert_eq!(o.get_size(false, false), 0);
    assert_eq!(o.fill(&mut [], &th), 0);
}

#[test]
#[should_panic(expected = "tcp options area too short")]
fn fill_into_a_short_area_panics_instead_of_writing_past_it() {
    let o = TcpOption::default();
    let mut out = [0u8; 5];
    o.fill(&mut out, &syn(false));
}

#[test]
#[should_panic]
fn fill_into_a_long_area_panics() {
    let o = TcpOption::default();
    let mut out = [0u8; 12];
    o.fill(&mut out, &syn(false));
}
