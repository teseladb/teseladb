//! The stream of one range (014 FR-003, FR-010): it starts after a mark, catches up with the
//! versions already in the LSM (`scan_time`) while it keeps the versions applied meanwhile, and
//! then goes live. A version reaches it once whichever way it comes: the versions applied while it
//! catches up may also be in the scan, and the ones at or below its start never leave.

use std::collections::BTreeSet;

use teseladb_hlc::Hlc;

/// A committed version of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// Key of the document.
    pub key: Vec<u8>,
    /// Commit mark.
    pub ts: Hlc,
    /// The document, or `None` for a deletion.
    pub value: Option<Vec<u8>>,
}

/// Where the stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Reading the versions already in the LSM.
    CatchUp,
    /// Taking the versions as they are applied.
    Live,
}

/// The stream of one range.
#[derive(Debug)]
pub struct RangeFeed {
    after: Hlc,
    mode: Mode,
    /// Versions applied while it catches up.
    held: Vec<Version>,
    /// What the scan gave, so a version applied meanwhile does not leave twice.
    seen: BTreeSet<(Vec<u8>, Hlc)>,
    /// The highest mark of a version out.
    last: Hlc,
}

impl RangeFeed {
    /// A stream after mark `after`, catching up.
    pub fn new(after: Hlc) -> RangeFeed {
        RangeFeed {
            after,
            mode: Mode::CatchUp,
            held: Vec::new(),
            seen: BTreeSet::new(),
            last: after,
        }
    }

    /// Where it is.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The highest mark of a version it let out (its start before any): a stream reopened after a
    /// split or a move of its range starts there (FR-010).
    pub fn last(&self) -> Hlc {
        self.last
    }

    /// Versions the scan of the LSM found: the ones above its start leave now.
    pub fn scanned(&mut self, versions: impl IntoIterator<Item = Version>) -> Vec<Version> {
        let out: Vec<Version> = versions.into_iter().filter(|v| v.ts > self.after).collect();
        for v in &out {
            self.seen.insert((v.key.clone(), v.ts));
            self.last = self.last.max(v.ts);
        }
        out
    }

    /// A version applied on the range: it leaves now if the stream is live, or waits for the end of
    /// the scan.
    pub fn applied(&mut self, v: Version) -> Option<Version> {
        if v.ts <= self.after {
            return None;
        }
        match self.mode {
            Mode::CatchUp => {
                self.held.push(v);
                None
            }
            Mode::Live => {
                self.last = self.last.max(v.ts);
                Some(v)
            }
        }
    }

    /// The scan ended: the versions applied meanwhile that it did not give leave, and the stream
    /// goes live.
    pub fn caught_up(&mut self) -> Vec<Version> {
        self.mode = Mode::Live;
        let held = std::mem::take(&mut self.held);
        let seen = std::mem::take(&mut self.seen);
        let out: Vec<Version> =
            held.into_iter().filter(|v| !seen.contains(&(v.key.clone(), v.ts))).collect();
        for v in &out {
            self.last = self.last.max(v.ts);
        }
        out
    }
}
