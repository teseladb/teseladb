//! Hand-built vectors following RFC 1950–1952: stored, fixed and dynamic blocks, overlapping
//! references, the maximum distance and the zlib and gzip headers.

use teseladb_deflate::{InflateError, Inflater, Status, gzip, inflate, zlib};

#[test]
fn stored_fixed_and_dynamic_blocks() {
    // Stored block, last: BFINAL=1 BTYPE=00, LEN=5, NLEN=!5.
    let stored = [0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o'];
    assert_eq!(inflate(&stored).expect("inflate succeeds"), b"hello");
    // Empty fixed block: 1 + 01 + code 256 (0000000).
    assert_eq!(inflate(&[0x03, 0x00]).expect("inflate succeeds"), b"");
    // "a" repeated with an overlapping reference (distance 1, length 9): what
    // `printf 'aaaaaaaaaa' | python3 -c 'import zlib,sys;print(zlib.compress(sys.stdin.buffer.read())[2:-4].hex())'`.
    assert_eq!(inflate(&[0x4b, 0x4c, 0x84, 0x01, 0x00]).expect("inflate succeeds"), b"aaaaaaaaaa");
    // Dynamic block (long text with a skewed alphabet: the dynamic code beats the fixed one).
    let text: Vec<u8> =
        (0..20_000u32).map(|i| b"eeeeeeetaoin shrdlu"[(i * 7919 % 19) as usize]).collect();
    let z = zlib::compress(&text, 9);
    assert_eq!(z[2] & 0b110, 0b100, "the compressor chose a dynamic block");
    assert_eq!(zlib::decompress(&z).expect("decompress succeeds").0, text);
}

#[test]
fn errors_on_invalid_streams() {
    use InflateError::*;
    let cases: &[(&[u8], InflateError)] = &[
        (&[0x07], BlockType),                               // BTYPE=11
        (&[0x01, 0x05, 0x00, 0x00, 0x00], StoredLength),    // NLEN not complementary
        (&[0x01, 0x05, 0x00, 0xfa, 0xff, b'h'], Truncated), // data missing
        (&[0x03], Truncated),                               // no end of block
        (&[0x63, 0x00, 0x42, 0x00], Distance),              // reference before the start
        (&[], Truncated),
    ];
    for (data, err) in cases {
        assert_eq!(inflate(data), Err(*err), "{data:02x?}");
    }
    // Dynamic block with more than 286 length codes (HLIT = 31 → 288).
    assert_eq!(inflate(&[0xfd, 0xff, 0xff, 0xff]), Err(Header));
    for e in [BlockType, StoredLength, Header, Code, Symbol, Distance, TooLarge, Truncated] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn output_limit_stops_bombs() {
    let zeros = vec![0u8; 1 << 20];
    let d = teseladb_deflate::deflate(&zeros, 9);
    assert!(d.len() < 2000);
    let mut inf = Inflater::with_limit(1000);
    let mut out = Vec::new();
    assert_eq!(inf.feed(&d, &mut out), Err(InflateError::TooLarge));
    let stored = teseladb_deflate::deflate(&zeros[..5000], 0);
    let mut inf = Inflater::with_limit(1000);
    assert_eq!(inf.feed(&stored, &mut Vec::new()), Err(InflateError::TooLarge));
    let mut inf = Inflater::with_limit(5000);
    let mut out = Vec::new();
    assert_eq!(inf.feed(&stored, &mut out).expect("feed succeeds").1, Status::Done);
    assert_eq!(inf.total_out(), 5000);
    assert!(inf.is_done());
}

#[test]
fn zlib_headers_and_checksum() {
    let z = zlib::compress(b"abc", 6);
    assert_eq!(&z[..2], &[0x78, 0x9c]);
    assert_eq!(&zlib::compress(b"", 1)[..2], &[0x78, 0x01]);
    assert_eq!(&zlib::compress(b"", 9)[..2], &[0x78, 0xda]);
    assert_eq!(&zlib::compress(b"", 3)[..2], &[0x78, 0x5e]);
    // `python3 -c 'import zlib;print(zlib.compress(b"abc").hex())'`
    let py = [0x78, 0x9c, 0x4b, 0x4c, 0x4a, 0x06, 0x00, 0x02, 0x4d, 0x01, 0x27];
    assert_eq!(zlib::decompress(&py).expect("decompress succeeds"), (b"abc".to_vec(), py.len()));
    let mut bad = py;
    bad[10] ^= 1;
    assert_eq!(zlib::decompress(&bad), Err(zlib::ZlibError::Checksum));
    for h in [[0x79, 0x9c], [0x78, 0x9d], [0x88, 0x1c], [0x78, 0xbb]] {
        assert_eq!(zlib::decompress(&h), Err(zlib::ZlibError::Header), "{h:02x?}");
    }
    assert_eq!(zlib::decompress(&py[..9]), Err(zlib::ZlibError::Truncated));
    assert!(matches!(zlib::decompress(&[0x78, 0x9c, 0x07]), Err(zlib::ZlibError::Inflate(_))));
    for e in [
        zlib::ZlibError::Header,
        zlib::ZlibError::Inflate(InflateError::Code),
        zlib::ZlibError::Checksum,
        zlib::ZlibError::Truncated,
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn gzip_members_and_optional_fields() {
    // `printf hello | gzip -n | xxd -i`
    let g = [
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xcb, 0x48, 0xcd, 0xc9, 0xc9,
        0x07, 0x00, 0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00,
    ];
    assert_eq!(gzip::decompress(&g).expect("decompress succeeds"), b"hello");
    // Two members in a row.
    let two = [&g[..], &gzip::compress(b" world", 9)].concat();
    assert_eq!(gzip::decompress(&two).expect("decompress succeeds"), b"hello world");
    // With FEXTRA, FNAME, FCOMMENT and FHCRC.
    let mut h = vec![0x1f, 0x8b, 8, 4 | 8 | 16 | 2, 0, 0, 0, 0, 0, 255];
    h.extend_from_slice(&[3, 0, b'x', b'y', b'z']);
    h.extend_from_slice(b"name\0comment\0");
    let [c0, c1, ..] = teseladb_hash::crc32(&h).to_le_bytes();
    let crc = u16::from_le_bytes([c0, c1]);
    h.extend_from_slice(&crc.to_le_bytes());
    let body = &gzip::compress(b"data", 6)[10..];
    let full = [&h[..], body].concat();
    assert_eq!(gzip::decompress(&full).expect("decompress succeeds"), b"data");
    // Byte by byte too.
    let mut dec = gzip::GzipDecoder::new();
    let mut out = Vec::new();
    for b in &full {
        dec.feed(std::slice::from_ref(b), &mut out).expect("feed succeeds");
    }
    assert!(dec.at_member_boundary());
    assert_eq!(out, b"data");
    // Errors.
    let mut bad_header_crc = full.clone();
    bad_header_crc[h.len() - 1] ^= 1;
    assert_eq!(gzip::decompress(&bad_header_crc), Err(gzip::GzipError::Header));
    let mut bad_crc = g;
    bad_crc[17] ^= 1;
    assert_eq!(gzip::decompress(&bad_crc), Err(gzip::GzipError::Checksum));
    let mut bad_size = g;
    bad_size[21] = 9;
    assert_eq!(gzip::decompress(&bad_size), Err(gzip::GzipError::Checksum));
    for hdr in [
        [0x1e, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3],
        [0x1f, 0x8c, 8, 0, 0, 0, 0, 0, 0, 3],
        [0x1f, 0x8b, 7, 0, 0, 0, 0, 0, 0, 3],
        [0x1f, 0x8b, 8, 0x20, 0, 0, 0, 0, 0, 3],
    ] {
        assert_eq!(gzip::decompress(&hdr), Err(gzip::GzipError::Header));
    }
    assert_eq!(gzip::decompress(&g[..12]), Err(gzip::GzipError::Truncated));
    assert_eq!(gzip::decompress(&g[..5]), Err(gzip::GzipError::Truncated));
    assert!(matches!(
        gzip::decompress(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 0x07]),
        Err(gzip::GzipError::Inflate(_))
    ));
    // Output limit between members.
    let mut lim = gzip::GzipDecoder::with_limit(6);
    assert!(lim.feed(&two, &mut Vec::new()).is_err());
    // Header cut inside each optional field.
    for cut in 10..h.len() {
        let mut d = gzip::GzipDecoder::new();
        d.feed(&full[..cut], &mut Vec::new()).expect("feed succeeds");
        assert!(!d.at_member_boundary());
    }
    for e in [
        gzip::GzipError::Header,
        gzip::GzipError::Inflate(InflateError::Code),
        gzip::GzipError::Checksum,
        gzip::GzipError::Truncated,
    ] {
        assert!(!e.to_string().is_empty());
    }
}
