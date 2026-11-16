//! Tests of the wire-level helpers of the native stack: packets (construction, sharing,
//! prepending into the headroom, trimming, linearizing, releasing, freeing on another shard,
//! printing), the out-of-order packet merger, the Internet checksum, byte-order conversion,
//! Ethernet, IPv4, IPv6 and UNIX-domain addresses, the hashers, protocol numbers and the
//! cross-shard wrapper.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::hash::{BuildHasher, Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use teseladb::engine::core::deleter::{Deleter, make_deleter};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::unaligned::Unaligned;
use teseladb::engine::net::byteorder::{
    AdjustEndianness, HtonInplace, NtohInplace, consume_be_slice, hton, hton_packed, hton_struct,
    htonl, htonq, htons, ntoh, ntoh_packed, ntoh_struct, ntohl, ntohq, ntohs, produce_be_slice,
    read_be_slice, write_be_slice,
};
use teseladb::engine::net::const_::{EthProtocolNum, IpProtocolNum};
use teseladb::engine::net::ethernet::{EthHdr, Ethernet, EthernetAddress, parse_ethernet_address};
use teseladb::engine::net::hash::{FxBuildHasher, FxHasher, IdentityHash};
use teseladb::engine::net::ip_checksum::{Checksummer, ip_checksum};
use teseladb::engine::net::ipv4_address::{Ipv4Address, is_unspecified};
use teseladb::engine::net::ipv6_address::Ipv6Address;
use teseladb::engine::net::node_ptr::{CrossShard, NodePtr};
use teseladb::engine::net::packet::{Fragment, OffloadInfo, Packet};
use teseladb::engine::net::packet_util::{MergerOffset, MergerTag, PacketMerger};
use teseladb::engine::net::socket_defs::{
    Ipv4Addr, Transport, hash_combine, hash_sockaddr_un, in6_addr, in6_are_addr_equal,
    in6_is_addr_v4mapped, sockaddr_un,
};
use teseladb::engine::net::unix_address::UnixDomainAddr;

/// The bytes of `p`, fragment after fragment.
fn bytes(p: &Packet) -> Vec<u8> {
    let mut v = Vec::new();
    for f in p.fragments().iter() {
        // SAFETY: a fragment of a live packet points at `size` readable bytes.
        v.extend_from_slice(unsafe { f.as_slice() });
    }
    v
}

/// A deleter that counts its runs in `c`.
fn counting(c: &Rc<Cell<u32>>) -> Deleter {
    let c = c.clone();
    make_deleter(move || c.set(c.get() + 1))
}

/// A static byte slice.
fn leaked(data: &[u8]) -> &'static mut [u8] {
    Box::leak(data.to_vec().into_boxed_slice())
}

fn frag(data: &'static mut [u8]) -> Fragment {
    Fragment { base: data.as_mut_ptr(), size: data.len() }
}

// ---------------------------------------------------------------------------------------
// Packets.
// ---------------------------------------------------------------------------------------

#[test]
fn packets_from_static_data_and_fragment_views() {
    let mut p = Packet::from_static_data(b"static bytes");
    assert_eq!(p.len(), 12);
    assert!(p.memory() > p.len());
    assert_eq!(p.nr_frags(), 1);
    let v = p.fragments();
    assert_eq!(v.len(), 1);
    assert!(!v.is_empty());
    // SAFETY: both pointers come from the same fragment array.
    assert_eq!(unsafe { v.end().offset_from(v.begin()) }, 1);
    assert_eq!(v[0].size, 12);
    assert_eq!(v.as_slice().len(), 1);
    assert_eq!(p.frag(0).size, 12);
    p.frag_mut(0).size = 6;
    assert_eq!(p.frag(0).size, 6);
    assert_eq!(p.fragment_array() as *const Fragment, p.fragments().begin());
    let empty = Fragment { base: std::ptr::null_mut(), size: 0 };
    // SAFETY: a zero-sized fragment reads nothing.
    assert!(unsafe { empty.as_slice() }.is_empty());
    let e = Packet::new();
    assert!(e.fragments().is_empty() || e.len() == 0);
    assert!(Packet::with_nr_frags(8).is_valid());
}

#[test]
fn null_packets_and_reset() {
    let mut p = Packet::from_data(b"x");
    assert!(p.is_valid());
    p.reset();
    assert!(!p.is_valid());
    assert!(!Packet::make_null_packet().is_valid());
}

#[test]
fn small_and_large_fragments_are_copied() {
    let small = vec![1u8; 50];
    let big = vec![2u8; 500];
    let p = Packet::from_data(&small);
    assert_eq!(bytes(&p), small);
    let p = Packet::from_data(&big);
    assert_eq!(bytes(&p), big);
    let p = Packet::from_fragment(Fragment { base: big.as_ptr() as *mut u8, size: 100 });
    assert_eq!(p.len(), 100);
}

#[test]
fn zero_copy_constructors_run_their_deleters() {
    let runs = Rc::new(Cell::new(0));
    let a = frag(leaked(b"abc"));
    let b = frag(leaked(b"defg"));
    let p = Packet::from_fragments(vec![a, b], counting(&runs));
    assert_eq!(p.len(), 7);
    assert_eq!(bytes(&p), b"abcdefg");
    drop(p);
    assert_eq!(runs.get(), 1);

    let frags = [frag(leaked(b"12")), frag(leaked(b"345"))];
    let p = Packet::from_iter(frags.iter(), counting(&runs));
    assert_eq!((p.len(), p.nr_frags()), (5, 2));
    assert_eq!(bytes(&p), b"12345");
    drop(p);
    assert_eq!(runs.get(), 2);

    let p = Packet::prepend_zero_copy(
        frag(leaked(b"head")),
        counting(&runs),
        Packet::from_data(b"tail"),
    );
    assert_eq!(bytes(&p), b"headtail");
    let p = Packet::append_zero_copy(p, frag(leaked(b"!")), counting(&runs));
    assert_eq!(bytes(&p), b"headtail!");
    let p = Packet::append_deleter(p, counting(&runs));
    drop(p);
    assert_eq!(runs.get(), 5);
}

#[test]
fn buffers_become_packets_without_copies() {
    let mut buf = TemporaryBuffer::<u8>::new(4);
    // SAFETY: the buffer has 4 writable bytes.
    unsafe { std::ptr::copy_nonoverlapping(b"buf1".as_ptr(), buf.get_write(), 4) };
    let base = buf.get_write();
    let p = Packet::from_buffer(buf);
    assert_eq!(p.frag(0).base, base);
    let p = Packet::append_buffer(p, TemporaryBuffer::<u8>::copy_from(b"buf2"));
    assert_eq!(bytes(&p), b"buf1buf2");
}

#[test]
fn copies_are_appended_and_prepended() {
    let p = Packet::from_data(b"middle");
    let p = Packet::append_copy(p, Fragment { base: b"-end".as_ptr() as *mut u8, size: 4 });
    // Fits in the headroom of the internal data area.
    let p = Packet::prepend_copy(Fragment { base: b"start-".as_ptr() as *mut u8, size: 6 }, p);
    assert_eq!(bytes(&p), b"start-middle-end");
    // Too large for the headroom: a fragment of its own.
    let big = vec![b'x'; 300];
    let p = Packet::prepend_copy(Fragment { base: big.as_ptr() as *mut u8, size: big.len() }, p);
    assert_eq!(p.len(), 316);
    assert_eq!(&bytes(&p)[..300], &big[..]);
    // Prepending to a packet whose first fragment is external.
    let ext = Packet::from_static_data(b"external");
    let p = Packet::prepend_copy(Fragment { base: b"<".as_ptr() as *mut u8, size: 1 }, ext);
    assert_eq!(bytes(&p), b"<external");
}

#[test]
fn headers_use_the_headroom_then_new_fragments() {
    let mut p = Packet::from_static_data(b"payload");
    // The first header moves into the internal area in front of the external data.
    let h = p.prepend_header::<[u8; 4]>(0);
    // SAFETY: room for 4 bytes was just made at `h`.
    unsafe { h.write_unaligned(*b"HDR1") };
    // A second header still fits.
    let h = p.prepend_uninitialized_header(4);
    // SAFETY: as above.
    unsafe { std::ptr::copy_nonoverlapping(b"HDR0".as_ptr(), h, 4) };
    assert_eq!(bytes(&p), b"HDR0HDR1payload");
    // One larger than the internal area gets its own allocation.
    let h = p.prepend_uninitialized_header(200);
    // SAFETY: room for 200 bytes was just made at `h`.
    unsafe { std::ptr::write_bytes(h, b'z', 200) };
    assert_eq!(p.len(), 215);
    assert_eq!(bytes(&p)[..200], [b'z'; 200]);
}

#[test]
fn headers_spanning_fragments_are_linearized() {
    let mut p = Packet::from_static_data(b"ab");
    p.append(Packet::from_static_data(b"cd"));
    p.append(Packet::from_static_data(b"ef"));
    assert_eq!(p.nr_frags(), 3);
    // Past the end.
    assert!(p.get_header_raw(5, 2).is_null());
    assert!(p.get_header::<[u8; 8]>(0).is_null());
    // Within one fragment: no copy.
    assert!(!p.get_header_raw(2, 2).is_null());
    assert_eq!(p.nr_frags(), 3);
    // Spanning the second and third fragments (not from the start).
    let h = p.get_header_raw(3, 2);
    // SAFETY: two readable bytes at `h`.
    assert_eq!(unsafe { std::slice::from_raw_parts(h, 2) }, b"de");
    assert_eq!(p.nr_frags(), 2);
    p.linearize_all();
    assert_eq!(p.nr_frags(), 1);
    assert_eq!(bytes(&p), b"abcdef");
}

#[test]
fn trimming_crosses_fragments() {
    let mut p = Packet::from_static_data(b"abc");
    p.append(Packet::from_static_data(b"def"));
    p.append(Packet::from_static_data(b"ghi"));
    p.trim_front(4);
    assert_eq!(bytes(&p), b"efghi");
    p.trim_back(4);
    assert_eq!(bytes(&p), b"e");
    // Trimming the back of data in the internal area gives the room back.
    let mut q = Packet::from_data(b"0123456789");
    q.trim_back(3);
    q.trim_front(2);
    assert_eq!(bytes(&q), b"23456");
    let mut q = Packet::from_data(b"xy");
    q.trim_front(2);
    assert_eq!(q.len(), 0);
}

#[test]
fn appending_to_an_empty_packet_takes_the_other() {
    let mut p = Packet::new();
    p.append(Packet::from_data(b"first"));
    assert_eq!(bytes(&p), b"first");
    p.reserve(10);
    for i in 0..10u8 {
        p.append(Packet::from_data(&[i]));
    }
    assert_eq!(p.len(), 15);
    p.reserve(1);
}

#[test]
fn shares_cover_ranges_and_keep_offload_info() {
    let mut p = Packet::from_static_data(b"0123");
    p.append(Packet::from_static_data(b"4567"));
    let oi = OffloadInfo {
        needs_csum: true,
        tso_seg_size: 9,
        protocol: IpProtocolNum::tcp,
        ..OffloadInfo::default()
    };
    p.set_offload_info(oi);
    p.offload_info_ref().reassembled = true;
    let mut s = p.share_range(3, 3);
    assert_eq!(bytes(&s), b"345");
    assert_eq!(s.nr_frags(), 2);
    let got = s.get_offload_info();
    assert!(got.needs_csum && got.reassembled);
    assert_eq!(got.tso_seg_size, 9);
    let whole = s.share();
    assert_eq!(bytes(&whole), b"345");
    assert_eq!(p.set_rss_hash(3), Some(3));
    assert_eq!(p.rss_hash(), Some(3));
}

#[test]
fn release_hands_out_every_fragment() {
    let mut p = Packet::from_static_data(b"external");
    let h = p.prepend_uninitialized_header(3);
    // SAFETY: room for 3 bytes at `h`.
    unsafe { std::ptr::copy_nonoverlapping(b"int".as_ptr(), h, 3) };
    let bufs = p.release();
    assert_eq!(bufs.len(), 2);
    assert_eq!(bufs[0].as_slice(), b"int");
    assert_eq!(bufs[1].as_slice(), b"external");
    let mut q = Packet::from_static_data(b"only");
    let mut seen = Vec::new();
    q.release_into(|b| seen.push(b.as_slice().to_vec()));
    assert_eq!(seen, vec![b"only".to_vec()]);
}

#[test]
fn packets_print_as_text_or_hex() {
    let mut p = Packet::from_static_data(b"hi\r\n\t");
    p.append(Packet::from_static_data(&[0x00, 0xff]));
    assert_eq!(format!("{p}"), "packet{\"hi\\r\\n\\t\", {00 ff}}");
    let q = Packet::from_static_data(&[0x41, 0x0b]);
    let s = format!("{q}");
    assert!(s.starts_with("packet{\"A\\x"), "{s}");
}

#[macro_use]
#[path = "common/cov_thread_case.rs"]
mod thread_case;

thread_case!(packets_are_freed_on_their_home_shard, {
    let runs = Arc::new(AtomicU32::new(0));
    let r = runs.clone();
    let mut p = Packet::from_data(b"home");
    let q = p.free_on_cpu(this_shard_id(), move || {
        r.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(bytes(&q), b"home");
    drop(p);
    drop(q);
    let mut tries = 0;
    while runs.load(Ordering::Relaxed) == 0 && tries < 1000 {
        sleep_steady(Duration::from_millis(1)).get().expect("sleep");
        tries += 1;
    }
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    let mut p = Packet::from_data(b"x");
    drop(p.free_on_cpu_default(this_shard_id()));
});

// ---------------------------------------------------------------------------------------
// Packet merger.
// ---------------------------------------------------------------------------------------

struct TestTag;
thread_local! {
    static TEST_LINEARIZATIONS: Cell<u64> = const { Cell::new(0) };
}
impl MergerTag for TestTag {
    fn linearization_count() -> &'static std::thread::LocalKey<Cell<u64>> {
        &TEST_LINEARIZATIONS
    }
}

/// The segments of `m` as (offset, bytes).
fn segments<O: MergerOffset + Copy>(m: &PacketMerger<O, TestTag>) -> Vec<(O, Vec<u8>)> {
    m.map.iter().map(|(k, p)| (*k, bytes(p))).collect()
}

#[test]
fn merger_handles_every_overlap() {
    let mut m = PacketMerger::<u32, TestTag>::default();
    let before = PacketMerger::<u32, TestTag>::linearizations();
    m.merge(10, Packet::from_data(b"klm"));
    assert_eq!(segments(&m), vec![(10, b"klm".to_vec())]);
    assert!(PacketMerger::<u32, TestTag>::linearizations() > before);
    // 1) Already held.
    m.merge(11, Packet::from_data(b"l"));
    assert_eq!(segments(&m), vec![(10, b"klm".to_vec())]);
    // 2) The new segment covers the old one.
    m.merge(8, Packet::from_data(b"ijklmn"));
    assert_eq!(segments(&m), vec![(8, b"ijklmn".to_vec())]);
    // 5/6) Disjoint.
    m.merge(30, Packet::from_data(b"EFG"));
    // 3) The new segment ends inside an old one.
    m.merge(27, Packet::from_data(b"BCDE"));
    assert_eq!(segments(&m), vec![(8, b"ijklmn".to_vec()), (27, b"BCDEFG".to_vec())]);
    // 4) The new segment starts inside an old one; then the hole closes and the two join.
    m.merge(12, Packet::from_data(b"mnopqrstuvwxyzABC"));
    assert_eq!(segments(&m), vec![(8, b"ijklmnopqrstuvwxyzABCDEFG".to_vec())]);
}

#[test]
fn merger_drops_segments_swallowed_by_a_merge() {
    let mut m = PacketMerger::<u64, TestTag>::default();
    m.merge(0, Packet::from_data(b"0123456789"));
    m.merge(12, Packet::from_data(b"cd"));
    m.merge(20, Packet::from_data(b"kl"));
    // Extends the first segment past the second, which is then dropped.
    m.merge(5, Packet::from_data(b"56789abcdefg"));
    assert_eq!(segments(&m), vec![(0, b"0123456789abcdefg".to_vec()), (20, b"kl".to_vec())]);
    assert_eq!(7u64.add_len(3), 10);
    assert_eq!(10u64.diff(7), 3);
    assert_eq!(u32::MAX.add_len(2), 1);
    assert_eq!(1u32.diff(u32::MAX), 2);
}

// ---------------------------------------------------------------------------------------
// Checksum.
// ---------------------------------------------------------------------------------------

/// The reference checksum of RFC 1071.
fn reference(data: &[u8]) -> u16 {
    let mut s: u32 = 0;
    for c in data.chunks(2) {
        s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
    }
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

#[test]
fn checksums_agree_with_the_reference() {
    let data: Vec<u8> = (0..1001u32).map(|i| (i * 31 % 256) as u8).collect();
    for len in [0, 1, 2, 3, 7, 8, 9, 15, 16, 17, 1001] {
        let mut c = Checksummer::default();
        c.sum_bytes(&data[..len]);
        assert_eq!(u16::from_be(c.get()), reference(&data[..len]), "len {len}");
        // SAFETY: the slice has `len` readable bytes.
        let sum = unsafe { ip_checksum(data.as_ptr(), len) };
        assert_eq!(u16::from_be(sum), reference(&data[..len]));
    }
    // Byte-wise, word-wise and odd-aligned sums are the same as the bytes.
    let mut a = Checksummer::default();
    a.sum_u8(1);
    a.sum_u16(0x0203);
    a.sum_u16(0x0405);
    assert_eq!(u16::from_be(a.get()), reference(&[1, 2, 3, 4, 5]));
    let mut b = Checksummer::default();
    b.sum(9u8);
    b.sum(0x0a0b_0c0du32);
    assert_eq!(u16::from_be(b.get()), reference(&[9, 0x0c, 0x0d, 0x0a, 0x0b]));
    let mut c = Checksummer::default();
    c.sum(0x0a0b_0c0du32);
    c.sum(0x0e0fu16);
    assert_eq!(u16::from_be(c.get()), reference(&[0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]));
    let mut d = Checksummer::default();
    teseladb::checksum_sum_many!(d, 1u8, 2u8, 0x0304u16);
    d.sum_bytes(&[]);
    assert_eq!(u16::from_be(d.get()), reference(&[1, 2, 3, 4]));
    // Odd fragments of a packet.
    let mut p = Packet::from_static_data(b"abc");
    p.append(Packet::from_static_data(b"defgh"));
    p.append(Packet::from_static_data(b"ijklmnopqrstu"));
    let mut e = Checksummer::default();
    e.sum_packet(&p);
    assert_eq!(u16::from_be(e.get()), reference(b"abcdefghijklmnopqrstu"));
    assert!(format!("{e:?}").contains("odd"));
}

// ---------------------------------------------------------------------------------------
// Byte order.
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct TwoFields {
    a: u16,
    b: u32,
}

impl AdjustEndianness for TwoFields {
    fn adjust_endianness<V: teseladb::engine::net::byteorder::EndianVisitor>(&mut self, v: &mut V) {
        teseladb::adjust_fields!(v, self.a, self.b);
    }
}

#[test]
fn byte_order_conversions() {
    assert_eq!(ntohs(htons(0x1234)), 0x1234);
    assert_eq!(htons(0x1234), 0x1234u16.to_be());
    assert_eq!(ntohl(htonl(0x1234_5678)), 0x1234_5678);
    assert_eq!(htonq(0x0102_0304_0506_0708), 0x0102_0304_0506_0708u64.to_be());
    assert_eq!(ntohq(htonq(42)), 42);
    assert_eq!(ntoh(5u8), 5);
    assert_eq!(hton(5u8), 5);
    assert_eq!(ntoh(hton(0x1234u16)), 0x1234);
    assert_eq!(ntoh(hton(0x1234_5678u32)), 0x1234_5678);
    assert_eq!(ntoh(hton(7u64)), 7);
    assert_eq!(ntoh(hton(-2i8)), -2);
    assert_eq!(ntoh(hton(-2i16)), -2);
    assert_eq!(hton(-2i32), (-2i32).to_be());
    assert_eq!(ntoh(hton(-2i32)), -2);
    assert_eq!(ntoh(hton(-2i64)), -2);
    ntoh(());
    hton(());
    assert_eq!(ntoh([1u8, 2, 3]), [1, 2, 3]);
    assert_eq!(hton([1u8, 2]), [1, 2]);
    let packed: Unaligned<u32> = Unaligned::new(0x0102_0304u32.to_be());
    assert_eq!(ntoh_packed(&packed), 0x0102_0304);
    assert_eq!(hton_packed(&Unaligned::new(0x0102_0304u32)), 0x0102_0304u32.to_be());
    assert_eq!(format!("{}", Unaligned::new(9u8)), "9");

    let host = TwoFields { a: 0x1234, b: 0x5678_9abc };
    let net = hton_struct(&host);
    assert_eq!(net, TwoFields { a: 0x1234u16.to_be(), b: 0x5678_9abcu32.to_be() });
    assert_eq!(ntoh_struct(&net), host);
    let mut x = host;
    x.adjust_endianness(&mut HtonInplace);
    x.adjust_endianness(&mut NtohInplace);
    assert_eq!(x, host);
    let (mut a, mut b) = (0x1234u16, 0x0102_0304u32);
    teseladb::hton_inplace!(a, b);
    assert_eq!((a, b), (0x1234u16.to_be(), 0x0102_0304u32.to_be()));
    teseladb::ntoh_inplace!(a, b);
    assert_eq!((a, b), (0x1234, 0x0102_0304));

    let mut buf = [0u8; 6];
    write_be_slice::<u32>(&mut buf, 0x0a0b_0c0d);
    assert_eq!(read_be_slice::<u32>(&buf), 0x0a0b_0c0d);
    {
        let mut w: &mut [u8] = &mut buf;
        produce_be_slice::<u16>(&mut w, 0x1122);
        produce_be_slice::<u16>(&mut w, 0x3344);
        assert_eq!(w.len(), 2);
    }
    let mut r: &[u8] = &buf;
    assert_eq!(consume_be_slice::<u16>(&mut r), 0x1122);
    assert_eq!(consume_be_slice::<u16>(&mut r), 0x3344);
    assert_eq!(r.len(), 2);
}

#[test]
#[should_panic]
fn reading_past_a_short_buffer_panics() {
    let _ = read_be_slice::<u32>(&[1, 2]);
}

// ---------------------------------------------------------------------------------------
// Addresses.
// ---------------------------------------------------------------------------------------

#[test]
fn ethernet_addresses() {
    assert_eq!(EthernetAddress::new().mac, [0; 6]);
    let a = EthernetAddress::from_list(&[1, 2, 3, 4, 5, 6]);
    assert_eq!(EthernetAddress::from_ptr(&[1, 2, 3, 4, 5, 6, 7]).mac, a.mac);
    let mut buf = [0u8; 8];
    a.write(&mut buf);
    assert_eq!(&buf[..6], &[1, 2, 3, 4, 5, 6]);
    assert_eq!(EthernetAddress::read(&buf).mac, a.mac);
    let mut r: &[u8] = &buf;
    assert_eq!(EthernetAddress::consume(&mut r).mac, a.mac);
    assert_eq!(r.len(), 2);
    let mut out = [0u8; 7];
    {
        let mut w: &mut [u8] = &mut out;
        a.produce(&mut w);
        assert_eq!(w.len(), 1);
    }
    assert_eq!(&out[..6], &[1, 2, 3, 4, 5, 6]);
    assert_eq!(EthernetAddress::size(), 6);
    assert_eq!(format!("{a}"), "01:02:03:04:05:06");
    assert_eq!(format!("{a:?}"), "01:02:03:04:05:06");
    assert_eq!(Ethernet::broadcast_address().mac, [0xff; 6]);
    assert_eq!(Ethernet::arp_hardware_type(), 1);
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&EthernetAddress { mac: [1, 2, 3, 4, 5, 6] }));

    let h = EthHdr { dst_mac: a, src_mac: EthernetAddress::new(), eth_proto: 0x0800 };
    let n = hton_struct(&h);
    assert_eq!({ n.eth_proto }, 0x0800u16.to_be());
    assert_eq!({ ntoh_struct(&n).eth_proto }, 0x0800);
    let mut e = a;
    e.adjust_endianness(&mut HtonInplace);
    assert_eq!(e.mac, a.mac);

    assert_eq!(
        parse_ethernet_address("0a:0B:0x0c:d:+e:ff".to_string()).expect("valid").mac,
        [10, 11, 12, 13, 14, 255]
    );
    assert!(parse_ethernet_address("1:2:3".to_string()).is_err());
    assert!(parse_ethernet_address("1:2:3:4:5:zz".to_string()).is_err());
    assert!(parse_ethernet_address("1:2:3:4:5:fffffffff".to_string()).is_err());
    assert_eq!(parse_ethernet_address("1:2:3:4:5:-1".to_string()).expect("wraps").mac[5], 0xff);
}

#[test]
fn ipv4_addresses() {
    assert_eq!({ Ipv4Address::new().ip }, 0);
    let a = Ipv4Address::from_ipv4_addr(Ipv4Addr::new(0x0a00_0001, 80));
    assert_eq!({ a.ip }, 0x0a00_0001);
    let mut buf = [0u8; 6];
    a.write(&mut buf);
    assert_eq!(&buf[..4], &[10, 0, 0, 1]);
    assert!(Ipv4Address::read(&buf) == a);
    let mut r: &[u8] = &buf;
    assert!(Ipv4Address::consume(&mut r) == a);
    assert_eq!(r.len(), 2);
    let mut out = [0u8; 5];
    {
        let mut w: &mut [u8] = &mut out;
        a.produce(&mut w);
        assert_eq!(w.len(), 1);
    }
    assert_eq!(&out[..4], &[10, 0, 0, 1]);
    assert_eq!(Ipv4Address::size(), 4);
    assert_eq!(format!("{a} {a:?}"), "10.0.0.1 10.0.0.1");
    assert!(is_unspecified(Ipv4Address::new()));
    assert!(!is_unspecified(a));
    let mut n = a;
    n.adjust_endianness(&mut HtonInplace);
    assert_eq!({ n.ip }, 0x0a00_0001u32.to_be());
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&Ipv4Address::from_u32(0x0a00_0001)));
}

#[test]
fn ipv6_addresses() {
    let a = Ipv6Address { ip: [1; 16] };
    let b = Ipv6Address { ip: [1; 16] };
    assert!(a == b);
    assert!(a != Ipv6Address { ip: [2; 16] });
    assert_eq!(Ipv6Address::size(), 16);
    assert_eq!(a.bytes(), [1; 16]);
    let mut c = a;
    c.adjust_endianness(&mut NtohInplace);
    assert!(c == a);
}

#[test]
fn unix_domain_addresses() {
    let path = UnixDomainAddr::from_c_str("/tmp/sock\0ignored");
    assert_eq!(path.name, b"/tmp/sock");
    assert_eq!(path.path_length(), 10);
    assert_eq!(path.path_bytes(), b"/tmp/sock\0");
    assert_eq!(format!("{path}"), "/tmp/sock");
    let abs = UnixDomainAddr::new(b"\0name\x01x");
    assert_eq!(abs.path_length(), 7);
    assert_eq!(format!("{abs}"), "@name_x");
    let unnamed = UnixDomainAddr::new(b"");
    assert_eq!(format!("{unnamed}"), "{unnamed}");
    assert!(path == UnixDomainAddr::new(b"/tmp/sock"));
    assert!(path != abs);
    let mut set = HashSet::new();
    set.insert(path.clone());
    assert!(set.contains(&UnixDomainAddr::from_c_str("/tmp/sock")));
}

#[test]
fn socket_definition_helpers() {
    let mut a = in6_addr { s6_addr: [0; 16] };
    a.set_s6_addr32(2, 0xffffu32.to_be());
    a.set_s6_addr32(3, 0x0a00_0001u32.to_be());
    assert!(in6_is_addr_v4mapped(&a));
    assert_eq!(a.s6_addr32(3), 0x0a00_0001u32.to_be());
    assert!(in6_are_addr_equal(&a, &a.clone()));
    assert!(!in6_is_addr_v4mapped(&in6_addr { s6_addr: [0; 16] }));
    let mut un = sockaddr_un { sun_family: 1, sun_path: [0; 108] };
    un.sun_path[..4].copy_from_slice(b"/abc");
    let h1 = hash_sockaddr_un(&un);
    un.sun_path[10] = b'z';
    assert_eq!(hash_sockaddr_un(&un), h1);
    assert!(format!("{un:?}").contains("sun_family"));
    let mut seed = 0usize;
    hash_combine(&mut seed, 7);
    assert_ne!(seed, 0);
    let mut set = HashSet::new();
    set.insert(Transport::TCP);
    assert!(set.contains(&Transport::TCP));
    assert!(!set.contains(&Transport::SCTP));
    let x = Ipv4Addr::new(1, 2);
    assert!(x == Ipv4Addr { ip: 1, port: 2 });
    assert!(!x.is_ip_unspecified());
    assert!(Ipv4Addr::default().is_ip_unspecified());
}

// ---------------------------------------------------------------------------------------
// Hashers, protocol numbers, pointers.
// ---------------------------------------------------------------------------------------

#[test]
fn hashers() {
    let mut h = FxHasher::default();
    h.write_u8(1);
    h.write_u16(2);
    h.write_u32(3);
    h.write_u64(4);
    h.write_usize(5);
    h.write(b"0123456789");
    let a = h.finish();
    let mut g = FxHasher::default();
    g.write_u8(1);
    g.write_u16(2);
    g.write_u32(3);
    g.write_u64(4);
    g.write_usize(5);
    g.write(b"0123456789");
    assert_eq!(a, g.finish());
    assert_ne!(a, FxHasher::default().finish());
    assert_eq!(FxBuildHasher::default().hash_one(7u32), FxBuildHasher::default().hash_one(7u32));

    // The identity hash: XOR of the integers, bytes folded big-endian.
    assert_eq!(IdentityHash::of(&0x1234u32), 0x1234);
    assert_eq!(IdentityHash::of(&5u8), 5);
    assert_eq!(IdentityHash::of(&5u16), 5);
    assert_eq!(IdentityHash::of(&5u64), 5);
    assert_eq!(IdentityHash::of(&5usize), 5);
    let mut i = IdentityHash::default();
    i.write(&[1, 2]);
    assert_eq!(i.finish(), 0x0102);
    let mut j = IdentityHash::default();
    3u32.hash(&mut j);
    6u32.hash(&mut j);
    assert_eq!(j.finish(), 5);
    assert!(format!("{i:?}").contains("IdentityHash"));
}

#[test]
fn protocol_numbers() {
    assert_eq!(IpProtocolNum::from_u8(1), IpProtocolNum::icmp);
    assert_eq!(IpProtocolNum::from_u8(6), IpProtocolNum::tcp);
    assert_eq!(IpProtocolNum::from_u8(17), IpProtocolNum::udp);
    assert_eq!(IpProtocolNum::from_u8(99), IpProtocolNum::unused);
    assert_eq!(IpProtocolNum::default(), IpProtocolNum::unused);
    assert_eq!(EthProtocolNum::arp as u16, 0x0806);
    assert_eq!(EthProtocolNum::ipv6 as u16, 0x86dd);
}

#[test]
fn cross_shard_values_and_nodes() {
    // SAFETY: the value never leaves this thread.
    let c = unsafe { CrossShard::new(Rc::new(5)) };
    assert_eq!(**c.get(), 5);
    let d = c.clone();
    assert_eq!(*d.into_inner(), 5);
    assert_eq!(*c.into_inner(), 5);
    let n = NodePtr::new(RefCell::new(1));
    // SAFETY: no other reference to the value is live.
    unsafe { *n.get_mut().borrow_mut() = 2 };
    // SAFETY: as above.
    assert_eq!(*unsafe { n.get() }.borrow(), 2);
    let raw = n.into_raw();
    // SAFETY: `raw` came from `into_raw` and is taken back once.
    let back = unsafe { NodePtr::from_raw(raw) };
    assert_eq!(back.as_ptr(), raw);
}
