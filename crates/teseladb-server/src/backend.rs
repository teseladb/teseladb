//! The commands' [`Backend`] on a running node (006 FR-014, FR-027): the catalog through core 0's
//! copy of Group 0 and its leader, the data range through its core, which runs the request if it
//! leads the range and forwards it to the leader otherwise. A client worker waits for each
//! answer; a request the range did not take (no leader yet, overloaded) is tried again until
//! [`REQUEST_DEADLINE`], and a write that may have been proposed only if it is retryable.

mod across;
mod intent_wait;
mod outside;
mod span;
mod spans;
mod together;
mod txn_reaper;

pub use outside::{blocks_read_outside, declined_outside, served_outside, unchanged_outside};
pub use txn_reaper::Reapers;
pub(crate) use txn_reaper::SCAN_EVERY;

use crate::authz_lease::{AuthGate, DEADLINE_SKEW};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_hlc::Hlc;
use teseladb_io::runtime::{CoreInput, CoreSender, CoreSenders};
use teseladb_meta::bootstrap::{DATA_END, DATA_START};
use teseladb_meta::catalog::{Catalog, CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::Group0Error;
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_meta::request::RequestId;
use teseladb_meta::security::{Principal, SecurityError, SecurityOp};
use teseladb_multiraft::NodeClock;
use teseladb_range::command::DedupKey;
use teseladb_range::host::RangeError;
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_router::read_pref::{Mode, ReadPreference, Replica, candidates};
use teseladb_router::table::RouteEntry;
use teseladb_wire::commands::session::KillScope;
use teseladb_wire::commands::{Backend, LdapLogin, Shared, WriteOp, lock, txn_stats};
use teseladb_wire::error::{
    CommandError, DATABASE_DIFFER_CASE, FAILED_TO_SATISFY_READ_PREFERENCE, INDEX_NOT_FOUND,
    INTERNAL_ERROR, NAMESPACE_EXISTS, NAMESPACE_NOT_FOUND, NETWORK_TIMEOUT, NOT_WRITABLE_PRIMARY,
    RangeContext, from_range,
};
use teseladb_types::NodeId;

use crate::catalog_ops::{CatalogAnswer, CatalogReq};
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;
use crate::routes::SharedRoutes;

/// RefDb's identifier and message of a certificate rotation that finished (measured on 9.0.4).
const ROTATION_LOG_ID: i32 = 4_988_500;
const ROTATION_DONE: &str = "Certificate rotation completed successfully";
/// Longest a command waits for the catalog or the range.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// How long a read whose read preference no replica satisfies, without `maxTimeMS`, waits for one
/// before `FailedToSatisfyReadPreference`: a refrouter 9.0.4's server selection limit (measured,
/// D-093).
pub const SELECTION_LIMIT: Duration = Duration::from_secs(15);
/// Pause before trying a request again.
const RETRY_PAUSE: Duration = Duration::from_millis(20);
/// Most documents a scan asks a core for at once.
const CORE_SCAN_PART: usize = 1024;
/// Tries of a scan on a connection's thread before a part of it goes to the core.
const OUTSIDE_SCAN_TRIES: usize = 3;
/// How much smaller each try of a scan on a connection's thread is than the one before.
const OUTSIDE_SCAN_SHRINK: usize = 8;
/// The smallest part a scan on a connection's thread tries.
const OUTSIDE_SCAN_LEAST_PART: usize = 64;

/// Sleeps until the wall clock reaches `until_ms`, or until `deadline` if that comes first: whether
/// the clock got there, for a commit that answers once the clock is at its mark (008 FR-008).
fn wait_clock(until_ms: u64, deadline: Instant) -> bool {
    let wall = crate::clock::wall_ms();
    if wall >= until_ms {
        return true;
    }
    let left = deadline.saturating_duration_since(Instant::now());
    thread::sleep(Duration::from_millis(until_ms - wall).min(left));
    crate::clock::wall_ms() >= until_ms
}

/// What a client worker needs to reach the node's cores.
#[derive(Clone, Debug)]
pub struct NodeHandle {
    /// This node.
    pub node: NodeId,
    /// What makes senders of the cores' mailboxes, in core order: each thread makes its own.
    pub senders: Vec<CoreSenders<NodeMsg>>,
    /// The node's clock.
    pub clock: NodeClock,
    /// The node's cursors and sessions, shared by its client workers and core 0.
    pub sessions: Arc<Shared>,
    /// The node's routing table (core 0 writes it).
    pub routes: SharedRoutes,
    /// The node's authorization lease, which core 0 renews (013 FR-012).
    pub auth: Arc<AuthGate>,
    /// Set by `shutdown` (007 FR-018): the node stops once its loop sees it.
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Where each core, in core order, publishes the views of the ranges it leads: a read of one
    /// key is served from them on the worker's thread when it can (005 FR-028, H-013).
    pub views: Vec<teseladb_range::view::RangeViews>,
    /// Where the monotonic time of the node's cores starts, which the views' times count from.
    pub origin: Instant,
    /// The node's data directory, whose files a read outside the cores reads its blocks from;
    /// `None` for a node in memory, whose reads all go to the cores.
    pub data_dir: Option<std::path::PathBuf>,
}

/// The [`Backend`] of one client worker.
#[derive(Debug)]
pub struct ServerBackend {
    h: NodeHandle,
    /// This backend's own senders to the cores (012 FR-005: one ring per thread and core).
    cores: Vec<CoreSender<NodeMsg>>,
    rng: u64,
    deadline: Duration,
    /// When the running command's `maxTimeMS` ends.
    max_until: Option<Instant>,
    /// What the diagnostic commands report; `None` for a backend without a client port.
    diag: Option<Arc<crate::diag::Diag>>,
    /// The identity providers of REFDB-OIDC of a backend without a client port (the HTTP
    /// port's); a backend with one reads them from [`Diag`](crate::diag::Diag).
    oidc: Option<Arc<crate::oidc_client::Oidc>>,
    /// The pins of the running operation's reads of the indexes (009 FR-006).
    pins: Vec<u64>,
    /// When the running operation took its first pin and the collections it pinned, for the log
    /// of one that holds them too long.
    pinned_since: Option<(Instant, Vec<u32>)>,
    /// The latest wait end of a write of the running operation that got no answer: a leader may
    /// still evaluate it until its deadline, so the pins wait for it (009 FR-006).
    unsettled: Option<Instant>,
    /// The running command's read preference (007 FR-013); `None`: the leader.
    read_pref: Option<ReadPreference>,
    /// While a scan with a read preference runs without `maxTimeMS`: when it gives up finding a
    /// replica, and the preference as the error writes it (D-093).
    selection: Option<(Instant, String)>,
    /// The mark the running command's reads are served at (008 FR-012, FR-013).
    read_mark: Option<teseladb_txn::causal::ReadMark>,
    /// The highest commit mark this backend's transactions got: the clock takes a received mark
    /// with its own node number, which in the same millisecond and sequence sorts below another
    /// node's, so the cluster time this backend reads at is never below it (017 SC-002).
    committed: Hlc,
    /// What its reads served without the cores keep (005 FR-028).
    outside: outside::Outside,
    /// How long it spins for a core's answer before it sleeps, after the answers it waited for
    /// (H-042).
    spin: std::cell::Cell<teseladb_io::spin::AdaptiveSpin>,
}

impl ServerBackend {
    /// A backend over the same node, for work on another thread, with its own random numbers.
    #[must_use]
    pub fn fresh(&mut self) -> ServerBackend {
        let seed = teseladb_wire::commands::Backend::random(self);
        ServerBackend::new(self.h.clone(), seed)
    }

    /// The node's routing table and what it publishes for the workers.
    pub fn routes(&self) -> &SharedRoutes {
        &self.h.routes
    }

    /// A backend over `h`, with `seed` for its random numbers.
    pub fn new(h: NodeHandle, seed: u64) -> ServerBackend {
        let cores = h.senders.iter().map(CoreSenders::sender).collect();
        ServerBackend {
            h,
            cores,
            rng: seed | 1,
            deadline: REQUEST_DEADLINE,
            max_until: None,
            diag: None,
            oidc: None,
            pins: Vec::new(),
            pinned_since: None,
            unsettled: None,
            read_pref: None,
            selection: None,
            read_mark: None,
            committed: Hlc { physical: 0, logical: 0 },
            outside: outside::Outside::default(),
            spin: std::cell::Cell::default(),
        }
    }

    /// The backend reporting the facts of `diag` to the diagnostic commands.
    pub fn with_diag(self, diag: Arc<crate::diag::Diag>) -> ServerBackend {
        let mut b = self;
        b.diag = Some(diag);
        b
    }

    /// The backend with the identity providers `oidc` (the HTTP port's, 019 FR-015).
    #[must_use]
    pub fn with_oidc(self, oidc: Option<Arc<crate::oidc_client::Oidc>>) -> ServerBackend {
        let mut b = self;
        b.oidc = oidc;
        b
    }

    /// The node's identity providers: this backend's, or its client port's.
    fn oidc(&self) -> Option<&crate::oidc_client::Oidc> {
        self.oidc.as_deref().or_else(|| self.diag.as_ref().and_then(|d| d.oidc()))
    }

    /// The backend with another deadline for each request.
    pub fn with_deadline(self, deadline: Duration) -> ServerBackend {
        let mut b = self;
        b.deadline = deadline;
        b
    }

    /// The end of a wait that starts now: the request deadline, or the command's `maxTimeMS` if
    /// it ends first.
    /// Range by range in key order, until the limit or the end (007 FR-003).
    /// A scan at the command's mark (008 FR-012, FR-013): a version in the mark's uncertainty
    /// window makes the command read again above it (FR-008), transparently, and the reply gives
    /// the mark it read at.
    fn scan_at(
        &mut self,
        (start, end, limit): (&[u8], &[u8], usize),
        mut mark: teseladb_txn::causal::ReadMark,
        backward: bool,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        let view = TxnView::default();
        let mut restarts = 0;
        loop {
            let rows = match backward {
                true => self.txn_scan_back(start, end, limit, mark.at, &view, mark.uncertain_to),
                false => self.txn_scan(start, end, limit, mark.at, &view, mark.uncertain_to),
            };
            match rows {
                Err(mut e) => match e.take_uncertain() {
                    Some(ts) if restarts < teseladb_txn::uncertainty::MAX_RESTARTS => {
                        txn_stats::uncertainty_restart(&mut lock(&self.h.sessions));
                        restarts += 1;
                        mark.at = mark.at.max(ts);
                        self.read_mark = Some(mark);
                    }
                    Some(_) | None => return Err(e),
                },
                rows => return rows,
            }
        }
    }

    /// [`ServerBackend::scan_outside`] of `[from, end)` for at most `limit` documents, again
    /// with smaller parts while writes keep sending it to the core: a long scan of a written
    /// collection rarely reads all of it with no write in between, and its parts are read at
    /// marks of their own, as a scan reads each range at its own (RefDb's scans yield too).
    fn scan_outside_in_parts(
        &mut self,
        from: &[u8],
        end: &[u8],
        limit: usize,
        concern: ReadConcern,
    ) -> Option<(Vec<u8>, teseladb_wire::commands::Rows)> {
        let mut want = limit;
        for _ in 0..OUTSIDE_SCAN_TRIES {
            if let Some((boundary, rows)) = self.scan_outside((from, end), (want, false), concern) {
                return Some((next_from(boundary, &rows, want), rows));
            }
            if want <= OUTSIDE_SCAN_LEAST_PART {
                return None;
            }
            want = (want / OUTSIDE_SCAN_SHRINK).max(OUTSIDE_SCAN_LEAST_PART);
        }
        None
    }

    fn scan_ranges(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // Range by range in key order, until the limit or the end (007 FR-003).
        let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let prefer = (self.read_pref.clone(), self.h.node, self.h.routes.tags());
        let mut from = start.to_vec();
        // Each range's part on this thread when its view serves it (005 FR-028), for a read at
        // the latest mark on the leaders.
        let outside = self.read_mark.is_none()
            && self.read_pref.as_ref().is_none_or(|p| p.mode == Mode::Primary);
        while from.as_slice() < end && rows.len() < limit {
            if outside
                && let Some((stop, more)) =
                    self.scan_outside_in_parts(&from, end, limit - rows.len(), concern)
            {
                rows.extend(more);
                from = stop;
                continue;
            }
            // The core reads a bounded part at a time: a scan of a whole collection there would
            // hold its writes behind it (H-051 of the native tests).
            let part = (limit - rows.len()).min(CORE_SCAN_PART);
            let left = u32::try_from(part).unwrap_or(u32::MAX);
            let stop = |e: &RouteEntry| match e.end.is_empty() || end <= e.end.as_slice() {
                true => end.to_vec(),
                false => e.end.clone(),
            };
            let build = |e: &RouteEntry, _| {
                scan_req(&prefer, e, (from.clone(), stop(e), left), (concern, false))
            };
            match self.data(&from, true, build)? {
                (e, RemoteReply::Rows(more)) => {
                    // A full part may stop before the range's end: the next one starts after it.
                    from = next_from(stop(&e), &more, part);
                    rows.extend(more);
                }
                (
                    _,
                    RemoteReply::Written { .. }
                    | RemoteReply::Value { .. }
                    | RemoteReply::Intents { .. }
                    | RemoteReply::Record(_)
                    | RemoteReply::Piece { .. }
                    | RemoteReply::Restored { .. }
                    | RemoteReply::Changes { .. }
                    | RemoteReply::Fenced { .. }
                    | RemoteReply::Live { .. }
                    | RemoteReply::DocCount { .. },
                ) => {
                    return Err(CommandError::new(INTERNAL_ERROR, "a scan answered with no rows"));
                }
            }
        }
        Ok(rows)
    }

    /// Range by range from the last one down, until the limit or the start (H-003): each range
    /// is asked for its part of `[start, to)` from its last key down, and `to` moves to where
    /// that part begins. `make` builds the request of a range for its part and the documents
    /// still missing. A range that ends below `to` (it split since the table was read) is routed
    /// again, without taking its rows.
    fn scan_ranges_back(
        &mut self,
        (start, end, limit): (&[u8], &[u8], usize),
        make: &PartRequest<'_>,
        outside: Option<ReadConcern>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut to = end.to_vec();
        let until = self.until();
        // Each range's part on this thread when its view serves it (005 FR-028), for a read at
        // the latest mark on the leaders (`outside`: its concern).
        let outside = outside.filter(|_| {
            self.read_mark.is_none()
                && self.read_pref.as_ref().is_none_or(|p| p.mode == Mode::Primary)
        });
        while start < to.as_slice() && rows.len() < limit {
            if let Some(concern) = outside
                && let Some((lo, more)) =
                    self.scan_outside((start, &to), (limit - rows.len(), true), concern)
            {
                rows.extend(more);
                to = lo;
                continue;
            }
            let left = u32::try_from(limit - rows.len()).unwrap_or(u32::MAX);
            let part = |e: &RouteEntry| {
                let lo = e.start.as_slice().max(start).to_vec();
                let hi = match e.end.is_empty() || to.as_slice() <= e.end.as_slice() {
                    true => to.clone(),
                    false => e.end.clone(),
                };
                (lo, hi)
            };
            let key =
                self.h.routes.lookup_below(&to).map_or_else(|| start.to_vec(), |e| part(&e).0);
            let build = |e: &RouteEntry, _| {
                let (lo, hi) = part(e);
                make(e, (lo, hi, left))
            };
            match self.data(&key, true, build)? {
                (e, RemoteReply::Rows(more)) => {
                    let (lo, hi) = part(&e);
                    if hi != to {
                        if Instant::now() >= until {
                            return Err(self.range_timeout());
                        }
                        thread::sleep(RETRY_PAUSE);
                        continue;
                    }
                    rows.extend(more);
                    to = lo;
                }
                _ => return Err(CommandError::new(INTERNAL_ERROR, "a scan answered with no rows")),
            }
        }
        Ok(rows)
    }

    /// The answer of node `node` to an HTTP request forwarded to it (019 FR-008), or `None`
    /// when it cannot be reached or has no HTTP port.
    pub fn forward_http(&self, node: u64, body: Vec<u8>) -> Option<Vec<u8>> {
        let until = self.until();
        let answers = self.ask(0, |tx| NodeMsg::ForwardHttp(NodeId(node), body, tx), until)??;
        answers.into_iter().next().filter(|a| !a.is_empty())
    }

    /// Sends node `to` the answer to the HTTP request it forwarded as its request `req`.
    pub fn answer_http(&self, to: NodeId, req: u64, body: Vec<u8>) {
        if let Some(core) = self.cores.first() {
            let _ = core.send(CoreInput::App(NodeMsg::HttpAnswer(to, req, body)));
        }
    }

    /// The ranges' time-out once the operation's deadline passed waiting for the routing table.
    fn range_timeout(&self) -> CommandError {
        let max_time_expired = self.max_until.is_some_and(|m| Instant::now() >= m);
        let ctx = RangeContext { retryable_write: false, max_time_expired };
        from_range(&RangeError::Timeout, ctx)
    }

    fn until(&self) -> Instant {
        let own = Instant::now() + self.deadline;
        self.max_until.map_or(own, |m| m.min(own))
    }

    fn ask<T>(
        &self,
        core: usize,
        msg: impl FnOnce(mpsc::Sender<T>) -> NodeMsg,
        until: Instant,
    ) -> Option<T> {
        let (tx, rx): (mpsc::Sender<T>, Receiver<T>) = mpsc::channel();
        let asked = Instant::now();
        self.cores.get(core)?.send(CoreInput::App(msg(tx))).ok()?;
        // Most answers come within a short spin, which saves the wake-up of this thread (H-013);
        // the spin follows how long the last answers took (H-042).
        let mut spin = self.spin.get();
        let mut answer = None;
        teseladb_io::spin::spin_until(spin.window(), || {
            answer = rx.try_recv().ok();
            answer.is_some()
        });
        let answer = answer
            .or_else(|| rx.recv_timeout(until.saturating_duration_since(Instant::now())).ok());
        spin.observe(asked.elapsed());
        self.spin.set(spin);
        answer
    }

    fn catalog(&mut self) -> Result<Arc<Catalog>, CommandError> {
        // Core 0 publishes its copy before it answers any change of it (H-017); before the
        // cluster exists there is none and core 0 answers that.
        if let Some(c) = self.h.routes.catalog() {
            return Ok(c);
        }
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Catalog(c)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Read, tx), until)
            {
                return Ok(c);
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    /// The catalog changes of this node's copy above `from` (014 FR-014).
    fn read_ddl(&mut self, from: Hlc) -> Result<teseladb_wire::commands::DdlBatch, CommandError> {
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Ddl(records, floor, resolved)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Ddl(from), tx), until)
            {
                return Ok(teseladb_wire::commands::DdlBatch { records, floor, resolved });
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    /// Runs a change of Group 0 (the catalog, a setting, the users and roles) and waits until
    /// this node's copy applied it; what Group 0 refused, if it refused it.
    fn group0_change(
        &mut self,
        req: CatalogReq,
    ) -> Result<Result<Option<CollectionEntry>, Group0Error>, CommandError> {
        let until = self.until();
        loop {
            match self.ask(0, |tx| NodeMsg::Catalog(req.clone(), tx), until) {
                Some(CatalogAnswer::Done(e)) => return Ok(Ok(e)),
                Some(CatalogAnswer::Refused(e)) => return Ok(Err(e)),
                Some(
                    CatalogAnswer::Catalog(_)
                    | CatalogAnswer::Settings(_)
                    | CatalogAnswer::Indexes(..)
                    | CatalogAnswer::SearchIndexes(_)
                    | CatalogAnswer::Principals(_)
                    | CatalogAnswer::Documents(_)
                    | CatalogAnswer::Dump(_)
                    | CatalogAnswer::Tenants(..)
                    | CatalogAnswer::Ddl(..)
                    | CatalogAnswer::Unavailable,
                )
                | None => {}
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    /// Releases the pins of the running operation's reads of the indexes: at once, or once the
    /// deadline of a write that got no answer has passed (009 FR-006).
    fn release_pins(&mut self) {
        let late = self.unsettled.take();
        if let Some((since, collections)) = self.pinned_since.take() {
            log_long_pin(since.elapsed(), &collections, late.is_some());
        }
        for pin in self.pins.drain(..) {
            match late {
                Some(until) => self.h.auth.release_pin_after(pin, until),
                None => self.h.auth.pins.release(pin),
            }
        }
    }

    /// The deadline a write whose wait ends at `until` carries (009 FR-006): the node's clock
    /// then, in the milliseconds of its marks, plus [`DEADLINE_SKEW`].
    /// The document under `key` read by its leader's core as a point read, which skips the files
    /// whose filters rule the key out (H-013); none for a key without one.
    fn point_on_leader(
        &mut self,
        key: &[u8],
        concern: ReadConcern,
    ) -> Result<Option<Vec<u8>>, CommandError> {
        let build = |_: &RouteEntry, _| RemoteReq::ReadWith {
            key: key.to_vec(),
            scan: None,
            backward: false,
            at: None,
            concern,
            view: TxnView::default(),
            uncertain_to: None,
        };
        match self.data(key, true, build)? {
            // A version that only serves the history is no document, as in a scan.
            (_, RemoteReply::Value { value, .. }) => {
                Ok(value.filter(|(t, _)| *t != teseladb_range::HISTORY_VALUE).map(|(_, v)| v))
            }
            _ => Err(CommandError::new(INTERNAL_ERROR, "a read answered with no value")),
        }
    }

    fn deadline_of(&self, until: Instant) -> u64 {
        let left = (until.saturating_duration_since(Instant::now()) + DEADLINE_SKEW).as_millis();
        let now = self.h.clock.0.peek().physical.max(crate::clock::wall_ms());
        now.saturating_add(u64::try_from(left).unwrap_or(u64::MAX))
    }

    /// A write sent with wait end `until` got no answer it can trust: the range may still
    /// evaluate it.
    fn note_unsettled(&mut self, until: Instant) {
        self.unsettled = Some(self.unsettled.map_or(until, |u| u.max(until)));
    }

    /// Runs a change of the catalog or of a setting with [`ServerBackend::group0_change`].
    fn catalog_change(&mut self, req: CatalogReq) -> Result<Option<CollectionEntry>, CommandError> {
        match self.group0_change(req)? {
            Ok(e) => Ok(e),
            Err(Group0Error::NamespaceExists) => {
                Err(CommandError::new(NAMESPACE_EXISTS, "the collection already exists"))
            }
            Err(Group0Error::NamespaceNotFound) => {
                Err(CommandError::new(NAMESPACE_NOT_FOUND, "ns does not exist"))
            }
            // The command names both databases (`commands::catalog::catalog_write`).
            Err(Group0Error::DatabaseDifferCase) => Err(CommandError::new(
                DATABASE_DIFFER_CASE,
                "can't have 2 databases that just differ on case",
            )),
            Err(e) => {
                Err(CommandError::new(INTERNAL_ERROR, format!("Group 0 refused the change: {e:?}")))
            }
        }
    }

    /// Reads documents of users or roles from this node's copy of Group 0.
    fn principal_read(&mut self, req: CatalogReq) -> Result<Vec<Vec<u8>>, CommandError> {
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Principals(docs)) =
                self.ask(0, |tx| NodeMsg::Catalog(req.clone(), tx), until)
            {
                return Ok(docs);
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    /// Runs on the range that holds `key` the request `build` makes for it: the range's entry
    /// and its answer, or the range's error. A range that refused the key (the routing table is
    /// behind a split) is looked up again.
    fn data(
        &mut self,
        key: &[u8],
        retryable: bool,
        build: impl Fn(&RouteEntry, u64) -> RemoteReq,
    ) -> Result<(RouteEntry, RemoteReply), CommandError> {
        let until = self.until();
        let until = self.selection.as_ref().map_or(until, |(at, _)| until.min(*at));
        let deadline = self.deadline_of(until);
        let mut group = None;
        // A read that met intents goes again at the mark it had, knowing their transactions.
        let mut pinned: Option<(Hlc, TxnView)> = None;
        loop {
            let mut sent = true;
            let mut answered = true;
            let mut plain_write = false;
            let e = match self.h.routes.lookup(key) {
                Some(entry) => {
                    let (g, core) = (entry.group, usize::from(entry.core));
                    group = Some(g);
                    let req = intent_wait::pin(build(&entry, deadline), pinned.as_ref());
                    let writes = matches!(req, RemoteReq::Write { .. } | RemoteReq::Txn { .. });
                    plain_write = matches!(req, RemoteReq::Write { .. });
                    let answer =
                        self.ask(core, |tx| NodeMsg::Data(DataReq::Remote(g, req), tx), until);
                    match answer {
                        Some(Ok(DataReply::Remote(r))) => return Ok((entry, r)),
                        Some(Ok(other)) => {
                            return Err(CommandError::new(
                                INTERNAL_ERROR,
                                format!("unexpected answer {other:?}"),
                            ));
                        }
                        Some(Err(e)) => {
                            if writes && unsettled(&e) {
                                self.note_unsettled(until);
                            }
                            e
                        }
                        None => {
                            if writes {
                                self.note_unsettled(until);
                            }
                            answered = false;
                            RangeError::Timeout
                        }
                    }
                }
                // The table does not have the key's range yet (the node is starting): nothing
                // was sent, and at the deadline nothing answered.
                None => {
                    sent = false;
                    RangeError::Timeout
                }
            };
            // A refusal before the proposal can always be tried again; after it, only a
            // retryable write or a read. An intent's round waits by itself only when its owner
            // is not known yet: once the intent is resolved, or the read knows its owner, it goes
            // again at once.
            let mut paused = false;
            let again = match &e {
                RangeError::NotLeader { .. } | RangeError::Overloaded | RangeError::OutOfRange => {
                    true
                }
                RangeError::Timeout | RangeError::Unavailable => retryable || !sent,
                // A write outside a transaction waits for the intent's owner (008 FR-002).
                RangeError::IntentConflict(owner) if plain_write => {
                    let owner = owner.clone();
                    self.wait_for_intent(&owner);
                    paused = true;
                    true
                }
                // A read pushes the owner above its mark and reads again knowing it (008 FR-003).
                RangeError::ReadIntent { owner, at } => {
                    let (owner, at) = (owner.clone(), *at);
                    let known = self.push_for_read(&owner, at, until);
                    let (mark, view) = pinned.get_or_insert_with(|| (at, TxnView::default()));
                    *mark = at;
                    match known {
                        Some(k) => {
                            view.known.insert(owner.txn, k);
                        }
                        None => thread::sleep(RETRY_PAUSE),
                    }
                    paused = true;
                    true
                }
                _ => false,
            };
            if again {
                count_retry(&e, paused, answered);
            }
            // Past the deadline while it waited for an intent's owner, it timed out.
            let e = match e {
                RangeError::IntentConflict(_) if plain_write => RangeError::Timeout,
                e => e,
            };
            // Past the selection limit no candidate served it: refused, or no answer in time.
            if let (RangeError::NotLeader { .. } | RangeError::Timeout, Some((at, pref)), Some(g)) =
                (&e, &self.selection, group)
                && Instant::now() >= *at
            {
                let msg = format!(
                    "Could not find host matching read preference {pref} for set range-{}",
                    g.0
                );
                return Err(CommandError::new(FAILED_TO_SATISFY_READ_PREFERENCE, msg));
            }
            if !again || Instant::now() >= until {
                let max_time_expired = self.max_until.is_some_and(|m| Instant::now() >= m);
                if again && !max_time_expired {
                    log_gave_up(group, &e, key);
                }
                let ctx = RangeContext { retryable_write: retryable, max_time_expired };
                return Err(from_range(&e, ctx));
            }
            if !paused {
                thread::sleep(RETRY_PAUSE);
            }
        }
    }
}

/// Where a scan goes on after a part that gave `rows`, with at most `want` asked for, of a range
/// that ends at `boundary`: past its last key when the part is full (the range may hold more), at
/// the range's end otherwise.
fn next_from(boundary: Vec<u8>, rows: &[(Vec<u8>, Vec<u8>)], want: usize) -> Vec<u8> {
    match (rows.len() >= want, rows.last()) {
        (true, Some((k, _))) => [k.as_slice(), &[0]].concat(),
        _ => boundary,
    }
}

/// Why requests to a range went again, by [`RETRY_REASONS`], since the process started.
static RETRIES: [std::sync::atomic::AtomicU64; RETRY_REASONS.len()] =
    [const { std::sync::atomic::AtomicU64::new(0) }; RETRY_REASONS.len()];

/// The labels of [`retry_counts`].
pub const RETRY_REASONS: [&str; 9] = [
    "not_leader",
    "overloaded",
    "out_of_range",
    "timeout",
    "intent_wait",
    "read_intent",
    "other",
    "unavailable",
    "no_answer",
];

/// Counts a request that goes to its range again after `e` (`paused`: it already waited for an
/// intent's owner; `answered`: its range answered `e`, which is otherwise a request that got no
/// answer before its wait ended).
fn count_retry(e: &RangeError, paused: bool, answered: bool) {
    let i = match e {
        _ if !answered => 8,
        RangeError::NotLeader { .. } => 0,
        RangeError::Overloaded => 1,
        RangeError::OutOfRange => 2,
        RangeError::Timeout => 3,
        RangeError::Unavailable => 7,
        RangeError::IntentConflict(_) if paused => 4,
        RangeError::ReadIntent { .. } => 5,
        _ => 6,
    };
    RETRIES[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many requests went to their range again for each reason of [`RETRY_REASONS`].
pub fn retry_counts() -> [u64; RETRY_REASONS.len()] {
    std::array::from_fn(|i| RETRIES[i].load(std::sync::atomic::Ordering::Relaxed))
}

/// Logs a request that tried range `group` again until its deadline: the range and the last
/// refusal tell an operator which range stopped answering and why.
fn log_gave_up(group: Option<teseladb_types::GroupId>, e: &RangeError, key: &[u8]) {
    crate::log::defer(crate::log::Pending {
        id: 0,
        msg: "a request gave up on its range at the deadline".to_owned(),
        attr: format!(
            "{{\"range\":{},\"lastError\":{:?},\"key\":{:?}}}",
            group.map_or(0, |g| g.0),
            format!("{e:?}"),
            hex(key)
        ),
    });
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The mark of a commit a transaction's request answered: a committed record's, or a one-phase
/// commit's.
fn committed_mark(reply: &teseladb_range::host::Reply) -> Option<Hlc> {
    use teseladb_range::host::Reply;
    use teseladb_range::intents::TxnStatus;
    match reply {
        Reply::Record(rec) if rec.status == TxnStatus::Committed => Some(rec.commit_ts),
        Reply::Intents { ts } => Some(*ts),
        _ => None,
    }
}

/// Prefix of the Group 0 settings that hold the cluster parameters (`setClusterParameter`).
pub const PARAMETER_PREFIX: &[u8] = b"clusterParameter.";

/// The cluster parameters among Group 0's settings, by name: the settings under
/// [`PARAMETER_PREFIX`], which the ordered map keeps together.
pub fn parameters_of(settings: &BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    settings
        .range(PARAMETER_PREFIX.to_vec()..)
        .take_while(|(k, _)| k.starts_with(PARAMETER_PREFIX))
        .filter_map(|(k, v)| {
            let name = k.strip_prefix(PARAMETER_PREFIX)?;
            Some((String::from_utf8(name.to_vec()).ok()?, v.clone()))
        })
        .collect()
}

/// The instant the process first read the monotonic clock, the origin of
/// [`Backend::now_micros`].
pub(crate) fn process_start() -> Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Whether a write that ended with `e` may still be evaluated by a leader: it timed out, or its
/// forwarded request lost its answer.
fn unsettled(e: &RangeError) -> bool {
    matches!(e, RangeError::Timeout | RangeError::Unavailable)
}

fn unavailable() -> CommandError {
    CommandError::new(NOT_WRITABLE_PRIMARY, "Group 0 has no leader to take the catalog change")
}

/// How far behind the node's clock Group 0's last mark may be before a change stream's read
/// asks for an empty entry (014 FR-014): well inside the 500 ms of SC-002.
const DDL_TICK_AFTER_MS: u64 = 100;
/// The wall clock of the last empty entry this node asked for, in milliseconds.
static LAST_DDL_TICK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// By range, how far behind the wall clock its resolved mark was at the last change stream read
/// of it, in milliseconds, and when that read was (014 FR-018).
static RANGE_LAG: std::sync::Mutex<BTreeMap<u64, (u64, u64)>> =
    std::sync::Mutex::new(BTreeMap::new());
/// A range whose streams stopped reading it this long ago leaves the metrics.
const RANGE_LAG_HOLD_MS: u64 = 10_000;

/// Notes the lag of range `g`'s resolved mark `mark` at wall clock `now_ms`.
fn note_range_lag(g: u64, mark: Hlc, now_ms: u64) {
    let mut lags = RANGE_LAG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    lags.retain(|_, (_, at)| now_ms.saturating_sub(*at) < RANGE_LAG_HOLD_MS);
    lags.insert(g, (now_ms.saturating_sub(mark.physical), now_ms));
}

/// The lag of each range change streams read in the last [`RANGE_LAG_HOLD_MS`], in
/// milliseconds, by range (014 FR-018).
pub fn range_lags(now_ms: u64) -> Vec<(u64, u64)> {
    let lags = RANGE_LAG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    lags.iter()
        .filter(|(_, (_, at))| now_ms.saturating_sub(*at) < RANGE_LAG_HOLD_MS)
        .map(|(g, (lag, _))| (*g, *lag))
        .collect()
}

/// Whether this read may ask for an empty entry now: none was asked for in the last
/// [`DDL_TICK_AFTER_MS`] / 2.
fn take_ddl_tick(now_ms: u64) -> bool {
    use std::sync::atomic::Ordering;
    let last = LAST_DDL_TICK_MS.load(Ordering::Relaxed);
    now_ms.saturating_sub(last) >= DDL_TICK_AFTER_MS / 2
        && LAST_DDL_TICK_MS
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// Logs an operation that held its reads of the indexes longer than
/// [`PIN_STALL`](crate::authz_lease::PIN_STALL), with its thread: while it ran, no change of the
/// indexes could pass the fence (009 FR-006).
fn log_long_pin(held: Duration, collections: &[u32], late: bool) {
    if held < crate::authz_lease::PIN_STALL {
        return;
    }
    let thread = thread::current().name().unwrap_or("unnamed").to_owned();
    // TeselaDB's own line: it has no RefDb identifier.
    crate::log::defer(crate::log::Pending {
        id: 0,
        msg: "an operation held the catalog changes of the indexes".to_owned(),
        attr: format!(
            "{{\"thread\":{thread:?},\"heldMillis\":{},\"collections\":{collections:?},\"late\":{late}}}",
            held.as_millis()
        ),
    });
}

impl Drop for ServerBackend {
    fn drop(&mut self) {
        self.release_pins();
        self.report_outside();
    }
}

impl Backend for ServerBackend {
    fn collection(&mut self, ns: &Namespace) -> Result<Option<CollectionEntry>, CommandError> {
        Ok(self.catalog()?.get(ns).cloned())
    }

    fn collections(&mut self) -> Result<Vec<(Namespace, CollectionEntry)>, CommandError> {
        Ok(self.catalog()?.collections.iter().map(|(n, e)| (n.clone(), e.clone())).collect())
    }

    fn databases(&mut self) -> Result<Vec<String>, CommandError> {
        Ok(self.catalog()?.databases_by_creation())
    }

    fn database_mark(&mut self, db: &str) -> Result<Option<Hlc>, CommandError> {
        Ok(self.catalog()?.db_marks.get(db).copied())
    }

    fn catalog_write(
        &mut self,
        cmd: CatalogCommand,
    ) -> Result<Option<CollectionEntry>, CommandError> {
        // A forwarded change's answer has no tenant: the namespace's qualified name gives it.
        let tenant = match &cmd {
            CatalogCommand::Create { ns, .. } => teseladb_meta::tenants::split_db(&ns.db).0,
            _ => teseladb_meta::tenants::DEFAULT_TENANT,
        };
        let entry = self.catalog_change(CatalogReq::Write(cmd))?;
        Ok(entry.map(|c| CollectionEntry { tenant, ..c }))
    }

    fn tenant_named(&mut self, name: &str) -> Result<Option<u32>, CommandError> {
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Tenants(all, _)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Tenants, tx), until)
            {
                return Ok(all.into_iter().find(|(_, n, _)| n == name).map(|(id, _, _)| id));
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn write_unchanged(&mut self, op: &WriteOp) -> Option<Vec<u8>> {
        self.write_outside(op)
    }

    fn write(&mut self, op: WriteOp) -> Result<Vec<u8>, CommandError> {
        let dedup = op.dedup.map(|k| DedupKey {
            lsid: k.lsid,
            txn_number: k.txn_number,
            stmt_id: k.stmt_id,
        });
        // A write is routed by its first key; without keys no range holds it.
        let Some(first) = op.keys.first().cloned() else {
            return Err(CommandError::new(INTERNAL_ERROR, "a write without keys has no range"));
        };
        let (keys, mutation, wc) = (op.keys, op.mutation, op.wc);
        let build = |_: &RouteEntry, deadline| RemoteReq::Write {
            keys: keys.clone(),
            mutation: mutation.clone(),
            dedup,
            wc,
            deadline: Some(deadline),
        };
        match self.data(&first, dedup.is_some(), build)?.1 {
            // The write's mark comes into the node's clock, as a commit's does (`txn_send`): a
            // leader just elected writes above the clocks, and the node's next transaction reads
            // at or above the write instead of writing under it (008 FR-008).
            RemoteReply::Written { ts, result } => {
                self.h.clock.0.update(ts);
                self.committed = self.committed.max(ts);
                Ok(result)
            }
            RemoteReply::Rows(_)
            | RemoteReply::Value { .. }
            | RemoteReply::Intents { .. }
            | RemoteReply::Record(_)
            | RemoteReply::Piece { .. }
            | RemoteReply::Restored { .. }
            | RemoteReply::Changes { .. }
            | RemoteReply::Fenced { .. }
            | RemoteReply::Live { .. }
            | RemoteReply::DocCount { .. } => {
                Err(CommandError::new(INTERNAL_ERROR, "a write answered with no result"))
            }
        }
    }

    fn scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // At the command's mark: on the leaders, which serve it once their clock gets there.
        if let Some(mark) = self.read_mark {
            return self.scan_at((start, end, limit), mark, false);
        }
        // A scan of one key on its leader, on this thread when its range's view serves it (005
        // FR-028): a find by `_id` asks for one.
        let primary = self.read_pref.as_ref().is_none_or(|p| p.mode == Mode::Primary);
        let one_key = end.len() == start.len() + 1 && end.starts_with(start) && end.ends_with(&[0]);
        // Its core reads it as a point read too: a key the filters of a file rule out costs no
        // block there, where a scan reads one from each file (H-010 of the native tests).
        if primary && one_key && limit > 0 {
            let value = match self.read_outside(start, concern) {
                Some(value) => value,
                None => self.point_on_leader(start, concern)?,
            };
            return Ok(value.map(|v| (start.to_vec(), v)).into_iter().collect());
        }
        // Without `maxTimeMS`, a refrouter looks for a replica for its selection limit (D-093).
        self.selection = self
            .read_pref
            .as_ref()
            .filter(|p| p.mode != Mode::Primary && self.max_until.is_none())
            .map(|p| (Instant::now() + SELECTION_LIMIT, p.describe()));
        let rows = self.scan_ranges(start, end, limit, concern);
        self.selection = None;
        rows
    }

    fn points(
        &mut self,
        keys: &[Vec<u8>],
        concern: ReadConcern,
    ) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
        // A read at the command's mark or with a read preference goes as a scan of each key,
        // which those paths serve; otherwise each key is a point read on its leader, which skips
        // the files its filters rule out (H-013).
        let primary = self.read_pref.as_ref().is_none_or(|p| p.mode == Mode::Primary);
        if self.read_mark.is_some() || !primary {
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                let rows = self.scan(k, &[k.as_slice(), &[0]].concat(), 1, concern)?;
                out.push(rows.into_iter().next().map(|(_, v)| v));
            }
            return Ok(out);
        }
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            // On this thread when the key's range published a view that serves it (005 FR-028).
            let value = match self.read_outside(key, concern) {
                Some(value) => value,
                None => self.point_on_leader(key, concern)?,
            };
            out.push(value);
        }
        Ok(out)
    }

    fn scan_back(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // As `scan`, from the last key down (H-003).
        if let Some(mark) = self.read_mark {
            return self.scan_at((start, end, limit), mark, true);
        }
        self.selection = self
            .read_pref
            .as_ref()
            .filter(|p| p.mode != Mode::Primary && self.max_until.is_none())
            .map(|p| (Instant::now() + SELECTION_LIMIT, p.describe()));
        let prefer = (self.read_pref.clone(), self.h.node, self.h.routes.tags());
        let make = |e: &RouteEntry, part| scan_req(&prefer, e, part, (concern, true));
        let rows = self.scan_ranges_back((start, end, limit), &make, Some(concern));
        self.selection = None;
        rows
    }

    fn scan_spans(
        &mut self,
        spans: &[teseladb_wire::commands::Span],
        concern: ReadConcern,
    ) -> Result<Vec<teseladb_wire::commands::Rows>, CommandError> {
        self.spans_together(spans, concern)
    }

    fn kill_everywhere(&mut self, scope: &KillScope) -> Result<(), CommandError> {
        let until = Instant::now() + self.deadline;
        let scope = scope.clone();
        match self.ask(0, |tx| NodeMsg::KillSessions(scope, tx), until) {
            Some(true) => Ok(()),
            Some(false) | None => Err(CommandError::new(
                NETWORK_TIMEOUT,
                "a node of the cluster did not answer the kill of the sessions",
            )),
        }
    }

    fn other_operations(&mut self) -> Result<Vec<teseladb_bson::Document>, CommandError> {
        let until = Instant::now() + self.deadline;
        let Some(Some(answers)) = self.ask(0, NodeMsg::CurrentOps, until) else {
            return Err(CommandError::new(
                NETWORK_TIMEOUT,
                "a node of the cluster did not answer the list of its operations",
            ));
        };
        let mut ops = Vec::new();
        for a in answers {
            let doc = teseladb_bson::RawDoc::from_bytes(&a)
                .map(teseladb_bson::Document::from)
                .map_err(|_| CommandError::new(NETWORK_TIMEOUT, "a node answered a bad list"))?;
            if let Some(teseladb_bson::Value::Array(items)) = doc.get("ops") {
                ops.extend(items.iter().filter_map(|v| match v {
                    teseladb_bson::Value::Document(d) => Some(d.clone()),
                    _ => None,
                }));
            }
        }
        Ok(ops)
    }

    fn voters(&mut self, start: &[u8], end: &[u8]) -> Result<usize, CommandError> {
        // A node that is starting fills its routing table after it is ready: it waits for the
        // span's ranges as a write waits for its key's (`data`).
        let until = self.until();
        loop {
            if let Some(n) = span::fewest_voters(&self.h.routes.entries(), start, end) {
                return Ok(n);
            }
            if Instant::now() >= until {
                return Err(self.range_timeout());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    // The dispatcher's wrapper keeps the plan of the operation it runs.
    fn note_plan(&mut self, _: teseladb_bson::Document) {}

    // The dispatcher's wrapper keeps it.
    fn note_plan_lazy(&mut self, _: teseladb_wire::ops::PendingPlan) {}

    // The dispatcher's wrapper counts what the operation examines.
    // The dispatcher's wrapper counts what the operation examines.
    fn not_examined(&mut self, _docs: u64) {}

    fn op_reads(&mut self) -> teseladb_wire::ops::OpReads {
        teseladb_wire::ops::OpReads::default()
    }

    fn op_client(&mut self) -> Option<teseladb_bson::Document> {
        None
    }

    fn op_plan(&mut self) -> Option<teseladb_bson::Document> {
        None
    }

    fn end_operation(&mut self) {
        self.release_pins();
        self.release_outside();
    }

    fn changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: Hlc,
    ) -> Result<teseladb_wire::commands::ChangeBatch, CommandError> {
        // The batch is resolved up to the lowest resolved mark of its ranges (FR-004, FR-005).
        let mut read = teseladb_changestream::read::IntervalRead::new(start, end, from);
        for r in self.range_changes(start, end, from, None)? {
            read.answered(r.end, r.rows, r.resolved, None);
        }
        Ok(read.finish(|r: &teseladb_range::changes::Versioned| r.ts))
    }

    fn fence(&mut self, start: &[u8], end: &[u8], on: bool) -> Result<Hlc, CommandError> {
        // Range by range in key order, each on its leader; a range that answers no mark has
        // nothing of the span.
        let mut mark = Hlc { physical: 0, logical: 0 };
        let mut walk = teseladb_changestream::read::IntervalRead::<()>::new(start, end, mark);
        while let Some(at) = walk.next().map(<[u8]>::to_vec) {
            let key = at.clone();
            let build = |e: &RouteEntry, _| RemoteReq::Fence {
                start: key.clone(),
                end: walk.stop(&e.end),
                on,
            };
            match self.data(&at, true, build)? {
                (e, RemoteReply::Fenced { ts }) => {
                    mark = mark.max(ts);
                    let next = walk.stop(&e.end);
                    walk.answered(next, Vec::new(), ts, None);
                }
                _ => {
                    return Err(CommandError::new(
                        INTERNAL_ERROR,
                        "a fence answered with something else",
                    ));
                }
            }
        }
        Ok(mark)
    }

    fn range_changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: Hlc,
        at: Option<Hlc>,
    ) -> Result<Vec<teseladb_wire::commands::RangeBatch>, CommandError> {
        // Range by range in key order, each read on its leader (014 FR-003, FR-010) and kept up
        // to its own resolved mark; with `at`, past the closed marks up to it (011 FR-010).
        let mut out = Vec::new();
        let wanted = at;
        let mut walk = teseladb_changestream::read::IntervalRead::<()>::new(start, end, from);
        while let Some(at) = walk.next().map(<[u8]>::to_vec) {
            let key = at.clone();
            let build = |e: &RouteEntry, _| RemoteReq::Changes {
                start: key.clone(),
                end: walk.stop(&e.end),
                from,
                at: wanted,
            };
            match self.data(&at, false, build)? {
                (e, RemoteReply::Changes { mut rows, to, oldest_intent }) => {
                    let mark = teseladb_changestream::resolved::resolved(to, oldest_intent);
                    note_range_lag(e.group.0, mark, crate::clock::wall_ms());
                    let resolved = mark.max(from);
                    rows.retain(|r| r.ts <= resolved);
                    let next = walk.stop(&e.end);
                    out.push(teseladb_wire::commands::RangeBatch {
                        start: at,
                        end: next.clone(),
                        rows,
                        resolved,
                    });
                    walk.answered(next, Vec::new(), to, oldest_intent);
                }
                _ => {
                    return Err(CommandError::new(
                        INTERNAL_ERROR,
                        "a change stream's read answered with something else",
                    ));
                }
            }
        }
        Ok(out)
    }

    fn catalog_changes(
        &mut self,
        from: Hlc,
    ) -> Result<teseladb_wire::commands::DdlBatch, CommandError> {
        let batch = self.read_ddl(from)?;
        // A quiet Group 0 leaves this copy's last mark behind: an empty entry moves it, at most
        // one at a time for the whole node.
        // Against the node's clock, which is at least the wall clock once it gave a mark.
        let wall = crate::clock::wall_ms();
        let behind =
            self.h.clock.0.peek().physical.max(wall).saturating_sub(batch.resolved.physical);
        if behind < DDL_TICK_AFTER_MS || !take_ddl_tick(wall) {
            return Ok(batch);
        }
        self.group0_change(CatalogReq::Tick)?.map_err(|e| {
            CommandError::new(INTERNAL_ERROR, format!("Group 0 refused an empty entry: {e:?}"))
        })?;
        self.read_ddl(from)
    }

    /// An empty entry of Group 0, answered once this node's copy applied it (`catalog_ops`): the
    /// copy then holds every change committed before it (013 T058).
    fn catalog_barrier(&mut self) -> Result<(), CommandError> {
        self.group0_change(CatalogReq::Tick)?.map(drop).map_err(|e| {
            CommandError::new(INTERNAL_ERROR, format!("Group 0 refused an empty entry: {e:?}"))
        })
    }

    fn txn_pause(&mut self) -> bool {
        thread::sleep(RETRY_PAUSE);
        Instant::now() < self.until()
    }

    fn txn_wait_clock(&mut self, until_ms: u64) -> bool {
        wait_clock(until_ms, self.until())
    }

    fn txn_route(&self, key: &[u8]) -> Option<teseladb_types::GroupId> {
        self.h.routes.lookup(key).map(|e| e.group)
    }

    fn txn_memory_budget(&self) -> usize {
        teseladb_txn::coordinator::MEMORY_BUDGET
    }

    fn txn_send(
        &mut self,
        g: teseladb_types::GroupId,
        op: teseladb_range::host::TxnOp,
    ) -> Result<teseladb_range::host::Reply, RangeError> {
        if matches!(op, teseladb_range::host::TxnOp::PushTxn { .. }) {
            txn_stats::pushed(&mut lock(&self.h.sessions));
        }
        let reply = self.ask_range(g, op).and_then(across::reply_of)?;
        // A commit's mark comes into the node's clock, as a received mark does: the node's next
        // transaction reads at or above it, so it neither misses the commit nor writes under it
        // and fails with a write conflict (017 SC-002).
        if let Some(ts) = committed_mark(&reply) {
            self.h.clock.0.update(ts);
            self.committed = self.committed.max(ts);
        }
        Ok(reply)
    }

    fn txn_get(
        &mut self,
        key: &[u8],
        at: Hlc,
        view: &TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Option<(u8, Vec<u8>)>, CommandError> {
        let build = |_: &RouteEntry, _| RemoteReq::ReadWith {
            key: key.to_vec(),
            scan: None,
            backward: false,
            at: Some(at),
            concern: ReadConcern::Majority,
            view: view.clone(),
            uncertain_to,
        };
        match self.data(key, true, build)?.1 {
            RemoteReply::Value { value, .. } => Ok(value),
            _ => Err(CommandError::new(INTERNAL_ERROR, "a read answered with no value")),
        }
    }

    fn txn_scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        at: Hlc,
        view: &TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // Range by range in key order at one mark, until the limit or the end (008 FR-003).
        let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut from = start.to_vec();
        while from.as_slice() < end && rows.len() < limit {
            let left = u32::try_from(limit - rows.len()).unwrap_or(u32::MAX);
            let stop = |e: &RouteEntry| match e.end.is_empty() || end <= e.end.as_slice() {
                true => end.to_vec(),
                false => e.end.clone(),
            };
            let build = |e: &RouteEntry, _| RemoteReq::ReadWith {
                key: from.clone(),
                scan: Some((stop(e), left)),
                backward: false,
                at: Some(at),
                concern: ReadConcern::Majority,
                view: view.clone(),
                uncertain_to,
            };
            match self.data(&from, true, build)? {
                (e, RemoteReply::Rows(more)) => {
                    rows.extend(more);
                    from = stop(&e);
                }
                _ => return Err(CommandError::new(INTERNAL_ERROR, "a scan answered with no rows")),
            }
        }
        Ok(rows)
    }

    fn txn_scan_back(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        at: Hlc,
        view: &TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // As `txn_scan`, range by range from the last one down (H-003).
        let make = |_: &RouteEntry, (lo, hi, left): (Vec<u8>, Vec<u8>, u32)| RemoteReq::ReadWith {
            key: lo,
            scan: Some((hi, left)),
            backward: true,
            at: Some(at),
            concern: ReadConcern::Majority,
            view: view.clone(),
            uncertain_to,
        };
        self.scan_ranges_back((start, end, limit), &make, None)
    }

    fn txn_gets(
        &mut self,
        keys: &[Vec<u8>],
        at: Hlc,
        view: &TxnView,
        uncertain_to: Option<Hlc>,
    ) -> Result<Vec<Option<(u8, Vec<u8>)>>, CommandError> {
        self.gets_together(keys, (at, view, uncertain_to))
    }

    fn txn_points(
        &mut self,
        keys: &[Vec<u8>],
        at: Hlc,
        view: &TxnView,
        uncertain_to: Option<Hlc>,
    ) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
        self.points_together(keys, (at, view, uncertain_to))
    }

    fn read_versions(
        &mut self,
        keys: &[Vec<u8>],
    ) -> Result<teseladb_wire::commands::Versions, CommandError> {
        self.versions(keys)
    }

    fn transact(
        &mut self,
        read_ts: Hlc,
        writes: Vec<(Vec<u8>, teseladb_range::intents::IntentOp)>,
        txn: Option<teseladb_range::intents::TxnId>,
    ) -> Result<Hlc, CommandError> {
        self.run_txn(read_ts, writes, txn)
    }

    fn indexes(&mut self, collection: u32) -> Result<Vec<IndexEntry>, CommandError> {
        // Core 0's published copy, pinned here without a message (009 FR-021); before anything
        // is published, core 0 answers and pins it.
        if let Some((all, pin)) = self.h.auth.pins.pin_published(collection) {
            self.pins.push(pin);
            let (_, collections) =
                self.pinned_since.get_or_insert_with(|| (Instant::now(), Vec::new()));
            if !collections.contains(&collection) {
                collections.push(collection);
            }
            return Ok(all);
        }
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Indexes(all, pin)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Indexes(collection), tx), until)
            {
                self.pins.push(pin);
                let (_, collections) =
                    self.pinned_since.get_or_insert_with(|| (Instant::now(), Vec::new()));
                if !collections.contains(&collection) {
                    collections.push(collection);
                }
                return Ok(all);
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn search_indexes(
        &mut self,
        collection: u32,
    ) -> Result<Vec<teseladb_meta::indexes::SearchIndexEntry>, CommandError> {
        // Core 0 publishes them with the indexes before it answers any change of them (009
        // FR-021, H-039): no message, so the search builder's look every 250 ms no longer keeps
        // core 0 awake.
        if let Some(all) = self.h.auth.pins.published_search(collection) {
            return Ok(all);
        }
        let until = self.until();
        loop {
            let ask = |tx| NodeMsg::Catalog(CatalogReq::SearchIndexes(collection), tx);
            if let Some(CatalogAnswer::SearchIndexes(all)) = self.ask(0, ask, until) {
                return Ok(all);
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn index_write(&mut self, cmd: IndexCommand) -> Result<(), CommandError> {
        // The change waits until no operation that read the indexes before it runs: this one
        // writes nothing more with what it read (an index build names its own indexes).
        self.release_pins();
        match self.group0_change(CatalogReq::Index(cmd))? {
            Ok(_) => Ok(()),
            Err(Group0Error::NamespaceNotFound) => {
                Err(CommandError::new(NAMESPACE_NOT_FOUND, "ns does not exist"))
            }
            Err(Group0Error::IndexNotFound) => {
                Err(CommandError::new(INDEX_NOT_FOUND, "index not found"))
            }
            Err(e) => Err(CommandError::new(INTERNAL_ERROR, format!("{e:?}"))),
        }
    }

    fn config_documents(&mut self, coll: &str) -> Result<Vec<teseladb_bson::Document>, CommandError> {
        let until = self.until();
        loop {
            let req = CatalogReq::Config(coll.to_owned());
            if let Some(CatalogAnswer::Documents(docs)) =
                self.ask(0, |tx| NodeMsg::Catalog(req, tx), until)
            {
                let raw = docs.iter().map(|b| teseladb_bson::RawDoc::from_bytes(b));
                return raw
                    .map(|d| d.map(teseladb_bson::Document::from).map_err(Into::into))
                    .collect();
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn config_settings_write(
        &mut self,
        writes: Vec<teseladb_wire::commands::config_settings::SettingsWrite>,
    ) -> Result<bool, CommandError> {
        let changes =
            writes.iter().map(crate::config_db::setting_change).collect::<Result<_, _>>()?;
        match self.group0_change(CatalogReq::SetSettings(changes))? {
            Ok(_) => Ok(true),
            Err(Group0Error::SettingConflict) => Ok(false),
            Err(e) => {
                Err(CommandError::new(INTERNAL_ERROR, format!("Group 0 refused the change: {e:?}")))
            }
        }
    }

    fn group0_settings(
        &mut self,
        prefix: &[u8],
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, CommandError> {
        // Core 0 publishes its copy before it answers any change of it (H-017).
        if let Some(all) = self.h.routes.settings() {
            return Ok(all
                .range(prefix.to_vec()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect());
        }
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Settings(all)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Settings, tx), until)
            {
                return Ok(all.into_iter().filter(|(k, _)| k.starts_with(prefix)).collect());
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn route_entries(&mut self) -> Result<Vec<teseladb_router::table::RouteEntry>, CommandError> {
        // A node that is starting fills its routing table after it is ready: the commands that
        // report it (shards, chunks, `getShardVersion`) wait until it holds the whole data space,
        // as `voters` waits for a span's, instead of answering with a part of the cluster.
        let until = self.until();
        loop {
            let entries = self.h.routes.entries();
            if span::fewest_voters(&entries, &DATA_START, &DATA_END).is_some() {
                return Ok(entries);
            }
            if Instant::now() >= until {
                return Err(self.range_timeout());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn doc_count(&mut self, tenant: u32, collection: u32) -> Result<Option<u64>, CommandError> {
        // At the command's mark, with a read preference or inside a transaction, the documents
        // are read as before; otherwise each range of the collection answers from its leader's
        // counts (007 FR-039), and one that is estimated sends the count back to the read.
        let primary = self.read_pref.as_ref().is_none_or(|p| p.mode == Mode::Primary);
        if self.read_mark.is_some() || !primary {
            return Ok(None);
        }
        let docs = teseladb_keys::space::KeyBuilder::index_prefix(tenant, collection, 0);
        let start = docs.as_bytes().to_vec();
        let end = teseladb_keys::space::KeyBuilder::index_prefix(tenant, collection, 1);
        let end = end.as_bytes().to_vec();
        if let Some(n) = self.count_outside((tenant, collection), (&start, &end)) {
            return Ok(Some(n));
        }
        if let Some(n) = self.doc_count_together((tenant, collection), (&start, &end)) {
            return Ok(Some(n));
        }
        let mut total = 0u64;
        let mut from = start;
        while from < end {
            let req = |_: &RouteEntry, _| RemoteReq::DocCount { tenant, collection };
            match self.data(&from, true, req)? {
                (e, RemoteReply::DocCount { n: Some(n) }) => {
                    total = total.saturating_add(n);
                    if e.end.is_empty() || e.end >= end {
                        break;
                    }
                    from = e.end;
                }
                (_, RemoteReply::DocCount { n: None }) => return Ok(None),
                _ => {
                    return Err(CommandError::new(
                        INTERNAL_ERROR,
                        "a count came with something else",
                    ));
                }
            }
        }
        Ok(Some(total))
    }

    fn range_live_bytes(&mut self, e: &RouteEntry) -> Result<u64, CommandError> {
        // Asked of the range that holds its first key, which answers from its state machine.
        match self.data(&e.start, true, |_, _| RemoteReq::Live)?.1 {
            RemoteReply::Live { bytes } => Ok(bytes),
            _ => Err(CommandError::new(INTERNAL_ERROR, "a range's size came with something else")),
        }
    }

    fn range_order(
        &mut self,
        cmd: teseladb_meta::command::Group0Command,
    ) -> Result<(), CommandError> {
        match self.group0_change(CatalogReq::Security(cmd))? {
            Ok(_) => Ok(()),
            Err(e) => Err(CommandError::new(
                teseladb_wire::error::CONFLICTING_OPERATION_IN_PROGRESS,
                format!("Group 0 refused the change of the ranges: {e:?}"),
            )),
        }
    }

    fn group0_settings_write(
        &mut self,
        changes: Vec<teseladb_meta::command::SettingChange>,
    ) -> Result<bool, CommandError> {
        match self.group0_change(CatalogReq::SetSettings(changes))? {
            Ok(_) => Ok(true),
            Err(Group0Error::SettingConflict) => Ok(false),
            Err(e) => {
                Err(CommandError::new(INTERNAL_ERROR, format!("Group 0 refused the change: {e:?}")))
            }
        }
    }

    fn request_shutdown(&mut self) -> Result<(), CommandError> {
        self.h.shutdown.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn cluster_parameters(&mut self) -> Result<BTreeMap<String, Vec<u8>>, CommandError> {
        // Core 0 publishes its copy before it answers any change of it (H-017): every
        // `aggregate` reads the presentation from here, without a message to core 0.
        if let Some(all) = self.h.routes.settings() {
            return Ok(parameters_of(&all));
        }
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Settings(all)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Settings, tx), until)
            {
                return Ok(parameters_of(&all));
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn write_gate(
        &mut self,
    ) -> Result<teseladb_wire::commands::write_block::WriteGate, CommandError> {
        Ok(self.h.routes.gate())
    }

    fn gate_everywhere(
        &mut self,
        want: teseladb_wire::commands::write_block::WriteGate,
    ) -> Result<bool, CommandError> {
        let until = self.until();
        Ok(self.ask(0, |tx| NodeMsg::GateEverywhere(want, tx), until) == Some(true))
    }

    fn fcv_limit(&mut self) -> Result<Option<String>, CommandError> {
        Ok(self.h.routes.fcv_limit())
    }

    fn ordered_ids(&mut self) -> Result<bool, CommandError> {
        Ok(self.h.routes.ordered_ids())
    }

    fn tenant_state(&mut self, tenant: u32) -> Option<teseladb_meta::tenants::TenantState> {
        self.h.routes.tenants().get(&tenant).cloned()
    }

    fn tenant_begin(
        &mut self,
        tenant: u32,
        user: &teseladb_auth::users::UserName,
        bytes: u64,
        grows: bool,
    ) -> Result<Option<u64>, CommandError> {
        let routes = self.h.routes.clone();
        let mut read = || crate::tenant_limits::storage_of(self, tenant).ok();
        crate::tenant_limits::begin(&routes, tenant, user, (bytes, grows), &mut read)
    }

    fn tenant_end(&mut self, tenant: u32) {
        self.h.routes.limiter().end(tenant);
    }

    fn tenant_name(&mut self, tenant: u32) -> Option<String> {
        self.h.routes.tenant_name(tenant)
    }

    fn set_cluster_parameter(&mut self, name: &str, value: Vec<u8>) -> Result<(), CommandError> {
        let key = [PARAMETER_PREFIX, name.as_bytes()].concat();
        let req = CatalogReq::SetSetting { key, value };
        self.catalog_change(req).map(|_| ())
    }

    fn set_read_preference(&mut self, pref: Option<ReadPreference>) {
        self.read_pref = pref;
        self.selection = None;
    }

    fn set_read_mark(&mut self, at: Option<teseladb_txn::causal::ReadMark>) {
        self.read_mark = at;
    }

    fn read_mark(&self) -> Option<teseladb_txn::causal::ReadMark> {
        self.read_mark
    }

    fn set_deadline(&mut self, at_ms: Option<u64>) {
        self.max_until = at_ms.map(|at| {
            let left = at.saturating_sub(self.now_micros() / 1000);
            Instant::now() + Duration::from_millis(left)
        });
    }

    fn now_ms(&self) -> u64 {
        self.h.clock.0.peek().physical
    }

    fn wall_ms(&self) -> u64 {
        crate::clock::wall_ms()
    }

    fn cluster_mark(&self) -> Hlc {
        self.h.clock.0.peek().max(self.committed)
    }

    fn observe_mark(&mut self, mark: Hlc) {
        self.h.clock.0.update(mark);
    }

    fn last_commit(&self) -> Option<Hlc> {
        Some(self.committed).filter(|c| *c > Hlc { physical: 0, logical: 0 })
    }

    fn now_micros(&self) -> u64 {
        u64::try_from(process_start().elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn authorization_lease(&self) -> bool {
        self.h.auth.valid()
    }

    fn random(&mut self) -> u64 {
        // xorshift64*.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn node(&self) -> u64 {
        self.h.node.0
    }

    fn principal(
        &mut self,
        kind: Principal,
        db: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, CommandError> {
        let req = CatalogReq::Principal { kind, db: db.to_owned(), name: name.to_owned() };
        Ok(self.principal_read(req)?.into_iter().next())
    }

    fn principal_by_id(
        &mut self,
        kind: Principal,
        id: &[u8],
    ) -> Result<Option<Vec<u8>>, CommandError> {
        let req = CatalogReq::PrincipalById { kind, id: id.to_vec() };
        Ok(self.principal_read(req)?.into_iter().next())
    }

    fn principals(
        &mut self,
        kind: Principal,
        db: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, CommandError> {
        self.principal_read(CatalogReq::Principals { kind, db: db.map(str::to_owned) })
    }

    fn security_write(
        &mut self,
        ops: Vec<SecurityOp>,
    ) -> Result<Result<(), SecurityError>, CommandError> {
        // A change that takes privileges away waits for the fence, which this operation's own
        // reads of the indexes would hold back; it writes no document after it.
        self.release_pins();
        // Idempotent, so that a retry after a lost answer gets the first result instead of
        // failing on the change it made.
        let req = RequestId { origin: NodeId(self.h.node.0), id: self.random() };
        let cmd = Group0Command::Idempotent { req, cmd: Box::new(Group0Command::Security(ops)) };
        match self.group0_change(CatalogReq::Security(cmd))? {
            Ok(_) => Ok(Ok(())),
            Err(Group0Error::PrincipalExists(p)) => Ok(Err(SecurityError::Exists(p))),
            Err(Group0Error::PrincipalNotFound(p)) => Ok(Err(SecurityError::NotFound(p))),
            Err(Group0Error::SecurityConflict) => Ok(Err(SecurityError::Conflict)),
            Err(e) => {
                Err(CommandError::new(INTERNAL_ERROR, format!("Group 0 refused the change: {e:?}")))
            }
        }
    }

    fn kerberos_keys(&self) -> Vec<teseladb_auth::kerberos::ServiceKey> {
        self.diag.as_ref().map(|d| d.kerberos().to_vec()).unwrap_or_default()
    }

    fn oidc_providers(&self) -> teseladb_auth::oidc::Providers {
        self.oidc().map(|o| o.providers.clone()).unwrap_or_default()
    }

    fn oidc_keys(
        &mut self,
        issuer: &str,
        refresh: bool,
    ) -> Result<teseladb_auth::oidc::KeySet, CommandError> {
        let failed = |m: &str| CommandError::new(teseladb_wire::error::OPERATION_FAILED, m);
        let (own, diag) = (self.oidc.clone(), self.diag.clone());
        let Some(oidc) = own.as_deref().or_else(|| diag.as_ref().and_then(|d| d.oidc())) else {
            return Err(failed("no identity provider"));
        };
        oidc.keys(issuer, refresh).map_err(|e| {
            let attr = teseladb_bson::Document::new().with("issuer", issuer).with("error", e);
            self.log_warning(7070200, "Failed to load JWKS", &attr);
            failed("Failed to load JWKS")
        })
    }

    fn aws_sts_url(&self) -> String {
        let configured = self.diag.as_ref().and_then(|d| d.sts()).map(|s| s.url.clone());
        configured.unwrap_or_else(|| teseladb_auth::aws::DEFAULT_STS_URL.to_owned())
    }

    fn aws_sts(
        &mut self,
        req: &teseladb_auth::aws::StsRequest,
    ) -> Result<(u16, Vec<u8>), CommandError> {
        let unreachable = |m: &str| CommandError::new(teseladb_wire::error::OPERATION_FAILED, m);
        let Some(sts) = self.diag.as_ref().and_then(|d| d.sts()).cloned() else {
            return Err(unreachable("no STS"));
        };
        sts.send(req).map_err(|e| {
            // RefDb's log line when the STS does not answer (measured on 9.0.4).
            let attr =
                teseladb_bson::Document::new().with("awsSTSURL", sts.url.as_str()).with("error", e);
            self.log_warning(4690900, "Failed connecting to AWS STS", &attr);
            unreachable("Failed connecting to AWS STS")
        })
    }

    fn invalidate_user_cache(&mut self) {
        if let Some(ldap) = self.diag.as_ref().and_then(|d| d.ldap()) {
            ldap.invalidate();
        }
    }

    fn ldap_login(&mut self, user: &str, password: &[u8]) -> Result<LdapLogin, CommandError> {
        let Some(ldap) = self.diag.as_ref().and_then(|d| d.ldap()) else {
            return Ok(LdapLogin::Refused);
        };
        // RefDb answers `Authentication failed.` whatever the LDAP server did; the reason
        // goes to the log.
        ldap.login(user, password).or_else(|e| {
            self.log_warning(
                24055,
                "Failed to bind to LDAP",
                &teseladb_bson::Document::new().with("error", e),
            );
            Ok(LdapLogin::Refused)
        })
    }

    fn ldap_groups(
        &mut self,
        user: &str,
    ) -> Result<Option<Vec<teseladb_auth::users::RoleName>>, CommandError> {
        let Some(ldap) = self.diag.as_ref().and_then(|d| d.ldap()) else { return Ok(None) };
        // An LDAP server that does not answer fails the login, with the reason in the log.
        ldap.groups(user).map_err(|e| {
            self.log_warning(
                24055,
                "Failed to bind to LDAP",
                &teseladb_bson::Document::new().with("error", e),
            );
            CommandError::new(teseladb_wire::error::AUTHENTICATION_FAILED, "Authentication failed.")
        })
    }

    fn secure_random(&mut self, out: &mut [u8]) -> Result<(), CommandError> {
        teseladb_sys::fill_random(out).map_err(|e| {
            CommandError::new(INTERNAL_ERROR, format!("the system gave no random bytes: {e}"))
        })
    }

    fn log_slow_query(&mut self, attr: &teseladb_bson::Document) {
        let attr = attr
            .to_bytes()
            .ok()
            .and_then(|b| {
                teseladb_bson::RawDoc::from_bytes(&b).ok().map(teseladb_bson::extjson::to_relaxed)
            })
            .unwrap_or_default();
        let line = crate::log::Pending {
            id: teseladb_wire::commands::profile::SLOW_QUERY_ID,
            msg: "Slow query".to_owned(),
            attr,
        };
        crate::log::defer_at(crate::log::Level::Info, line);
    }

    fn log_warning(&mut self, id: i32, msg: &str, attr: &teseladb_bson::Document) {
        // A line that does not encode says so instead of its attributes.
        let attr = match attr.to_bytes() {
            Ok(b) => match teseladb_bson::RawDoc::from_bytes(&b) {
                Ok(raw) => teseladb_bson::extjson::to_relaxed(raw),
                Err(e) => e.to_string(),
            },
            Err(e) => e.to_string(),
        };
        crate::log::defer(crate::log::Pending { id, msg: msg.to_owned(), attr });
    }

    fn thread_cpu_nanos(&mut self) -> Option<u64> {
        // As RefDb 9.0.4, whose operation CPU timers exist on Linux alone: elsewhere no
        // operation reports `cpuNanos`, and none pays the system calls that read a thread's CPU
        // time (four per operation, H-013 of the native tests on macOS).
        if cfg!(target_os = "linux") { teseladb_sys::host::thread_cpu_nanos().ok() } else { None }
    }

    fn host_port(&mut self) -> String {
        self.diag.as_ref().map_or_else(|| ":0".to_owned(), |d| d.host_port().to_owned())
    }

    fn process(&mut self) -> teseladb_wire::commands::diagnostics::ProcessFacts {
        self.diag.as_ref().map(|d| d.facts()).unwrap_or_default()
    }

    fn rotate_certificates(&mut self, message: Option<&str>) -> Result<(), CommandError> {
        let Some(certs) = self.diag.as_ref().and_then(|d| d.certificates()) else {
            return Ok(());
        };
        certs
            .reload()
            .map_err(|e| CommandError::new(teseladb_wire::error::INVALID_SSL_CONFIGURATION, e))?;
        // RefDb's line, with the operator's note.
        let attr = match message {
            Some(m) => {
                let mut text = String::from("{\"message\":");
                crate::log::json_string(&mut text, m);
                text.push('}');
                text
            }
            None => String::from("{}"),
        };
        let line = crate::log::Pending { id: ROTATION_LOG_ID, msg: ROTATION_DONE.to_owned(), attr };
        crate::log::defer_at(crate::log::Level::Info, line);
        Ok(())
    }
}

/// The request of a range for its part `(start, end, documents still missing)` of a backward
/// read (H-003).
type PartRequest<'a> = dyn Fn(&RouteEntry, (Vec<u8>, Vec<u8>, u32)) -> RemoteReq + 'a;

/// The scan of `[start, end)` of at most `limit` documents in range `e`, from its last key down
/// if `backward` (H-003): on the leader, or, with a read preference other than `primary`
/// (`prefer`: the preference, this node and the nodes' tags), on its candidates in turn (007
/// FR-013).
pub(crate) fn scan_req(
    (pref, me, tags): &(Option<ReadPreference>, NodeId, Arc<crate::routes::NodeTags>),
    e: &RouteEntry,
    (start, end, limit): (Vec<u8>, Vec<u8>, u32),
    (concern, backward): (ReadConcern, bool),
) -> RemoteReq {
    let Some(pref) = pref.as_ref().filter(|p| p.mode != Mode::Primary) else {
        return RemoteReq::Scan { start, end, limit, concern, backward };
    };
    let replicas: Vec<Replica> = e
        .voters
        .iter()
        .map(|n| Replica {
            node: *n,
            leader: e.leader_hint == Some(*n),
            // No round trips are measured yet: this node's replica is the nearest.
            latency_ms: u64::from(n != me),
            tags: tags.get(n).cloned().unwrap_or_default(),
        })
        .collect();
    // Each candidate goes with whether the leader may serve the read, should it turn out to lead
    // the range: `secondary` never reads the primary, whatever the router knows of the leader.
    let leader_ok = pref.mode != Mode::Secondary;
    let nodes = candidates(pref, &replicas).into_iter().map(|n| (n, leader_ok)).collect();
    let max_stale_ms = pref.max_staleness_ms;
    RemoteReq::FollowerScan { start, end, limit, max_stale_ms, nodes, backward }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod part_tests {
    use super::next_from;

    fn rows(keys: &[&[u8]]) -> Vec<(Vec<u8>, Vec<u8>)> {
        keys.iter().map(|k| (k.to_vec(), Vec::new())).collect()
    }

    /// A full part of a scan goes on past its last key, not at the range's end: a part smaller
    /// than the whole scan that took the range's end as where it stopped skipped the rest of the
    /// range (H-051 of the native tests: 27,190 documents read as lost in the sustained load).
    #[test]
    fn a_full_part_goes_on_past_its_last_key_and_a_short_one_at_the_end() {
        let end = b"\x02".to_vec();
        assert_eq!(next_from(end.clone(), &rows(&[b"\x01a", b"\x01b"]), 2), b"\x01b\x00".to_vec());
        assert_eq!(next_from(end.clone(), &rows(&[b"\x01a"]), 2), end);
        assert_eq!(next_from(end.clone(), &[], 2), end);
    }
}
