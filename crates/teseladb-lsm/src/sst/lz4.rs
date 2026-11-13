//! The LZ4 block format (FR-037), written here from the format's description.
//!
//! A block is a series of sequences. Each sequence is a token byte (high four bits: the number of
//! literals; low four bits: the match length minus 4), more length bytes when a field is 15 (each
//! adds its value, until one below 255), the literals, a 2-byte little-endian offset back into
//! the output (1 to 65535) and more length bytes for the match. The last sequence has literals
//! only. The last 5 bytes of the input are literals, and the last match starts at least 12 bytes
//! before the end.
//!
//! The LSM stores a block's decompressed length before it, as a varint.

use super::{Bad, get_varint, put_varint};

const MIN_MATCH: usize = 4;
/// The last bytes that are always literals.
const LAST_LITERALS: usize = 5;
/// A match starts at least this far from the end of the input.
const MF_LIMIT: usize = 12;
const MAX_OFFSET: usize = 65_535;
const HASH_BITS: u32 = 14;

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn hash(v: u32) -> usize {
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

/// Writes `n` past the 15 of its token field.
fn put_length(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

fn put_sequence(out: &mut Vec<u8>, literals: &[u8], matched: Option<(usize, usize)>) {
    let lit = literals.len();
    let ml = matched.map_or(0, |(_, len)| len - MIN_MATCH);
    out.push(((lit.min(15) as u8) << 4) | ml.min(15) as u8);
    if lit >= 15 {
        put_length(out, lit - 15);
    }
    out.extend_from_slice(literals);
    if let Some((offset, _)) = matched {
        out.extend_from_slice(&(offset as u16).to_le_bytes());
        if ml >= 15 {
            put_length(out, ml - 15);
        }
    }
}

/// `input` as an LZ4 block.
pub(crate) fn compress_block(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2 + 16);
    let mut table = vec![0usize; 1 << HASH_BITS];
    let mut anchor = 0;
    let mut i = 0;
    if input.len() > MF_LIMIT {
        let limit = input.len() - MF_LIMIT;
        // Positions in `table` are stored plus one: zero is an empty slot.
        while i < limit {
            let h = hash(read_u32(input, i));
            let candidate = table[h];
            table[h] = i + 1;
            let found = candidate
                .checked_sub(1)
                .filter(|c| i - c <= MAX_OFFSET && read_u32(input, *c) == read_u32(input, i));
            let Some(c) = found else {
                i += 1;
                continue;
            };
            let end = input.len() - LAST_LITERALS;
            let mut len = MIN_MATCH;
            while i + len < end && input[c + len] == input[i + len] {
                len += 1;
            }
            put_sequence(&mut out, &input[anchor..i], Some((i - c, len)));
            i += len;
            anchor = i;
        }
    }
    put_sequence(&mut out, &input[anchor..], None);
    out
}

/// Reads the more length bytes of a field that was 15.
fn get_length(b: &[u8], at: &mut usize) -> Result<usize, Bad> {
    let mut n = 0usize;
    loop {
        let byte = *b.get(*at).ok_or(Bad)?;
        *at += 1;
        n = n.checked_add(usize::from(byte)).ok_or(Bad)?;
        if byte != 255 {
            return Ok(n);
        }
    }
}

/// The bytes an LZ4 block decompresses to, which must be `size` long.
pub(crate) fn decompress_block(block: &[u8], size: usize) -> Result<Vec<u8>, Bad> {
    let mut out = Vec::with_capacity(size);
    let mut at = 0;
    loop {
        let token = *block.get(at).ok_or(Bad)?;
        at += 1;
        let mut lit = usize::from(token >> 4);
        if lit == 15 {
            lit += get_length(block, &mut at)?;
        }
        let end = at.checked_add(lit).filter(|e| *e <= block.len()).ok_or(Bad)?;
        if out.len() + lit > size {
            return Err(Bad);
        }
        out.extend_from_slice(&block[at..end]);
        at = end;
        if at == block.len() {
            return if out.len() == size { Ok(out) } else { Err(Bad) };
        }
        let offset = usize::from(u16::from_le_bytes(
            block.get(at..at + 2).ok_or(Bad)?.try_into().map_err(|_| Bad)?,
        ));
        at += 2;
        if offset == 0 || offset > out.len() {
            return Err(Bad);
        }
        let mut len = usize::from(token & 15);
        if len == 15 {
            len += get_length(block, &mut at)?;
        }
        len += MIN_MATCH;
        if out.len() + len > size {
            return Err(Bad);
        }
        // The match may overlap what it writes: byte by byte from `offset` back.
        let from = out.len() - offset;
        for k in 0..len {
            let b = out[from + k];
            out.push(b);
        }
    }
}

/// `payload` compressed: its length as a varint, then its LZ4 block.
pub(crate) fn compress(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, payload.len() as u64);
    out.extend_from_slice(&compress_block(payload));
    out
}

/// What [`compress`] wrote, back; at most `max` bytes.
pub(crate) fn decompress(body: &[u8], max: usize) -> Result<Vec<u8>, Bad> {
    let mut at = 0;
    let size = usize::try_from(get_varint(body, &mut at)?).map_err(|_| Bad)?;
    if size > max {
        return Err(Bad);
    }
    decompress_block(&body[at..], size)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Blocks written by hand from the format's description.
    #[test]
    fn hand_written_blocks_decode() {
        // No input: one token with no literals.
        assert_eq!(decompress_block(&[0x00], 0).unwrap(), b"");
        // Literals only.
        assert_eq!(decompress_block(&[0x30, b'a', b'b', b'c'], 3).unwrap(), b"abc");
        // One literal, a match of 9 at offset 1 (token 0x15: 1 literal, 5 + 4), then five
        // literals: fifteen a.
        let block = [0x15, b'a', 0x01, 0x00, 0x50, b'a', b'a', b'a', b'a', b'a'];
        assert_eq!(decompress_block(&block, 15).unwrap(), [b'a'; 15]);
        // A match overlapping its own output: "ab" and six more at offset 2.
        let block = [0x22, b'a', b'b', 0x02, 0x00, 0x00];
        assert_eq!(decompress_block(&block, 8).unwrap(), b"abababab");
        // Twenty literals: 15 in the token and 5 more.
        let mut block = vec![0xF0, 0x05];
        block.extend(b'A'..b'A' + 20);
        assert_eq!(decompress_block(&block, 20).unwrap(), (b'A'..b'A' + 20).collect::<Vec<_>>());
        // A match of 4 + 15 + 255 + 1 = 275 bytes at offset 1: lengths past 255 go on.
        let block = [0x1F, b'z', 0x01, 0x00, 0xFF, 0x01, 0x00];
        assert_eq!(decompress_block(&block, 276).unwrap(), vec![b'z'; 276]);
    }

    #[test]
    fn damaged_blocks_are_refused() {
        // Offset 0, an offset past the output, a block cut short, a size that does not match.
        assert_eq!(decompress_block(&[0x10, b'a', 0x00, 0x00, 0x00], 5), Err(Bad));
        assert_eq!(decompress_block(&[0x10, b'a', 0x02, 0x00, 0x00], 5), Err(Bad));
        assert_eq!(decompress_block(&[0x30, b'a'], 3), Err(Bad));
        assert_eq!(decompress_block(&[0x30, b'a', b'b', b'c'], 2), Err(Bad));
        assert_eq!(decompress_block(&[0x30, b'a', b'b', b'c'], 4), Err(Bad));
        assert_eq!(decompress_block(&[0x15, b'a', 0x01, 0x00, 0x00], 3), Err(Bad));
        assert_eq!(decompress_block(&[0xF0], 3), Err(Bad));
        assert_eq!(decompress_block(&[], 0), Err(Bad));
        assert_eq!(decompress(&[0x80], 10), Err(Bad));
        assert_eq!(decompress(&compress(&[1; 100]), 99), Err(Bad));
    }

    #[test]
    fn what_it_compresses_comes_back_and_follows_the_end_rules() {
        let doc: Vec<u8> =
            (0..4000u32).map(|i| b"{\"name\":\"value\",\"n\":"[(i % 20) as usize]).collect();
        let mut noise = Vec::new();
        let mut x = 0x9E37_79B9u32;
        for _ in 0..3000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            noise.push(x as u8);
        }
        let inputs: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            vec![7; 13],
            vec![7; 100_000],
            doc.clone(),
            noise.clone(),
            [&doc[..], &noise[..], &doc[..]].concat(),
        ];
        for input in inputs {
            let block = compress_block(&input);
            assert_eq!(decompress_block(&block, input.len()).unwrap(), input);
            assert_eq!(decompress(&compress(&input), usize::MAX).unwrap(), input);
            // The last bytes are literals: the block ends with them.
            let tail = input.len().min(LAST_LITERALS);
            assert!(block.ends_with(&input[input.len() - tail..]));
        }
        assert!(compress_block(&doc).len() * 4 < doc.len());
        assert!(compress_block(&vec![7; 100_000]).len() < 500);
    }
}
