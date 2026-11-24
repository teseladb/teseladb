//! The entries of an index scan read a pull at a time (023 FR-012): a scan of key intervals keeps
//! where it is in each interval and reads the next entries only when its caller asks for them, so
//! a scan of a whole index holds one pull of entries at a time instead of every entry of its
//! intervals, and a read that stops early (a limit, a trial's budget) reads no further. Between
//! two pulls the caller may give the shard to other work. The entries come out in the order and
//! with the values the whole read gave before.

use std::collections::VecDeque;

use teseladb_bson::Value;
use teseladb_keys::{DOCUMENT_BUCKETS, DOCUMENTS, KeyBuilder, KeyRange, values_len};
use teseladb_meta::catalog::CollectionEntry;
use teseladb_plan::checker::{Visit, Walker};
use teseladb_range::read::ReadConcern;

use super::super::{Backend, Span};
use super::{Entry, KEY_CHUNK, PlanIndex, any_entry, given_document_id, in_bucket, index_span};
use crate::error::{CommandError, INTERNAL_ERROR};

/// The most entries one pull reads.
pub(super) const MOST_PULLED: usize = 4 * KEY_CHUNK;

/// An entry a scan examined: the entry within its intervals, or `None` outside them, and whether
/// the scan seeks after it (a key outside the intervals of a scan that checks its keys).
type Examined = (Option<Entry>, bool);

/// The entries a scan examines, in its order, read as its caller takes them: those already read
/// (`ready`) and what is left to read. `None` is an entry outside the scan's intervals.
pub(super) struct Scanned<'a> {
    ready: VecDeque<Examined>,
    rest: Option<Rest<'a>>,
    /// The seeks of the entries taken, the first one included.
    seeks: u64,
    /// Whether it reads key intervals, which a scan that stops after an entry goes on with from
    /// that entry (010 FR-035).
    resumable: bool,
}

/// What a scan has left to read.
enum Rest<'a> {
    Intervals(Intervals<'a>),
    Walked(Walked<'a>),
}

impl<'a> Scanned<'a> {
    /// A scan whose entries were all read, with its seeks.
    pub(super) fn listed(entries: Vec<Option<Entry>>, seeks: u64) -> Scanned<'a> {
        let ready = entries.into_iter().map(|e| (e, false)).collect();
        Scanned { ready, rest: None, seeks, resumable: false }
    }

    /// A scan of what `rest` reads, which reads nothing until it is pulled.
    fn reading(rest: Rest<'a>) -> Scanned<'a> {
        let resumable = matches!(rest, Rest::Intervals(_));
        Scanned { ready: VecDeque::new(), rest: Some(rest), seeks: 1, resumable }
    }

    /// Whether it reads key intervals, which a scan that stops after an entry goes on with from
    /// that entry's key ([`Entry::at`]).
    pub(super) fn resumable(&self) -> bool {
        self.resumable
    }

    /// The entries read and not taken yet.
    pub(super) fn ready_len(&self) -> usize {
        self.ready.len()
    }

    /// Whether every entry of the scan was read.
    pub(super) fn read_all(&self) -> bool {
        match &self.rest {
            None => true,
            Some(Rest::Intervals(r)) => r.ended,
            Some(Rest::Walked(w)) => w.ended,
        }
    }

    /// The seeks of the entries taken so far, the first one included: a scan that checks its
    /// keys seeks once more after each key outside its intervals (H-020 of the native tests).
    pub(super) fn seeks(&self) -> u64 {
        self.seeks
    }

    /// The primary keys of the entries read and not taken yet whose document the scan did not
    /// read with them, in its order.
    pub(super) fn ready_pending(&self) -> Vec<Vec<u8>> {
        pending_of(self.ready.iter())
    }

    /// Reads at most `want` more entries; gives the primary keys of those without their document,
    /// as [`Scanned::ready_pending`].
    pub(super) fn pull<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        want: usize,
    ) -> Result<Vec<Vec<u8>>, CommandError> {
        let want = want.max(1);
        let read = match self.rest.as_mut() {
            None => return Ok(Vec::new()),
            Some(Rest::Intervals(r)) => {
                r.next_entries(backend, want)?.into_iter().map(|e| (e, false)).collect()
            }
            Some(Rest::Walked(w)) => w.next_entries(backend, want)?,
        };
        let pending = pending_of(read.iter());
        self.ready.extend(read);
        Ok(pending)
    }

    /// The next entry, if one was read.
    pub(super) fn take(&mut self) -> Option<Option<Entry>> {
        let (entry, seeks) = self.ready.pop_front()?;
        self.seeks += u64::from(seeks);
        Some(entry)
    }

    /// Every entry of the scan, read to its end.
    pub(super) fn all<B: Backend + ?Sized>(
        mut self,
        backend: &mut B,
    ) -> Result<Vec<Option<Entry>>, CommandError> {
        while !self.read_all() {
            self.pull(backend, MOST_PULLED)?;
        }
        Ok(self.ready.into_iter().map(|(e, _)| e).collect())
    }
}

fn pending_of<'e>(entries: impl Iterator<Item = &'e Examined>) -> Vec<Vec<u8>> {
    entries
        .filter_map(|(e, _)| e.as_ref())
        .filter(|e| e.doc.is_none())
        .map(|e| e.pk.clone())
        .collect()
}

/// A scan that checks its keys, read as its caller pulls it: the entries of `cursor` from its
/// scan's first interval to its last, each walked by `walker` as it is read, the reference
/// server's index bounds checker ([`teseladb_plan::checker`]); the walk needs no key after the
/// one it is at. When the walk runs out of keys, the scan examines the next key of the index in
/// `after`, which is outside the leading field's intervals. With `decode`, the entries it gives
/// keep their values.
pub(super) fn walked<'a>(
    cursor: RangeCursor<'a>,
    walker: Walker,
    decode: bool,
    after: (Vec<u8>, Vec<u8>),
) -> Scanned<'a> {
    let ended = walker.done();
    Scanned::reading(Rest::Walked(Walked { cursor, walker, decode, after, at: 0, ended }))
}

struct Walked<'a> {
    cursor: RangeCursor<'a>,
    walker: Walker,
    decode: bool,
    after: (Vec<u8>, Vec<u8>),
    /// The keys read so far.
    at: usize,
    ended: bool,
}

impl Walked<'_> {
    /// The next entries the scan examines, at least `want` unless it ends.
    fn next_entries<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        want: usize,
    ) -> Result<Vec<Examined>, CommandError> {
        let mut out = Vec::new();
        let mut visits = Vec::new();
        while !self.ended && out.len() < want {
            if self.walker.done() {
                self.ended = true;
                break;
            }
            if self.cursor.done {
                let (from, to) = &self.after;
                let (index, tenant, concern) =
                    (self.cursor.index, self.cursor.coll.0, self.cursor.concern);
                if from < to && any_entry(backend, tenant, index, (from, to), concern)? {
                    out.push((None, false));
                }
                self.ended = true;
                break;
            }
            for mut entry in self.cursor.next_entries(backend, want - out.len())? {
                if self.walker.done() {
                    break;
                }
                // The walk's values move out of the entry unless the read gives them back.
                let values = match self.decode {
                    true => entry.values.clone(),
                    false => entry.values.take(),
                };
                let seeks = self.walker.seeks();
                visits.clear();
                self.walker.step(&values.unwrap_or_default(), self.at, &mut visits);
                self.at += 1;
                // Each key outside the intervals but the one that ends the walk makes it seek.
                let mut seeking = self.walker.seeks() - seeks;
                let mut entry = Some(entry);
                for v in &visits {
                    match v {
                        Visit::Within(_) => out.push((entry.take(), false)),
                        Visit::Outside(_) => {
                            out.push((None, seeking > 0));
                            seeking = seeking.saturating_sub(1);
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// A scan of the key intervals `ranges` of `index`, read as its caller pulls it: the intervals in
/// the scan's order, and, between them when there are several, whether the stretch up to the
/// next interval (or the index's end) holds a key, which the scan examines as one entry outside
/// its intervals (measured on the reference router 9.0.4: `_id` in `[2, 5, 7]` over `_id` 1 to 8
/// examines 6 keys).
pub(super) fn intervals<'a>(
    (tenant, collection): (u32, u32),
    index: &'a PlanIndex,
    ranges: Vec<KeyRange>,
    (concern, decode): (ReadConcern, bool),
    backward: bool,
) -> Scanned<'a> {
    let order: Vec<usize> = match backward {
        true => (0..ranges.len()).rev().collect(),
        false => (0..ranges.len()).collect(),
    };
    let ended = ranges.is_empty();
    Scanned::reading(Rest::Intervals(Intervals {
        coll: (tenant, collection),
        index,
        ranges,
        order,
        step: 0,
        current: None,
        read: (concern, decode),
        backward,
        ended,
    }))
}

struct Intervals<'a> {
    coll: (u32, u32),
    index: &'a PlanIndex,
    ranges: Vec<KeyRange>,
    /// The intervals in the scan's order.
    order: Vec<usize>,
    /// Where the scan is in `order`.
    step: usize,
    /// The interval being read.
    current: Option<RangeCursor<'a>>,
    read: (ReadConcern, bool),
    backward: bool,
    ended: bool,
}

impl Intervals<'_> {
    fn next_entries<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        want: usize,
    ) -> Result<Vec<Option<Entry>>, CommandError> {
        let mut out = Vec::new();
        while !self.ended && out.len() < want {
            let Some(&i) = self.order.get(self.step) else {
                self.ended = true;
                break;
            };
            let (index, coll, read, backward) = (self.index, self.coll, self.read, self.backward);
            let r = &self.ranges[i];
            let cursor = self
                .current
                .get_or_insert_with(|| RangeCursor::open(coll, index, r, read, backward));
            out.extend(cursor.next_entries(backend, want - out.len())?.into_iter().map(Some));
            if !cursor.done {
                continue;
            }
            self.current = None;
            if self.ranges.len() > 1 && self.past_interval(backend)? {
                out.push(None);
            }
            self.step += 1;
        }
        Ok(out)
    }

    /// Whether the stretch between the interval at `step` and the next one the scan reads, or the
    /// index's end, holds a key.
    fn past_interval<B: Backend + ?Sized>(&self, backend: &mut B) -> Result<bool, CommandError> {
        let (first, past) = index_span(self.coll, self.index);
        let end_of = |r: &KeyRange| r.end.clone().unwrap_or_else(|| past.clone());
        let i = self.order[self.step];
        let next = self.order.get(self.step + 1).copied();
        let ranges = &self.ranges;
        let (from, to) = match (self.backward, next) {
            (false, Some(n)) => (end_of(&ranges[i]), ranges[n].start.clone()),
            (false, None) => (end_of(&ranges[i]), past.clone()),
            (true, Some(n)) => (end_of(&ranges[n]), ranges[i].start.clone()),
            (true, None) => (first, ranges[i].start.clone()),
        };
        Ok(from < to && any_entry(backend, self.coll.0, self.index, (&from, &to), self.read.0)?)
    }
}

/// One key interval of an index, read a pull at a time in the scan's direction.
pub(super) struct RangeCursor<'a> {
    index: &'a PlanIndex,
    coll: (u32, u32),
    concern: ReadConcern,
    /// Whether each entry's values are decoded.
    decode: bool,
    backward: bool,
    way: Way,
    /// Whether the interval was read to its end.
    done: bool,
}

/// How an interval is read.
enum Way {
    /// The index of `_id` of a collection in `_id` order (015 FR-009, H-027): its entries are the
    /// documents' keys, read as an interval; `[lo, hi)` is what is left.
    Ordered { head: usize, lo: Vec<u8>, hi: Vec<u8> },
    /// One value of the index of `_id`: its document, read as a point (H-013).
    Point { pk: Vec<u8> },
    /// The index of `_id` of a collection in hash buckets: the buckets are merged as sorted lists
    /// ([`Lane`]), opened at the first pull.
    Buckets { from: Vec<u8>, to: Vec<u8>, lanes: Option<Vec<Lane>> },
    /// Any other index: its keys in `[start, end)`; forward the read goes on from `from`,
    /// backward below `to` (H-003).
    Keys { head: usize, start: Vec<u8>, from: Vec<u8>, to: Vec<u8>, end: Vec<u8> },
}

impl<'a> RangeCursor<'a> {
    /// A cursor over interval `r` of `index` of collection `coll` (tenant and number).
    pub(super) fn open(
        coll: (u32, u32),
        index: &'a PlanIndex,
        r: &KeyRange,
        (concern, decode): (ReadConcern, bool),
        backward: bool,
    ) -> RangeCursor<'a> {
        let (_, past) = index_span(coll, index);
        let end = r.end.clone().unwrap_or(past);
        let docs = KeyBuilder::index_prefix(coll.0, coll.1, DOCUMENTS);
        let way = if index.ordered {
            Way::Ordered { head: docs.as_bytes().len(), lo: r.start.clone(), hi: end }
        } else if index.id_index {
            match super::point_pk(docs.as_bytes(), (&r.start, &end)) {
                Some(pk) => Way::Point { pk: pk.to_vec() },
                None => Way::Buckets { from: r.start.clone(), to: end, lanes: None },
            }
        } else {
            let head = KeyBuilder::index_prefix(coll.0, coll.1, index.entry.id).as_bytes().len();
            Way::Keys { head, start: r.start.clone(), from: r.start.clone(), to: end.clone(), end }
        };
        RangeCursor { index, coll, concern, decode, backward, way, done: false }
    }

    /// The next entries of the interval, at most `want`; fewer only at its end.
    pub(super) fn next_entries<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        want: usize,
    ) -> Result<Vec<Entry>, CommandError> {
        if self.done {
            return Ok(Vec::new());
        }
        let (coll, concern, backward) = (self.coll, self.concern, self.backward);
        let mut out = Vec::new();
        let done = match &mut self.way {
            Way::Ordered { head, lo, hi } => {
                ordered(backend, (*head, lo, hi), concern, (want, backward), &mut out)?
            }
            Way::Point { pk } => {
                point(backend, coll, pk, concern, &mut out)?;
                true
            }
            Way::Buckets { from, to, lanes } => {
                let lanes = match lanes {
                    Some(l) => l,
                    None => lanes.insert(open_lanes(
                        backend,
                        coll,
                        (from, to),
                        concern,
                        (want, backward),
                    )?),
                };
                merged(backend, coll, lanes, concern, (want, backward), &mut out)?
            }
            Way::Keys { head, start, from, to, end } => {
                let index = self.index;
                let decode = self.decode;
                let mut sink = |key: &[u8], value: &[u8]| {
                    out.push(key_entry(index, *head, (key, value), decode)?);
                    Ok(())
                };
                let way = (want, backward);
                keys(backend, (start, from, to, end), concern, way, &mut sink)?
            }
        };
        self.done = done;
        Ok(out)
    }
}

/// Reads at most `want` documents of `[lo, hi)` of a collection in `_id` order, a chunk at a time,
/// narrowing the interval to what is left; whether it reached the interval's end.
fn ordered<B: Backend + ?Sized>(
    backend: &mut B,
    (head, lo, hi): (usize, &mut Vec<u8>, &mut Vec<u8>),
    concern: ReadConcern,
    (want, backward): (usize, bool),
    out: &mut Vec<Entry>,
) -> Result<bool, CommandError> {
    while out.len() < want {
        let chunk = (want - out.len()).min(KEY_CHUNK);
        let rows = match backward {
            true => backend.scan_back(lo, hi, chunk, concern)?,
            false => backend.scan(lo, hi, chunk, concern)?,
        };
        let short = rows.len() < chunk;
        let last = rows.last().map(|(k, _)| k.clone());
        for (key, value) in rows {
            let pk = key.get(head..).unwrap_or_default().to_vec();
            let local = given_document_id(&value);
            out.push(Entry { pk, local, values: None, at: key.clone(), doc: Some((key, value)) });
        }
        match last {
            Some(k) if !short && backward => *hi = k,
            Some(k) if !short => *lo = [k.as_slice(), &[0]].concat(),
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// The document of one value of the index of `_id`, read as a point (H-013): a key that holds
/// every key starting with the value's encoding holds that value's own document alone, in the
/// bucket of its hash, since encodings are self-delimiting.
fn point<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    pk: &[u8],
    concern: ReadConcern,
    out: &mut Vec<Entry>,
) -> Result<(), CommandError> {
    let key = KeyBuilder::document_of_pk(tenant, collection, pk).as_bytes().to_vec();
    let found = backend.points(std::slice::from_ref(&key), concern)?;
    out.extend(found.into_iter().flatten().map(|v| Entry {
        pk: pk.to_vec(),
        local: given_document_id(&v),
        values: None,
        at: key.clone(),
        doc: Some((key.clone(), v)),
    }));
    Ok(())
}

/// Reads at most `want` keys of `[start, end)` of an index other than that of `_id`, a chunk at
/// a time; forward from `from`, backward below `to`, which it moves past what it read. Gives each
/// key and value to `sink`; whether it reached the interval's end.
fn keys<B: Backend + ?Sized>(
    backend: &mut B,
    (start, from, to, end): (&[u8], &mut Vec<u8>, &mut Vec<u8>, &[u8]),
    concern: ReadConcern,
    (want, backward): (usize, bool),
    sink: &mut dyn FnMut(&[u8], &[u8]) -> Result<(), CommandError>,
) -> Result<bool, CommandError> {
    let mut read = 0;
    while read < want {
        let chunk = (want - read).min(KEY_CHUNK);
        let rows = match backward {
            true => backend.scan_back(start, to, chunk, concern)?,
            false => backend.scan(from, end, chunk, concern)?,
        };
        let short = rows.len() < chunk;
        read += rows.len();
        for (key, value) in &rows {
            sink(key, value)?;
        }
        match rows.last() {
            Some((k, _)) if !short && backward => to.clone_from(k),
            Some((k, _)) if !short => *from = [k.as_slice(), &[0]].concat(),
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// The entry of an index other than that of `_id` stored at `key` with `value`; `head` is the
/// length of the index's prefix. With `decode`, its values; `None` when they do not decode (a
/// collation's).
fn key_entry(
    index: &PlanIndex,
    head: usize,
    (key, value): (&[u8], &[u8]),
    decode: bool,
) -> Result<Entry, CommandError> {
    let malformed = || CommandError::new(INTERNAL_ERROR, "an index entry that does not decode");
    let rest = key.get(head..).ok_or_else(malformed)?;
    let cut = head + values_len(rest, &index.dirs).map_err(|_| malformed())?;
    let values = match decode {
        true => index.values_of(&key[head..cut], value),
        false => None,
    };
    // A unique index keeps the primary key in the entry's value, not in its key.
    let pk = match index.catalog.unique {
        true => teseladb_keys::space::split_unique_value(value).map_err(|_| malformed())?.0,
        false => &key[cut..],
    };
    let local = teseladb_keys::local_id::entry_id(value)
        .filter(|id| teseladb_keys::local_id::is_given(*id));
    Ok(Entry { pk: pk.to_vec(), local, values, at: key.to_vec(), doc: None })
}

/// The entries each bucket of a read of the index of `_id` reads at a time for a pull of `want`:
/// each bucket holds about a sixteenth of what the pull gives, and two more cover an uneven
/// share; a full pull reads whole chunks.
fn per_bucket(want: usize) -> usize {
    match want >= MOST_PULLED {
        true => KEY_CHUNK,
        false => (want / usize::from(DOCUMENT_BUCKETS) + 2).clamp(1, KEY_CHUNK),
    }
}

/// The buckets of a read of `[from, to)` of the index of `_id`, each with its first rows, read
/// together in one trip (H-009b of the native tests); backward, from each bucket's end down.
fn open_lanes<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    (from, to): (&[u8], &[u8]),
    concern: ReadConcern,
    (want, backward): (usize, bool),
) -> Result<Vec<Lane>, CommandError> {
    let docs = KeyBuilder::index_prefix(tenant, collection, DOCUMENTS);
    let per = per_bucket(want);
    let spans: Vec<Span> = (0..DOCUMENT_BUCKETS)
        .map(|bucket| {
            let (start, end) = in_bucket(docs.as_bytes(), bucket, (from, to));
            Span { start, end, limit: per, backward }
        })
        .collect();
    let reads = backend.scan_spans(&spans, concern)?;
    Ok(spans.into_iter().zip(reads).map(|(span, rows)| Lane::new(span, rows, per)).collect())
}

/// Takes at most `want` documents of the buckets `lanes` of a read of the index of `_id`, in key
/// order (backward, from the highest, H-003): the next one each time is the lowest first key of
/// the buckets, and only a bucket that runs out is read again. An entry of this index is its
/// document, which the caller keeps instead of reading it again. Whether the buckets ran out.
fn merged<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    lanes: &mut [Lane],
    concern: ReadConcern,
    (want, backward): (usize, bool),
    out: &mut Vec<Entry>,
) -> Result<bool, CommandError> {
    let head = KeyBuilder::index_prefix(tenant, collection, DOCUMENTS).as_bytes().len();
    let per = per_bucket(want);
    fn pk_of(key: &[u8], head: usize) -> &[u8] {
        key.get(head + 1..).unwrap_or_default()
    }
    let mut taken = 0;
    while taken < want {
        for lane in lanes.iter_mut() {
            lane.refill(backend, per, concern)?;
        }
        let heads = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.rows.front().map(|(k, _)| (i, pk_of(k, head))));
        let next = match backward {
            true => heads.max_by(|a, b| a.1.cmp(b.1)),
            false => heads.min_by(|a, b| a.1.cmp(b.1)),
        };
        let Some((i, _)) = next else { return Ok(true) };
        let Some((key, value)) = lanes[i].rows.pop_front() else { return Ok(true) };
        out.push(Entry {
            pk: pk_of(&key, head).to_vec(),
            local: given_document_id(&value),
            values: None,
            // The key in the index's interval: the documents' prefix and the primary key.
            at: [&key[..head], pk_of(&key, head)].concat(),
            doc: Some((key, value)),
        });
        taken += 1;
    }
    Ok(false)
}

/// One bucket of a read of the index of `_id`: the rows read and not given yet, and where its read
/// goes on when it may hold more.
struct Lane {
    rows: VecDeque<(Vec<u8>, Vec<u8>)>,
    /// Where the read goes on: forward, its first key; backward, the key it stops below.
    next: Option<Vec<u8>>,
    span: Span,
}

impl Lane {
    fn new(span: Span, rows: Vec<(Vec<u8>, Vec<u8>)>, asked: usize) -> Lane {
        let next = Self::after(&rows, asked, span.backward);
        Lane { rows: rows.into(), next, span }
    }

    /// Where the read goes on after `rows`, when they filled what was asked and the bucket may
    /// hold more: the key after the last one, or backward the last one itself (excluded).
    fn after(rows: &[(Vec<u8>, Vec<u8>)], asked: usize, backward: bool) -> Option<Vec<u8>> {
        match rows.last() {
            Some((k, _)) if rows.len() >= asked && backward => Some(k.clone()),
            Some((k, _)) if rows.len() >= asked => Some([k.as_slice(), &[0]].concat()),
            _ => None,
        }
    }

    /// Reads on once its rows ran out and the bucket may hold more.
    fn refill<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        asked: usize,
        concern: ReadConcern,
    ) -> Result<(), CommandError> {
        if !self.rows.is_empty() {
            return Ok(());
        }
        let Some(next) = self.next.take() else { return Ok(()) };
        let rows = match self.span.backward {
            true => backend.scan_back(&self.span.start, &next, asked, concern)?,
            false => backend.scan(&next, &self.span.end, asked, concern)?,
        };
        self.next = Self::after(&rows, asked, self.span.backward);
        self.rows.extend(rows);
        Ok(())
    }
}

/// Reads the values of each entry of `index` of `coll` within `[start, end)` (an empty end is the
/// index's end) in key order, as the stored data has them (010 FR-029), and gives them to `each`
/// a pull at a time, calling `pause` after each pull, where the caller may give the CPU to other
/// work (023 FR-012). Stops, and gives `false`, at an entry whose values do not decode.
pub(crate) fn index_values<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    index: &PlanIndex,
    (start, end): &(Vec<u8>, Vec<u8>),
    pause: &mut dyn FnMut(),
    each: &mut dyn FnMut(Vec<Vec<Value>>),
) -> Result<bool, CommandError> {
    let r = KeyRange { start: start.clone(), end: (!end.is_empty()).then(|| end.clone()) };
    let read = (ReadConcern::Local, true);
    let mut cursor = RangeCursor::open((coll.tenant, coll.id), index, &r, read, false);
    while !cursor.done {
        let entries = cursor.next_entries(backend, MOST_PULLED)?;
        let Some(values) = entries.into_iter().map(|e| e.values).collect::<Option<Vec<_>>>() else {
            return Ok(false);
        };
        each(values);
        pause();
    }
    Ok(true)
}
