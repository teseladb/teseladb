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

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_collation::{Collation, ParsedCollation, parse_collation};
use teseladb_keys::{DOCUMENTS, KeyBuilder};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_mql::projection::{Projection, ProjectionContext};
use teseladb_mql::{Filter, FilterUse, MatchContext};
use teseladb_range::read::ReadConcern;
use teseladb_session::cursor::{Cursor, CursorError, cursor_id};
use teseladb_session::tailable::{Tail, lost_message};

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
/// Documents read from the range at a time.
pub const SCAN_CHUNK: usize = 1000;
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
fn raw_bytes(d: &Document) -> Result<Vec<u8>, CommandError> {
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
        let projection = match projection {
            Some(p) if !p.is_empty() => {
                let pb = raw_bytes(p)?;
                let raw = RawDoc::from_bytes_with(&pb, command_limits())?;
                Some(Projection::parse(raw).map_err(from_mql)?)
            }
            Some(_) | None => None,
        };
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
        let ctx = self.ctx();
        let Some(position) = self.filter.match_position(doc, &ctx).map_err(from_mql)? else {
            return Ok(None);
        };
        match &self.projection {
            None => Ok(Some(Document::from(doc))),
            Some(p) => {
                let pctx = ProjectionContext { matcher: ctx, position, sort_key };
                p.apply(doc, &pctx).map(Some).map_err(from_mql)
            }
        }
    }

    /// Document `doc`, which this plan's filter already matched, projected.
    fn matched(
        &self,
        doc: RawDoc<'_>,
        sort_key: Option<&[Value]>,
    ) -> Result<Document, CommandError> {
        self.apply(doc, sort_key)?.ok_or_else(|| {
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
        let rows = backend.scan(&next, &end, SCAN_CHUNK, concern)?;
        let short = rows.len() < SCAN_CHUNK;
        for (key, bytes) in rows {
            next = after(&key);
            examined += 1;
            // A stored document always parses: one that does not is a fault, not a miss.
            let doc = RawDoc::from_bytes(&bytes)?;
            if plan.filter.matches(doc, &ctx).map_err(from_mql)? && !each(key, &bytes)? {
                return Ok(examined);
            }
        }
        if short {
            return Ok(examined);
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
    let (mut next, end) = super::timeseries::scan_range(coll, &spec, &bounds)?;
    loop {
        let rows = backend.scan(&next, &end, SCAN_CHUNK, concern)?;
        let short = rows.len() < SCAN_CHUNK;
        for (key, bytes) in rows {
            next = after(&key);
            let bucket = RawDoc::from_bytes(&bytes)?;
            if !bounds.may_match(&spec, bucket, ctx.collation).map_err(ts_error)? {
                continue;
            }
            for mb in spec.unpack_raw(bucket).map_err(ts_error)? {
                let doc = RawDoc::from_bytes(&mb)?;
                if plan.filter.matches(doc, &ctx).map_err(from_mql)? && !each(key.clone(), &mb)? {
                    return Ok(());
                }
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
        docs.push(plan.matched(RawDoc::from_bytes(bytes)?, None)?);
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
                    let n = d.to_bytes().map_or(0, |b| b.len());
                    if bytes + n > MAX_BATCH_BYTES && !out.is_empty() {
                        self.buffered.push_front(d);
                        return Ok(self.finish(out, false));
                    }
                    bytes += n;
                    out.push(d);
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
        let (next, end, filter, projection, collation, concern, vars) = match &mut self.source {
            Source::Scan { next, end, filter, projection, collation, concern, vars } => {
                (next, end, filter, projection, collation, concern, vars)
            }
            Source::Done => return Ok(()),
        };
        let plan =
            Plan::new(filter, projection.as_ref(), collation.as_ref())?.with_vars(vars.as_ref());
        let rows = backend.scan(next, end, SCAN_CHUNK, *concern)?;
        let short = rows.len() < SCAN_CHUNK;
        for (key, bytes) in rows {
            *next = after(&key);
            // A stored document always parses: one that does not is a fault, not a miss.
            let doc = RawDoc::from_bytes(&bytes)?;
            let mut buffered = false;
            if let Some(d) = plan.apply(doc, None)? {
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
    let lsid = args::lsid(ctx.cmd)?;
    let id = cursor_id(ctx.backend.node(), ctx.backend.random());
    let c = Cursor {
        ns: ns.to_owned(),
        lsid,
        owner: ctx.backend.authenticated_user().map(|u| u.to_string()),
        no_timeout,
        last_use_ms: ctx.backend.now_ms(),
        state,
    };
    lock(ctx.shared).cursors.put(id, c);
    Ok(id)
}

/// Counts a read of a whole collection (`metrics.queryExecutor.collectionScans`).
pub(crate) fn count_scan(shared: &super::Shared, ns: &Namespace) {
    lock(shared).status.collection_scan(&full(ns));
}

/// `find`.
pub fn find<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    let a = find_args(ctx.backend, ctx.cmd)?;
    if let Some(u) = super::uuid::expected(ctx.cmd, FIND)? {
        super::uuid::check(ctx.backend, &ns, u)?;
    }
    let ns_text = full(&ns);
    if ns.coll == super::kinds::SYSTEM_VIEWS {
        return system_views(ctx, &ns, &a);
    }
    if let Some(docs) = super::authz_collections::documents(ctx.backend, &ns)? {
        return generated_find(ctx, &ns, &a, docs);
    }
    if let Some(view) = super::views::resolve(ctx.backend, &ns)? {
        let stages = super::views::find_stages(ctx.cmd, &ns_text)?;
        super::views::check_collation(&view, a.collation.as_ref())?;
        let docs =
            super::aggregate::read_through(ctx.backend, &ns, &stages, a.vars.as_ref(), a.concern)?;
        // The pipeline's `$limit` knows its end once it has given all its documents.
        let ends = a.limit.is_some_and(|l| u64::try_from(docs.len()).is_ok_and(|n| n == l));
        return buffered_reply(ctx, &ns_text, &a, docs, ends);
    }
    let Some(coll) = ctx.backend.collection(&ns)? else {
        // Validate the filter even when there is nothing to read, as RefDb does.
        Plan::new(&a.filter, a.projection.as_ref(), a.collation.as_ref())?;
        return Ok(cursor_reply(0, &ns_text, "firstBatch", Vec::new()));
    };
    let collation = a.collation.clone().or_else(|| options_collation(&coll));
    Plan::new(&a.filter, a.projection.as_ref(), collation.as_ref())?
        .check_metadata(a.sort.as_ref())?;
    if a.tailable.is_some() {
        check_tailable(&ns, &coll, &a)?;
    }
    check_filter(&a.filter, collation.as_ref(), a.vars.as_ref())?;
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
    let executor = |e: CommandError| executor_error(FIND, &ns_text, e);
    // An `_id` equality without a collation reads that document's key alone.
    let range = match super::crud::id_equality(&a.filter).filter(|_| collation.is_none()) {
        Some(id) => match super::crud::id_doc_key(ctx.backend, &coll, &id)? {
            Some(key) => {
                let end = after(&key);
                (key, end)
            }
            // A capped collection without that `_id`: nothing to read.
            None => (documents_of(&coll).0, documents_of(&coll).0),
        },
        None => documents_of(&coll),
    };
    // With a sort the whole collection is read, as without an `_id` equality.
    if a.sort.is_some() || range == documents_of(&coll) {
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
        let parsed = super::plan_exec::parsed_filter(&a.filter, a.vars.as_ref())?;
        let sort = match ctx.cmd.get("sort") {
            Some(Value::Document(s)) => sort_directions(s),
            _ => Vec::new(),
        };
        let read = super::plan_exec::ReadPlan {
            filter: &a.filter,
            parsed: &parsed,
            sort,
            limit: a.limit.map(|l| a.skip.saturating_add(l)),
            hint: {
                let indexes = super::plan_exec::plan_indexes(ctx.backend, &coll)?;
                super::hints::position(ctx.cmd.get("hint"), &indexes)?
            },
            vars: a.vars.as_ref(),
            concern: a.concern,
            intersect: super::lock(ctx.shared).params.hash_intersection,
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
            shape: super::plan_cache::read_shape(&parsed, Some(&sort_doc), Some(&projection), None),
        };
        super::plan_exec::planned_documents(ctx.backend, &coll, &read, note).map_err(executor)?
    } else {
        None
    };
    let mut state = match &a.sort {
        _ if measured => measurements_cursor(ctx.backend, (&coll, ctx.cmd), &a, collation.as_ref())
            .map_err(executor)?,
        None if planned.is_some() => {
            planned_cursor(planned.unwrap_or_default(), &a, collation.as_ref()).map_err(executor)?
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
            },
            buffered: VecDeque::new(),
            skip: a.skip,
            left: a.limit,
            tail: a.tailable.map(Tail::new),
            read_mark: None,
            stream: None,
        },
        Some(sort) => {
            sorted(ctx.backend, &coll, &a, sort, collation.as_ref(), planned).map_err(executor)?
        }
    };
    state.read_mark = ctx.backend.read_mark();
    let size = Some(a.batch_size.unwrap_or(DEFAULT_FIRST_BATCH));
    let (docs, short) = state.batch(ctx.backend, size).map_err(executor)?;
    // A `refrouter` 9.0 closes the cursor once the limit is reached (measured); a tailable one
    // stays open unless the collection had no document at all.
    let exhausted = match &state.tail {
        Some(t) => !t.stays_open(),
        None => short || a.single_batch || state.left == Some(0),
    };
    let id = keep(ctx, &ns_text, state, exhausted, a.no_timeout)?;
    Ok(cursor_reply(id, &ns_text, "firstBatch", docs))
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
fn query_tree(filter: &Document) -> String {
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
fn planned_cursor(
    rows: Vec<Vec<u8>>,
    a: &FindArgs,
    collation: Option<&Document>,
) -> Result<QueryCursor, CommandError> {
    let plan = Plan::new(&a.filter, a.projection.as_ref(), collation)?.with_vars(a.vars.as_ref());
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let take = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut docs = Vec::new();
    for bytes in rows.iter().skip(skip).take(take) {
        docs.push(plan.matched(RawDoc::from_bytes(bytes)?, None)?);
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
    for bytes in rows.iter().rev().skip(skip).take(take) {
        docs.push(plan.matched(RawDoc::from_bytes(bytes)?, None)?);
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
    let skip = usize::try_from(a.skip).unwrap_or(usize::MAX);
    let limit = a.limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut out = Vec::new();
    for (key, b) in rows.into_iter().skip(skip).take(limit) {
        let key: Vec<Value> = key.into_iter().map(|k| k.0).collect();
        out.push(plan.matched(RawDoc::from_bytes(&b)?, Some(&key))?);
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
    let bytes = projection.to_bytes()?;
    check_computed(RawDoc::from_bytes(&bytes)?, &ctx)
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
    check_filter_syntax(filter, collation, vars)?;
    // A constant of an `$expr` that fails fails the command before any document is read.
    teseladb_exec::expr::optimize::optimize_filter(filter, vars).map_err(from_mql)?;
    Ok(())
}

/// Checks the filter of a read as RefDb parses it, before it optimises its `$expr`s.
pub(crate) fn check_filter_syntax(
    filter: &Document,
    collation: Option<&Document>,
    vars: Option<&Document>,
) -> Result<(), CommandError> {
    let plan = Plan::new(filter, None, collation)?;
    super::engines::check_geo(&plan.filter).map_err(from_mql)?;
    let names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
    let bytes = filter.to_bytes()?;
    teseladb_exec::pipeline::check_exprs(
        RawDoc::from_bytes(&bytes)?,
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

fn sorted<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    a: &FindArgs,
    sort: &SortSpec,
    collation: Option<&Document>,
    planned: Option<Vec<Vec<u8>>>,
) -> Result<QueryCursor, CommandError> {
    let plan = Plan::new(&a.filter, a.projection.as_ref(), collation)?.with_vars(a.vars.as_ref());
    let coll_collation = collation_of(collation)?;
    let col: Option<&dyn teseladb_bson::Collator> = coll_collation.as_ref().map(|c| c as _);
    let mut rows: Vec<(Vec<SortKey>, Document)> = Vec::new();
    let mut memory = 0usize;
    let mut err = None;
    let range = documents_of(coll);
    let vars = a.vars.as_ref();
    let mut keep = |bytes: &[u8]| -> Result<bool, CommandError> {
        memory += bytes.len();
        if memory > SORT_MEMORY_LIMIT && !a.allow_disk_use {
            err = Some(CommandError::new(
                QUERY_EXCEEDED_MEMORY_LIMIT,
                "Sort exceeded memory limit of 104857600 bytes, but did not opt in to external sorting.",
            ));
            return Ok(false);
        }
        let raw = RawDoc::from_bytes(bytes)?;
        rows.push((sort.keys(raw, col), Document::from(raw)));
        Ok(true)
    };
    match planned {
        // The plan's documents, in its order (an index that gives the sort's order keeps it).
        Some(docs) => {
            for d in &docs {
                if !keep(d)? {
                    break;
                }
            }
        }
        None => {
            for_each_match_with(backend, range, &a.filter, collation, vars, a.concern, |_, b| {
                keep(b)
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
    for (key, d) in rows.into_iter().skip(skip).take(take) {
        let bytes = raw_bytes(&d)?;
        let raw = RawDoc::from_bytes(&bytes)?;
        let key: Vec<Value> = key.into_iter().map(|k| k.0).collect();
        docs.push(plan.matched(raw, Some(&key))?);
    }
    let mut c = QueryCursor::buffered(docs);
    c.left = a.limit;
    Ok(c)
}

/// `getMore`.
pub fn get_more<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
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
    let lsid = args::lsid(ctx.cmd)?;
    let now = ctx.backend.now_ms();
    let taken = {
        let mut g = lock(ctx.shared);
        let owner = g.cursors.get(id).map(|c| (c.ns.clone(), c.lsid));
        g.cursors.take(id, &ns_text, lsid, now).map_err(|e| (e, owner))
    };
    let mut c = match taken {
        Ok(c) => c,
        Err((e, owner)) => return Err(get_more_refused(id, &ns_text, lsid, e, owner)),
    };
    // At the mark of the command that opened it (008 FR-012).
    ctx.backend.set_read_mark(c.state.read_mark);
    // `maxTimeMS` of a `getMore` is only the wait of a tailable `awaitData` cursor. The cursor
    // was taken: like a `refrouter` 9.0.4, the error kills it (measured).
    let awaits = c.state.stream.is_some() || c.state.tail.as_ref().is_some_and(|t| t.await_data);
    if max_time.is_some() && !awaits {
        let msg = "maxTimeMS can only be used with getMore for tailable, awaitData cursors";
        return Err(CommandError::new(crate::error::BAD_VALUE, msg));
    }
    if let Some(stream) = c.state.stream.as_mut() {
        let (docs, token) = super::change_stream::more(ctx.backend, stream, batch, max_time)?;
        lock(ctx.shared).cursors.put(id, c);
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
        let (docs, _) = state.batch(backend, batch)?;
        if !docs.is_empty() || backend.now_ms() >= deadline || !backend.pause() {
            return Ok(docs);
        }
    }
}

/// The error of a `getMore` on `ns` in session `lsid` that cursor `id`, of namespace and session
/// `owner`, refused with `e` (`refrouter` 9.0.4, measured).
fn get_more_refused(
    id: i64,
    ns: &str,
    lsid: Option<u128>,
    e: CursorError,
    owner: Option<(String, Option<u128>)>,
) -> CommandError {
    let (cursor_ns, cursor_lsid) = owner.unwrap_or_default();
    let created = |l: Option<u128>| session_text(l.unwrap_or_default());
    match e {
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
        CursorError::NotInSession => CommandError::new(
            CURSOR_NOT_IN_SESSION,
            format!(
                "Cannot run getMore on cursor {id}, which was not created in a session, in \
                 session {}",
                created(lsid)
            ),
        ),
        CursorError::SessionRequired => CommandError::new(
            CURSOR_SESSION_REQUIRED,
            format!(
                "Cannot run getMore on cursor {id}, which was created in session {}, without \
                 an lsid",
                created(cursor_lsid)
            ),
        ),
        CursorError::OtherSession => CommandError::new(
            CURSOR_OTHER_SESSION,
            format!(
                "Cannot run getMore on cursor {id}, which was created in session {}, in \
                 session {}",
                created(cursor_lsid),
                created(lsid)
            ),
        ),
    }
}

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
    let mut guard = lock(ctx.shared);
    for id in parsed {
        if guard.cursors.remove(id) {
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
        let (docs, _) =
            super::aggregate::read_through_staged(ctx.backend, ns, (&stages, None), concern)?
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
    let docs = super::aggregate::read_through(ctx.backend, ns, &stages, None, concern)?;
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
        intersect: super::lock(ctx.shared).params.hash_intersection,
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
        shape: super::plan_cache::read_shape(&parsed, None, None, distinct),
    };
    super::plan_exec::planned_documents(ctx.backend, coll, &read, note)
}
