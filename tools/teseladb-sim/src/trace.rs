//! Trace of a simulation: every processed event is summarized and accumulated into a SHA-256.
//! With `keep` the lines are kept too, and the CLI writes them to
//! `.sim-artifacts/<seed>/trace.log` (the simulator does no I/O).

use teseladb_hash::Sha256;
use teseladb_types::Nanos;

/// Trace in progress.
#[derive(Debug)]
pub struct Trace {
    hasher: Sha256,
    lines: Option<Vec<String>>,
    events: u64,
}

impl Trace {
    /// Empty trace; `keep` keeps the lines besides the hash.
    pub fn new(keep: bool) -> Trace {
        Trace { hasher: Sha256::new(), lines: keep.then(Vec::new), events: 0 }
    }

    /// Records an event.
    pub fn record(&mut self, now: Nanos, seq: u64, kind: &str, node: u32, summary: &str) {
        self.events += 1;
        let mut buf = Vec::with_capacity(32 + kind.len() + summary.len());
        buf.extend_from_slice(&now.0.to_le_bytes());
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.extend_from_slice(&node.to_le_bytes());
        buf.extend_from_slice(kind.as_bytes());
        buf.push(0);
        buf.extend_from_slice(summary.as_bytes());
        buf.push(b'\n');
        self.hasher.update(&buf);
        if let Some(lines) = &mut self.lines {
            lines.push(format!("{:>14} {seq:>9} n{node} {kind} {summary}", now.0));
        }
    }

    /// Recorded events.
    pub fn events(&self) -> u64 {
        self.events
    }

    /// Kept lines (if requested).
    pub fn lines(&self) -> Option<&[String]> {
        self.lines.as_deref()
    }

    /// Hash final.
    pub fn finish(self) -> ([u8; 32], Option<Vec<String>>) {
        (self.hasher.finalize(), self.lines)
    }
}

/// Hash in hexadecimal (the first 16 bytes are enough to identify a trace in a report).
pub fn short_hex(hash: &[u8; 32]) -> String {
    hash[..16].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn same_events_same_hash() {
        let run = |keep: bool, last: &str| {
            let mut t = Trace::new(keep);
            t.record(Nanos(1), 1, "tick", 0, "a");
            t.record(Nanos(2), 2, "disk", 1, last);
            assert_eq!(t.events(), 2);
            assert_eq!(t.lines().is_some(), keep);
            t.finish()
        };
        let (a, lines) = run(true, "b");
        let (b, none) = run(false, "b");
        let (c, _) = run(false, "c");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(none.is_none());
        assert_eq!(lines.unwrap().len(), 2);
        assert_eq!(short_hex(&a).len(), 32);
    }
}
