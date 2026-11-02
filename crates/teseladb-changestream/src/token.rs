//! The resume token (014 FR-006, contract `resume-token.md`, data model §4): `{_data: <hex>}`,
//! opaque to the client, with the format version, the commit mark (12 bytes), the transaction (16
//! bytes, zeros outside one), the position in the transaction (varint), the type of the event and
//! the key of the document: the fields of the merge order, so a token names one point of it on
//! any node.

use teseladb_hlc::Hlc;

use crate::order::EventKey;

/// Version of the format of a whole event's token.
pub const TOKEN_VERSION: u8 = 1;
/// Version of the format of a fragment's token (`$changeStreamSplitLargeEvent`, FR-011): the
/// fragment's number (varint) after the type.
pub const FRAGMENT_TOKEN_VERSION: u8 = 2;

/// Type of an event (014 FR-002), as the token stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    /// `insert`.
    Insert = 1,
    /// `update`.
    Update = 2,
    /// `replace`.
    Replace = 3,
    /// `delete`.
    Delete = 4,
    /// `drop`.
    Drop = 5,
    /// `rename`.
    Rename = 6,
    /// `dropDatabase`.
    DropDatabase = 7,
    /// `invalidate`.
    Invalidate = 8,
    /// `create`.
    Create = 9,
    /// `createIndexes`.
    CreateIndexes = 10,
    /// `dropIndexes`.
    DropIndexes = 11,
    /// `modify`.
    Modify = 12,
    /// `shardCollection`.
    ShardCollection = 13,
    /// `refineCollectionShardKey`.
    RefineCollectionShardKey = 14,
    /// `reshardCollection`.
    ReshardCollection = 15,
}

impl EventKind {
    const ALL: [EventKind; 15] = [
        EventKind::Insert,
        EventKind::Update,
        EventKind::Replace,
        EventKind::Delete,
        EventKind::Drop,
        EventKind::Rename,
        EventKind::DropDatabase,
        EventKind::Invalidate,
        EventKind::Create,
        EventKind::CreateIndexes,
        EventKind::DropIndexes,
        EventKind::Modify,
        EventKind::ShardCollection,
        EventKind::RefineCollectionShardKey,
        EventKind::ReshardCollection,
    ];

    /// The type a token byte names.
    pub fn from_byte(b: u8) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| *k as u8 == b)
    }
}

/// A decoded token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeToken {
    /// Where its event is in the merge.
    pub event: EventKey,
    /// The event's type.
    pub kind: EventKind,
    /// The fragment of the event, from 1, when `$changeStreamSplitLargeEvent` split it.
    pub fragment: Option<u32>,
}

/// A point of the merge a stream can resume from: after an event, or after every event up to a
/// mark (a high-water mark, what `postBatchResumeToken` gives when no event is left to return).
/// A high-water mark is written as a token with event type 0 and no key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Position {
    /// After this event.
    Event(ResumeToken),
    /// After every event at or below this mark.
    HighWater(Hlc),
}

/// Event type byte of a high-water mark.
const HIGH_WATER: u8 = 0;

impl Position {
    /// The `_data` string.
    pub fn encode(&self) -> String {
        match self {
            Position::Event(t) => t.encode(),
            Position::HighWater(ts) => {
                let event = EventKey { ts: *ts, txn: 0, op: 0, key: Vec::new() };
                hex(&raw(&event, HIGH_WATER))
            }
        }
    }

    /// Reads a `_data` string.
    pub fn decode(data: &str) -> Result<Position, TokenError> {
        let (event, kind, fragment) = parse_fragment(data)?;
        let plain = event.key.is_empty() && event.txn == 0 && event.op == 0;
        if kind == HIGH_WATER && plain && fragment.is_none() {
            return Ok(Position::HighWater(event.ts));
        }
        let kind = EventKind::from_byte(kind).ok_or(TokenError::Kind(kind))?;
        Ok(Position::Event(ResumeToken { event, kind, fragment }))
    }

    /// The mark of the point.
    pub fn ts(&self) -> Hlc {
        match self {
            Position::Event(t) => t.event.ts,
            Position::HighWater(ts) => *ts,
        }
    }
}

/// Why a token does not decode (the command answers `InvalidResumeToken`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// Not an even number of hexadecimal digits.
    NotHex,
    /// A format version this node does not know.
    Version(u8),
    /// Shorter than its fields.
    Truncated,
    /// An event type byte no type has.
    Kind(u8),
}

impl ResumeToken {
    /// The token of a whole event.
    pub fn new(event: EventKey, kind: EventKind) -> ResumeToken {
        ResumeToken { event, kind, fragment: None }
    }

    /// The `_data` string.
    pub fn encode(&self) -> String {
        hex(&raw_fragment(&self.event, self.kind as u8, self.fragment))
    }

    /// Reads a `_data` string of an event.
    pub fn decode(data: &str) -> Result<ResumeToken, TokenError> {
        let (event, kind, fragment) = parse_fragment(data)?;
        let kind = EventKind::from_byte(kind).ok_or(TokenError::Kind(kind))?;
        Ok(ResumeToken { event, kind, fragment })
    }
}

fn raw(e: &EventKey, kind: u8) -> Vec<u8> {
    raw_fragment(e, kind, None)
}

fn put_var(b: &mut Vec<u8>, n: u32) {
    let mut n = u64::from(n);
    loop {
        let low = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            b.push(low);
            return;
        }
        b.push(low | 0x80);
    }
}

fn get_var(mut rest: &[u8]) -> Result<(u32, &[u8]), TokenError> {
    let mut n: u64 = 0;
    for shift in (0..35).step_by(7) {
        let (&byte, r) = rest.split_first().ok_or(TokenError::Truncated)?;
        rest = r;
        n |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
    }
    Ok((u32::try_from(n).map_err(|_| TokenError::Truncated)?, rest))
}

fn raw_fragment(e: &EventKey, kind: u8, fragment: Option<u32>) -> Vec<u8> {
    let mut b = Vec::with_capacity(32 + e.key.len());
    b.push(if fragment.is_some() { FRAGMENT_TOKEN_VERSION } else { TOKEN_VERSION });
    b.extend_from_slice(&e.ts.physical.to_be_bytes());
    b.extend_from_slice(&e.ts.logical.to_be_bytes());
    b.extend_from_slice(&e.txn.to_be_bytes());
    put_var(&mut b, e.op);
    b.push(kind);
    if let Some(f) = fragment {
        put_var(&mut b, f);
    }
    b.extend_from_slice(&e.key);
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// The fields of a token and its fragment, if it is a fragment's.
fn parse_fragment(data: &str) -> Result<(EventKey, u8, Option<u32>), TokenError> {
    let b = unhex(data)?;
    let (&version, rest) = b.split_first().ok_or(TokenError::Truncated)?;
    if version != TOKEN_VERSION && version != FRAGMENT_TOKEN_VERSION {
        return Err(TokenError::Version(version));
    }
    let (physical, rest) = take::<8>(rest)?;
    let (logical, rest) = take::<4>(rest)?;
    let (txn, rest) = take::<16>(rest)?;
    let (op, rest) = get_var(rest)?;
    let (&kind, mut key) = rest.split_first().ok_or(TokenError::Truncated)?;
    let mut fragment = None;
    if version == FRAGMENT_TOKEN_VERSION {
        let (f, k) = get_var(key)?;
        fragment = Some(f);
        key = k;
    }
    let ts = Hlc { physical: u64::from_be_bytes(physical), logical: u32::from_be_bytes(logical) };
    Ok((EventKey { ts, txn: u128::from_be_bytes(txn), op, key: key.to_vec() }, kind, fragment))
}

fn take<const N: usize>(b: &[u8]) -> Result<([u8; N], &[u8]), TokenError> {
    let (head, rest) = b.split_at_checked(N).ok_or(TokenError::Truncated)?;
    Ok((head.try_into().map_err(|_| TokenError::Truncated)?, rest))
}

fn unhex(s: &str) -> Result<Vec<u8>, TokenError> {
    if !s.len().is_multiple_of(2) {
        return Err(TokenError::NotHex);
    }
    let digit = |c: u8| (c as char).to_digit(16).ok_or(TokenError::NotHex);
    s.as_bytes().chunks(2).map(|p| Ok((digit(p[0])? * 16 + digit(p[1])?) as u8)).collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn token(op: u32) -> ResumeToken {
        ResumeToken {
            fragment: None,
            event: EventKey {
                ts: Hlc { physical: 1_730_198_400_123, logical: 7 },
                txn: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
                op,
                key: b"\x07doc-42".to_vec(),
            },
            kind: EventKind::Update,
        }
    }

    #[test]
    fn a_token_reads_back_what_it_was_made_of() {
        for op in [0, 1, 127, 128, 300, u32::MAX] {
            let t = token(op);
            let data = t.encode();
            assert!(data.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
            assert_eq!(ResumeToken::decode(&data), Ok(t), "op {op}");
        }
        assert_eq!(ResumeToken::decode(&token(5).encode().to_lowercase()), Ok(token(5)));
        for kind in EventKind::ALL {
            let t = ResumeToken { kind, ..token(0) };
            assert_eq!(ResumeToken::decode(&t.encode()), Ok(t));
        }
    }

    #[test]
    fn a_damaged_token_is_refused() {
        let data = token(300).encode();
        assert_eq!(ResumeToken::decode("0"), Err(TokenError::NotHex));
        assert_eq!(ResumeToken::decode("zz"), Err(TokenError::NotHex));
        assert_eq!(ResumeToken::decode(""), Err(TokenError::Truncated));
        assert_eq!(ResumeToken::decode(&format!("03{}", &data[2..])), Err(TokenError::Version(3)));
        // Cut inside the mark, inside the varint, and before the type.
        assert_eq!(ResumeToken::decode(&data[..10]), Err(TokenError::Truncated));
        assert_eq!(ResumeToken::decode(&data[..(1 + 12 + 16) * 2 + 2]), Err(TokenError::Truncated));
        assert_eq!(ResumeToken::decode(&data[..(1 + 12 + 16 + 2) * 2]), Err(TokenError::Truncated));
        let bad_kind = format!("{}FF", &data[..(1 + 12 + 16 + 2) * 2]);
        assert_eq!(ResumeToken::decode(&bad_kind), Err(TokenError::Kind(0xff)));
        // A position wider than 32 bits.
        let wide = format!("{}FFFFFFFF7F04", &data[..(1 + 12 + 16) * 2]);
        assert_eq!(ResumeToken::decode(&wide), Err(TokenError::Truncated));
    }

    #[test]
    fn a_high_water_mark_reads_back_and_is_no_event() {
        let ts = Hlc { physical: 9, logical: 3 };
        let data = Position::HighWater(ts).encode();
        assert_eq!(Position::decode(&data), Ok(Position::HighWater(ts)));
        assert_eq!(Position::HighWater(ts).ts(), ts);
        assert_eq!(ResumeToken::decode(&data), Err(TokenError::Kind(0)));
        let t = token(4);
        assert_eq!(Position::decode(&t.encode()), Ok(Position::Event(t.clone())));
        assert_eq!(Position::Event(t.clone()).encode(), t.encode());
        assert_eq!(Position::Event(t.clone()).ts(), t.event.ts);
        // Type 0 with a key is no high-water mark, and no event either.
        let keyed = format!("{data}AB");
        assert_eq!(Position::decode(&keyed), Err(TokenError::Kind(0)));
        // Nor with a transaction, a position or a fragment number.
        let plain = EventKey { ts, txn: 0, op: 0, key: Vec::new() };
        for (txn, op, fragment) in [(1, 0, None), (0, 1, None), (0, 0, Some(1))] {
            let e = EventKey { txn, op, ..plain.clone() };
            let data = hex(&raw_fragment(&e, HIGH_WATER, fragment));
            assert_eq!(
                Position::decode(&data),
                Err(TokenError::Kind(0)),
                "{txn} {op} {fragment:?}"
            );
        }
        assert_eq!(Position::decode("zz"), Err(TokenError::NotHex));
    }

    #[test]
    fn a_fragments_token_carries_its_number_and_the_place_of_its_event() {
        for f in [1, 2, 300] {
            let t = ResumeToken { fragment: Some(f), ..token(128) };
            let data = t.encode();
            assert!(data.starts_with("02"));
            assert_eq!(ResumeToken::decode(&data), Ok(t.clone()));
            assert_eq!(Position::decode(&data), Ok(Position::Event(t.clone())));
            assert_eq!(Position::Event(t.clone()).ts(), t.event.ts);
        }
        let whole = token(5).encode();
        assert!(whole.starts_with("01"));
        // A fragment number cut short.
        let cut = format!("02{}", &whole[2..(1 + 12 + 16 + 1 + 1) * 2]);
        assert_eq!(ResumeToken::decode(&format!("{cut}80")), Err(TokenError::Truncated));
        assert_eq!(ResumeToken::decode("03"), Err(TokenError::Version(3)));
    }
}
