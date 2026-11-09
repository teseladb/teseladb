//! Reads that meet intents (008 FR-003, FR-005, contract `txn-protocol.md`, «Reglas en el
//! rango»). A read at mark t that finds an intent at or below t does not wait: it needs the state
//! of the intent's transaction, which its record in another range may hold. The reader carries
//! what it learned ([`TxnView`]): the transactions it pushed above t (it reads the version below
//! their intents), those it found committed (it reads the intent's value if the commit is at or
//! below t) and those it found aborted. An intent of a transaction it knows nothing about stops
//! the read with the intent's owner: the layer above pushes it with `PushTxn(succ(t))` and reads
//! again, and resolves the intents of a finished one in the background (lazy resolution). A
//! transaction reads its own intents, whatever their mark.

use std::cell::RefCell;
use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_lsm::{BackScan, Found, Kind, Lookup, Lsm, Scan, Step, TimeScan};

use crate::intents::{Intent, IntentOp, IntentOwner, TxnId, pred};
use crate::sm::split_value;

/// The newest mark: a transaction looks for its own intents at it.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

/// What a reader knows about a transaction whose intent it met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Known {
    /// Pending, pushed above the read: it commits above it, if ever.
    Pushed,
    /// Committed at this mark.
    Committed(Hlc),
    /// Aborted.
    Aborted,
}

/// What a read knows about transactions: its own and the ones it pushed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxnView {
    /// The reader's own transaction, whose intents it reads.
    pub own: Option<TxnId>,
    /// The transactions it learned about.
    pub known: BTreeMap<TxnId, Known>,
}

/// What a read does with an intent it met at or below its mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The intent's value is the visible version.
    Value(IntentOp),
    /// The visible version is below the intent.
    Below(Hlc),
    /// Its transaction has to be pushed first.
    Push(IntentOwner),
    /// Its transaction committed at this mark, above the read and inside its uncertainty window
    /// (008 FR-008): the commit may have been acknowledged before the read began.
    Uncertain(Hlc),
}

/// Why a visible version could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFault {
    /// An intent of a transaction the reader knows nothing about.
    Intent(IntentOwner),
    /// The LSM failed, or an intent does not decode.
    Unavailable,
    /// A version above the read's mark and inside its uncertainty window, at this mark (008
    /// FR-008): it may have been written before the read began, so the read goes again above it.
    Uncertain(Hlc),
}

impl TxnView {
    /// What a read at `read_ts`, with its uncertainty window up to `uncertain_to`, does with
    /// intent `intent` found on `key` at `at`.
    pub fn decide(
        &self,
        key: &[u8],
        intent: Intent,
        at: Hlc,
        (read_ts, uncertain_to): (Hlc, Option<Hlc>),
    ) -> Decision {
        if self.own == Some(intent.txn) {
            return Decision::Value(intent.op);
        }
        // Faulty variant (008 T024): a reader takes an intent it knows nothing about for one of an
        // aborted transaction and reads below it without pushing, missing a commit at or below
        // its mark that nobody resolved yet.
        #[cfg(teseladb_bug = "reader_ignores_intent")]
        if !self.known.contains_key(&intent.txn) {
            return Decision::Below(at);
        }
        match self.known.get(&intent.txn) {
            Some(Known::Committed(c)) if *c <= read_ts => Decision::Value(intent.op),
            Some(Known::Committed(c)) if uncertain_to.is_some_and(|u| *c <= u) => {
                Decision::Uncertain(*c)
            }
            Some(Known::Committed(_) | Known::Pushed | Known::Aborted) => Decision::Below(at),
            None => Decision::Push(IntentOwner {
                txn: intent.txn,
                record_key: intent.record_key,
                key: key.to_vec(),
                ts: at,
            }),
        }
    }
}

/// A stored version as a document: `(type, value)`, `None` for a deletion.
pub type Visible = Option<(u8, Vec<u8>)>;

fn op_value(op: IntentOp) -> Visible {
    match op {
        IntentOp::Put { value_type, value } => Some((value_type, value)),
        IntentOp::Delete => None,
    }
}

/// One lookup of `key` at `at`: `None` while a block is being read.
fn lookup(lsm: &RefCell<Lsm>, key: &[u8], at: Hlc) -> Option<Result<Option<Found>, ReadFault>> {
    let view = lsm.borrow().view();
    let found = view.get(&mut lsm.borrow_mut(), key, at);
    match found {
        Lookup::Hit(f) => Some(Ok(Some(f))),
        Lookup::NotFound => Some(Ok(None)),
        Lookup::Miss(_) => None,
        Lookup::Failed(_) => Some(Err(ReadFault::Unavailable)),
    }
}

/// What a version of `key` found by a read at `read_ts` with its uncertainty window up to
/// `uncertain_to` gives: the document, or the mark to read below.
pub fn resolve_found(
    view: &TxnView,
    key: &[u8],
    f: Found,
    mark: (Hlc, Option<Hlc>),
) -> Result<Result<Visible, Hlc>, ReadFault> {
    if f.kind != Kind::Intent {
        return Ok(Ok(split_value(&f.value).map(|(t, v)| (t, v.to_vec()))));
    }
    let intent = Intent::decode(&f.value).map_err(|_| ReadFault::Unavailable)?;
    match view.decide(key, intent, f.ts, mark) {
        Decision::Value(op) => Ok(Ok(op_value(op))),
        Decision::Below(at) => Ok(Err(pred(at))),
        Decision::Push(owner) => Err(ReadFault::Intent(owner)),
        Decision::Uncertain(c) => Err(ReadFault::Uncertain(c)),
    }
}

/// The visible version of `key` at `read_ts` for a reader with `view`: `None` while a block is
/// being read (the caller runs the I/O and calls again).
pub fn read_visible(
    lsm: &RefCell<Lsm>,
    key: &[u8],
    (read_ts, uncertain_to): (Hlc, Option<Hlc>),
    view: &TxnView,
) -> Option<Result<Visible, ReadFault>> {
    if let Some(own) = view.own {
        // Its own intent, at whatever mark it was written.
        match lookup(lsm, key, NEWEST)? {
            Ok(Some(f)) if f.kind == Kind::Intent => match Intent::decode(&f.value) {
                Ok(i) if i.txn == own => return Some(Ok(op_value(i.op))),
                Ok(_) => {}
                Err(_) => return Some(Err(ReadFault::Unavailable)),
            },
            Ok(_) => {}
            Err(e) => return Some(Err(e)),
        }
    }
    read_from(lsm, key, read_ts, (read_ts, uncertain_to), view)
}

/// The visible version of `key` for a read at `read_ts`, looking from mark `at` down.
fn read_from(
    lsm: &RefCell<Lsm>,
    key: &[u8],
    mut at: Hlc,
    mark: (Hlc, Option<Hlc>),
    view: &TxnView,
) -> Option<Result<Visible, ReadFault>> {
    loop {
        let found = match lookup(lsm, key, at)? {
            Ok(Some(f)) => f,
            Ok(None) => return Some(Ok(None)),
            Err(e) => return Some(Err(e)),
        };
        match resolve_found(view, key, found, mark) {
            Ok(Ok(v)) => return Some(Ok(v)),
            // Below an intent there is no other one: intents of other transactions conflict.
            Ok(Err(below)) => at = below,
            Err(e) => return Some(Err(e)),
        }
    }
}

/// A document a scan returns: its key and its `(value type, value)`.
pub type Row = (Vec<u8>, (u8, Vec<u8>));

/// The intents of the reader's own transaction in a scan's interval, above its read mark.
struct OwnScan {
    txn: TxnId,
    scan: TimeScan,
    next: Option<(Vec<u8>, Visible)>,
    done: bool,
    /// A backward scan's intents, read whole and taken from the last one (a transaction has few).
    rest: Vec<(Vec<u8>, Visible)>,
}

/// The LSM's scan of a [`ScanState`], in key order or from the last key down (H-003).
enum MainScan {
    Forward(Scan),
    Backward(BackScan),
}

impl MainScan {
    fn next(&mut self, lsm: &mut Lsm) -> Step {
        match self {
            MainScan::Forward(s) => s.next(lsm),
            MainScan::Backward(s) => s.next(lsm),
        }
    }
}

/// The keys of `[start, end)` that hold an intent of `txn`, in key order, at most `limit` of them
/// (the reaper's search of an interval it cannot list key by key, 008 record GC); `None` while a
/// block is being read.
pub fn intent_keys(
    lsm: &RefCell<Lsm>,
    (start, end): (&[u8], &[u8]),
    txn: TxnId,
    limit: usize,
) -> Option<Result<Vec<Vec<u8>>, ReadFault>> {
    let zero = Hlc { physical: 0, logical: 0 };
    let mut scan = lsm.borrow().view().scan_time(start, end, zero, NEWEST);
    let mut keys: Vec<Vec<u8>> = Vec::new();
    while keys.len() < limit {
        match scan.next(&mut lsm.borrow_mut()) {
            Step::Item(i) if i.kind == Kind::Intent => match Intent::decode(&i.value) {
                Ok(intent) if intent.txn == txn && keys.last() != Some(&i.key) => keys.push(i.key),
                Ok(_) => {}
                Err(_) => return Some(Err(ReadFault::Unavailable)),
            },
            Step::Item(_) => {}
            Step::Done => break,
            Step::Miss(_) => return None,
            Step::Failed(_) => return Some(Err(ReadFault::Unavailable)),
        }
    }
    Some(Ok(keys))
}

/// A scan in progress for a reader with a [`TxnView`]: the LSM's scan at the read mark, where
/// each intent is decided as a point read is, merged with the reader's own intents above the mark.
pub struct ScanState {
    main: MainScan,
    /// From the last key down (H-003).
    backward: bool,
    read_ts: Hlc,
    /// The end of its uncertainty window.
    uncertain_to: Option<Hlc>,
    own: Option<OwnScan>,
    /// The main scan's next document, decided.
    main_next: Option<(Vec<u8>, Visible)>,
    main_done: bool,
    /// A key of the main scan whose visible version is below an intent: `(key, mark)`.
    below: Option<(Vec<u8>, Hlc)>,
    rows: Vec<Row>,
}

impl ScanState {
    /// The scan's mark.
    pub fn read_ts(&self) -> Hlc {
        self.read_ts
    }

    /// A scan of `[start, end)` at `read_ts`, with its uncertainty window up to `uncertain_to`,
    /// for a reader with `view`; `backward` reads it from the last key down (H-003).
    pub fn new(
        lsm: &RefCell<Lsm>,
        (start, end): (&[u8], &[u8]),
        (read_ts, uncertain_to): (Hlc, Option<Hlc>),
        view: &TxnView,
        backward: bool,
    ) -> ScanState {
        let v = lsm.borrow().view();
        let own = view.own.map(|txn| OwnScan {
            txn,
            scan: v.scan_time(start, end, read_ts, NEWEST),
            next: None,
            done: false,
            rest: Vec::new(),
        });
        let main = match backward {
            true => MainScan::Backward(v.scan_back(start, end, read_ts)),
            false => MainScan::Forward(v.scan(start, end, read_ts)),
        };
        ScanState {
            main,
            backward,
            read_ts,
            uncertain_to,
            own,
            main_next: None,
            main_done: false,
            below: None,
            rows: Vec::new(),
        }
    }

    /// Fills `main_next`: `Some(Ok(()))` once it is there or the scan ended, `None` while a block
    /// is being read.
    fn fill_main(&mut self, lsm: &RefCell<Lsm>, view: &TxnView) -> Option<Result<(), ReadFault>> {
        while self.main_next.is_none() && !self.main_done {
            if let Some((key, at)) = self.below.clone() {
                match read_from(lsm, &key, at, (self.read_ts, self.uncertain_to), view)? {
                    Ok(v) => self.main_next = Some((key, v)),
                    Err(e) => return Some(Err(e)),
                }
                self.below = None;
                continue;
            }
            let step = self.main.next(&mut lsm.borrow_mut());
            match step {
                Step::Item(i) => {
                    let found = Found { ts: i.ts, kind: i.kind, value: i.value };
                    match resolve_found(view, &i.key, found, (self.read_ts, self.uncertain_to)) {
                        Ok(Ok(v)) => self.main_next = Some((i.key, v)),
                        Ok(Err(below)) => self.below = Some((i.key, below)),
                        Err(e) => return Some(Err(e)),
                    }
                }
                Step::Done => self.main_done = true,
                Step::Miss(_) => return None,
                Step::Failed(_) => return Some(Err(ReadFault::Unavailable)),
            }
        }
        Some(Ok(()))
    }

    /// Fills the next own intent, if the reader has a transaction: the next in key order, or for
    /// a backward scan the last one not taken, once all of them are read.
    fn fill_own(&mut self, lsm: &RefCell<Lsm>) -> Option<Result<(), ReadFault>> {
        let backward = self.backward;
        let Some(o) = self.own.as_mut() else { return Some(Ok(())) };
        while (backward || o.next.is_none()) && !o.done {
            let step = o.scan.next(&mut lsm.borrow_mut());
            match step {
                Step::Item(i) if i.kind == Kind::Intent => match Intent::decode(&i.value) {
                    Ok(intent) if intent.txn == o.txn => {
                        let found = (i.key, op_value(intent.op));
                        match backward {
                            true => o.rest.push(found),
                            false => o.next = Some(found),
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return Some(Err(ReadFault::Unavailable)),
                },
                Step::Item(_) => {}
                Step::Done => o.done = true,
                Step::Miss(_) => return None,
                Step::Failed(_) => return Some(Err(ReadFault::Unavailable)),
            }
        }
        if backward && o.next.is_none() {
            o.next = o.rest.pop();
        }
        Some(Ok(()))
    }

    /// Moves the scan on until it has `limit` documents or the interval ends (the documents), or
    /// a block is being read (`None`).
    pub fn step(
        &mut self,
        lsm: &RefCell<Lsm>,
        view: &TxnView,
        limit: usize,
    ) -> Option<Result<Vec<Row>, ReadFault>> {
        while self.rows.len() < limit {
            if let Err(e) = self.fill_main(lsm, view)? {
                return Some(Err(e));
            }
            if let Err(e) = self.fill_own(lsm)? {
                return Some(Err(e));
            }
            let own_next = self.own.as_mut().and_then(|o| o.next.take());
            let (key, visible) = match (self.main_next.take(), own_next) {
                (None, None) => break,
                (Some(m), None) => m,
                (None, Some(o)) => o,
                (Some(m), Some(o)) => {
                    // The reader's own intent wins over what the main scan saw on its key; the
                    // one that comes first in the scan's direction goes first.
                    let first = |a: &[u8], b: &[u8]| match self.backward {
                        true => a > b,
                        false => a < b,
                    };
                    if o.0 == m.0 || first(&o.0, &m.0) {
                        if o.0 != m.0 {
                            self.main_next = Some(m);
                        }
                        o
                    } else {
                        if let Some(own) = self.own.as_mut() {
                            own.next = Some(o);
                        }
                        m
                    }
                }
            };
            // A version that only serves the history (014) is no document.
            if let Some(v) = visible.filter(|(t, _)| *t != crate::HISTORY_VALUE) {
                self.rows.push((key, v));
            }
        }
        Some(Ok(std::mem::take(&mut self.rows)))
    }
}

/// The check of a read's uncertainty window (008 FR-008): a version of another writer in `(mark,
/// limit]` of the read's interval means the read has to go again above it.
pub struct Window {
    scan: TimeScan,
    own: Option<TxnId>,
    max: Option<Hlc>,
}

impl Window {
    /// The window `(mark, limit]` of `[start, end)` for a reader whose own transaction is `own`.
    pub fn new(
        lsm: &RefCell<Lsm>,
        (start, end): (&[u8], &[u8]),
        (mark, limit): (Hlc, Hlc),
        own: Option<TxnId>,
    ) -> Window {
        let scan = lsm.borrow().view().scan_time(start, end, mark, limit);
        Window { scan, own, max: None }
    }

    /// The newest mark of another writer's version in the window (`None`: none), once the window
    /// is read; `None` while a block is being read.
    pub fn step(&mut self, lsm: &RefCell<Lsm>) -> Option<Result<Option<Hlc>, ReadFault>> {
        loop {
            let step = self.scan.next(&mut lsm.borrow_mut());
            match step {
                Step::Item(i) => {
                    let own = i.kind == Kind::Intent
                        && Intent::decode(&i.value).is_ok_and(|x| Some(x.txn) == self.own);
                    if !own {
                        self.max = Some(self.max.map_or(i.ts, |m| m.max(i.ts)));
                    }
                }
                Step::Done => return Some(Ok(self.max)),
                Step::Miss(_) => return None,
                Step::Failed(_) => return Some(Err(ReadFault::Unavailable)),
            }
        }
    }
}
