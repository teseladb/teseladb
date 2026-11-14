//! HPACK, the header compression of HTTP/2 (RFC 7541), without I/O: the static and dynamic
//! tables, the decoder of a header block and the encoder.

use std::collections::VecDeque;

use crate::huffman;

/// The default size of the dynamic table (`SETTINGS_HEADER_TABLE_SIZE`, RFC 9113 section 6.5.2).
pub const DEFAULT_TABLE_SIZE: usize = 4096;

/// What each entry costs in the table besides its name and value (RFC 7541, section 4.1).
const ENTRY_OVERHEAD: usize = 32;

/// A header field: its name and value as octets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The name, in lower case for HTTP/2.
    pub name: Vec<u8>,
    /// The value.
    pub value: Vec<u8>,
}

impl Field {
    /// The field `name: value`.
    #[must_use]
    pub fn new(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Field {
        Field { name: name.into(), value: value.into() }
    }

    fn size(&self) -> usize {
        self.name.len() + self.value.len() + ENTRY_OVERHEAD
    }
}

/// Why a header block cannot be decoded: each is a `COMPRESSION_ERROR` of the connection
/// (RFC 9113, section 4.3), except [`HpackError::ListTooLarge`], which the stream refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpackError {
    /// The block ends inside a representation.
    Truncated,
    /// An integer does not fit in 32 bits.
    Integer,
    /// An index outside the static and dynamic tables, or 0.
    Index,
    /// A Huffman string with a bad code or bad padding.
    Huffman,
    /// A table size update above the size the decoder allows, or after a field.
    TableSize,
    /// The decoded list is larger than the decoder's limit (`SETTINGS_MAX_HEADER_LIST_SIZE`).
    ListTooLarge,
}

/// The static table (RFC 7541, appendix A), from index 1.
const STATIC: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// The dynamic table: the newest entry first (RFC 7541, section 2.3.2).
#[derive(Clone, Debug)]
struct Table {
    entries: VecDeque<Field>,
    size: usize,
    max: usize,
}

impl Table {
    fn new(max: usize) -> Table {
        Table { entries: VecDeque::new(), size: 0, max }
    }

    fn resize(&mut self, max: usize) {
        self.max = max;
        self.evict(0);
    }

    /// Evicts the oldest entries until `room` more octets fit.
    fn evict(&mut self, room: usize) {
        while self.size + room > self.max {
            let Some(old) = self.entries.pop_back() else {
                break;
            };
            self.size -= old.size();
        }
    }

    /// Adds `f`; an entry larger than the table empties it and is not added (section 4.4).
    fn add(&mut self, f: Field) {
        let size = f.size();
        self.evict(size);
        if size <= self.max {
            self.size += size;
            self.entries.push_front(f);
        }
    }

    /// The field at `index` of the address space of the static and dynamic tables.
    fn get(&self, index: usize) -> Result<Field, HpackError> {
        match index {
            0 => Err(HpackError::Index),
            i if i <= STATIC.len() => {
                let (n, v) = STATIC[i - 1];
                Ok(Field::new(n, v))
            }
            i => self.entries.get(i - STATIC.len() - 1).cloned().ok_or(HpackError::Index),
        }
    }

    /// The index of `f` in both tables, and whether its value matches too.
    fn find(&self, f: &Field) -> Option<(usize, bool)> {
        let mut by_name = None;
        let tables = STATIC
            .iter()
            .map(|(n, v)| (n.as_bytes(), v.as_bytes()))
            .chain(self.entries.iter().map(|e| (e.name.as_slice(), e.value.as_slice())));
        for (i, (n, v)) in tables.enumerate() {
            if n == f.name.as_slice() {
                if v == f.value.as_slice() {
                    return Some((i + 1, true));
                }
                by_name.get_or_insert((i + 1, false));
            }
        }
        by_name
    }
}

/// Reads an integer of `prefix` bits starting at `block[*at]` (RFC 7541, section 5.1).
fn read_int(block: &[u8], at: &mut usize, prefix: u8) -> Result<usize, HpackError> {
    let mask = (1u16 << prefix) - 1;
    let first = *block.get(*at).ok_or(HpackError::Truncated)?;
    *at += 1;
    let mut n = usize::from(u16::from(first) & mask);
    if n < usize::from(mask) {
        return Ok(n);
    }
    let mut shift = 0u32;
    loop {
        let b = *block.get(*at).ok_or(HpackError::Truncated)?;
        *at += 1;
        let more = usize::from(b & 0x7f).checked_shl(shift).ok_or(HpackError::Integer)?;
        n = n.checked_add(more).filter(|n| u32::try_from(*n).is_ok()).ok_or(HpackError::Integer)?;
        if b & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
        if shift > 28 {
            return Err(HpackError::Integer);
        }
    }
}

/// Writes `n` with `prefix` bits, the first octet's other bits being `flags`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "each pushed value is below the prefix's maximum or masked to 7 bits"
)]
fn write_int(out: &mut Vec<u8>, flags: u8, prefix: u8, n: usize) {
    let max = (1usize << prefix) - 1;
    if n < max {
        out.push(flags | n as u8);
        return;
    }
    out.push(flags | max as u8);
    let mut rest = n - max;
    while rest >= 0x80 {
        out.push((rest & 0x7f) as u8 | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// Reads a string literal (RFC 7541, section 5.2).
fn read_string(block: &[u8], at: &mut usize) -> Result<Vec<u8>, HpackError> {
    let huffman = block.get(*at).ok_or(HpackError::Truncated)? & 0x80 != 0;
    let len = read_int(block, at, 7)?;
    let end = at.checked_add(len).filter(|e| *e <= block.len()).ok_or(HpackError::Truncated)?;
    let raw = &block[*at..end];
    *at = end;
    if huffman { huffman::decode(raw) } else { Ok(raw.to_vec()) }
}

/// Writes a string literal, coded with Huffman when that is shorter.
fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    if huffman::encoded_len(s) < s.len() {
        let coded = huffman::encode(s);
        write_int(out, 0x80, 7, coded.len());
        out.extend_from_slice(&coded);
    } else {
        write_int(out, 0, 7, s.len());
        out.extend_from_slice(s);
    }
}

/// The decoder of the header blocks of one connection.
#[derive(Clone, Debug)]
pub struct Decoder {
    table: Table,
    /// The largest table the peer may ask for (our `SETTINGS_HEADER_TABLE_SIZE`).
    allowed: usize,
    /// The largest decoded list, by the size of RFC 9113 section 6.5.2.
    max_list: usize,
}

impl Decoder {
    /// A decoder whose table may grow to `allowed` octets and whose lists may reach `max_list`.
    #[must_use]
    pub fn new(allowed: usize, max_list: usize) -> Decoder {
        Decoder { table: Table::new(allowed), allowed, max_list }
    }

    /// The fields of header block `block`; the dynamic table changes as the block says.
    ///
    /// # Errors
    ///
    /// An [`HpackError`]: the block ends early, an integer or index is out of range, a Huffman string is bad,
    /// a table size update is too large or misplaced, or the list passes the limit.
    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<Field>, HpackError> {
        let mut at = 0;
        let mut fields = Vec::new();
        let mut list = 0usize;
        while at < block.len() {
            let b = block[at];
            let field = if b & 0x80 != 0 {
                self.table.get(read_int(block, &mut at, 7)?)?
            } else if b & 0xc0 == 0x40 {
                let f = self.literal(block, &mut at, 6)?;
                self.table.add(f.clone());
                f
            } else if b & 0xe0 == 0x20 {
                // A size update only before the first field (RFC 7541, section 4.2).
                let size = read_int(block, &mut at, 5)?;
                if !fields.is_empty() || size > self.allowed {
                    return Err(HpackError::TableSize);
                }
                self.table.resize(size);
                continue;
            } else {
                // Without indexing (0000) or never indexed (0001).
                self.literal(block, &mut at, 4)?
            };
            list = list.saturating_add(field.size());
            if list > self.max_list {
                return Err(HpackError::ListTooLarge);
            }
            fields.push(field);
        }
        Ok(fields)
    }

    fn literal(&self, block: &[u8], at: &mut usize, prefix: u8) -> Result<Field, HpackError> {
        let index = read_int(block, at, prefix)?;
        let name = match index {
            0 => read_string(block, at)?,
            i => self.table.get(i)?.name,
        };
        Ok(Field { name, value: read_string(block, at)? })
    }

    /// The octets of the dynamic table now.
    #[must_use]
    pub fn table_size(&self) -> usize {
        self.table.size
    }
}

/// Names whose values never enter a table, so that no one can guess them from its size
/// (RFC 7541, section 7.1.3).
const SENSITIVE: [&[u8]; 4] = [b"authorization", b"proxy-authorization", b"cookie", b"set-cookie"];

/// The encoder of the header blocks of one connection.
#[derive(Clone, Debug)]
pub struct Encoder {
    table: Table,
    /// A size the peer allowed that the next block announces.
    pending: Option<usize>,
}

impl Default for Encoder {
    fn default() -> Encoder {
        Encoder::new()
    }
}

impl Encoder {
    /// An encoder with the default table of 4 096 octets.
    #[must_use]
    pub fn new() -> Encoder {
        Encoder { table: Table::new(DEFAULT_TABLE_SIZE), pending: None }
    }

    /// Takes the peer's `SETTINGS_HEADER_TABLE_SIZE`: the table never grows past the default.
    pub fn set_max_table_size(&mut self, size: usize) {
        let size = size.min(DEFAULT_TABLE_SIZE);
        if size != self.table.max {
            self.table.resize(size);
            self.pending = Some(size);
        }
    }

    /// The block of `fields`.
    pub fn encode(&mut self, fields: &[Field]) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(size) = self.pending.take() {
            write_int(&mut out, 0x20, 5, size);
        }
        for f in fields {
            let sensitive = SENSITIVE.contains(&f.name.as_slice());
            match self.table.find(f) {
                Some((i, true)) if !sensitive => write_int(&mut out, 0x80, 7, i),
                found => {
                    let name_index = found.map_or(0, |(i, _)| i);
                    if sensitive {
                        write_int(&mut out, 0x10, 4, name_index);
                    } else {
                        write_int(&mut out, 0x40, 6, name_index);
                    }
                    if name_index == 0 {
                        write_string(&mut out, &f.name);
                    }
                    write_string(&mut out, &f.value);
                    if !sensitive {
                        self.table.add(f.clone());
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits
            .chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).expect("hex"), 16).expect("hex"))
            .collect()
    }

    fn fields(pairs: &[(&str, &str)]) -> Vec<Field> {
        pairs.iter().map(|(n, v)| Field::new(*n, *v)).collect()
    }

    /// One step of a series: a block in hex, the fields it decodes to and the table size after it.
    type Step<'a> = (&'a str, &'a [(&'a str, &'a str)], usize);

    /// Each block of a series decodes to its fields and leaves the table of that size.
    fn series(size: usize, steps: &[Step<'_>]) {
        let mut d = Decoder::new(size, 1 << 16);
        for (block, want, table) in steps {
            assert_eq!(d.decode(&hex(block)).expect(block), fields(want), "{block}");
            assert_eq!(d.table_size(), *table, "{block}");
        }
    }

    const REQ1: &[(&str, &str)] = &[
        (":method", "GET"),
        (":scheme", "http"),
        (":path", "/"),
        (":authority", "www.example.com"),
    ];
    const REQ2: &[(&str, &str)] = &[
        (":method", "GET"),
        (":scheme", "http"),
        (":path", "/"),
        (":authority", "www.example.com"),
        ("cache-control", "no-cache"),
    ];
    const REQ3: &[(&str, &str)] = &[
        (":method", "GET"),
        (":scheme", "https"),
        (":path", "/index.html"),
        (":authority", "www.example.com"),
        ("custom-key", "custom-value"),
    ];
    const RESP1: &[(&str, &str)] = &[
        (":status", "302"),
        ("cache-control", "private"),
        ("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
        ("location", "https://www.example.com"),
    ];
    const RESP2: &[(&str, &str)] = &[
        (":status", "307"),
        ("cache-control", "private"),
        ("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
        ("location", "https://www.example.com"),
    ];
    const RESP3: &[(&str, &str)] = &[
        (":status", "200"),
        ("cache-control", "private"),
        ("date", "Mon, 21 Oct 2013 20:13:22 GMT"),
        ("location", "https://www.example.com"),
        ("content-encoding", "gzip"),
        ("set-cookie", "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1"),
    ];

    #[test]
    fn rfc_c3_requests_without_huffman() {
        series(
            4096,
            &[
                ("8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d", REQ1, 57),
                ("8286 84be 5808 6e6f 2d63 6163 6865", REQ2, 110),
                (
                    "8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65",
                    REQ3,
                    164,
                ),
            ],
        );
    }

    #[test]
    fn rfc_c4_requests_with_huffman() {
        series(
            4096,
            &[
                ("8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff", REQ1, 57),
                ("8286 84be 5886 a8eb 1064 9cbf", REQ2, 110),
                ("8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf", REQ3, 164),
            ],
        );
    }

    #[test]
    fn rfc_c5_responses_without_huffman() {
        series(256, &[
            (
                "4803 3330 3258 0770 7269 7661 7465 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133
                 2032 303a 3133 3a32 3120 474d 546e 1768 7474 7073 3a2f 2f77 7777 2e65 7861 6d70
                 6c65 2e63 6f6d",
                RESP1,
                222,
            ),
            ("4803 3330 37c1 c0bf", RESP2, 222),
            (
                "88c1 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 2032 303a 3133 3a32 3220 474d
                 54c0 5a04 677a 6970 7738 666f 6f3d 4153 444a 4b48 514b 425a 584f 5157 454f 5049
                 5541 5851 5745 4f49 553b 206d 6178 2d61 6765 3d33 3630 303b 2076 6572 7369 6f6e
                 3d31",
                RESP3,
                215,
            ),
        ]);
    }

    #[test]
    fn rfc_c6_responses_with_huffman() {
        series(256, &[
            (
                "4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6
                 2d1b ff6e 919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3",
                RESP1,
                222,
            ),
            ("4883 640e ffc1 c0bf", RESP2, 222),
            (
                "88c1 6196 d07a be94 1054 d444 a820 0595 040b 8166 e084 a62d 1bff c05a 839b d9ab
                 77ad 94e7 821d d7f2 e6c7 b335 dfdf cd5b 3960 d5af 2708 7f36 72c1 ab27 0fb5 291f
                 9587 3160 65c0 03ed 4ee5 b106 3d50 07",
                RESP3,
                215,
            ),
        ]);
    }

    #[test]
    fn rfc_c1_integers() {
        // C.1.1 to C.1.3: 10 in a 5-bit prefix, 1337 in a 5-bit prefix, 42 in eight bits.
        for (n, prefix, octets) in
            [(10, 5, vec![0x0a]), (1337, 5, vec![0x1f, 0x9a, 0x0a]), (42, 8, vec![42])]
        {
            let mut out = Vec::new();
            write_int(&mut out, 0, prefix, n);
            assert_eq!(out, octets);
            assert_eq!(read_int(&octets, &mut 0, prefix), Ok(n));
        }
    }

    #[test]
    fn rfc_c2_literals_and_the_never_indexed_one() {
        let mut d = Decoder::new(4096, 1 << 16);
        // C.2.1: literal with indexing, new name.
        let block = hex("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572");
        assert_eq!(d.decode(&block), Ok(fields(&[("custom-key", "custom-header")])));
        assert_eq!(d.table_size(), 55);
        // C.2.2: without indexing; C.2.3: never indexed. The table keeps its size.
        let mut d = Decoder::new(4096, 1 << 16);
        assert_eq!(
            d.decode(&hex("040c 2f73 616d 706c 652f 7061 7468")),
            Ok(fields(&[(":path", "/sample/path")]))
        );
        assert_eq!(
            d.decode(&hex("1008 7061 7373 776f 7264 0673 6563 7265 74")),
            Ok(fields(&[("password", "secret")]))
        );
        assert_eq!(d.table_size(), 0);
        // C.2.4: indexed.
        assert_eq!(d.decode(&[0x82]), Ok(fields(&[(":method", "GET")])));
    }

    #[test]
    fn the_encoder_gives_blocks_the_decoder_reads_back() {
        let mut e = Encoder::new();
        let mut d = Decoder::new(DEFAULT_TABLE_SIZE, 1 << 16);
        for list in [REQ1, REQ2, REQ3, RESP3, RESP3] {
            let block = e.encode(&fields(list));
            assert_eq!(d.decode(&block).expect("block"), fields(list));
        }
        // The second time a list is all in the tables, one octet per field.
        let block = e.encode(&fields(REQ1));
        assert_eq!(block.len(), REQ1.len());
        // The rfc's first request, Huffman coded as the encoder writes it.
        let mut fresh = Encoder::new();
        assert_eq!(fresh.encode(&fields(REQ1)), hex("8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff"));
    }

    #[test]
    fn credentials_are_never_indexed() {
        let mut e = Encoder::new();
        let secret = fields(&[("authorization", "Bearer abc"), ("cookie", "a=b")]);
        let block = e.encode(&secret);
        assert_eq!(block[0] & 0xf0, 0x10, "{block:?}");
        assert_eq!(e.table.size, 0);
        let mut d = Decoder::new(DEFAULT_TABLE_SIZE, 1 << 16);
        assert_eq!(d.decode(&block), Ok(secret.clone()));
        // Sent again, the value goes again.
        assert_eq!(e.encode(&secret), block);
    }

    #[test]
    fn table_size_updates_follow_the_settings() {
        let mut e = Encoder::new();
        e.set_max_table_size(100);
        e.set_max_table_size(100);
        let block = e.encode(&fields(&[("x-a", "1")]));
        assert_eq!(&block[..2], &[0x3f, 0x45], "{block:?}");
        let mut d = Decoder::new(DEFAULT_TABLE_SIZE, 1 << 16);
        assert_eq!(d.decode(&block), Ok(fields(&[("x-a", "1")])));
        // An update after a field, or above what the decoder allows.
        assert_eq!(d.decode(&[0x82, 0x20]), Err(HpackError::TableSize));
        let mut small = Decoder::new(100, 1 << 16);
        assert_eq!(small.decode(&[0x3f, 0x46]), Err(HpackError::TableSize));
        // An update to 0 empties the table.
        assert_eq!(d.decode(&[0x20]), Ok(Vec::new()));
        assert_eq!(d.table_size(), 0);
        // An entry larger than the table empties it.
        let mut tiny = Decoder::new(40, 1 << 16);
        tiny.decode(&hex("4003 782d 6103 3132 33")).expect("x-a: 123 fits in 40");
        assert_eq!(tiny.table_size(), 38);
        tiny.decode(&hex("4003 782d 620a 3031 3233 3435 3637 3839")).expect("x-b");
        assert_eq!(tiny.table_size(), 0);
    }

    #[test]
    fn malformed_blocks_are_refused() {
        let mut d = Decoder::new(DEFAULT_TABLE_SIZE, 1 << 16);
        assert_eq!(d.decode(&[0x80]), Err(HpackError::Index));
        assert_eq!(d.decode(&[0xbe]), Err(HpackError::Index));
        assert_eq!(d.decode(&[0xff]), Err(HpackError::Truncated));
        assert_eq!(d.decode(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]), Err(HpackError::Integer));
        assert_eq!(d.decode(&[0x40, 0x05, b'a']), Err(HpackError::Truncated));
        assert_eq!(d.decode(&[0x40, 0x81, 0x18, 0x00]), Err(HpackError::Huffman));
        let mut small = Decoder::new(DEFAULT_TABLE_SIZE, 40);
        assert_eq!(
            small.decode(&hex("4003 782d 610a 3031 3233 3435 3637 3839")),
            Err(HpackError::ListTooLarge)
        );
    }
}
