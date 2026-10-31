//! The resume token (014 FR-006, contract `resume-token.md`, data model §4): `{_data: <hex>}`,
//! opaque to the client, with the format version, the commit mark (12 bytes), the transaction (16
//! bytes, zeros outside one), the position in the transaction (varint), the type of the event and
//! the key of the document: the fields of the merge order, so a token names one point of it on
//! any node.

use teseladb_hlc::Hlc;

use crate::order::EventKey;

/// Version of the format this module writes.
pub const TOKEN_VERSION: u8 = 1;

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
    /// The `_data` string.
    pub fn encode(&self) -> String {
        let e = &self.event;
        let mut b = Vec::with_capacity(32 + e.key.len());
        b.push(TOKEN_VERSION);
        b.extend_from_slice(&e.ts.physical.to_be_bytes());
        b.extend_from_slice(&e.ts.logical.to_be_bytes());
        b.extend_from_slice(&e.txn.to_be_bytes());
        let mut op = u64::from(e.op);
        loop {
            let low = (op & 0x7f) as u8;
            op >>= 7;
            if op == 0 {
                b.push(low);
                break;
            }
            b.push(low | 0x80);
        }
        b.push(self.kind as u8);
        b.extend_from_slice(&e.key);
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    /// Reads a `_data` string.
    pub fn decode(data: &str) -> Result<ResumeToken, TokenError> {
        let b = unhex(data)?;
        let (&version, rest) = b.split_first().ok_or(TokenError::Truncated)?;
        if version != TOKEN_VERSION {
            return Err(TokenError::Version(version));
        }
        let (physical, rest) = take::<8>(rest)?;
        let (logical, rest) = take::<4>(rest)?;
        let (txn, mut rest) = take::<16>(rest)?;
        let mut op: u64 = 0;
        for shift in (0..35).step_by(7) {
            let (&byte, r) = rest.split_first().ok_or(TokenError::Truncated)?;
            rest = r;
            op |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }
        let op = u32::try_from(op).map_err(|_| TokenError::Truncated)?;
        let (&kind, key) = rest.split_first().ok_or(TokenError::Truncated)?;
        let kind = EventKind::from_byte(kind).ok_or(TokenError::Kind(kind))?;
        let ts =
            Hlc { physical: u64::from_be_bytes(physical), logical: u32::from_be_bytes(logical) };
        let event = EventKey { ts, txn: u128::from_be_bytes(txn), op, key: key.to_vec() };
        Ok(ResumeToken { event, kind })
    }
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
mod tests {
    use super::*;

    fn token(op: u32) -> ResumeToken {
        ResumeToken {
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
        assert_eq!(ResumeToken::decode(&format!("02{}", &data[2..])), Err(TokenError::Version(2)));
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
}
