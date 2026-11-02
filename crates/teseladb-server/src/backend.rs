//! The commands' [`Backend`] on a running node (006 FR-014, FR-027): the catalog through core 0's
//! copy of Group 0 and its leader, the data range through its core, which runs the request if it
//! leads the range and forwards it to the leader otherwise. A client worker waits for each
//! answer; a request the range did not take (no leader yet, overloaded) is tried again until
//! [`REQUEST_DEADLINE`], and a write that may have been proposed only if it is retryable.

mod across;
mod intent_wait;
mod txn_reaper;

pub(crate) use txn_reaper::SCAN_EVERY;

use crate::authz_lease::{AuthGate, DEADLINE_SKEW};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_hlc::Hlc;
use teseladb_io::runtime::{CoreInput, CoreSender, CoreSenders};
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
use teseladb_types::NodeId;
use teseladb_wire::commands::session::KillScope;
use teseladb_wire::commands::{Backend, LdapLogin, Shared, WriteOp, lock, txn_stats};
use teseladb_wire::error::{
    CommandError, DATABASE_DIFFER_CASE, FAILED_TO_SATISFY_READ_PREFERENCE, INDEX_NOT_FOUND,
    INTERNAL_ERROR, NAMESPACE_EXISTS, NAMESPACE_NOT_FOUND, NETWORK_TIMEOUT, NOT_WRITABLE_PRIMARY,
    RangeContext, from_range,
};

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
    /// The pins of the running operation's reads of the indexes (009 FR-006).
    pins: Vec<u64>,
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
}

impl ServerBackend {
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
            pins: Vec::new(),
            unsettled: None,
            read_pref: None,
            selection: None,
            read_mark: None,
        }
    }

    /// The backend reporting the facts of `diag` to the diagnostic commands.
    pub fn with_diag(self, diag: Arc<crate::diag::Diag>) -> ServerBackend {
        let mut b = self;
        b.diag = Some(diag);
        b
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
        start: &[u8],
        end: &[u8],
        limit: usize,
        mut mark: teseladb_txn::causal::ReadMark,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        let view = TxnView::default();
        let mut restarts = 0;
        loop {
            match self.txn_scan(start, end, limit, mark.at, &view, mark.uncertain_to) {
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
        while from.as_slice() < end && rows.len() < limit {
            let left = u32::try_from(limit - rows.len()).unwrap_or(u32::MAX);
            let stop = |e: &RouteEntry| match e.end.is_empty() || end <= e.end.as_slice() {
                true => end.to_vec(),
                false => e.end.clone(),
            };
            let build =
                |e: &RouteEntry, _| scan_req(&prefer, e, (from.clone(), stop(e), left), concern);
            match self.data(&from, true, build)? {
                (e, RemoteReply::Rows(more)) => {
                    rows.extend(more);
                    from = stop(&e);
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
                    | RemoteReply::Fenced { .. },
                ) => {
                    return Err(CommandError::new(INTERNAL_ERROR, "a scan answered with no rows"));
                }
            }
        }
        Ok(rows)
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
        self.cores.get(core)?.send(CoreInput::App(msg(tx))).ok()?;
        rx.recv_timeout(until.saturating_duration_since(Instant::now())).ok()
    }

    fn catalog(&mut self) -> Result<Arc<Catalog>, CommandError> {
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
                    | CatalogAnswer::Tenants(_)
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
        for pin in self.pins.drain(..) {
            match late {
                Some(until) => self.h.auth.release_pin_after(pin, until),
                None => self.h.auth.pins.release(pin),
            }
        }
    }

    /// The deadline a write whose wait ends at `until` carries (009 FR-006): the node's clock
    /// then, in the milliseconds of its marks, plus [`DEADLINE_SKEW`].
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
            // retryable write or a read.
            let again = match &e {
                RangeError::NotLeader { .. } | RangeError::Overloaded | RangeError::OutOfRange => {
                    true
                }
                RangeError::Timeout | RangeError::Unavailable => retryable || !sent,
                // A write outside a transaction waits for the intent's owner (008 FR-002).
                RangeError::IntentConflict(owner) if plain_write => {
                    let owner = owner.clone();
                    self.wait_for_intent(&owner);
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
                    true
                }
                _ => false,
            };
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
                let ctx = RangeContext { retryable_write: retryable, max_time_expired };
                return Err(from_range(&e, ctx));
            }
            thread::sleep(RETRY_PAUSE);
        }
    }
}

/// Prefix of the Group 0 settings that hold the cluster parameters (`setClusterParameter`).
pub const PARAMETER_PREFIX: &[u8] = b"clusterParameter.";

/// The cluster parameters among Group 0's settings, by name.
pub fn parameters_of(settings: &BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    settings
        .iter()
        .filter_map(|(k, v)| {
            let name = k.strip_prefix(PARAMETER_PREFIX)?;
            Some((String::from_utf8(name.to_vec()).ok()?, v.clone()))
        })
        .collect()
}

/// The instant the process first read the monotonic clock, the origin of
/// [`Backend::now_micros`].
fn process_start() -> Instant {
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

impl Drop for ServerBackend {
    fn drop(&mut self) {
        self.release_pins();
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
            if let Some(CatalogAnswer::Tenants(all)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Tenants, tx), until)
            {
                return Ok(all.into_iter().find(|(_, n)| n == name).map(|(id, _)| id));
            }
            if Instant::now() >= until {
                return Err(unavailable());
            }
            thread::sleep(RETRY_PAUSE);
        }
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
            RemoteReply::Written { result, .. } => Ok(result),
            RemoteReply::Rows(_)
            | RemoteReply::Value { .. }
            | RemoteReply::Intents { .. }
            | RemoteReply::Record(_)
            | RemoteReply::Piece { .. }
            | RemoteReply::Restored { .. }
            | RemoteReply::Changes { .. }
            | RemoteReply::Fenced { .. } => {
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
            return self.scan_at(start, end, limit, mark);
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
        let overlaps = |e: &RouteEntry| {
            (e.end.is_empty() || start < e.end.as_slice()) && e.start.as_slice() < end
        };
        self.h
            .routes
            .entries()
            .iter()
            .filter(|e| overlaps(e))
            .map(|e| e.voters.len())
            .min()
            .ok_or_else(unavailable)
    }

    // The dispatcher's wrapper keeps the plan of the operation it runs.
    fn note_plan(&mut self, _: teseladb_bson::Document) {}

    // The dispatcher's wrapper counts what the operation examines.
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
    }

    fn changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: Hlc,
    ) -> Result<teseladb_wire::commands::ChangeBatch, CommandError> {
        // The batch is resolved up to the lowest resolved mark of its ranges (FR-004, FR-005).
        let mut read = teseladb_changestream::read::IntervalRead::new(start, end, from);
        for r in self.range_changes(start, end, from)? {
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
    ) -> Result<Vec<teseladb_wire::commands::RangeBatch>, CommandError> {
        // Range by range in key order, each read on its leader (014 FR-003, FR-010) and kept up
        // to its own resolved mark.
        let mut out = Vec::new();
        let mut walk = teseladb_changestream::read::IntervalRead::<()>::new(start, end, from);
        while let Some(at) = walk.next().map(<[u8]>::to_vec) {
            let key = at.clone();
            let build = |e: &RouteEntry, _| RemoteReq::Changes {
                start: key.clone(),
                end: walk.stop(&e.end),
                from,
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
        self.ask_range(g, op).and_then(across::reply_of)
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
        let until = self.until();
        loop {
            if let Some(CatalogAnswer::Indexes(all, pin)) =
                self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Indexes(collection), tx), until)
            {
                self.pins.push(pin);
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

    fn config_documents(
        &mut self,
        coll: &str,
    ) -> Result<Vec<teseladb_bson::Document>, CommandError> {
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

    fn request_shutdown(&mut self) -> Result<(), CommandError> {
        self.h.shutdown.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn cluster_parameters(&mut self) -> Result<BTreeMap<String, Vec<u8>>, CommandError> {
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
        self.h.clock.0.peek()
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
        self.diag.as_ref().and_then(|d| d.oidc()).map(|o| o.providers.clone()).unwrap_or_default()
    }

    fn oidc_keys(
        &mut self,
        issuer: &str,
        refresh: bool,
    ) -> Result<teseladb_auth::oidc::KeySet, CommandError> {
        let failed = |m: &str| CommandError::new(teseladb_wire::error::OPERATION_FAILED, m);
        let diag = self.diag.clone();
        let Some(oidc) = diag.as_ref().and_then(|d| d.oidc()) else {
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
        teseladb_sys::host::thread_cpu_nanos().ok()
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

/// The scan of `[start, end)` of at most `limit` documents in range `e`: on the leader, or, with a
/// read preference other than `primary` (`prefer`: the preference, this node and the nodes' tags),
/// on its candidates in turn (007 FR-013).
fn scan_req(
    (pref, me, tags): &(Option<ReadPreference>, NodeId, Arc<crate::routes::NodeTags>),
    e: &RouteEntry,
    (start, end, limit): (Vec<u8>, Vec<u8>, u32),
    concern: ReadConcern,
) -> RemoteReq {
    let Some(pref) = pref.as_ref().filter(|p| p.mode != Mode::Primary) else {
        return RemoteReq::Scan { start, end, limit, concern };
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
    let nodes =
        candidates(pref, &replicas).into_iter().map(|n| (n, e.leader_hint == Some(n))).collect();
    RemoteReq::FollowerScan { start, end, limit, max_stale_ms: pref.max_staleness_ms, nodes }
}
