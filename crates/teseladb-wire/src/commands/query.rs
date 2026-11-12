//! `find`, `getMore`, `killCursors`, `count` and `distinct` (FR-006, FR-008, FR-017). Without
//! indexes (009) and the planner (010) a query walks the collection's documents in key order, in
//! chunks, filtering, skipping and projecting as it goes; with a `sort` it reads the whole
//! collection, sorts in memory (100 MiB without `allowDiskUse`, as RefDb) and serves the result
//! from the cursor.
//!
//! A batch ends at its `batchSize` (101 documents by default in the first batch, none in later
//! ones) or at 16 MiB; the cursor closes when a batch comes back shorter than its size, as a
//! `refrouter` 7.0 answers (measured): a batch that fills up exactly leaves the cursor open and the
//! next one comes back empty with id 0.

use std::collections::VecDeque;

use teseladb_collation::{Collation, ParsedCollation, parse_collation};
use teseladb_keys::{DOCUMENTS, KeyBuilder};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::tenants::{qualified_db, split_db};
use teseladb_mql::projection::{Projection, ProjectionContext};
use teseladb_mql::{Filter, FilterUse, MatchContext};
use teseladb_range::read::ReadConcern;
use teseladb_session::cursor::{Cursor, CursorError, cursor_id};
use teseladb_session::tailable::{Tail, lost_message};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::args::{self, missing};
use super::mutation::from_mql;
use super::names::{self, full};
use super::sort::{SortKey, SortSpec};
use super::{Backend, Ctx, lock, session_text};
use crate::MAX_BSON_OBJECT_SIZE;
use crate::error::{
    BAD_VALUE, CURSOR_NOT_FOUND, CURSOR_NOT_IN_SESSION, CURSOR_OTHER_SESSION,
    CURSOR_SESSION_REQUIRED, CommandError, QUERY_EXCEEDED_MEMORY_LIMIT, UNAUTHORIZED,
};
use crate::msg::command_limits;
use crate::types::{COUNT_NUMBER_TYPES, wrong_type, wrong_types};

/// `find`.
pub const FIND: &str = "find";
/// `getMore`.
pub const GET_MORE: &str = "getMore";
/// `killCursors`.
pub const KILL_CURSORS: &str = "killCursors";
/// `count`.
pub const COUNT: &str = "count";
/// `distinct`.
pub const DISTINCT: &str = "distinct";

/// Documents in a first batch without `batchSize`.
pub const DEFAULT_FIRST_BATCH: usize = 101;
/// Largest batch, in bytes (`maxBsonObjectSize`).
pub const MAX_BATCH_BYTES: usize = MAX_BSON_OBJECT_SIZE as usize;
/// Most memory a sort uses without `allowDiskUse`.
pub const SORT_MEMORY_LIMIT: usize = 100 * 1024 * 1024;
/// Documents read from the range at a time, at most.
pub const SCAN_CHUNK: usize = 1000;
/// Documents a collection scan reads first: each refill doubles it up to [`SCAN_CHUNK`], as an
/// adaptive readahead grows, so that a read that wants few documents does not read a thousand
/// (H-012 of the native tests).
pub const SCAN_FIRST: usize = 16;
/// The sort field of the collection's own order.
const NATURAL: &str = "$natural";

/// Where a cursor's documents come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// The collection's documents from key `next` to `end`, filtered and projected.
    Scan {
        /// Next key to read.
        next: Vec<u8>,
        /// End of the collection's documents.
        end: Vec<u8>,
        /// Filter.
        filter: Document,
        /// Projection.
        projection: Option<Document>,
        /// Collation.
        collation: Option<Document>,
        /// Read concern.
        concern: ReadConcern,
        /// The command's `let` variables, for its `getMore`s too.
        vars: Option<Document>,
        /// Documents the next refill reads ([`SCAN_FIRST`], doubling up to [`SCAN_CHUNK`]).
        chunk: usize,
    },
    /// The entries of `local.oplog.rs` above mark `from`, filtered and projected (018 FR-009).
    Oplog {
        /// The mark read up to.
        from: teseladb_hlc::Hlc,
        /// Filter.
        filter: Document,
        /// Projection.
        projection: Option<Document>,
        /// The command's `let` variables.
        vars: Option<Document>,
    },
    /// The computed results wait in the node's temporary store from `offset` (012 FR-013).
    Spilled {
        /// The run.
        run: std::sync::Arc<super::cursor_spill::SpilledRun>,
        /// Where the next document starts.
        offset: u64,
    },
    /// Everything is in the buffer.
    Done,
}

/// The state of a query cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryCursor {
    /// The source.
    pub source: Source,
    /// Documents ready to return.
    pub buffered: VecDeque<Document>,
    /// Documents still to skip.
    pub skip: u64,
    /// Documents still to return (`limit`).
    pub left: Option<u64>,
    /// Where a tailable cursor of a capped collection is (015 FR-004).
    pub tail: Option<Tail>,
    /// The mark its reads are served at, and whether its replies give it (008 FR-012).
    pub read_mark: Option<teseladb_txn::causal::ReadMark>,
    /// The change stream it reads (014 FR-013).
    pub stream: Option<Box<super::change_stream::ChangeStream>>,
    /// The command that opened it and the plan that command ran, which the profiler writes for
    /// each `getMore` (010 FR-015).
    pub origin: Option<Box<Origin>>,
    /// Bytes of computed results in `buffered`, which count in the node's part for cursors
    /// (012 FR-013); the buffer of a scan is not counted.
    pub held: usize,
    /// `returnKey` of a `find`: each document goes out as the index key that found it, `{}`
    /// without one (FR-008).
    pub return_key: bool,
}

/// The field of the projection that carries a `returnKey`'s index key to the cursor.
const RETURN_KEY_FIELD: &str = "\u{1}returnKey";

/// Document `d` of a `returnKey` cursor: the index key it carries, or `{}`.
fn returned_key(d: Document) -> Document {
    match d.get(RETURN_KEY_FIELD) {
        Some(Value::Document(k)) => k.clone(),
        _ => Document::new(),
    }
}

/// The field of a `getMore`'s profile with the command that opened its cursor.
pub const ORIGINATING_COMMAND: &str = "originatingCommand";

/// The command that opened a cursor, as the profiler shows it with each `getMore`.
#[derive(Clone, Debug, PartialEq)]
pub struct Origin {
    /// The command (`originatingCommand`).
    pub command: Document,
    /// What the planner reported of its read (`planSummary`, the hashes of its shape).
    pub plan: Option<Document>,
}

pub(crate) fn collation_of(doc: Option<&Document>) -> Result<Option<Collation>, CommandError> {
    let Some(d) = doc else { return Ok(None) };
    let bytes = d.to_bytes()?;
    let raw = RawDoc::from_bytes(&bytes)?;
    match parse_collation(raw) {
        Ok(ParsedCollation::Simple) => Ok(None),
        Ok(ParsedCollation::Collation(c, _)) => Ok(Some(c)),
        Err(e) => {
            Err(CommandError::new(crate::error::code(e.code).unwrap_or(BAD_VALUE), e.message))
        }
    }
}

/// A filter, projection and collation parsed once for a chunk.
struct Plan {
    filter: Filter,
    projection: Option<Projection>,
    collation: Option<Collation>,
    /// The command's `let` variables (FR-032).
    vars: Option<Document>,
}

/// A part of a command (filter, projection) or a stored document in BSON, within a command's
/// limits.
pub(crate) fn raw_bytes(d: &Document) -> Result<Vec<u8>, CommandError> {
    d.to_bytes_with(command_limits()).map_err(CommandError::from)
}

impl Plan {
    fn new(
        filter: &Document,
        projection: Option<&Document>,
        collation: Option<&Document>,
    ) -> Result<Plan, CommandError> {
        let fb = raw_bytes(filter)?;
        let filter =
            Filter::parse(RawDoc::from_bytes_with(&fb, command_limits())?, FilterUse::Query)
                .map_err(from_mql)?;
        let projection = parse_projection(projection)?;
        Ok(Plan { filter, projection, collation: collation_of(collation)?, vars: None })
    }

    /// The plan with the command's `let` variables.
    fn with_vars(self, vars: Option<&Document>) -> Plan {
        Plan { vars: vars.cloned(), ..self }
    }

    fn ctx(&self) -> MatchContext<'_> {
        MatchContext {
            collation: self.collation.as_ref().map(|c| c as &dyn teseladb_bson::Collator),
            ext: &super::engines::NodeExtensions,
            vars: self.vars.as_ref(),
        }
    }

    /// The document if it matches, projected; `sort_key` holds the values it was sorted by.
    fn apply(
        &self,
        doc: RawDoc<'_>,
        sort_key: Option<&[Value]>,
    ) -> Result<Option<Document>, CommandError> {
        self.apply_keyed(doc, sort_key, None)
    }

    /// [`Plan::apply`] with the key of the index entry the document was read by
    /// (`{$meta: "indexKey"}`).
    fn apply_keyed(
        &self,
        doc: RawDoc<'_>,
        sort_key: Option<&[Value]>,
        index_key: Option<&Document>,
    ) -> Result<Option<Document>, CommandError> {
        let ctx = self.ctx();
        let Some(position) = self.filter.match_position(doc, &ctx).map_err(from_mql)? else {
            return Ok(None);
        };
        match &self.projection {
            // A document read back is canonical BSON (it was stored as `to_bytes` wrote it): the
            // reply copies its bytes instead of building and writing its values again (H-009c).
            None => Ok(Some(Document::from_canonical(doc))),
            Some(p) => {
                let pctx = ProjectionContext { matcher: ctx, position, sort_key, index_key };
                p.apply(doc, &pctx).map(Some).map_err(from_mql)
            }
        }
    }

    /// [`Plan::apply`] on a row read from the node's own storage, taken whole: only the
    /// document's frame is checked, since it was validated when it came in and comes back under
    /// the LSM's checksums, and without a projection the reply keeps its bytes (003 FR-002, H-009
    /// of the native tests). A frame that does not hold is a fault, not a miss.
    fn apply_row(
        &self,
        bytes: Vec<u8>,
        index_key: Option<&Document>,
    ) -> Result<Option<Document>, CommandError> {
        let doc = RawDoc::from_stored(&bytes)?;
        let ctx = self.ctx();
        let Some(position) = self.filter.match_position(doc, &ctx).map_err(from_mql)? else {
            return Ok(None);
        };
        match &self.projection {
            None => Ok(Some(Document::from_stored(bytes)?)),
            Some(p) => {
                let pctx = ProjectionContext { matcher: ctx, position, sort_key: None, index_key };
                p.apply(doc, &pctx).map(Some).map_err(from_mql)
            }
        }
    }

    /// [`Plan::apply_row`] on a row this plan's filter already matched.
    fn matched_row(
        &self,
        bytes: Vec<u8>,
        index_key: Option<&Document>,
    ) -> Result<Document, CommandError> {
        self.apply_row(bytes, index_key)?.ok_or_else(|| {
            CommandError::new(crate::error::INTERNAL_ERROR, "a matched document matches no more")
        })
    }

    /// Document `doc`, which this plan's filter already matched, projected.
    fn matched(
        &self,
        doc: RawDoc<'_>,
        sort_key: Option<&[Value]>,
        index_key: Option<&Document>,
    ) -> Result<Document, CommandError> {
        self.apply_keyed(doc, sort_key, index_key)?.ok_or_else(|| {
            CommandError::new(crate::error::INTERNAL_ERROR, "a matched document matches no more")
        })
    }

    /// The `$meta` of the projection, then of the sort, that this query cannot give.
    fn check_metadata(&self, sort: Option<&SortSpec>) -> Result<(), CommandError> {
        let (text, near) = (self.filter.uses_text(), self.filter.uses_near());
        if let Some(p) = &self.projection {
            p.check_metadata(text, near, sort.is_some()).map_err(from_mql)?;
        }
        match sort {
            Some(s) => s.check_metadata(text, near),
            None => Ok(()),
        }
    }
}

/// Projection `p` parsed; `None` when there is none or it is empty.
fn parse_projection(p: Option<&Document>) -> Result<Option<Projection>, CommandError> {
    match p {
        Some(p) if !p.is_empty() => {
            let pb = raw_bytes(p)?;
            let raw = RawDoc::from_bytes_with(&pb, command_limits())?;
            Ok(Some(Projection::parse(raw).map_err(from_mql)?))
        }
        Some(_) | None => Ok(None),
    }
}

/// The documents interval of collection `c`.
pub fn documents_of(c: &CollectionEntry) -> (Vec<u8>, Vec<u8>) {
    let start = KeyBuilder::index_prefix(c.tenant, c.id, DOCUMENTS).as_bytes().to_vec();
    let end = KeyBuilder::index_prefix(c.tenant, c.id, DOCUMENTS + 1).as_bytes().to_vec();
    (start, end)
}

/// The key right after `k`.
fn after(k: &[u8]) -> Vec<u8> {
    let mut n = k.to_vec();
    n.push(0);
    n
}

/// Reads matching raw documents of `[start, end)` with `plan`'s filter (unprojected), calling
/// `each` for each one until it says stop.
pub fn for_each_match<B: Backend + ?Sized>(
    backend: &mut B,
    range: (Vec<u8>, Vec<u8>),
    filter: &Document,
    collation: Option<&Document>,
    concern: ReadConcern,
    each: impl FnMut(Vec<u8>, &[u8]) -> Result<bool, CommandError>,
) -> Result<(), CommandError> {
    for_each_match_with(backend, range, filter, collation, None, concern, each)
}

/// [`for_each_match`] with the command's `let` variables.
pub fn for_each_match_with<B: Backend + ?Sized>(
    backend: &mut B,
    range: (Vec<u8>, Vec<u8>),
    filter: &Document,
    collation: Option<&Document>,
    vars: Option<&Document>,
    concern: ReadConcern,
    each: impl FnMut(Vec<u8>, &[u8]) -> Result<bool, CommandError>,
) -> Result<(), CommandError> {
    for_each_match_counted(backend, range, filter, collation, vars, concern, each).map(|_| ())
}

/// [`for_each_match_with`], giving the number of documents it read (`docsExamined` of
/// `explain`).
pub(crate) fn for_each_match_counted<B: Backend + ?Sized>(
    backend: &mut B,
    (start, end): (Vec<u8>, Vec<u8>),
    filter: &Document,
    collation: Option<&Document>,
    vars: Option<&Document>,
    concern: ReadConcern,
    mut each: impl FnMut(Vec<u8>, &[u8]) -> Result<bool, CommandError>,
) -> Result<u64, CommandError> {
    let plan = Plan::new(filter, None, collation)?.with_vars(vars);
    let ctx = plan.ctx();
    let mut next = start;
    let mut examined = 0u64;
    loop {
        // The filter runs where the documents are read, and only those it accepts are copied
        // (010 FR-027). A stored document always parses: one that does not is a fault, not a
        // miss, and it stops the scan.
        let mut fault: Option<CommandError> = None;
        let mut keep = |bytes: &[u8]| {
            if fault.is_some() {
                return false;
            }
            let matched = RawDoc::from_bytes(bytes)
                .map_err(CommandError::from)
                .and_then(|doc| plan.filter.matches(doc, &ctx).map_err(from_mql));
            matched.unwrap_or_else(|e| {
                fault = Some(e);
                false
            })
        };
        let found = backend.scan_matching(&next, &end, SCAN_CHUNK, concern, &mut keep)?;
        if let Some(e) = fault {
            return Err(e);
        }
        for ((key, bytes), seen) in found.rows.into_iter().zip(found.seen) {
            if !each(key, &bytes)? {
                return Ok(examined + seen as u64);
            }
        }
        examined += found.examined as u64;
        match found.next {
            Some(from) => next = from,
            None => return Ok(examined),
        }
    }
}

/// Whether a command asks for a time series' buckets as they are stored (`rawData`).
fn raw_data(cmd: &Document) -> bool {
    matches!(cmd.get("rawData"), Some(Value::Boolean(true)))
}

/// The time series layout of `coll` for a command that reads its measurements: `None` for
/// another collection or with `rawData`.
fn measurements_of(
    coll: &CollectionEntry,
    cmd: &Document,
) -> Result<Option<teseladb_mql::timeseries_write::BucketSpec>, CommandError> {
    if raw_data(cmd) {
        return Ok(None);
    }
    super::timeseries::spec_of(coll)
}

fn ts_error(e: teseladb_mql::timeseries_write::TsError) -> CommandError {
    CommandError::new(crate::error::INTERNAL_ERROR, format!("{e:?}"))
}

/// [`for_each_match_with`] over the documents of `coll`; a time series' measurements, unpacked
/// from the buckets its filter may match (015 FR-007), unless the command asks for `rawData`.
pub fn for_each_doc<B: Backend + ?Sized>(
    backend: &mut B,
    (coll, cmd): (&CollectionEntry, &Document),
    filter: &Document,
    collation: Option<&Document>,
    vars: Option<&Document>,
    concern: ReadConcern,
    mut each: impl FnMut(Vec<u8>, &[u8]) -> Result<bool, CommandError>,
) -> Result<(), CommandError> {
    let Some(spec) = measurements_of(coll, cmd)? else {
        return for_each_match_with(
            backend,
            documents_of(coll),
            filter,
            collation,
            vars,
            concern,
            each,
        );
    };
    let bounds = teseladb_exec::timeseries_read::Bounds::of(&spec, filter);
    let plan = Plan::new(filter, None, collation)?.with_vars(vars);
    let ctx = plan.ctx();
    // The measurements of one bucket that match; `false` when `each` wants no more.
    let mut bucket = |key: &[u8], bytes: &[u8]| -> Result<bool, CommandError> {
        let bucket = RawDoc::from_bytes(bytes)?;
        if !bounds.may_match(&spec, bucket, ctx.collation).map_err(ts_error)? {
            return Ok(true);
        }
        for mb in spec.unpack_raw(bucket).map_err(ts_error)? {
            let doc = RawDoc::from_bytes(&mb)?;
            if plan.filter.matches(doc, &ctx).map_err(from_mql)? && !each(key.to_vec(), &mb)? {
                return Ok(false);
            }
        }
        Ok(true)
    };
    // An index of the buckets compares strings without a collation: a read with one scans them.
    let indexed = match collation {
        Some(_) => None,
        None => super::timeseries::indexed_buckets(backend, coll, &spec, filter, concern)?,
    };
    if let Some(found) = indexed {
        for (key, bytes) in found {
            if !bucket(&key, &bytes)? {
                return Ok(());
            }
        }
        return Ok(());
    }
    let (mut next, end) = super::timeseries::scan_range(coll, &spec, &bounds)?;
    loop {
        let rows = backend.scan(&next, &end, SCAN_CHUNK, concern)?;
        let short = rows.len() < SCAN_CHUNK;
        for (key, bytes) in rows {
            next = after(&key);
            if !bucket(&key, &bytes)? {
                return Ok(());
            }
        }
        if short {
            return Ok(());
        }
    }
}

/// The cursor of a `find` over the measurements of a time series: matched, sorted (or in the
/// buckets' order, backward with `$natural: -1`), skipped, limited and projected.
fn measurements_cursor<B: Backend + ?Sized>(
    backend: &mut B,
    (coll, cmd): (&CollectionEntry, &Document),
    a: &FindArgs,
    collation: Option<&Document>,
) -> Result<QueryCursor, CommandError> {
    let mut rows: Vec<Vec<u8>> = Vec::new();
    for_each_doc(
        backend,
        (coll, cmd),
        &a.filter,
        collation,
        a.vars.as_ref(),
        a.concern,
        |_, b| {
            rows.push(b.to_vec());
            Ok(true)
        },
    )?;
    if let Some(sort) = &a.sort {
        let c = collation_of(collation)?;
        let col: Option<&dyn teseladb_bson::Collator> = c.as_ref().map(|c| c as _);
        let mut keyed = Vec::with_capacity(rows.len());
        for r in rows {
            let keys = sort.keys(RawDoc::from_bytes(&r)?, col);
            keyed.push((keys, r));
        }
        keyed.sort_by(|x, y| sort.compare_keys(&x.0, &y.0, col));
        rows = keyed.into_iter().map(|(_, r)| r).collect();
    }
    if a.natural == Some(false) {
        rows.reverse();
    }
    let plan = Plan::new(&a.filter, a.projection.as_ref(), collation)?.with_vars(a.vars.as_ref());
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let take = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut docs = Vec::new();
    for bytes in rows.iter().skip(skip).take(take) {
        docs.push(plan.matched(RawDoc::from_bytes(bytes)?, None, None)?);
    }
    let mut c = QueryCursor::buffered(docs);
    c.left = a.limit;
    Ok(c)
}

impl QueryCursor {
    /// A cursor over documents already computed.
    pub fn buffered(docs: Vec<Document>) -> QueryCursor {
        QueryCursor {
            source: Source::Done,
            buffered: docs.into(),
            skip: 0,
            left: None,
            tail: None,
            read_mark: None,
            stream: None,
            origin: None,
            held: 0,
            return_key: false,
        }
    }

    /// The next batch: at most `size` documents (`None`: no count limit) and 16 MiB; whether
    /// the cursor is exhausted.
    pub fn batch<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        size: Option<usize>,
    ) -> Result<(Vec<Document>, bool), CommandError> {
        let want = match (size, self.left) {
            (Some(s), Some(l)) => Some(s.min(usize::try_from(l).unwrap_or(usize::MAX))),
            (Some(s), None) => Some(s),
            (None, Some(l)) => Some(usize::try_from(l).unwrap_or(usize::MAX)),
            (None, None) => None,
        };
        let mut out = Vec::new();
        let mut bytes = 0usize;
        loop {
            if want.is_some_and(|w| out.len() >= w) {
                return Ok(self.finish(out, false));
            }
            match self.buffered.pop_front() {
                Some(d) => {
                    let n = d.encoded_len();
                    if bytes + n > MAX_BATCH_BYTES && !out.is_empty() {
                        self.buffered.push_front(d);
                        return Ok(self.finish(out, false));
                    }
                    self.held = self.held.saturating_sub(n);
                    bytes += n;
                    out.push(if self.return_key { returned_key(d) } else { d });
                    if let Some(t) = &mut self.tail {
                        t.took();
                    }
                }
                None if self.source == Source::Done => {
                    let short = want.is_none_or(|w| out.len() < w);
                    return Ok(self.finish(out, short));
                }
                None => {
                    // A tailable cursor that read up to the newest document ends its batch there.
                    if let Some(t) = self.tail.as_mut().filter(|t| t.caught_up) {
                        t.reached_newest();
                        return Ok(self.finish(out, false));
                    }
                    self.refill(backend)?;
                }
            }
        }
    }

    fn finish(&mut self, out: Vec<Document>, exhausted: bool) -> (Vec<Document>, bool) {
        if let Some(l) = &mut self.left {
            *l = l.saturating_sub(out.len() as u64);
        }
        (out, exhausted)
    }

    /// Reads the next chunk of the source into the buffer.
    fn refill<B: Backend + ?Sized>(&mut self, backend: &mut B) -> Result<(), CommandError> {
        if let Source::Oplog { .. } = &self.source {
            return self.refill_oplog(backend);
        }
        if let Source::Spilled { run, offset } = &mut self.source {
            let (next, read) = super::cursor_spill::refill(run, *offset, &mut self.buffered)?;
            self.held += read;
            match next {
                Some(o) => *offset = o,
                None => self.source = Source::Done,
            }
            return Ok(());
        }
        let (next, end, filter, projection, collation, concern, vars, size) = match &mut self.source
        {
            Source::Scan { next, end, filter, projection, collation, concern, vars, chunk } => {
                let size = *chunk;
                *chunk = chunk.saturating_mul(2).min(SCAN_CHUNK);
                (next, end, filter, projection, collation, concern, vars, size)
            }
            Source::Done | Source::Spilled { .. } | Source::Oplog { .. } => return Ok(()),
        };
        let plan =
            Plan::new(filter, projection.as_ref(), collation.as_ref())?.with_vars(vars.as_ref());
        let rows = backend.scan(next, end, size, *concern)?;
        let mut short = rows.len() < size;
        let read = rows.len();
        for (seen, (key, bytes)) in rows.into_iter().enumerate() {
            // The limit is reached: the rest of the read is left for no one, as a `COLLSCAN`
            // stops at its limit, and its `docsExamined` stops there too.
            if self.left.is_some_and(|l| self.buffered.len() as u64 >= l) {
                backend.not_examined(u64::try_from(read - seen).unwrap_or(u64::MAX));
                short = false;
                break;
            }
            *next = after(&key);
            let mut buffered = false;
            if let Some(d) = plan.apply_row(bytes, None)? {
                if self.skip > 0 {
                    self.skip -= 1;
                } else {
                    self.buffered.push_back(d);
                    buffered = true;
                }
            }
            if let Some(t) = &mut self.tail {
                t.read(key, buffered);
            }
        }
        match (&mut self.tail, short) {
            // A tailable cursor goes on from there in its next `getMore`.
            (Some(t), true) => t.caught_up = true,
            (None, true) => self.source = Source::Done,
            (_, false) => {}
        }
        Ok(())
    }
}

impl QueryCursor {
    /// Reads the oplog's entries past the cursor's mark into the buffer: a tailable cursor
    /// stays on the newest one, any other ends there.
    fn refill_oplog<B: Backend + ?Sized>(&mut self, backend: &mut B) -> Result<(), CommandError> {
        let Source::Oplog { from, filter, projection, vars } = &mut self.source else {
            return Ok(());
        };
        let plan = Plan::new(filter, projection.as_ref(), None)?.with_vars(vars.as_ref());
        let read = super::oplog::read(backend, *from)?;
        *from = read.resolved;
        for item in read.items {
            let bytes = item.doc.to_bytes()?;
            let mut buffered = false;
            if let Some(d) = plan.apply(RawDoc::from_bytes(&bytes)?, None)? {
                if self.skip > 0 {
                    self.skip -= 1;
                } else {
                    self.buffered.push_back(d);
                    buffered = true;
                }
            }
            if let Some(t) = &mut self.tail {
                t.read(item.key(), buffered);
            }
        }
        match &mut self.tail {
            Some(t) => t.caught_up = true,
            None => self.source = Source::Done,
        }
        Ok(())
    }
}

/// A `find` on `local.oplog.rs` (018 FR-009): a tailable one reads on from the newest entry on
/// each `getMore`; `oplogReplay` is accepted and ignored, as on RefDb 9.0.
fn oplog_find<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    a: &FindArgs,
) -> Result<Document, CommandError> {
    if a.tailable.is_none() {
        // `$natural: -1` with a limit (`refdump --oplog`'s first and last entry) reads back
        // from the newest entry only as far as it needs.
        let newest = match (a.natural, &a.sort) {
            (Some(false), None) if a.filter.is_empty() => a.limit.map(|l| {
                let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
                usize::try_from(l).unwrap_or(usize::MAX).saturating_add(skip)
            }),
            _ => None,
        };
        let docs = super::oplog::documents(ctx.backend, &a.filter, newest)?;
        return generated_find(ctx, ns, a, docs);
    }
    let ns_text = full(ns);
    Plan::new(&a.filter, a.projection.as_ref(), a.collation.as_ref())?;
    let from = super::oplog::lower_bound(&a.filter)
        .unwrap_or(teseladb_hlc::Hlc { physical: 0, logical: 0 });
    let mut state = QueryCursor {
        source: Source::Oplog {
            from,
            filter: a.filter.clone(),
            projection: a.projection.clone(),
            vars: a.vars.clone(),
        },
        buffered: VecDeque::new(),
        skip: a.skip,
        left: a.limit,
        tail: a.tailable.map(Tail::new),
        read_mark: None,
        stream: None,
        origin: None,
        held: 0,
        return_key: false,
    };
    let size = Some(a.batch_size.unwrap_or(DEFAULT_FIRST_BATCH));
    let (docs, _) = state.batch(ctx.backend, size)?;
    let exhausted = state.tail.as_ref().is_none_or(|t| !t.stays_open());
    let id = keep(ctx, &ns_text, state, exhausted, a.no_timeout)?;
    Ok(cursor_reply(id, &ns_text, "firstBatch", docs))
}

/// What `find` reads.
pub(crate) struct FindArgs {
    pub(crate) filter: Document,
    pub(crate) projection: Option<Document>,
    sort: Option<SortSpec>,
    pub(crate) skip: u64,
    pub(crate) limit: Option<u64>,
    batch_size: Option<usize>,
    single_batch: bool,
    /// `tailable`, with `awaitData`.
    tailable: Option<bool>,
    /// A `$natural` sort: `true` forward, `false` backward.
    pub(crate) natural: Option<bool>,
    no_timeout: bool,
    allow_disk_use: bool,
    pub(crate) collation: Option<Document>,
    pub(crate) concern: ReadConcern,
    /// `let`, resolved.
    pub(crate) vars: Option<Document>,
    /// `min` and `max` (FR-008).
    bounds: Option<super::min_max::Bounds>,
    /// `returnKey` (FR-008).
    return_key: bool,
}

/// The `let` of command `name`, checked and resolved (FR-032): the variables of its filter,
/// projection and update.
pub(crate) fn command_let<B: Backend + ?Sized>(
    backend: &mut B,
    cmd: &Document,
    name: &str,
) -> Result<Option<Document>, CommandError> {
    // The `let` is resolved before the system variables exist: `$$NOW` there is 51144.
    let mut vars = match args::doc(cmd, name, "let")? {
        Some(l) => {
            teseladb_mql::vars::resolve_let(l, &super::engines::NodeExtensions).map_err(from_mql)?
        }
        None => Document::new(),
    };
    vars.insert("NOW", Value::DateTime(backend.now_ms() as i64));
    let (time, increment) = teseladb_hlc::to_timestamp(backend.cluster_mark());
    vars.insert("CLUSTER_TIME", Value::Timestamp { time, increment });
    vars.insert("USER_ROLES", user_roles(backend)?);
    Ok(Some(vars))
}

/// `$$USER_ROLES`: the user's roles, direct and inherited, as `{_id: "db.role", role, db}` in the
/// order of their `_id` (measured on a `refrouter` 9.0.4).
fn user_roles<B: Backend + ?Sized>(backend: &mut B) -> Result<Value, CommandError> {
    let mut roles: Vec<(String, Document)> = super::diagnostics::authenticated_roles(backend)?
        .into_iter()
        .map(|r| {
            let id = format!("{}.{}", r.db, r.role);
            let d =
                Document::new().with("_id", id.as_str()).with("role", &*r.role).with("db", &*r.db);
            (id, d)
        })
        .collect();
    roles.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Value::Array(roles.into_iter().map(|(_, d)| Value::Document(d)).collect()))
}

fn non_negative(v: Option<i64>, field: &str) -> Result<Option<u64>, CommandError> {
    match v {
        Some(n) if n < 0 => Err(CommandError::new(
            BAD_VALUE,
            format!("BSON field '{field}' value must be >= 0, actual value '{n}'"),
        )),
        Some(n) => Ok(Some(n as u64)),
        None => Ok(None),
    }
}

/// The direction of a `$natural` sort (`true` forward), `None` without one; `$natural` with
/// another value, or with other fields, is `BadValue` (measured on a `refrouter` 9.0.4).
fn natural_sort(sort: Option<&Value>) -> Result<Option<bool>, CommandError> {
    let Some(Value::Document(s)) = sort else { return Ok(None) };
    let Some(v) = s.get(NATURAL) else { return Ok(None) };
    let direction = match v {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        Value::Double(n) if n.fract() == 0.0 => Some(*n as i64),
        _ => None,
    };
    match (direction, s.len()) {
        (Some(1), 1) => Ok(Some(true)),
        (Some(-1), 1) => Ok(Some(false)),
        _ => Err(CommandError::new(
            BAD_VALUE,
            "$natural sort cannot be set to a value other than -1 or 1.",
        )),
    }
}

/// `returnKey` of `find`: a boolean (measured on a `refrouter` 9.0.4 on 05-11-2026: `returnKey: 1`
/// is 14 «Field 'returnKey' should be a boolean value, but found: int»).
fn return_key(cmd: &Document) -> Result<bool, CommandError> {
    match cmd.get("returnKey") {
        None => Ok(false),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(v) => Err(CommandError::new(
            crate::error::TYPE_MISMATCH,
            format!(
                "Field 'returnKey' should be a boolean value, but found: {}",
                crate::types::type_name(v)
            ),
        )
        .parsed()),
    }
}

/// An object option of `find`, refused with `find`'s own message when it is another type.
fn find_object(cmd: &Document, field: &str) -> Result<Option<Document>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::Document(d)) => Ok(Some(d.clone())),
        // The router's parse of the command (008, as a `refrouter` 9.0.4 raises it).
        Some(_) => Err(CommandError::new(
            crate::error::TYPE_MISMATCH,
            format!("Expected field {field} to be of type object"),
        )
        .parsed()),
    }
}

pub(crate) fn find_args<B: Backend + ?Sized>(
    backend: &mut B,
    cmd: &Document,
) -> Result<FindArgs, CommandError> {
    let filter = find_object(cmd, "filter")?.unwrap_or_default();
    let projection = find_object(cmd, "projection")?;
    // A `$natural` hint reads in that direction when nothing sorts (measured).
    let hinted = match cmd.get("hint") {
        Some(h @ Value::Document(d)) if d.get(NATURAL).is_some() => {
            natural_sort(Some(h)).map_err(|_| {
                CommandError::new(
                    BAD_VALUE,
                    "$natural hint cannot be set to a value other than -1 or 1.",
                )
            })?
        }
        _ => None,
    };
    let natural = natural_sort(cmd.get("sort"))?
        .or(hinted
            .filter(|_| !matches!(cmd.get("sort"), Some(Value::Document(s)) if !s.is_empty())));
    let sort = match cmd.get("sort") {
        Some(Value::Document(_)) if natural.is_some() => None,
        Some(Value::Document(s)) if !s.is_empty() => Some(SortSpec::parse(s)?),
        Some(Value::Document(_)) | None => None,
        Some(_) => {
            return Err(CommandError::new(
                crate::error::TYPE_MISMATCH,
                "Expected field sort to be of type object",
            ));
        }
    };
    super::hints::check_hint_type(cmd)?;
    let tailable = args::boolean(cmd, FIND, "tailable")?.unwrap_or(false);
    let await_data = args::boolean(cmd, FIND, "awaitData")?.unwrap_or(false);
    if await_data && !tailable {
        let msg = "Cannot set 'awaitData' without also setting 'tailable'";
        return Err(CommandError::new(crate::error::FAILED_TO_PARSE, msg));
    }
    let skip = non_negative(args::int(cmd, FIND, "skip")?, "skip")?.unwrap_or(0);
    let single_batch = args::boolean(cmd, FIND, "singleBatch")?.unwrap_or(false);
    // Measured on a `refrouter` 9.0.4.
    if tailable && (sort.is_some() || natural == Some(false)) {
        let msg = "cannot use tailable option with a sort other than {$natural: 1}";
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    if tailable && single_batch {
        let msg = "cannot use tailable option with the 'singleBatch' option";
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    let limit = non_negative(args::int(cmd, FIND, "limit")?, "limit")?.filter(|&n| n > 0);
    let batch_size = non_negative(args::int(cmd, FIND, "batchSize")?, "batchSize")?
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    Ok(FindArgs {
        filter,
        projection,
        sort,
        skip,
        limit,
        batch_size,
        single_batch,
        tailable: tailable.then_some(await_data),
        natural,
        no_timeout: args::boolean(cmd, FIND, "noCursorTimeout")?.unwrap_or(false),
        // True by default since 6.0 (measured on a `refrouter` 9.0.4: 110 MiB sort without it).
        allow_disk_use: args::boolean(cmd, FIND, "allowDiskUse")?.unwrap_or(true),
        collation: find_object(cmd, "collation")?,
        concern: args::read_concern(cmd, FIND)?,
        vars: command_let(backend, cmd, FIND)?,
        bounds: super::min_max::Bounds::of(find_object(cmd, "min")?, find_object(cmd, "max")?),
        return_key: return_key(cmd)?,
    })
}

/// The collection a command names in its first field.
pub fn target<B: Backend + ?Sized>(ctx: &Ctx<'_, B>) -> Result<Namespace, CommandError> {
    match ctx.first() {
        Value::String(s) => {
            let ns = names::ns(ctx.db, s);
            names::check(&ns)?;
            Ok(ns)
        }
        v => Err(CommandError::new(
            crate::error::INVALID_NAMESPACE,
            format!("Collection name has invalid type {}", crate::types::type_name(v)),
        )),
    }
}

/// The cursor document of a reply.
pub fn cursor_reply(id: i64, ns: &str, batch_field: &str, docs: Vec<Document>) -> Document {
    let docs: Vec<Value> = docs.into_iter().map(Value::Document).collect();
    let cursor = if batch_field == "firstBatch" {
        Document::new().with(batch_field, docs).with("id", id).with("ns", ns)
    } else {
        Document::new().with("id", id).with("ns", ns).with(batch_field, docs)
    };
    Document::new().with("cursor", cursor).with("ok", 1.0)
}

/// Keeps `cursor` open unless `exhausted`; its identifier (0 when closed).
pub fn keep<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &str,
    state: QueryCursor,
    exhausted: bool,
    no_timeout: bool,
) -> Result<i64, CommandError> {
    if exhausted {
        return Ok(0);
    }
    let state = within_cursor_part(ctx, state)?;
    let lsid = args::lsid(ctx.cmd)?;
    let id = cursor_id(ctx.backend.node(), ctx.backend.random());
    let c = Cursor {
        // A cursor is its tenant's (013 FR-024).
        ns: qualified_db(ctx.backend.tenant(), ns),
        lsid,
        owner: ctx.backend.authenticated_user().map(|u| u.to_string()),
        no_timeout,
        last_use_ms: ctx.backend.now_ms(),
        state,
    };
    lock(ctx.shared).cursors.put(id, c);
    Ok(id)
}

/// Writes the computed results of `state` to the node's temporary store when holding them would
/// take the open cursors past their part of memory (012 FR-013, D-105).
fn within_cursor_part<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    mut state: QueryCursor,
) -> Result<QueryCursor, CommandError> {
    if state.source != Source::Done || state.buffered.is_empty() {
        return Ok(state);
    }
    let (limit, store, others) = {
        let g = lock(ctx.shared);
        (
            g.spill.cursor_bytes,
            g.spill.store.clone(),
            g.cursors.iter().map(|c| c.state.held).sum::<usize>(),
        )
    };
    let (Some(limit), Some(store)) = (limit, store) else { return Ok(state) };
    let own = super::cursor_spill::bytes_of(&state.buffered);
    if own + others <= limit {
        state.held = own;
        return Ok(state);
    }
    let run = super::cursor_spill::spill(&store, &mut state.buffered)?;
    state.source = Source::Spilled { run, offset: 0 };
    state.held = 0;
    Ok(state)
}

/// Counts a read of a whole collection (`metrics.queryExecutor.collectionScans`).
pub(crate) fn count_scan(shared: &super::Shared, ns: &Namespace) {
    lock(shared).status.collection_scan(&full(ns));
}

/// `find`.
pub fn find<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    let mut a = find_args(ctx.backend, ctx.cmd)?;
    // `returnKey` gives the index key of each document and drops the projection (measured on a
    // `refrouter` 9.0.4 on 05-11-2026).
    if a.return_key {
        // An `_id` equality reads its document's key alone, through the index of `_id`.
        if a.collation.is_none() && super::crud::id_equality(&a.filter).is_some() {
            a.projection = Some(Document::new().with("_id", 1));
            a.return_key = false;
        } else {
            let key = Document::new().with("$meta", "indexKey");
            a.projection = Some(Document::new().with("_id", 0).with(RETURN_KEY_FIELD, key));
        }
    }
    if let Some(u) = super::uuid::expected(ctx.cmd, FIND)? {
        super::uuid::check(ctx.backend, &ns, u)?;
    }
    let ns_text = full(&ns);
    if ns.coll == super::kinds::SYSTEM_VIEWS {
        return system_views(ctx, &ns, &a);
    }
    if super::oplog::is_oplog(&ns) {
        return oplog_find(ctx, &ns, &a);
    }
    if let Some(docs) = super::authz_collections::documents(ctx.backend, &ns)? {
        return generated_find(ctx, &ns, &a, docs);
    }
    if let Some(view) = super::views::resolve(ctx.backend, &ns)? {
        let stages = super::views::find_stages(ctx.cmd, &ns_text)?;
        super::views::check_collation(&view, a.collation.as_ref())?;
        // The view's stages that the node answers itself read it (`$indexStats`, measured).
        let stages = (stages.as_slice(), a.vars.as_ref());
        let docs =
            super::aggregate::read_through_node(ctx.backend, ctx.shared, &ns, stages, a.concern)?;
        // The pipeline's `$limit` knows its end once it has given all its documents.
        let ends = a.limit.is_some_and(|l| u64::try_from(docs.len()).is_ok_and(|n| n == l));
        return buffered_reply(ctx, &ns_text, &a, docs, ends);
    }
    let Some(coll) = ctx.backend.collection(&ns)? else {
        // Validate the filter even when there is nothing to read, as RefDb does.
        Plan::new(&a.filter, a.projection.as_ref(), a.collation.as_ref())
            .map_err(CommandError::query_parse)?;
        if super::views::has_text(&a.filter) {
            return text_find(ctx, &ns);
        }
        return Ok(cursor_reply(0, &ns_text, "firstBatch", Vec::new()));
    };
    if super::views::has_text(&a.filter) {
        return text_find(ctx, &ns);
    }
    if let Some(reply) = express_find(ctx, &ns_text, &coll, &a)? {
        return Ok(reply);
    }
    let collation = a.collation.clone().or_else(|| options_collation(&coll));
    // Its errors come before the statement joins a transaction (008, measured on 03-11-2026).
    // The filter is parsed once, here, for the checks and the planner, as RefDb builds one
    // canonical query from the command.
    let first = Plan::new(&a.filter, a.projection.as_ref(), collation.as_ref())
        .map_err(CommandError::query_parse)?;
    first.check_metadata(a.sort.as_ref())?;
    if a.tailable.is_some() {
        check_tailable(&ns, &coll, &a)?;
    }
    let optimized = check_planned_filter(&first, &a.filter, a.vars.as_ref())?;
    if let Some(p) = &a.projection {
        check_projection(p, a.vars.as_ref())?;
    }
    let sort_doc = match ctx.cmd.get("sort") {
        Some(Value::Document(s)) => Some(s),
        _ => None,
    };
    let hinted = super::hints::Hinted {
        filter: &a.filter,
        sort: sort_doc,
        projection: a.projection.as_ref(),
        vars: a.vars.as_ref(),
    };
    super::hints::check_hint(ctx.backend, (&ns, &coll), ctx.cmd, &hinted)?;
    let min_max = match &a.bounds {
        Some(b) => {
            let indexes = super::plan_exec::plan_indexes(ctx.backend, &coll)?;
            let hint = ctx.cmd.get("hint");
            Some((b, super::min_max::check(b, (&ns, &hinted), hint, &indexes)?))
        }
        None => None,
    };
    if let Some(near) = super::near_find::split(&a.filter) {
        return near_find(ctx, &ns, &a, &near);
    }
    let executor = |e: CommandError| executor_error(FIND, &ns_text, e);
    // An `_id` equality without a collation reads that document's key alone.
    let range = match super::crud::id_equality(&a.filter).filter(|_| collation.is_none()) {
        Some(id) => match super::crud::id_doc_key(ctx.backend, &coll, &id)? {
            // The read goes through the index of `_id` (`$indexStats` counts it).
            Some(key) => {
                ctx.backend.note_plan(super::plan_exec::used_id_index(false));
                let end = after(&key);
                (key, end)
            }
            // A capped collection without that `_id`: nothing to read.
            None => (documents_of(&coll).0, documents_of(&coll).0),
        },
        None => documents_of(&coll),
    };
    // Only a read of the whole collection is a collection scan: an `_id` equality reads its
    // document alone, with a sort or without one (a `refnode` 9.0.4 counts none then, measured).
    if range == documents_of(&coll) {
        count_scan(ctx.shared, &ns);
    }
    let measured = measurements_of(&coll, ctx.cmd)?.is_some();
    // The planner's index plan, when one wins: its documents in its order (010 FR-001).
    let plannable = !measured
        && collation.is_none()
        && a.natural.is_none()
        && a.tailable.is_none()
        && range == documents_of(&coll);
    let planned = if plannable {
        let parsed = match &optimized {
            None => first.filter.parsed_query(),
            Some(o) => super::plan_exec::parsed_of_optimized(o)?,
        };
        let sort = match ctx.cmd.get("sort") {
            Some(Value::Document(s)) => sort_directions(s),
            _ => Vec::new(),
        };
        let read = super::plan_exec::ReadPlan {
            filter: &a.filter,
            parsed: &parsed,
            sort,
            // `min` and `max` take the plan's keys after it ran: it is not cut before.
            limit: a.limit.filter(|_| min_max.is_none()).map(|l| a.skip.saturating_add(l)),
            // The indexes are read for a hint only: without one, the planner reads them.
            hint: match ctx.cmd.get("hint") {
                None => None,
                Some(h) => {
                    let indexes = super::plan_exec::plan_indexes(ctx.backend, &coll)?;
                    super::hints::position(Some(h), &indexes)?
                }
            },
            vars: a.vars.as_ref(),
            concern: a.concern,
            intersect: super::hot::of(ctx.shared)
                .hash_intersection
                .load(std::sync::atomic::Ordering::Relaxed),
            index_keys: min_max.is_some()
                || (reads_index_key(a.projection.as_ref()) && !id_point(&a.filter)),
            key_only: a.return_key,
        };
        let sort_doc = match ctx.cmd.get("sort") {
            Some(Value::Document(s)) => s.clone(),
            _ => Document::new(),
        };
        let projection = a.projection.clone().unwrap_or_default();
        let note = super::plan_exec::CacheNote {
            shared: ctx.shared,
            ns: &ns,
            from: Document::new()
                .with("query", a.filter.clone())
                .with("sort", sort_doc.clone())
                .with("projection", projection.clone()),
            shape: super::plan_cache::read_shape(
                &parsed,
                &a.filter,
                Some(&sort_doc),
                Some(&projection),
                None,
            ),
        };
        let planned =
            super::plan_exec::planned_keyed(ctx.backend, &coll, &read, note).map_err(executor)?;
        match (planned, &min_max) {
            (Some(p), Some((b, pattern))) => Some(bounded(p, b, pattern)?),
            (p, _) => p,
        }
    } else {
        None
    };
    let mut state = match &a.sort {
        _ if measured => measurements_cursor(ctx.backend, (&coll, ctx.cmd), &a, collation.as_ref())
            .map_err(executor)?,
        None if planned.is_some() => {
            planned_cursor(first, planned.unwrap_or_default(), &a).map_err(executor)?
        }
        None if a.natural == Some(false) => {
            backward(ctx.backend, range, &a, collation.as_ref()).map_err(executor)?
        }
        None => QueryCursor {
            source: Source::Scan {
                next: range.0,
                end: range.1,
                filter: a.filter.clone(),
                projection: a.projection.clone(),
                collation: collation.clone(),
                concern: a.concern,
                vars: a.vars.clone(),
                chunk: SCAN_FIRST,
            },
            buffered: VecDeque::new(),
            skip: a.skip,
            left: a.limit,
            tail: a.tailable.map(Tail::new),
            read_mark: None,
            stream: None,
            origin: None,
            held: 0,
            return_key: false,
        },
        // A plan whose index reads in the sort's order gives its documents sorted, as an
        // `IXSCAN` with no `SORT` stage above it: they are not sorted again unless the
        // projection shows their sort key. Past the sort's memory limit without `allowDiskUse`
        // they go through the sort, which refuses them as before (measured on a `refrouter` 9.0.4).
        Some(_)
            if planned.as_ref().is_some_and(|p| {
                p.2 && (a.allow_disk_use
                    || p.0.iter().map(Vec::len).sum::<usize>() <= SORT_MEMORY_LIMIT)
            }) && !reads_meta(a.projection.as_ref(), "sortKey") =>
        {
            planned_cursor(first, planned.unwrap_or_default(), &a).map_err(executor)?
        }
        Some(sort) => sorted(ctx.backend, (first, range), &a, sort, collation.as_ref(), planned)
            .map_err(executor)?,
    };
    state.return_key = a.return_key;
    let size = Some(a.batch_size.unwrap_or(DEFAULT_FIRST_BATCH));
    let (docs, short) = state.batch(ctx.backend, size).map_err(executor)?;
    // The mark the first batch read at, which a read in its uncertainty window raised (FR-008):
    // the batches that follow read there.
    state.read_mark = ctx.backend.read_mark();
    // A `refrouter` 9.0 closes the cursor once the limit is reached (measured); a tailable one
    // stays open unless the collection had no document at all.
    let exhausted = match &state.tail {
        Some(t) => !t.stays_open(),
        None => short || a.single_batch || state.left == Some(0),
    };
    let id = keep(ctx, &ns_text, state, exhausted, a.no_timeout)?;
    Ok(cursor_reply(id, &ns_text, "firstBatch", docs))
}

/// What a simple projection of the express path needs: no collation, variable, position, sort
/// key or index key (it names fields only).
const EXPRESS_PROJECTION: ProjectionContext<'static> = ProjectionContext {
    matcher: MatchContext { collation: None, ext: &super::engines::NodeExtensions, vars: None },
    position: None,
    sort_key: None,
    index_key: None,
};

/// A `find` of one `_id` by equality, answered without the planner as RefDb's express path
/// answers it (the conditions are in H-013 of the native tests' findings), which TeselaDB's
/// `explain` already shows as `EXPRESS_IXSCAN`. The filter is the
/// `_id` alone; a simple projection or none ([`super::explain::simple_projection`]); no `skip`, `hint`, `min`, `max`, `returnKey`, `batchSize`,
/// `showRecordId` or tailable cursor; a sort only of fields in either direction (one document
/// is in any order); no collation, the command's or the collection's; not a time series. The
/// document is read as before, at the key of its `_id` with the same note of the `_id` index,
/// and the filter is not applied again: without a collation that key is the equality itself
/// (H-013 of the native tests: planning and filtering were most of a read by `_id`). `None`
/// when the command is not one of those.
fn express_find<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns_text: &str,
    coll: &CollectionEntry,
    a: &FindArgs,
) -> Result<Option<Document>, CommandError> {
    let plain_sort = a.sort.is_none()
        || matches!(ctx.cmd.get("sort"), Some(Value::Document(s))
            if s.iter().all(|(_, v)| matches!(v, Value::Int32(1 | -1) | Value::Int64(1 | -1))
                || matches!(v, Value::Double(d) if *d == 1.0 || *d == -1.0)));
    let eligible = a.filter.len() == 1
        && a.projection.as_ref().is_none_or(super::explain::simple_projection)
        && a.skip == 0
        && a.bounds.is_none()
        && !a.return_key
        && a.batch_size.is_none()
        && a.tailable.is_none()
        && a.natural.is_none()
        && a.collation.is_none()
        && plain_sort
        && ctx.cmd.get("hint").is_none()
        && !matches!(ctx.cmd.get("showRecordId"), Some(Value::Boolean(true)))
        && options_collation(coll).is_none();
    if !eligible {
        return Ok(None);
    }
    let Some(id) = super::crud::id_equality(&a.filter) else { return Ok(None) };
    if measurements_of(coll, ctx.cmd)?.is_some() {
        return Ok(None);
    }
    // Parsed before the read: a projection that mixes inclusion and exclusion fails also for an
    // `_id` that is not there.
    let projection = parse_projection(a.projection.as_ref())?;
    let mut docs = Vec::with_capacity(1);
    for bytes in read_by_id(ctx.backend, coll, &id, a.concern)? {
        // A stored document always parses: one that does not is a fault, not a miss.
        let raw = RawDoc::from_bytes(&bytes)?;
        docs.push(match &projection {
            None => Document::from(raw),
            Some(p) => p.apply(raw, &EXPRESS_PROJECTION).map_err(from_mql)?,
        });
    }
    Ok(Some(cursor_reply(0, ns_text, "firstBatch", docs)))
}

/// The `_id` a `count` or `distinct` reads by RefDb's express path (`IDHACK` or
/// `EXPRESS_IXSCAN`, measured on a `refrouter` 9.0.4, H-011 of the native tests): its query is that
/// `_id` equality alone, with no collation (the command's or the collection's, `collation`), no
/// `hint`, on a collection that is not a time series.
pub(crate) fn express_id(
    coll: &CollectionEntry,
    query: &Document,
    collation: Option<&Document>,
    cmd: &Document,
) -> Result<Option<Value>, CommandError> {
    if query.len() != 1
        || collation.is_some()
        || cmd.get("hint").is_some()
        || super::timeseries::spec_of(coll)?.is_some()
    {
        return Ok(None);
    }
    Ok(super::crud::id_equality(query))
}

/// The stored document of `_id` `id` in `coll`, read at its key as RefDb's express path
/// reads it, with the same note of the index of `_id` (`$indexStats` counts it); none when it is
/// not there.
pub(crate) fn read_by_id<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    id: &Value,
    concern: ReadConcern,
) -> Result<Vec<Vec<u8>>, CommandError> {
    // A capped collection without that `_id` has no key for it: nothing to read.
    let Some(key) = super::crud::id_doc_key(backend, coll, id)? else { return Ok(Vec::new()) };
    backend.note_plan(super::plan_exec::used_id_index(false));
    let end = after(&key);
    Ok(backend.scan(&key, &end, 1, concern)?.into_iter().map(|(_, bytes)| bytes).collect())
}

/// A `find` with `$near` or `$nearSphere`: its stages read through the aggregation engine,
/// nearest first (009 T035).
fn near_find<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    a: &FindArgs,
    near: &super::near_find::NearFind,
) -> Result<Document, CommandError> {
    let ns_text = full(ns);
    let stages = near.stages(ctx.cmd, &ns_text)?;
    let empty = Value::Document(Document::new());
    let sort = ctx.cmd.get("sort").cloned().unwrap_or_else(|| empty.clone());
    let projection = ctx.cmd.get("projection").cloned().unwrap_or_else(|| empty.clone());
    let stages = (stages.as_slice(), a.vars.as_ref());
    let docs = super::aggregate::read_through_node(ctx.backend, ctx.shared, ns, stages, a.concern)
        .map_err(|e| {
            let shown = (&sort, &projection);
            near.planner_error(e, &ns_text, &query_tree(&near.rest), shown)
        })?;
    let ends = a.limit.is_some_and(|l| u64::try_from(docs.len()).is_ok_and(|n| n == l));
    buffered_reply(ctx, &ns_text, a, docs, ends)
}

/// A `find` whose filter has a `$text` (011 FR-001): the aggregation of its stages, whose first
/// `$match` the collection's text index answers with each document's `textScore`.
fn text_find<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
) -> Result<Document, CommandError> {
    let stages: Vec<Value> =
        super::views::stages_of(ctx.cmd).into_iter().map(Value::Document).collect();
    let mut cursor = Document::new();
    if let Some(size) = ctx.cmd.get("batchSize") {
        cursor.push("batchSize", size.clone());
    }
    let mut agg = Document::new()
        .with("aggregate", ns.coll.clone())
        .with("pipeline", Value::Array(stages))
        .with("cursor", cursor);
    for option in ["collation", "let", "readConcern", "maxTimeMS", "comment"] {
        if let Some(v) = ctx.cmd.get(option) {
            agg.push(option, v.clone());
        }
    }
    let mut inner = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db: ctx.db, cmd: &agg };
    super::aggregate::aggregate_from(&mut inner, true)
}

/// Refuses a tailable `find` on a collection that is not capped, as a `refrouter` 9.0.4 does
/// (measured): a view or a time series cannot be tailed, and any other collection answers with
/// the query it was given.
fn check_tailable(
    ns: &Namespace,
    coll: &CollectionEntry,
    a: &FindArgs,
) -> Result<(), CommandError> {
    use teseladb_meta::collection_types::CollectionKind;
    match super::kinds::kind_of(coll)? {
        CollectionKind::Capped { .. } => Ok(()),
        CollectionKind::View { .. } | CollectionKind::TimeSeries(_) => Err(CommandError::new(
            crate::error::INVALID_PIPELINE_OPERATOR,
            "Tailable cursors are not supported in aggregation.",
        )),
        CollectionKind::Normal | CollectionKind::Clustered { .. } => {
            let projection = a.projection.clone().unwrap_or_default();
            Err(CommandError::new(
                BAD_VALUE,
                format!(
                    "error processing query: ns={}Tree: {}\nSort: {{}}\nProj: {}\n tailable cursor requested on non capped collection",
                    full(ns),
                    query_tree(&a.filter),
                    super::display(&Value::Document(projection)),
                ),
            ))
        }
    }
}

/// A filter as RefDb writes its query tree in a message: each condition `path $op value`, and
/// `$and` over them, indented by four spaces, when there are several or none.
pub(super) fn query_tree(filter: &Document) -> String {
    let mut leaves = Vec::new();
    for (path, cond) in filter.iter() {
        match cond {
            Value::Document(ops) if ops.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => {
                for (op, v) in ops.iter() {
                    leaves.push(format!("{path} {op} {}", super::display(v)));
                }
            }
            v => leaves.push(format!("{path} $eq {}", super::display(v))),
        }
    }
    match leaves.as_slice() {
        [one] => one.clone(),
        many => std::iter::once("$and".to_owned())
            .chain(many.iter().map(|l| format!("    {l}")))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// The fields and directions of a sort document.
fn sort_directions(sort: &Document) -> Vec<(String, i32)> {
    sort.iter()
        .filter(|(k, _)| *k != "$natural")
        .map(|(k, v)| {
            let neg = match v {
                Value::Int32(n) => *n < 0,
                Value::Int64(n) => *n < 0,
                Value::Double(x) => *x < 0.0,
                _ => false,
            };
            (k.to_owned(), if neg { -1 } else { 1 })
        })
        .collect()
}

/// The documents an index plan found, in its order, skipped, limited and projected.
/// The documents of plan `planned` whose index key is within `bounds` (`min` and `max` of the
/// index with key pattern `pattern`).
fn bounded(
    (rows, keys, in_order): super::plan_exec::Planned,
    bounds: &super::min_max::Bounds,
    pattern: &Document,
) -> Result<super::plan_exec::Planned, CommandError> {
    let mut kept: super::plan_exec::Planned = (Vec::new(), Vec::new(), in_order);
    for (row, key) in rows.into_iter().zip(keys) {
        let inside = match &key {
            Some(k) => super::min_max::within(bounds, pattern, k)?,
            None => false,
        };
        if inside {
            kept.0.push(row);
            kept.1.push(key);
        }
    }
    Ok(kept)
}

/// The command's filter and projection come parsed in `plan`.
fn planned_cursor(
    plan: Plan,
    (rows, keys, _): super::plan_exec::Planned,
    a: &FindArgs,
) -> Result<QueryCursor, CommandError> {
    let plan = plan.with_vars(a.vars.as_ref());
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let take = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut docs = Vec::new();
    // The rows come from the node's own storage (003 FR-002).
    for (i, bytes) in rows.into_iter().enumerate().skip(skip).take(take) {
        let key = keys.get(i).and_then(Option::as_ref);
        docs.push(plan.matched_row(bytes, key)?);
    }
    let mut c = QueryCursor::buffered(docs);
    c.left = a.limit;
    Ok(c)
}

/// The matching documents of `range` from the newest key to the oldest (`$natural: -1`),
/// skipped, limited and projected.
fn backward<B: Backend + ?Sized>(
    backend: &mut B,
    range: (Vec<u8>, Vec<u8>),
    a: &FindArgs,
    collation: Option<&Document>,
) -> Result<QueryCursor, CommandError> {
    let plan = Plan::new(&a.filter, a.projection.as_ref(), collation)?.with_vars(a.vars.as_ref());
    let mut rows: Vec<Vec<u8>> = Vec::new();
    for_each_match_with(
        backend,
        range,
        &a.filter,
        collation,
        a.vars.as_ref(),
        a.concern,
        |_, b| {
            rows.push(b.to_vec());
            Ok(true)
        },
    )?;
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let take = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut docs = Vec::new();
    for bytes in rows.into_iter().rev().skip(skip).take(take) {
        docs.push(plan.matched_row(bytes, None)?);
    }
    let mut c = QueryCursor::buffered(docs);
    c.left = a.limit;
    Ok(c)
}

/// `find` on `system.views`: the definitions of the database's views from the catalog (015
/// FR-002), filtered, skipped, limited and projected; in the catalog's order, which is the
/// order of their `_id`.
fn system_views<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    a: &FindArgs,
) -> Result<Document, CommandError> {
    let plan = Plan::new(&a.filter, a.projection.as_ref(), a.collation.as_ref())?
        .with_vars(a.vars.as_ref());
    let mut docs = Vec::new();
    for d in super::kinds::view_documents(ctx.backend, &ns.db)? {
        let b = raw_bytes(&d)?;
        if let Some(out) = plan.apply(RawDoc::from_bytes(&b)?, None)? {
            docs.push(out);
        }
    }
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let limit = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let docs: Vec<Document> = docs.into_iter().skip(skip).take(limit).collect();
    let ns_text = full(ns);
    let mut state = QueryCursor::buffered(docs);
    let size = Some(a.batch_size.unwrap_or(DEFAULT_FIRST_BATCH));
    let (first, short) = state.batch(ctx.backend, size)?;
    let id = keep(ctx, &ns_text, state, short || a.single_batch, a.no_timeout)?;
    Ok(cursor_reply(id, &ns_text, "firstBatch", first))
}

/// The documents of `docs` that match `plan`'s filter, in BSON.
fn generated_matches(plan: &Plan, docs: &[Document]) -> Result<Vec<Vec<u8>>, CommandError> {
    let ctx = plan.ctx();
    let mut out = Vec::new();
    for d in docs {
        let b = raw_bytes(d)?;
        if plan.filter.match_position(RawDoc::from_bytes(&b)?, &ctx).map_err(from_mql)?.is_some() {
            out.push(b);
        }
    }
    Ok(out)
}

/// `find` on a collection the node generates rather than reads from a range (the users and
/// roles of `admin`, 013 FR-030): filtered, sorted, skipped, limited and projected.
fn generated_find<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    a: &FindArgs,
    docs: Vec<Document>,
) -> Result<Document, CommandError> {
    let plan = Plan::new(&a.filter, a.projection.as_ref(), a.collation.as_ref())?
        .with_vars(a.vars.as_ref());
    plan.check_metadata(a.sort.as_ref())?;
    let collation = collation_of(a.collation.as_ref())?;
    let col: Option<&dyn teseladb_bson::Collator> = collation.as_ref().map(|c| c as _);
    let mut rows: Vec<(Vec<SortKey>, Vec<u8>)> = Vec::new();
    for b in generated_matches(&plan, &docs)? {
        let key = match &a.sort {
            Some(s) => s.keys(RawDoc::from_bytes(&b)?, col),
            None => Vec::new(),
        };
        rows.push((key, b));
    }
    if let Some(s) = &a.sort {
        rows.sort_by(|x, y| s.compare_keys(&x.0, &y.0, col));
    }
    // `$natural: -1` reads the generated documents from the last (the oplog's newest entry).
    if a.natural == Some(false) {
        rows.reverse();
    }
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let limit = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut out = Vec::new();
    for (key, b) in rows.into_iter().skip(skip).take(limit) {
        let key: Vec<Value> = key.into_iter().map(|k| k.0).collect();
        out.push(plan.matched(RawDoc::from_bytes(&b)?, Some(&key), None)?);
    }
    buffered_reply(ctx, &full(ns), a, out, false)
}

/// The reply of a `find` whose documents are all in `out`: its first batch, and a cursor that
/// `getMore` continues unless the batch holds them all and the read knows it (`ends`).
fn buffered_reply<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns_text: &str,
    a: &FindArgs,
    out: Vec<Document>,
    ends: bool,
) -> Result<Document, CommandError> {
    let mut state = QueryCursor::buffered(out);
    let size = Some(a.batch_size.unwrap_or(DEFAULT_FIRST_BATCH));
    let (first, short) = state.batch(ctx.backend, size)?;
    let done = short || a.single_batch || (ends && state.buffered.is_empty());
    let id = keep(ctx, ns_text, state, done, a.no_timeout)?;
    Ok(cursor_reply(id, ns_text, "firstBatch", first))
}

/// An error found while command `command` (`find`, `count`, `distinct`) reads `ns`, as RefDb
/// words it once the command was parsed (measured on a `refrouter` 9.0.4); a time limit, an
/// interruption and a read preference no replica satisfies (the router's own error, measured
/// 01-11-2026 on `find`, `count` and `distinct`) keep their own message.
fn executor_error(command: &str, ns: &str, mut e: CommandError) -> CommandError {
    if outside_executor(&e) {
        return e;
    }
    e.errmsg = if command == FIND {
        format!("Executor error during find command: {ns} :: caused by :: {}", e.errmsg)
    } else {
        format!(
            "Executor error during {command} command on namespace: {ns} :: caused by :: {}",
            e.errmsg
        )
    };
    e
}

/// Whether `e` keeps its own message instead of the executor's (time limit, interruption, read
/// preference).
fn outside_executor(e: &CommandError) -> bool {
    [
        crate::error::MAX_TIME_MS_EXPIRED.code,
        crate::error::INTERRUPTED.code,
        crate::error::FAILED_TO_SATISFY_READ_PREFERENCE.code,
    ]
    .contains(&e.code.code)
}

/// Checks the shape of a `find` projection as the command parses it (inclusion and exclusion
/// together, positional operators…).
pub(crate) fn check_projection_shape(projection: &Document) -> Result<(), CommandError> {
    Plan::new(&Document::new(), Some(projection), None).map(|_| ())
}

/// The operators of a `find` projection that are no expression.
const PROJECTION_OPERATORS: [&str; 3] = ["$slice", "$elemMatch", "$meta"];

/// Checks the computed fields of a `find` projection before any document is read, as RefDb
/// parses them with the command (an unknown operator is 31325 there).
pub(crate) fn check_projection(
    projection: &Document,
    vars: Option<&Document>,
) -> Result<(), CommandError> {
    let names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
    let ctx = teseladb_exec::expr::Ctx::checked(names);
    let bytes = raw_bytes(projection)?;
    check_computed(RawDoc::from_bytes_with(&bytes, command_limits())?, &ctx)
}

fn check_computed(fields: RawDoc<'_>, ctx: &teseladb_exec::expr::Ctx) -> Result<(), CommandError> {
    for (_, v) in fields.iter() {
        match v {
            RawValue::Document(d) => match d.iter().next() {
                Some((op, _)) if PROJECTION_OPERATORS.contains(&op) => {}
                Some((op, _)) if op.starts_with('$') => {
                    teseladb_exec::pipeline::projected_expr(v, ctx).map_err(from_mql)?;
                    optimized(v, ctx)?;
                }
                _ => check_computed(d, ctx)?,
            },
            RawValue::String(s) if s.starts_with('$') => {
                teseladb_exec::pipeline::projected_expr(v, ctx).map_err(from_mql)?;
            }
            // An array is a literal, folded as an expression's.
            RawValue::Array(_) => optimized(v, ctx)?,
            _ => {}
        }
    }
    Ok(())
}

/// A computed field of a projection as RefDb optimises it when it parses it: a constant that
/// fails fails the command with its own message.
fn optimized(v: RawValue<'_>, ctx: &teseladb_exec::expr::Ctx) -> Result<(), CommandError> {
    let scope = teseladb_exec::expr::Scope::default();
    teseladb_exec::expr::optimize::optimize(&Value::from(v), ctx, &scope).map_err(from_mql)?;
    Ok(())
}

/// Checks the filter of a read before any document is read, as RefDb parses it with the
/// command: its operators, the expressions of its `$expr`s with the variables `vars`, and
/// their constants.
pub(crate) fn check_filter(
    filter: &Document,
    collation: Option<&Document>,
    vars: Option<&Document>,
) -> Result<(), CommandError> {
    check_planned_filter(&Plan::new(filter, None, collation)?, filter, vars).map(drop)
}

/// [`check_filter`] with the filter already parsed in `plan`; the filter with its `$expr`s
/// optimised, or `None` when it has none and stays as it is.
fn check_planned_filter(
    plan: &Plan,
    filter: &Document,
    vars: Option<&Document>,
) -> Result<Option<Document>, CommandError> {
    check_planned_syntax(plan, filter, vars)?;
    if !has_expr(filter) {
        return Ok(None);
    }
    // A constant of an `$expr` that fails fails the command before any document is read.
    teseladb_exec::expr::optimize::optimize_filter(filter, vars).map(Some).map_err(from_mql)
}

/// Whether `$expr` appears anywhere in `d`: only then is there an expression to check or to
/// optimise.
fn has_expr(d: &Document) -> bool {
    fn in_value(v: &Value) -> bool {
        match v {
            Value::Document(d) => has_expr(d),
            Value::Array(items) => items.iter().any(in_value),
            _ => false,
        }
    }
    d.iter().any(|(k, v)| k == "$expr" || in_value(v))
}

/// Checks the filter of a read as RefDb parses it, before it optimises its `$expr`s.
pub(crate) fn check_filter_syntax(
    filter: &Document,
    collation: Option<&Document>,
    vars: Option<&Document>,
) -> Result<(), CommandError> {
    check_planned_syntax(&Plan::new(filter, None, collation)?, filter, vars)
}

/// [`check_filter_syntax`] with the filter already parsed in `plan`.
fn check_planned_syntax(
    plan: &Plan,
    filter: &Document,
    vars: Option<&Document>,
) -> Result<(), CommandError> {
    super::engines::check_geo(&plan.filter).map_err(from_mql)?;
    super::js::check_where(&plan.filter).map_err(from_mql)?;
    if !has_expr(filter) {
        return Ok(());
    }
    let names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
    let bytes = raw_bytes(filter)?;
    teseladb_exec::pipeline::check_exprs(
        RawDoc::from_bytes_with(&bytes, command_limits())?,
        &teseladb_exec::expr::Ctx::checked(names),
    )
    .map_err(from_mql)
}

/// The collation in a collection's options.
pub fn options_collation(c: &CollectionEntry) -> Option<Document> {
    let raw = RawDoc::from_bytes(&c.options).ok()?;
    match raw.get("collation") {
        Some(teseladb_bson::RawValue::Document(d)) => Some(Document::from(d)),
        _ => None,
    }
}

/// The documents of `range` that match, in the order of `sort`: the whole collection, or the key
/// of an `_id` equality, which gives one document at most whatever the sort.
/// The command's filter and projection come parsed in `plan`.
fn sorted<B: Backend + ?Sized>(
    backend: &mut B,
    (plan, range): (Plan, (Vec<u8>, Vec<u8>)),
    a: &FindArgs,
    sort: &SortSpec,
    collation: Option<&Document>,
    planned: Option<super::plan_exec::Planned>,
) -> Result<QueryCursor, CommandError> {
    let plan = plan.with_vars(a.vars.as_ref());
    let coll_collation = collation_of(collation)?;
    let col: Option<&dyn teseladb_bson::Collator> = coll_collation.as_ref().map(|c| c as _);
    // Each document stays in its stored bytes until the plan gives it (H-009 of the native
    // tests: a tree built for the sort, written again and read again cost more than the read).
    let mut rows: Vec<(Vec<SortKey>, Vec<u8>, Option<Document>)> = Vec::new();
    let mut memory = 0usize;
    let mut err = None;
    let vars = a.vars.as_ref();
    let mut keep = |bytes: Vec<u8>, index_key: Option<Document>| -> Result<bool, CommandError> {
        memory += bytes.len();
        if memory > SORT_MEMORY_LIMIT && !a.allow_disk_use {
            err = Some(CommandError::new(
                QUERY_EXCEEDED_MEMORY_LIMIT,
                "Sort exceeded memory limit of 104857600 bytes, but did not opt in to external sorting.",
            ));
            return Ok(false);
        }
        let keys = sort.keys(RawDoc::from_bytes(&bytes)?, col);
        rows.push((keys, bytes, index_key));
        Ok(true)
    };
    match planned {
        // The plan's documents, in its order (an index that gives the sort's order keeps it).
        Some((docs, keys, _)) => {
            let mut keys = keys.into_iter();
            for d in docs {
                if !keep(d, keys.next().flatten())? {
                    break;
                }
            }
        }
        None => {
            for_each_match_with(backend, range, &a.filter, collation, vars, a.concern, |_, b| {
                keep(b.to_vec(), None)
            })?;
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    rows.sort_by(|x, y| sort.compare_keys(&x.0, &y.0, col));
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let take = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut docs = Vec::new();
    for (key, bytes, index_key) in rows.into_iter().skip(skip).take(take) {
        let raw = RawDoc::from_bytes(&bytes)?;
        let key: Vec<Value> = key.into_iter().map(|k| k.0).collect();
        docs.push(plan.matched(raw, Some(&key), index_key.as_ref())?);
    }
    let mut c = QueryCursor::buffered(docs);
    c.left = a.limit;
    Ok(c)
}

/// Whether `filter` reads one `_id` by equality to a value that is not a document, an array or
/// a regular expression, which RefDb answers without the planner (`EXPRESS`): it gives no
/// index key then (measured).
pub(crate) fn id_point(filter: &Document) -> bool {
    let mut fields = filter.iter();
    match (fields.next(), fields.next()) {
        (Some(("_id", v)), None) => {
            !matches!(v, Value::Document(_) | Value::Array(_) | Value::Regex { .. })
        }
        _ => false,
    }
}

/// Whether projection `p` reads `{$meta: "indexKey"}`, at any depth.
fn reads_index_key(p: Option<&Document>) -> bool {
    reads_meta(p, "indexKey")
}

/// Whether projection `p` reads `{$meta: <meta>}`, at any depth.
fn reads_meta(p: Option<&Document>, meta: &str) -> bool {
    fn reads(v: &Value, meta: &str) -> bool {
        match v {
            Value::Document(d) => {
                matches!(d.get("$meta"), Some(Value::String(m)) if m == meta)
                    || d.iter().any(|(_, v)| reads(v, meta))
            }
            Value::Array(a) => a.iter().any(|v| reads(v, meta)),
            _ => false,
        }
    }
    p.is_some_and(|d| d.iter().any(|(_, v)| reads(v, meta)))
}

/// `getMore`.
pub fn get_more<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let start = super::query_stats_start(ctx.backend);
    let reply = next_batch(ctx);
    let Ok(id) = args::long(ctx.first(), "getMore.getMore") else { return reply };
    let (docs, open) = match &reply {
        Ok(r) => match r.get("cursor") {
            Some(Value::Document(c)) => (
                match c.get("nextBatch") {
                    Some(Value::Array(b)) => i64::try_from(b.len()).unwrap_or(i64::MAX),
                    _ => 0,
                },
                c.get("id") == Some(&Value::Int64(id)),
            ),
            _ => (0, false),
        },
        Err(_) => (0, lock(ctx.shared).cursors.get(id).is_some()),
    };
    let batch = super::query_stats_batch(ctx.backend, &start, docs);
    let now = i64::try_from(ctx.backend.now_ms()).unwrap_or(i64::MAX);
    let mut g = lock(ctx.shared);
    g.query_stats.more(id, batch);
    if !open {
        g.query_stats.close(id, now);
    }
    drop(g);
    reply
}

/// The next batch of a cursor (`getMore`).
fn next_batch<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let id = args::long(ctx.first(), "getMore.getMore")?;
    let coll = args::string(ctx.cmd, GET_MORE, "collection")?
        .ok_or_else(|| missing(GET_MORE, "collection"))?;
    let ns = names::ns(ctx.db, coll);
    names::check(&ns)?;
    let ns_text = full(&ns);
    let batch = non_negative(args::int(ctx.cmd, GET_MORE, "batchSize")?, "batchSize")?
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        .filter(|&n| n > 0);
    let max_time = crate::ops::max_time_ms(ctx.cmd, GET_MORE)?.filter(|ms| *ms > 0);
    // `term` is a `long`, checked before the cursor (measured on a `refrouter` 9.0.4 on 05-11-2026).
    if let Some(v) = ctx.cmd.get("term").filter(|v| !matches!(v, Value::Int64(_))) {
        return Err(crate::types::wrong_type("getMore.term", v, "long"));
    }
    let lsid = args::lsid(ctx.cmd)?;
    let now = ctx.backend.now_ms();
    let taken = {
        let mut g = lock(ctx.shared);
        let tenant = ctx.backend.tenant();
        match g.cursors.get(id).map(|c| (split_db(&c.ns), c.lsid)) {
            // Another tenant's cursor is not there for this one (013 FR-024).
            Some(((t, _), _)) if t != tenant => Err((CursorError::NotFound, None)),
            owner => {
                let owner = owner.map(|((_, n), l)| (n.to_owned(), l));
                let own_ns = qualified_db(tenant, &ns_text);
                g.cursors.take(id, &own_ns, lsid, now).map_err(|e| (e, owner))
            }
        }
    };
    let mut c = match taken {
        Ok(c) => c,
        Err((e, owner)) => {
            let member = super::config_db::as_member(ctx.backend)?;
            let user = ctx.backend.authenticated_user();
            let uid = super::txn::user_uid(user.as_ref());
            let checked = user.is_some() && checks_cursor_sessions(ctx.backend);
            let node = Refuser { member, checked, uid: &uid };
            return Err(get_more_refused((id, &ns_text, lsid), e, owner, node));
        }
    };
    // `failGetMoreAfterCursorCheckout` (FR-034): the cursor was taken, and the error kills it as
    // on a `refrouter` 9.0.4 (measured).
    let stream = c.state.stream.is_some();
    let fault = lock(ctx.shared).failpoint.as_mut().and_then(|fp| fp.get_more_error(stream));
    if let Some(e) = fault {
        return Err(e);
    }
    // At the mark of the command that opened it (008 FR-012).
    ctx.backend.set_read_mark(c.state.read_mark);
    // The profiler writes the command that opened it and that command's plan (010 FR-015).
    if let Some(origin) = &c.state.origin {
        let mut plan = origin.plan.clone().unwrap_or_default();
        plan.push(ORIGINATING_COMMAND, origin.command.clone());
        ctx.backend.note_plan(plan);
    }
    // `maxTimeMS` of a `getMore` is only the wait of a tailable `awaitData` cursor. The cursor
    // was taken: like a `refrouter` 9.0.4, the error kills it (measured).
    let awaits = c.state.stream.is_some() || c.state.tail.as_ref().is_some_and(|t| t.await_data);
    if max_time.is_some() && !awaits {
        let msg = "maxTimeMS can only be used with getMore for tailable, awaitData cursors";
        return Err(CommandError::new(crate::error::BAD_VALUE, msg));
    }
    if let Some(stream) = c.state.stream.as_mut() {
        let (docs, token, ended) =
            super::change_stream::more(ctx.backend, stream, batch, max_time)?;
        // An `invalidate` closes the cursor in the same batch (measured on 01-11).
        let id = if ended {
            0
        } else {
            lock(ctx.shared).cursors.put(id, c);
            id
        };
        let docs: Vec<Value> = docs.into_iter().map(Value::Document).collect();
        let cursor = Document::new()
            .with("id", id)
            .with("ns", ns_text)
            .with("nextBatch", docs)
            .with("postBatchResumeToken", token);
        return Ok(Document::new().with("cursor", cursor).with("ok", 1.0));
    }
    if c.state.tail.is_some() {
        let docs = tail_more(ctx.backend, &mut c.state, batch, max_time)?;
        lock(ctx.shared).cursors.put(id, c);
        return Ok(cursor_reply(id, &ns_text, "nextBatch", docs));
    }
    let (docs, short) = c.state.batch(ctx.backend, batch)?;
    let exhausted = short || c.state.left == Some(0);
    let out_id = if exhausted {
        0
    } else {
        lock(ctx.shared).cursors.put(id, c);
        id
    };
    Ok(cursor_reply(out_id, &ns_text, "nextBatch", docs))
}

/// The next batch of a tailable cursor (015 FR-004): the documents written after the last one it
/// read, waiting for some up to its wait when it has `awaitData`. Its last document gone is
/// `CappedPositionLost`, and the cursor dies.
fn tail_more<B: Backend + ?Sized>(
    backend: &mut B,
    state: &mut QueryCursor,
    batch: Option<usize>,
    max_time: Option<u64>,
) -> Result<Vec<Document>, CommandError> {
    let (last, wait) = match &state.tail {
        Some(t) => (t.last.clone(), t.wait_ms(max_time)),
        None => {
            return Err(CommandError::new(crate::error::INTERNAL_ERROR, "not a tailable cursor"));
        }
    };
    if let (Some(last), Source::Scan { end, .. }) = (&last, &state.source) {
        let (_, versions) = backend.read_versions(std::slice::from_ref(last))?;
        if versions.first().is_none_or(Option::is_none) {
            let newer = backend.scan(&after(last), end, 1, ReadConcern::Local)?;
            let seq: [u8; 8] = last
                .get(last.len().saturating_sub(8)..)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| {
                    CommandError::new(crate::error::INTERNAL_ERROR, "a capped key without seq")
                })?;
            // RefDb numbers the records of a collection from 1.
            let record = u64::from_be_bytes(seq).saturating_add(1);
            let msg = lost_message(newer.is_empty(), record);
            return Err(CommandError::new(crate::error::CAPPED_POSITION_LOST, msg));
        }
    }
    let deadline = backend.now_ms().saturating_add(wait);
    loop {
        // Each wait reads again past the newest document it saw.
        state.tail.iter_mut().for_each(|t| t.caught_up = false);
        let (docs, _) = match state.batch(backend, batch) {
            Ok(read) => read,
            // The `maxTimeMS` of the `getMore` ran out during the wait, while it read again: like
            // a `refnode` 9.0.4, an `awaitData` cursor answers an empty batch and stays open.
            Err(e) if wait > 0 && e.code.code == crate::error::MAX_TIME_MS_EXPIRED.code => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        if !docs.is_empty() || backend.now_ms() >= deadline || !backend.pause() {
            return Ok(docs);
        }
    }
}

/// Whether the session check of RefDb's `AuthorizationSessionImpl` applies to the running
/// command's user: the node authorizes commands and the user has no `impersonate` on the cluster
/// (`__system`). The caller has checked that a user is authenticated.
fn checks_cursor_sessions<B: Backend + ?Sized>(backend: &B) -> bool {
    backend.privileges().is_some_and(|p| {
        !teseladb_auth::authz::allows(
            &p,
            &teseladb_auth::authz::Target::Cluster,
            teseladb_auth::actions::ActionType::Impersonate,
        )
    })
}

/// The node refusing a `getMore`: whether it presents itself as a member of a replica set,
/// whether the authorization's session check applies, and the user's `uid`.
struct Refuser<'a> {
    member: bool,
    checked: bool,
    uid: &'a str,
}

/// The error of a `getMore` on `ns` in session `lsid` that cursor `id`, of namespace and session
/// `owner`, refused with `e`, as RefDb 9.0.4 gives it. With an authenticated user on a node
/// that authorizes, a session mismatch is 13 `Unauthorized` naming both sessions
/// (`AuthorizationSessionImpl::checkCursorSessionPrivilege`); otherwise a `refrouter` answers 50799,
/// 50800 or 50801 (`validateLSID` in `cluster_find.cpp`) and a member of a replica set 50736,
/// 50737 or 50738. Sessions are named with the user's `uid`.
fn get_more_refused(
    (id, ns, lsid): (i64, &str, Option<u128>),
    e: CursorError,
    owner: Option<(String, Option<u128>)>,
    node: Refuser<'_>,
) -> CommandError {
    let (cursor_ns, cursor_lsid) = owner.unwrap_or_default();
    let named = |l: u128| session_text(l).replace(super::ANONYMOUS_UID, node.uid);
    let created = |l: Option<u128>| named(l.unwrap_or_default());
    let shown = |l: Option<u128>| l.map_or_else(|| NO_SESSION.to_owned(), named);
    let codes = match node.member {
        true => [CURSOR_NOT_IN_SESSION, CURSOR_SESSION_REQUIRED, CURSOR_OTHER_SESSION],
        false => [
            crate::error::ROUTER_CURSOR_NOT_IN_SESSION,
            crate::error::ROUTER_CURSOR_SESSION_REQUIRED,
            crate::error::ROUTER_CURSOR_OTHER_SESSION,
        ],
    };
    match e {
        CursorError::NotFound if node.member => {
            CommandError::new(CURSOR_NOT_FOUND, format!("cursor id {id} not found"))
        }
        CursorError::NotFound => {
            CommandError::new(CURSOR_NOT_FOUND, format!("Cursor not found (id: {id})."))
        }
        CursorError::OtherNamespace => CommandError::new(
            UNAUTHORIZED,
            format!(
                "Requested getMore on namespace '{ns}', but cursor belongs to a different \
                 namespace {cursor_ns}"
            ),
        ),
        CursorError::NotInSession | CursorError::SessionRequired | CursorError::OtherSession
            if node.checked =>
        {
            CommandError::new(
                UNAUTHORIZED,
                format!(
                    "Cursor session id ({}) is not the same as the operation context's session \
                     id ({})",
                    shown(cursor_lsid),
                    shown(lsid)
                ),
            )
        }
        CursorError::NotInSession => CommandError::new(
            codes[0],
            format!(
                "Cannot run getMore on cursor {id}, which was not created in a session, in \
                 session {}",
                created(lsid)
            ),
        ),
        CursorError::SessionRequired => CommandError::new(
            codes[1],
            format!(
                "Cannot run getMore on cursor {id}, which was created in session {}, without \
                 an lsid",
                created(cursor_lsid)
            ),
        ),
        CursorError::OtherSession => CommandError::new(
            codes[2],
            format!(
                "Cannot run getMore on cursor {id}, which was created in session {}, in \
                 session {}",
                created(cursor_lsid),
                created(lsid)
            ),
        ),
    }
}

/// How a `refrouter` names a missing session in its messages.
const NO_SESSION: &str = "none";

/// `killCursors`: the namespace is validated but, as on a `refrouter` 9.0.4 (measured), it does
/// not have to be the cursor's.
pub fn kill_cursors<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    target(ctx)?;
    let ids = args::array(ctx.cmd, KILL_CURSORS, "cursors")?
        .ok_or_else(|| missing(KILL_CURSORS, "cursors"))?;
    let mut parsed = Vec::new();
    for (i, v) in ids.iter().enumerate() {
        parsed.push(args::long(v, &format!("killCursors.cursors.{i}"))?);
    }
    let (mut killed, mut not_found) = (Vec::new(), Vec::new());
    let now = i64::try_from(ctx.backend.now_ms()).unwrap_or(i64::MAX);
    let tenant = ctx.backend.tenant();
    let mut guard = lock(ctx.shared);
    for id in parsed {
        // Another tenant's cursor is not there for this one (013 FR-024).
        let own = guard.cursors.get(id).is_some_and(|c| split_db(&c.ns).0 == tenant);
        if own && guard.cursors.remove(id) {
            guard.query_stats.close(id, now);
            killed.push(Value::Int64(id));
        } else {
            not_found.push(Value::Int64(id));
        }
    }
    Ok(Document::new()
        .with("cursorsKilled", killed)
        .with("cursorsNotFound", not_found)
        .with("cursorsAlive", Vec::<Value>::new())
        .with("cursorsUnknown", Vec::<Value>::new())
        .with("ok", 1.0))
}

/// `count`.
pub fn count<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    let filter = args::doc(ctx.cmd, COUNT, "query")?.cloned().unwrap_or_default();
    let skip = non_negative(args::int_of(ctx.cmd, COUNT, "skip", &COUNT_NUMBER_TYPES)?, "skip")?
        .unwrap_or(0);
    let limit = match ctx.cmd.get("limit") {
        None => None,
        Some(v) => Some(crate::ops::exact_integer(v, "limit").unwrap_or_else(|| {
            Err(wrong_types(&format!("{COUNT}.limit"), v, &COUNT_NUMBER_TYPES))
        })?),
    }
    .map(i64::unsigned_abs)
    .filter(|&l| l > 0);
    let concern = args::read_concern(ctx.cmd, COUNT)?;
    let collation = args::doc(ctx.cmd, COUNT, "collation")?.cloned();
    super::hints::check_hint_type(ctx.cmd)?;
    // A `refrouter` parses the filter and the collation itself; what fails after that fails on its
    // shard and comes back after the shard's name, here the node's (D-102).
    check_filter_syntax(&filter, collation.as_ref(), None)?;
    let node = ctx.backend.node();
    let asked = CountArgs { filter, skip, limit, concern, collation };
    count_on_node(ctx, &ns, asked)
        .map_err(|e| if outside_executor(&e) { e } else { super::failed_on(node, e) })
}

/// What `count` asks once parsed.
struct CountArgs {
    filter: Document,
    skip: u64,
    limit: Option<u64>,
    concern: ReadConcern,
    collation: Option<Document>,
}

/// The part of `count` a shard runs.
fn count_on_node<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    a: CountArgs,
) -> Result<Document, CommandError> {
    let CountArgs { filter, skip, limit, concern, collation } = a;
    let node = ctx.backend.node();
    // A `refrouter` asks its shard for `skip + limit` documents and skips them itself: the shard's
    // count is the matches up to that bound.
    let bound = limit.map(|l| l.saturating_add(skip));
    if let Some(view) = super::views::resolve(ctx.backend, ns)? {
        super::views::check_collation(&view, collation.as_ref())?;
        let mut stages = super::views::distinct_stages(&filter);
        if let Some(b) = bound {
            stages.push(Document::new().with("$limit", i64::try_from(b).unwrap_or(i64::MAX)));
        }
        // The view's pipeline fails as it parses with its own error, and as it runs as the
        // aggregation a `refrouter` makes of the count (measured).
        let (docs, _) = super::aggregate::read_through_staged(
            ctx.backend,
            ctx.shared,
            ns,
            (&stages, None),
            concern,
        )?
        .map_err(|e| {
            if outside_executor(&e) {
                return e;
            }
            let mut e = executor_error(super::aggregate::AGGREGATE, &full(ns), e);
            e.code = crate::error::COUNT_ON_VIEW_FAILED;
            e
        })?;
        let matched = u64::try_from(docs.len()).unwrap_or(u64::MAX);
        return Ok(counted(node, matched, skip, limit));
    }
    let mut matched: u64 = 0;
    let generated = match super::authz_collections::documents(ctx.backend, ns)? {
        None if ns.coll == super::kinds::SYSTEM_VIEWS => {
            Some(super::kinds::view_documents(ctx.backend, &ns.db)?)
        }
        other => other,
    };
    match ctx.backend.collection(ns)? {
        _ if generated.is_some() => {
            let plan = Plan::new(&filter, None, collation.as_ref())?;
            let all = generated_matches(&plan, generated.as_deref().unwrap_or_default())?;
            let all = u64::try_from(all.len()).unwrap_or(u64::MAX);
            matched = bound.map_or(all, |b| all.min(b));
        }
        None => {}
        Some(c) => {
            let collation = collation.or_else(|| options_collation(&c));
            check_filter(&filter, collation.as_ref(), None)?;
            let hinted =
                super::hints::Hinted { filter: &filter, sort: None, projection: None, vars: None };
            super::hints::check_hint(ctx.backend, (ns, &c), ctx.cmd, &hinted)?;
            let ns_text = full(ns);
            // Without a filter, the counts its ranges keep answer without reading a document
            // (007 FR-039, H-004); when one is estimated, the documents are read as before.
            if filter.is_empty()
                && let Some(n) = ctx.backend.doc_count(c.tenant, c.id)?
            {
                return Ok(counted(node, bound.map_or(n, |b| n.min(b)), skip, limit));
            }
            // One `_id` counts its document without the planner (`IDHACK`, as RefDb).
            if let Some(id) = express_id(&c, &filter, collation.as_ref(), ctx.cmd)? {
                let n =
                    u64::try_from(read_by_id(ctx.backend, &c, &id, concern)?.len()).unwrap_or(0);
                return Ok(counted(node, bound.map_or(n, |b| n.min(b)), skip, limit));
            }
            // An index plan that wins counts its documents (T019).
            if collation.is_none()
                && let Some(docs) = planned(ctx, (ns, &c), &filter, bound, concern, None)
                    .map_err(|e| executor_error(COUNT, &ns_text, e))?
            {
                let n = u64::try_from(docs.len()).unwrap_or(u64::MAX);
                return Ok(counted(node, bound.map_or(n, |b| n.min(b)), skip, limit));
            }
            count_scan(ctx.shared, ns);
            for_each_doc(
                ctx.backend,
                (&c, ctx.cmd),
                &filter,
                collation.as_ref(),
                None,
                concern,
                |_, _| {
                    matched += 1;
                    Ok(bound.is_none_or(|b| matched < b))
                },
            )
            .map_err(|e| executor_error(COUNT, &ns_text, e))?;
        }
    }
    Ok(counted(node, matched, skip, limit))
}

/// The reply of `count`: `n` after the skip and the limit, and the node's own count of
/// `matched` under `shards`, as a `refrouter` gives its shard's.
fn counted(node: u64, matched: u64, skip: u64, limit: Option<u64>) -> Document {
    let left = matched.saturating_sub(skip);
    let n = limit.map_or(left, |l| left.min(l));
    let int = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
    Document::new()
        .with("n", int(n))
        .with("shards", Document::new().with(super::shard_name(node), int(matched)))
        .with("ok", 1.0)
}

/// Adds to `found` the values at `key` of document `bytes` that it does not hold yet (an array
/// gives each of its items).
fn add_distinct(
    found: &mut Vec<(SortKey, Value)>,
    bytes: &[u8],
    key: &str,
    spec: &SortSpec,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<(), CommandError> {
    let raw = RawDoc::from_bytes(bytes)?;
    for v in super::sort::values_at(raw, key) {
        let items: Vec<teseladb_bson::RawValue<'_>> = match v {
            teseladb_bson::RawValue::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        for item in items {
            let value = Value::from(item);
            let holder = Document::new().with("v", value.clone());
            let hb = raw_bytes(&holder)?;
            let hr = RawDoc::from_bytes(&hb)?;
            let k = spec.keys(hr, col).remove(0);
            let dup = found.iter().any(|(fk, _)| {
                spec.compare_keys(std::slice::from_ref(fk), std::slice::from_ref(&k), col).is_eq()
            });
            if !dup {
                found.push((k, value));
            }
        }
    }
    Ok(())
}

/// `distinct` on view `ns`: the view's documents matching `filter`, then their values at `key`
/// in the view's collation (015 FR-001).
fn view_distinct<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    view: &teseladb_plan::views::Resolved,
    (key, filter): (&str, &Document),
    given: Option<&Document>,
    concern: ReadConcern,
) -> Result<Document, CommandError> {
    super::views::check_collation(view, given)?;
    if key.is_empty() {
        return Err(CommandError::new(
            super::sort::EMPTY_PATH,
            "FieldPath cannot be constructed with empty string",
        ));
    }
    let stages = super::views::distinct_stages(filter);
    let docs =
        super::aggregate::read_through_node(ctx.backend, ctx.shared, ns, (&stages, None), concern)?;
    let own = match &view.collation {
        Some(b) => Some(Document::from(RawDoc::from_bytes(b)?)),
        None => None,
    };
    let collation = collation_of(own.as_ref())?;
    let col: Option<&dyn teseladb_bson::Collator> = collation.as_ref().map(|c| c as _);
    let spec = SortSpec { fields: vec![(String::from("v"), true)], metas: Vec::new() };
    let mut found: Vec<(SortKey, Value)> = Vec::new();
    for d in docs {
        add_distinct(&mut found, &d.to_bytes()?, key, &spec, col)?;
    }
    found.sort_by(|a, b| {
        spec.compare_keys(std::slice::from_ref(&a.0), std::slice::from_ref(&b.0), col)
    });
    let values: Vec<Value> = found.into_iter().map(|(_, v)| v).collect();
    Ok(Document::new().with("values", values).with("ok", 1.0))
}

/// `distinct`.
pub fn distinct<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    // A required field that is missing fails before the collection name (measured).
    if ctx.cmd.get("key").is_none() {
        return Err(missing(DISTINCT, "key"));
    }
    let ns = target(ctx)?;
    let key = match ctx.cmd.get("key") {
        Some(Value::String(k)) => k.clone(),
        Some(v) => return Err(wrong_type("distinct.key", v, "string")),
        None => return Err(missing(DISTINCT, "key")),
    };
    let filter = args::doc(ctx.cmd, DISTINCT, "query")?.cloned().unwrap_or_default();
    super::hints::check_hint_type(ctx.cmd)?;
    let concern = args::read_concern(ctx.cmd, DISTINCT)?;
    let collation_doc = args::doc(ctx.cmd, DISTINCT, "collation")?.cloned();
    let spec = SortSpec { fields: vec![(String::from("v"), true)], metas: Vec::new() };
    let mut found: Vec<(SortKey, Value)> = Vec::new();
    if let Some(view) = super::views::resolve(ctx.backend, &ns)? {
        return view_distinct(ctx, &ns, &view, (&key, &filter), collation_doc.as_ref(), concern);
    }
    let generated = super::authz_collections::documents(ctx.backend, &ns)?;
    let collation_doc = match (&generated, ctx.backend.collection(&ns)?) {
        (Some(docs), _) => {
            let collation = collation_of(collation_doc.as_ref())?;
            let col: Option<&dyn teseladb_bson::Collator> = collation.as_ref().map(|c| c as _);
            let plan = Plan::new(&filter, None, collation_doc.as_ref())?;
            for b in generated_matches(&plan, docs)? {
                add_distinct(&mut found, &b, &key, &spec, col)?;
            }
            collation_doc
        }
        (None, Some(c)) => {
            let collation_doc = collation_doc.or_else(|| options_collation(&c));
            let collation = collation_of(collation_doc.as_ref())?;
            let col: Option<&dyn teseladb_bson::Collator> = collation.as_ref().map(|c| c as _);
            check_filter(&filter, collation_doc.as_ref(), None)?;
            let hinted =
                super::hints::Hinted { filter: &filter, sort: None, projection: None, vars: None };
            super::hints::check_hint(ctx.backend, (&ns, &c), ctx.cmd, &hinted)?;
            let ns_text = full(&ns);
            // One `_id` reads its document without the planner, as RefDb's express path.
            if let Some(id) = express_id(&c, &filter, collation_doc.as_ref(), ctx.cmd)? {
                for bytes in read_by_id(ctx.backend, &c, &id, concern)? {
                    add_distinct(&mut found, &bytes, &key, &spec, col)?;
                }
                return Ok(Document::new()
                    .with("values", sorted_values(found, &spec, col))
                    .with("ok", 1.0));
            }
            // An index plan that wins gives the documents (T019).
            let through = if collation_doc.is_none() {
                planned(ctx, (&ns, &c), &filter, None, concern, Some(&key))
                    .map_err(|e| executor_error(DISTINCT, &ns_text, e))?
            } else {
                None
            };
            if let Some(docs) = through {
                for bytes in &docs {
                    add_distinct(&mut found, bytes, &key, &spec, col)?;
                }
                return Ok(Document::new()
                    .with("values", sorted_values(found, &spec, col))
                    .with("ok", 1.0));
            }
            count_scan(ctx.shared, &ns);
            for_each_doc(
                ctx.backend,
                (&c, ctx.cmd),
                &filter,
                collation_doc.as_ref(),
                None,
                concern,
                |_, bytes| {
                    add_distinct(&mut found, bytes, &key, &spec, col)?;
                    Ok(true)
                },
            )
            .map_err(|e| executor_error(DISTINCT, &ns_text, e))?;
            collation_doc
        }
        (None, None) => {
            Plan::new(&filter, None, collation_doc.as_ref())?;
            collation_doc
        }
    };
    let collation = collation_of(collation_doc.as_ref())?;
    let col: Option<&dyn teseladb_bson::Collator> = collation.as_ref().map(|c| c as _);
    Ok(Document::new().with("values", sorted_values(found, &spec, col)).with("ok", 1.0))
}

/// The values `distinct` found, in its order.
fn sorted_values(
    mut found: Vec<(SortKey, Value)>,
    spec: &SortSpec,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Vec<Value> {
    found.sort_by(|a, b| {
        spec.compare_keys(std::slice::from_ref(&a.0), std::slice::from_ref(&b.0), col)
    });
    found.into_iter().map(|(_, v)| v).collect()
}

/// The documents of `coll` matching `filter` that the winning index plan reads (`bound` at
/// most), or `None` when the collection scan wins.
fn planned<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    (ns, coll): (&Namespace, &CollectionEntry),
    filter: &Document,
    bound: Option<u64>,
    concern: ReadConcern,
    distinct: Option<&str>,
) -> Result<Option<Vec<Vec<u8>>>, CommandError> {
    let indexes = super::plan_exec::plan_indexes(ctx.backend, coll)?;
    if indexes.is_empty() {
        return Ok(None);
    }
    let parsed = super::plan_exec::parsed_filter(filter, None)?;
    let read = super::plan_exec::ReadPlan {
        filter,
        parsed: &parsed,
        sort: Vec::new(),
        limit: bound,
        hint: super::hints::position(ctx.cmd.get("hint"), &indexes)?,
        vars: None,
        concern,
        intersect: super::hot::of(ctx.shared)
            .hash_intersection
            .load(std::sync::atomic::Ordering::Relaxed),
        index_keys: false,
        key_only: false,
    };
    let note = super::plan_exec::CacheNote {
        shared: ctx.shared,
        ns,
        from: {
            let d = Document::new()
                .with("query", filter.clone())
                .with("sort", Document::new())
                .with("projection", Document::new());
            match distinct {
                Some(k) => d.with("distinct", Document::new().with("key", k)),
                None => d,
            }
        },
        shape: super::plan_cache::read_shape(
            &parsed,
            filter,
            None,
            None,
            super::plan_hash::distinct_key(
                &parsed,
                distinct,
                &super::plan_exec::key_indexes(&indexes),
            ),
        ),
    };
    super::plan_exec::planned_documents(ctx.backend, coll, &read, note)
}

#[cfg(test)]
mod tests {
    use super::super::explain::simple_projection;
    use teseladb_bson::extjson;

    fn simple(text: &str) -> bool {
        simple_projection(&extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}")))
    }

    #[test]
    fn a_simple_projection_includes_or_excludes_top_fields_with_numbers_or_booleans() {
        for p in [r#"{}"#, r#"{"v":1}"#, r#"{"_id":0,"w":1}"#, r#"{"v":true}"#, r#"{"v":0.5}"#] {
            assert!(simple(p), "{p}");
        }
        let long = r#"{"v":{"$numberLong":"0"}}"#;
        assert!(simple(long), "{long}");
        for p in [
            r#"{"a":{"b":1}}"#,
            r#"{"a.b":1}"#,
            r#"{"a.$":1}"#,
            r#"{"a":{"$slice":1}}"#,
            r#"{"a":{"$elemMatch":{"b":1}}}"#,
            r#"{"s":{"$meta":"textScore"}}"#,
            r#"{"w":"$v"}"#,
            r#"{"w":"text"}"#,
            r#"{"w":[1]}"#,
            r#"{"v":null}"#,
        ] {
            assert!(!simple(p), "{p}");
        }
    }
}
