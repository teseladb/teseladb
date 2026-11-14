//! Utilities shared by the integration tests.
#![allow(dead_code, missing_docs)]

use std::fmt::Write as _;

/// Deterministic data of length `n`: byte i = (31·i + 7) mod 256.
pub fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| i.wrapping_mul(31).wrapping_add(7).to_le_bytes()[0]).collect()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

/// (length, crc32c, adler32, sha1, sha256) of `pattern(n)`, computed with an independent
/// implementation (Python: hashlib, zlib and bit-by-bit CRC32C).
pub const CASES: &[(usize, u32, u32, &str, &str)] = &[
    (
        0,
        0x0000_0000,
        0x0000_0001,
        "da39a3ee5e6b4b0d3255bfef95601890afd80709",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ),
    (
        1,
        0x86b7_37ba,
        0x0008_0008,
        "5d1be7e9dda1ee8896be5b7e34a85ee16452a7b4",
        "ca358758f6d27e6cf45272937977a748fd88391db679ceda7dc7bf1f005ee879",
    ),
    (
        3,
        0x765a_7c83,
        0x00a9_0073,
        "d2df16b976a43628c63ea246436fccf6d7635d09",
        "647674a296197442f518bcca323ec605dd8d098b2d4f22ee1fdcdd2bb753a189",
    ),
    (
        8,
        0x4079_5c72,
        0x0b30_039d,
        "090a21896cc276ea75ecae78af863a34cb135e1e",
        "4fb900ca3f5832fcc475b79bf07217bf0edfe9d39ea10f5cf624246ff68b47de",
    ),
    (
        55,
        0xdab4_8fa4,
        0xf129_1b55,
        "749bbefb28edc4638b28b2b9a9e03ab9a4032b90",
        "8aa994584139d128848eeebc4e815639ba5ab6e6e39574195a63ac4f14f7c43b",
    ),
    (
        56,
        0x8563_b7ae,
        0x0d3d_1c05,
        "a5b6e9c29d201c774753ff8e7fb64931656f5e63",
        "ad574708f75c044c9b85de64cb568ee7711ff4f36448c6242f053ba8f6cc2b63",
    ),
    (
        57,
        0xc155_2087,
        0x2a11_1cd4,
        "eb0737bed5451790722b2df351829ce117e3d9dd",
        "5b46e502092be01b1100193e089fdda95638c12e19a1d24f308eb2c3d3ae849d",
    ),
    (
        63,
        0xd701_3350,
        0xdfcc_1f39,
        "d1a454409359fc372b4d22b3cea6488d6ba1be00",
        "280ed3e8ff1df845b2e7dfe6ac6cee817bef20e783cc65abc41b818b4d2fe076",
    ),
    (
        64,
        0x2b1d_65d8,
        0xffad_1fe1,
        "39a0d8b645ad85f1f976731ed112ac9455e28b78",
        "c6ab9724ade5b6a7a1edfffb12f3aa9181351355af8fd08c919952ad211339dd",
    ),
    (
        65,
        0x1c1b_b57f,
        0x2064_20a8,
        "d0c96e18890114a14716e9686528d2e3fdba8d9e",
        "788367c73c7ddf4c53f65e68cc0d943e6227ab55b0e78ba63ace822b1c6301c0",
    ),
    (
        119,
        0xbfe1_4203,
        0xe46e_3b75,
        "562ecf8a430f8e1056e3619bae33628e9a1d0a4e",
        "3d610547d68216dedf7435a4fb6260353911f6b3fd3f18805ddb8be285d726fe",
    ),
    (
        120,
        0x300a_15ea,
        0x2062_3be5,
        "353f6d2bf0e91aa91b74a2e0b3f297510f7d825f",
        "1f80156a804cb7862ad113e8200e9d74499723e7c7854d5f48776d3148e09656",
    ),
    (
        127,
        0x7e96_a9c7,
        0xd420_3f59,
        "bebc42d2d3d1e5fb8ad8895c2dcef2d68a6c279a",
        "192409cd280e14b743642ad1343fbd3e82d9305de72c078117745a679210cc3d",
    ),
    (
        128,
        0xae5b_4c7a,
        0x13f0_3fc1,
        "0060f2a7e34b6e4d459f560197ef93243732a400",
        "cc548ca2dec1f6fe4f58b2e27aa9c7521607df1130d140b55a4dad0665302356",
    ),
    (
        1000,
        0xff52_ee97,
        0xd9f3_f1bc,
        "414475341017ec91703435a6f290324818f983e9",
        "5097e7d587352f5097062ae679f37bda5802d9f875aba14c8cb4d1a188ada179",
    ),
    (
        4096,
        0xe1c2_f7e8,
        0x1ca9_f86a,
        "2c177f7cc0e199dab44868ac2d42a03814e38e75",
        "d41d438c379110c7f7b2c561b1f04f26c1b4549110791f8e022f48974280c13e",
    ),
    (
        5552,
        0xeae7_e900,
        0x2cf4_cdbf,
        "d87d5b6af4a574cf1290216e52acf25192bce36b",
        "65e854151ead3e7dda12828e0c05c1dcdacd86106c6a1026d046219f2751eaf1",
    ),
    (
        5553,
        0xd778_0a80,
        0xfb0a_ce16,
        "0f7111508120dc41b6ddb41739527d3015444963",
        "ccaca7aeb638e4d5df61df4fba222b0de57fc789d1fedeea40442cc83672fea7",
    ),
    (
        10000,
        0xe128_5b52,
        0x4197_7596,
        "0a573c2292d5cdf622dec01189c4b481f84cc8d5",
        "470b2cd71bff57ce8be0be3fc23df273052c4bb10a1235fddb8f158d6f928546",
    ),
    (
        65537,
        0xfa47_8090,
        0x02b6_8779,
        "93a2e226feb6f62cfd4161b0dce5806729c542d6",
        "74f3260f4a9adaf7271c910ba2d111dbae7a176afb487e1b7f655897eadf1093",
    ),
];
