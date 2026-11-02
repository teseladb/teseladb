//! Write fences of a range (018 FR-002, `spec/tla/Reshard.tla`'s `blockedW`): the spans of
//! collections a resharding holds in its critical section. A [`crate::command::Command::Fence`]
//! adds or lifts one; from that entry on, a `Write` or `PutIntents` with a key in a fenced span
//! is refused at every replica ([`crate::sm::RangeOutput::Fenced`]) and the leader refuses it
//! before proposing it. The resolution of intents laid before the fence still applies: their
//! transaction was decided.
//!
//! A range keeps its fences in a local key, in its snapshots, and hands the part past a cut to
//! the range a split creates; a merge takes the right range's fences too.

use teseladb_types::{DecodeError, Decoder, Encoder};

/// A fenced span `[start, end)` of the data space.
pub type Span = (Vec<u8>, Vec<u8>);

/// Whether `key` is in one of `fences`.
pub fn covers(fences: &[Span], key: &[u8]) -> bool {
    fences.iter().any(|(s, e)| s.as_slice() <= key && key < e.as_slice())
}

/// `fences` with `span` added (`on`) or lifted; the same span twice is one fence.
pub fn with(fences: &[Span], span: &Span, on: bool) -> Vec<Span> {
    let mut out: Vec<Span> = fences.iter().filter(|f| *f != span).cloned().collect();
    if on {
        out.push(span.clone());
        out.sort();
    }
    out
}

/// The parts of `fences` inside `[start, end)` (an empty `end`: no end).
pub fn clip(fences: &[Span], start: &[u8], end: &[u8]) -> Vec<Span> {
    fences
        .iter()
        .filter_map(|(s, e)| {
            let s = s.as_slice().max(start).to_vec();
            let e = if end.is_empty() { e.clone() } else { e.as_slice().min(end).to_vec() };
            (s < e).then_some((s, e))
        })
        .collect()
}

/// Writes `fences` with `e`.
pub fn put(e: &mut Encoder, fences: &[Span]) {
    e.var(fences.len() as u64);
    for (s, end) in fences {
        e.bytes(s).bytes(end);
    }
}

/// Reads fences [`put`] wrote.
pub fn get(d: &mut Decoder<'_>) -> Result<Vec<Span>, DecodeError> {
    let n = d.var()?;
    let mut out = Vec::new();
    for _ in 0..n {
        out.push((d.bytes()?.to_vec(), d.bytes()?.to_vec()));
    }
    Ok(out)
}

/// The value of a range's fences key.
pub fn encode(fences: &[Span]) -> Vec<u8> {
    let mut e = Encoder::new();
    put(&mut e, fences);
    e.finish()
}

/// Fences from the value [`encode`] gave.
pub fn decode(v: &[u8]) -> Result<Vec<Span>, DecodeError> {
    let mut d = Decoder::new(v);
    let out = get(&mut d)?;
    d.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(s: &[u8], e: &[u8]) -> Span {
        (s.to_vec(), e.to_vec())
    }

    #[test]
    fn adds_lifts_and_clips() {
        let a = span(b"\x01b", b"\x01d");
        let f = with(&[], &a, true);
        assert_eq!(with(&f, &a, true), f);
        assert!(covers(&f, b"\x01c") && !covers(&f, b"\x01d") && !covers(&f, b"\x01a"));
        assert!(with(&f, &a, false).is_empty());
        assert_eq!(clip(&f, b"\x01c", b"\x01z"), vec![span(b"\x01c", b"\x01d")]);
        assert_eq!(clip(&f, b"\x01a", b"\x01c"), vec![span(b"\x01b", b"\x01c")]);
        assert_eq!(clip(&f, b"\x01a", b""), f);
        assert!(clip(&f, b"\x01d", b"\x01z").is_empty());
        assert_eq!(decode(&encode(&f)), Ok(f));
        assert!(decode(&[1]).is_err());
    }
}
