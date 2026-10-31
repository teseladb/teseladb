//! Envelope of a proposed entry (data model §4): `kind` (u16, `0` reserved for a leader's `Noop`),
//! the HLC mark the leader stamped it with (12 bytes: milliseconds and the logical part, 005
//! FR-018) and the state machine's payload. The log adds the CRC of the record.

use teseladb_hlc::Hlc;
use teseladb_types::{DecodeError, Decoder, Encoder};

/// `kind` of an entry for the state machine.
pub const KIND_COMMAND: u16 = 1;

/// A decoded envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Envelope<'a> {
    /// HLC mark of the entry.
    pub ts: Hlc,
    /// Payload for the state machine.
    pub payload: &'a [u8],
}

/// Encodes a command entry.
pub fn encode(ts: Hlc, payload: &[u8]) -> Vec<u8> {
    let mut e = Encoder::new();
    e.u16(KIND_COMMAND).u64(ts.physical).u32(ts.logical).raw(payload);
    e.finish()
}

/// Decodes an entry written by [`encode`]; any other `kind` is an error.
pub fn decode(data: &[u8]) -> Result<Envelope<'_>, DecodeError> {
    let mut d = Decoder::new(data);
    let at = d.position();
    if d.u16()? != KIND_COMMAND {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    let ts = Hlc { physical: d.u64()?, logical: d.u32()? };
    let payload = d.raw(d.remaining())?;
    Ok(Envelope { ts, payload })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_unknown_kinds() {
        let ts = Hlc { physical: 1_700_000_000_123, logical: (7 << 10) | 3 };
        let b = encode(ts, b"put k v");
        let e = decode(&b).unwrap();
        assert_eq!((e.ts, e.payload), (ts, &b"put k v"[..]));
        let zero = Hlc { physical: 5, logical: 0 };
        assert_eq!(decode(&encode(zero, b"")).unwrap().payload, b"");
        let mut other = b.clone();
        other[0] = 0;
        assert!(decode(&other).is_err());
        assert!(decode(&b[..5]).is_err());
    }
}
