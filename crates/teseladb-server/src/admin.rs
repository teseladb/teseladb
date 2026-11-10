//! `teseladb admin` (007 FR-020, contract `admin-cli.md`): the routes of the administration port
//! ([`route`], over [`AdminBackend`]), what a node answers on them ([`NodeAdmin`]) and the
//! command line that calls them ([`parse`], [`run`]). Answers are relaxed Extended JSON; `/metrics`
//! is Prometheus' text format and `/healthz` has no body.

use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use teseladb_auth::authz;
use teseladb_auth::privileges::{self, Privilege};
use teseladb_auth::users::{EXTERNAL_DB, RoleName, UserName};
use teseladb_io::runtime::{CoreInput, CoreSender, CoreSenders};
use teseladb_meta::catalog::{Catalog, Namespace};
use teseladb_meta::command::Group0Command;
use teseladb_meta::drain::DrainError;
use teseladb_rest::media::{self, Format};
use teseladb_wire::commands::parameters::{GET_CLUSTER_PARAMETER, SET_CLUSTER_PARAMETER};
use teseladb_wire::commands::roles::Graph;
use teseladb_wire::commands::{Backend, Ctx, Shared, run as run_command};
use teseladb_bson::{Document, Value};
use teseladb_tls::{CertifiedKey, ClientConfig, Trust};
use teseladb_types::{GroupId, NodeId};
use teseladb_x509::{Certificate, PrivateKey};

use crate::admin_http::{self, Caller, HttpRequest, HttpResponse, JSON, Method, PROMETHEUS};
use crate::backend::{NodeHandle, ServerBackend};
use crate::catalog_ops::{CatalogAnswer, CatalogReq};
use crate::cli::{TLS_CA_FILE, TLS_CERTIFICATE_KEY_FILE};
use crate::data::{DataReply, DataReq};
use crate::enroll::Enroller;
use crate::logic::NodeMsg;

/// `GET`: 200 if the node serves, 503 if not; no body.
pub const HEALTHZ: &str = "/healthz";
/// `GET`: the node's metrics.
pub const METRICS: &str = "/metrics";
/// `GET`: the nodes of the cluster.
pub const NODES: &str = "/admin/nodes";
/// `GET`: the ranges (`?collection=db.c` for those of a collection).
pub const RANGES: &str = "/admin/ranges";
/// `POST /admin/ranges/<id>/split`, `/merge` and `/relocate?core=<n>`.
pub const RANGE: &str = "/admin/ranges/";
/// `GET` or `POST /admin/config/<parameter>`.
pub const CONFIG: &str = "/admin/config/";
/// `POST`: a node enrollment token (`?ttl=<seconds>`).
pub const TOKENS: &str = "/admin/tokens";
/// `POST`: a new key and certificate for the node, loaded without a restart (013 FR-006).
pub const CERT_ROTATE: &str = "/admin/certificates/rotate";
/// Last part of the path that splits a range.
pub const SPLIT: &str = "split";
/// Last part of the path that merges a range with its right neighbour.
pub const MERGE: &str = "merge";
/// Last part of the path that moves a range to another core (007 FR-011).
pub const RELOCATE: &str = "relocate";
/// `POST /admin/nodes/<id>/drain` and `/remove` (007 FR-015).
pub const NODE: &str = "/admin/nodes/";
/// Last part of the path that drains a node.
pub const DRAIN: &str = "drain";
/// Last part of the path that removes a node.
pub const REMOVE: &str = "remove";
/// How long a token lives without `--ttl`.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);
/// How long a node's core may take to answer the port.
pub const CORE_WAIT: Duration = Duration::from_secs(5);
// The client waits for the longest handler: a core's answer and then a Group 0 change.
const _: () = assert!(
    admin_http::ANSWER_WAIT.as_secs() > CORE_WAIT.as_secs() + crate::node::ENROLL_WAIT.as_secs()
);

/// Why the port could not do what it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminError {
    /// HTTP status.
    pub status: u16,
    /// What went wrong.
    pub message: String,
}

impl AdminError {
    /// A request that cannot be done as asked (400).
    pub fn bad(message: impl Into<String>) -> AdminError {
        AdminError { status: 400, message: message.into() }
    }

    /// Something the request names does not exist (404).
    pub fn not_found(message: impl Into<String>) -> AdminError {
        AdminError { status: 404, message: message.into() }
    }

    /// The node cannot do it now (503).
    pub fn unavailable(message: impl Into<String>) -> AdminError {
        AdminError { status: 503, message: message.into() }
    }
}

/// The answer of an administration route.
pub type Answer = Result<Document, AdminError>;

/// What a node does for the administration port.
pub trait AdminBackend: Send + Sync {
    /// Whether the node serves: the cluster exists and Group 0 has its addresses.
    fn healthy(&self) -> bool;
    /// The node's metrics in Prometheus' text format.
    fn metrics(&self) -> String;
    /// `{nodes: [{_id, voter, learner, state}], group0Leader, self, featureCompatibilityVersion: {current, available}}`,
    /// `state` being `active`, `draining` or `removed`, `self` the node that answers (whoever runs the node ties its
    /// address to its number with it, to drain it, 021 FR-040) and `available` the feature compatibility version every
    /// node runs, where a change of version node by node ends (021 FR-041).
    fn nodes(&self) -> Answer;
    /// `{ranges: [{_id, start, end, core, voters, leaderHint}]}`, of collection `ns` if given.
    fn ranges(&self, ns: Option<&str>) -> Answer;
    /// Asks Group 0 to split `range` at the key of document `at` (`{ns, _id}`), or where a split by
    /// size would cut it.
    fn split(&self, range: GroupId, at: Option<Document>) -> Answer;
    /// Asks Group 0 to merge `range` with its right neighbour.
    fn merge(&self, range: GroupId) -> Answer;
    /// Asks Group 0 to move `range` to core `core` of every node (007 FR-011).
    fn relocate(&self, range: GroupId, core: u16) -> Answer;
    /// Drains node `node` (`true`) or removes it (007 FR-015).
    fn drain(&self, node: NodeId, drain: bool) -> Answer;
    /// `getClusterParameter: name`.
    fn parameter(&self, name: &str) -> Answer;
    /// `setClusterParameter: {name: value}`.
    fn set_parameter(&self, name: &str, value: Document) -> Answer;
    /// A node enrollment token valid for `ttl` (013 FR-005): `{token}`.
    fn create_token(&self, ttl: Duration) -> Answer;
    /// A new node key and certificate of the cluster's CA, in use at once (013 FR-006).
    fn rotate_certificates(&self) -> Answer;
    /// The privileges of `user`, with those of the roles it inherits; none if it does not exist.
    fn privileges(&self, user: &UserName) -> Result<Vec<Privilege>, AdminError>;
    /// A step of a backup or a restore (007 FR-021).
    fn backup(&self, op: crate::backup::Op) -> Answer;
    /// `{tenants: [{_id, name}]}` (007 FR-025).
    fn tenants(&self) -> Answer;
    /// Creates tenant `name`: `{_id}`.
    fn create_tenant(&self, name: &str) -> Answer;
    /// Drops tenant `name`, which has no database and no user left.
    fn drop_tenant(&self, name: &str) -> Answer;
    /// Reads or changes the limits, the access list or the suspension of tenant `name` (007
    /// FR-030, FR-034, FR-035).
    fn tenant_state(&self, name: &str, op: crate::admin_tenant_state::StateOp) -> Answer;
    /// Runs a command of the API keys (`createApiKey`, `listApiKeys`, `revokeApiKey`, 019
    /// FR-016) with the port's authority.
    fn api_keys(&self, cmd: &Document) -> Answer;
}

/// The built-in role whose privileges `/metrics` asks for (013 FR-017).
pub const METRICS_ROLE: &str = "clusterMonitor";
/// The built-in role whose privileges the commands ask for (013 FR-017).
pub const COMMANDS_ROLE: &str = "clusterAdmin";

fn refused(status: u16, message: String) -> HttpResponse {
    json(Err(AdminError { status, message }))
}

/// The answer of the port to `req` from `caller`: `/healthz` for anyone; `/metrics` for a user
/// with the privileges of `clusterMonitor`, every other route with those of `clusterAdmin`
/// (013 FR-017), and anything for a node of the cluster or on a node without TLS.
pub fn answer(req: &HttpRequest, caller: &Caller, b: &dyn AdminBackend) -> HttpResponse {
    if req.path == HEALTHZ {
        return route(req, b);
    }
    let user = match caller {
        Caller::Open | Caller::Member => return route(req, b),
        Caller::Anonymous => {
            return refused(
                401,
                "the administration port needs a client certificate over TLS".to_owned(),
            );
        }
        Caller::User(name) => UserName::new(name.clone(), EXTERNAL_DB),
    };
    let role = if req.path == METRICS { METRICS_ROLE } else { COMMANDS_ROLE };
    let needed = privileges::builtin(&RoleName::new(role, "admin")).unwrap_or_default();
    match b.privileges(&user) {
        Ok(has) if authz::holds_all(&has, &needed) => route(req, b),
        Ok(_) => refused(
            403,
            format!("{user} is not authorized: {} needs the privileges of {role}", req.path),
        ),
        Err(e) => json(Err(e)),
    }
}

/// The answer of the port to `req`.
pub fn route(req: &HttpRequest, b: &dyn AdminBackend) -> HttpResponse {
    let path = req.path.as_str();
    let get = req.method == Method::Get;
    let wrong_method = || HttpResponse::text(405, "");
    match path {
        HEALTHZ if get => HttpResponse::text(if b.healthy() { 200 } else { 503 }, ""),
        METRICS if get => {
            HttpResponse { status: 200, content_type: PROMETHEUS, body: b.metrics().into_bytes() }
        }
        NODES if get => json(b.nodes()),
        RANGES if get => json(b.ranges(req.param("collection"))),
        TOKENS if !get => match req.param("ttl").map(str::parse::<u64>) {
            None => json(b.create_token(DEFAULT_TTL)),
            Some(Ok(secs)) => json(b.create_token(Duration::from_secs(secs))),
            Some(Err(_)) => json(Err(AdminError::bad("ttl: seconds"))),
        },
        CERT_ROTATE if !get => json(b.rotate_certificates()),
        HEALTHZ | METRICS | NODES | RANGES | TOKENS | CERT_ROTATE => wrong_method(),
        _ => {
            if let Some(answer) = crate::backup::route(req, |op| b.backup(op)) {
                return answer;
            }
            if let Some(answer) = crate::admin_tenants::route(path, req, b) {
                return answer;
            }
            if let Some(answer) = crate::admin_apikeys::route(path, req, b) {
                return answer;
            }
            if let Some(rest) = path.strip_prefix(RANGE) {
                return range_route(rest, req, b);
            }
            if let Some(rest) = path.strip_prefix(NODE) {
                return node_route(rest, req, b);
            }
            if let Some(name) = path.strip_prefix(CONFIG) {
                return match req.method {
                    Method::Get => json(b.parameter(name)),
                    Method::Post => json(body_doc(req).and_then(|d| match d {
                        Some(d) => b.set_parameter(name, d),
                        None => Err(AdminError::bad("the parameter's value is missing")),
                    })),
                };
            }
            HttpResponse::text(404, "")
        }
    }
}

/// `POST /admin/ranges/<id>/split`, `/merge` and `/relocate?core=<n>`.
fn range_route(rest: &str, req: &HttpRequest, b: &dyn AdminBackend) -> HttpResponse {
    let Some((id, op)) = rest.split_once('/') else { return HttpResponse::text(404, "") };
    if op != SPLIT && op != MERGE && op != RELOCATE {
        return HttpResponse::text(404, "");
    }
    if req.method != Method::Post {
        return HttpResponse::text(405, "");
    }
    let Ok(range) = id.parse::<u64>().map(GroupId) else {
        return json(Err(AdminError::bad(format!("not a range: {id}"))));
    };
    if op == MERGE {
        return json(b.merge(range));
    }
    if op == RELOCATE {
        return match req.param("core").map(str::parse::<u16>) {
            Some(Ok(core)) => json(b.relocate(range, core)),
            _ => json(Err(AdminError::bad("core: the number of a core"))),
        };
    }
    json(body_doc(req).and_then(|at| b.split(range, at)))
}

/// The state of node `node` as the port names it: `active`, `draining` or `removed` (007 FR-015).
fn node_state(
    states: &[(NodeId, teseladb_meta::command::NodeState)],
    node: NodeId,
) -> &'static str {
    use teseladb_meta::command::NodeState;
    match states.iter().find(|(id, _)| *id == node).map(|(_, s)| *s) {
        Some(NodeState::Draining) => "draining",
        Some(NodeState::Removed) => "removed",
        Some(NodeState::Active) | None => "active",
    }
}

/// `POST /admin/nodes/<id>/drain` and `/remove`.
fn node_route(rest: &str, req: &HttpRequest, b: &dyn AdminBackend) -> HttpResponse {
    let Some((id, op)) = rest.split_once('/') else { return HttpResponse::text(404, "") };
    if op != DRAIN && op != REMOVE {
        return HttpResponse::text(404, "");
    }
    if req.method != Method::Post {
        return HttpResponse::text(405, "");
    }
    match id.parse::<u64>() {
        Ok(n) => json(b.drain(NodeId(n), op == DRAIN)),
        Err(_) => json(Err(AdminError::bad(format!("not a node: {id}")))),
    }
}

/// The request's body as a document (`None` if it is empty).
pub(crate) fn body_doc(req: &HttpRequest) -> Result<Option<Document>, AdminError> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    media::decode(&req.body, Format::Relaxed)
        .map(Some)
        .map_err(|e| AdminError::bad(format!("the body is not an Extended JSON document: {e:?}")))
}

/// An answer as relaxed Extended JSON: the document, or `{error}` with its status.
pub(crate) fn json(a: Answer) -> HttpResponse {
    let (status, doc) = match a {
        Ok(d) => (200, d),
        Err(e) => (e.status, Document::new().with("error", e.message)),
    };
    match media::encode(&doc, Format::Relaxed) {
        Ok(body) => HttpResponse { status, content_type: JSON, body },
        Err(e) => HttpResponse::text(500, &format!("{e:?}")),
    }
}

/// What a node answers on the administration port: its cores through their mailboxes.
#[derive(Clone)]
pub struct NodeAdmin {
    /// The node's handle (cores, routes, sessions, clock, authorization).
    pub handle: NodeHandle,
    /// Issues enrollment tokens, on a node with a PKI.
    pub enroller: Option<Enroller>,
    /// What `cert rotate` renews, on a node with a PKI.
    pub certificates: Option<std::sync::Arc<crate::certs::Certificates>>,
    /// The node's memory and its split (012 FR-013); `None` when the system did not say how much
    /// there is and none was given.
    pub memory: Option<crate::memory::MemoryBudget>,
    /// The metrics of the HTTP port (019 FR-030), when the node opened it.
    pub http_metrics: Option<std::sync::Arc<std::sync::Mutex<teseladb_http::metrics::HttpMetrics>>>,
    /// The data directory, whose file system `/metrics` measures (FR-038); `None` for a node kept
    /// in memory.
    pub data_dir: Option<std::path::PathBuf>,
}

/// Writes the state of the machine (007 FR-038): the disk of `data_dir`, the process's resident
/// memory and how long the clients' commands took.
fn machine_metrics(out: &mut String, data_dir: Option<&std::path::Path>) {
    if let Some((used, total)) = data_dir.and_then(|d| teseladb_sys::host::fs_usage(d).ok()) {
        let rows = [
            (
                "teseladb_disk_used_bytes",
                "Bytes used on the file system of the data directory.",
                used,
            ),
            ("teseladb_disk_total_bytes", "Bytes of the file system of the data directory.", total),
        ];
        for (name, help, n) in rows {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {n}");
        }
    }
    if let Ok(m) = teseladb_sys::host::memory() {
        let name = "teseladb_process_resident_memory_bytes";
        let _ = writeln!(out, "# HELP {name} Resident bytes of the node's process.");
        let _ = writeln!(out, "# TYPE {name} gauge\n{name} {}", m.resident);
    }
    teseladb_wire::commands::durations::render(out);
}

impl NodeAdmin {
    /// `{current, available}`: the feature compatibility version the cluster runs and the latest every node that is not
    /// removed runs, the one `setFeatureCompatibilityVersion` can go to (007 FR-027).
    fn fcv(&self) -> Result<Document, AdminError> {
        let Some(CatalogAnswer::Settings(all)) =
            self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Settings, tx))
        else {
            return Err(AdminError::unavailable("Group 0 did not answer"));
        };
        let current =
            teseladb_wire::commands::fcv::version_of(&crate::backend::parameters_of(&all));
        let available = self
            .handle
            .routes
            .fcv_limit()
            .unwrap_or_else(|| teseladb_wire::commands::fcv::VERSIONS[0].to_owned());
        Ok(Document::new().with("current", current).with("available", available))
    }

    /// Sends `msg` to core `core` and waits for its answer, as long as [`CORE_WAIT`].
    pub fn ask<T>(&self, core: usize, msg: impl FnOnce(mpsc::Sender<T>) -> NodeMsg) -> Option<T> {
        let (tx, rx) = mpsc::channel();
        self.handle.senders.get(core)?.sender().send(CoreInput::App(msg(tx))).ok()?;
        rx.recv_timeout(CORE_WAIT).ok()
    }

    /// A sender to core 0 for this request (the port's threads share this backend).
    fn core0(&self) -> Result<CoreSender<NodeMsg>, AdminError> {
        let senders = self.handle.senders.first();
        senders
            .map(CoreSenders::sender)
            .ok_or_else(|| AdminError::unavailable("the node has no cores"))
    }

    pub(crate) fn catalog(&self) -> Result<Arc<Catalog>, AdminError> {
        match self.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Read, tx)) {
            Some(CatalogAnswer::Catalog(c)) => Ok(c),
            _ => Err(AdminError::unavailable("the catalog did not answer")),
        }
    }

    /// The identifier of collection `ns` (`db.c`).
    fn collection(&self, ns: &str) -> Result<teseladb_meta::catalog::CollectionEntry, AdminError> {
        let (db, coll) =
            ns.split_once('.').ok_or_else(|| AdminError::bad(format!("not a namespace: {ns}")))?;
        let name = Namespace { db: db.to_owned(), coll: coll.to_owned() };
        let catalog = self.catalog()?;
        catalog
            .get(&name)
            .cloned()
            .ok_or_else(|| AdminError::not_found(format!("no collection {ns}")))
    }

    /// This node's replica of range `g`, asked of the core that holds it.
    fn status(&self, g: GroupId) -> Option<crate::data::DataStatus> {
        let core = usize::from(self.handle.routes.entry(g)?.core);
        match self.ask(core, |tx| NodeMsg::Data(DataReq::Status(g), tx))? {
            Ok(DataReply::Status(s)) => Some(s),
            _ => None,
        }
    }

    /// Where a split by size would cut this node's replica of range `g`.
    fn cut(&self, g: GroupId) -> Option<Vec<u8>> {
        let core = usize::from(self.handle.routes.entry(g)?.core);
        match self.ask(core, |tx| NodeMsg::Data(DataReq::Cut(g), tx))? {
            Ok(DataReply::Cut(cut)) => cut,
            _ => None,
        }
    }

    pub(crate) fn group0(&self, cmd: Group0Command) -> Result<(), AdminError> {
        crate::node::group0_change(&self.core0()?, cmd).map_err(AdminError::unavailable)
    }

    /// Runs a command of the wire protocol on database `admin`, as a client of this node would.
    pub(crate) fn command(&self, cmd: &Document) -> Answer {
        let mut backend = ServerBackend::new(self.handle.clone(), self.handle.node.0);
        let shared: &Shared = &self.handle.sessions;
        let mut ctx = Ctx { backend: &mut backend, shared, db: "admin", cmd };
        match run_command(&mut ctx) {
            Some(Ok(d)) => Ok(d),
            Some(Err(e))
                if e.code.code == teseladb_wire::error::KEY_NOT_FOUND.code
                    || e.code.code == teseladb_wire::error::USER_NOT_FOUND.code =>
            {
                Err(AdminError::not_found(e.errmsg))
            }
            Some(Err(e)) => Err(AdminError::bad(e.errmsg)),
            None => Err(AdminError::bad("not a command of the node")),
        }
    }
}

/// `bytes` in hexadecimal.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn node_value(n: NodeId) -> Value {
    Value::Int64(i64::try_from(n.0).unwrap_or(i64::MAX))
}

/// The disk's metrics: its syncs, which bound what a confirmed write costs (H-008 of the native
/// tests against RefDb).
fn disk_metrics(out: &mut String, syncs: u64, flushes: u64, flush_micros: u64) {
    let name = "teseladb_disk_syncs_total";
    let _ = writeln!(out, "# HELP {name} Syncs of files and folders to the disk.");
    let _ = writeln!(out, "# TYPE {name} counter\n{name} {syncs}");
    let name = "teseladb_disk_device_flushes_total";
    let _ = writeln!(
        out,
        "# HELP {name} Flushes of a drive's cache, each shared by the syncs that waited for it."
    );
    let _ = writeln!(out, "# TYPE {name} counter\n{name} {flushes}");
    let name = "teseladb_disk_device_flush_micros_total";
    let _ = writeln!(
        out,
        "# HELP {name} Microseconds the flushes of the drives' caches took: with the flushes, their \
         mean, which grows with what the drive has waiting in its cache."
    );
    let _ = writeln!(out, "# TYPE {name} counter\n{name} {flush_micros}");
}

/// The reads the connections' threads served without the cores (005 FR-028), and those whose
/// range's view sent them back to its core (a write in flight, an intent, a view past its time).
fn outside_metrics(out: &mut String, (served, declined, unchanged): (u64, u64, u64)) {
    let name = "teseladb_reads_outside_total";
    let _ = writeln!(out, "# HELP {name} Reads served on a connection's thread without a core.");
    let _ = writeln!(out, "# TYPE {name} counter\n{name} {served}");
    let name = "teseladb_reads_outside_declined_total";
    let _ = writeln!(
        out,
        "# HELP {name} Reads a range's view sent back to its core: a write in flight on the range, \
         an intent or a view past its time."
    );
    let _ = writeln!(out, "# TYPE {name} counter\n{name} {declined}");
    let name = "teseladb_writes_unchanged_outside_total";
    let _ = writeln!(
        out,
        "# HELP {name} Writes that changed nothing answered on a connection's thread without a core."
    );
    let _ = writeln!(out, "# TYPE {name} counter\n{name} {unchanged}");
}

/// The transactions' metrics (008 FR-019).
fn txn_metrics(out: &mut String, t: &teseladb_wire::commands::status::TxnCounts) {
    let counters = [
        ("teseladb_txn_commits_total", "Transactions committed.", t.committed),
        ("teseladb_txn_pushes_total", "Transactions pushed by readers and writers.", t.pushes),
        (
            "teseladb_txn_uncertainty_restarts_total",
            "Reads that met a version in their uncertainty window.",
            t.uncertainty_restarts,
        ),
    ];
    for (name, help, n) in counters {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {n}");
    }
    let name = "teseladb_txn_aborts_total";
    let _ = writeln!(out, "# HELP {name} Transactions aborted, by the error that ended them.");
    let _ = writeln!(out, "# TYPE {name} counter");
    for (cause, n) in &t.abort_causes {
        let _ = writeln!(out, "{name}{{cause=\"{cause}\"}} {n}");
    }
    let name = "teseladb_txn_unresolved_intents";
    let _ = writeln!(out, "# HELP {name} Intents of finished transactions still to resolve.");
    let _ = writeln!(out, "# TYPE {name} gauge\n{name} {}", t.unresolved_intents);
}

/// The change streams' metrics (014 FR-018): cursors open, events returned and, by range, how far
/// behind the clock its resolved mark was at the last read of a stream.
fn stream_metrics(out: &mut String, (open, events): (u64, u64), lags: &[(u64, u64)]) {
    let name = "teseladb_changestream_cursors";
    let _ = writeln!(out, "# HELP {name} Change stream cursors open.");
    let _ = writeln!(out, "# TYPE {name} gauge\n{name} {open}");
    let name = "teseladb_changestream_events_total";
    let _ = writeln!(out, "# HELP {name} Events change stream cursors returned.");
    let _ = writeln!(out, "# TYPE {name} counter\n{name} {events}");
    let name = "teseladb_changestream_range_lag_ms";
    let _ = writeln!(out, "# HELP {name} How far behind the clock a range's resolved mark was.");
    let _ = writeln!(out, "# TYPE {name} gauge");
    for (g, lag) in lags {
        let _ = writeln!(out, "{name}{{range=\"{g}\"}} {lag}");
    }
}

/// The search metrics (011 FR-026): queries and their time, the indexer's lag behind the read's
/// mark, segments, segments rebuilt and their memory.
fn search_metrics(out: &mut String, m: &teseladb_wire::commands::search_ops::SearchMetrics) {
    let rows = [
        ("teseladb_search_queries_total", "Search queries answered.", "counter", m.queries),
        (
            "teseladb_search_query_micros_total",
            "Microseconds search queries took.",
            "counter",
            m.query_micros,
        ),
        (
            "teseladb_search_indexer_lag_ms",
            "How far behind the read's mark the last search index brought up to date was.",
            "gauge",
            m.indexer_lag_ms,
        ),
        ("teseladb_search_segments", "Segments of the search indexes served.", "gauge", m.segments),
        (
            "teseladb_search_merges_total",
            "Search segments rebuilt when their ranges split or merged.",
            "counter",
            m.merges,
        ),
        (
            "teseladb_search_segment_bytes",
            "Estimated bytes of the search segments.",
            "gauge",
            m.memory_bytes,
        ),
    ];
    for (name, help, kind, n) in rows {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {n}");
    }
}

impl AdminBackend for NodeAdmin {
    fn healthy(&self) -> bool {
        self.ask(0, NodeMsg::View).is_some_and(|v| v.ready())
    }

    fn metrics(&self) -> String {
        let mut out = String::new();
        let up = u8::from(self.healthy());
        let entries = self.handle.routes.entries();
        let _ = writeln!(out, "# HELP teseladb_up Whether the node serves (1) or not (0).");
        let _ = writeln!(out, "# TYPE teseladb_up gauge\nteseladb_up {up}");
        let _ = writeln!(out, "# HELP teseladb_ranges Ranges in the node's routing table.");
        let _ = writeln!(out, "# TYPE teseladb_ranges gauge\nteseladb_ranges {}", entries.len());
        let mine: Vec<(GroupId, crate::data::DataStatus)> = entries
            .iter()
            .filter_map(|e| Some((e.group, self.status(e.group).filter(|s| s.role.is_some())?)))
            .collect();
        let _ = writeln!(
            out,
            "# HELP teseladb_range_applied_index Highest entry this node's replica of the range applied."
        );
        let _ = writeln!(out, "# TYPE teseladb_range_applied_index gauge");
        for (g, s) in &mine {
            let _ =
                writeln!(out, "teseladb_range_applied_index{{range=\"{}\"}} {}", g.0, s.applied);
        }
        let _ = writeln!(out, "# HELP teseladb_range_leader Whether this node leads the range.");
        let _ = writeln!(out, "# TYPE teseladb_range_leader gauge");
        for (g, s) in &mine {
            let leads = u8::from(s.leader == self.handle.node);
            let _ = writeln!(out, "teseladb_range_leader{{range=\"{}\"}} {leads}", g.0);
        }
        let status = teseladb_wire::commands::txn_stats::status(&teseladb_wire::commands::lock(
            &self.handle.sessions,
        ));
        txn_metrics(&mut out, &status.txn);
        disk_metrics(
            &mut out,
            teseladb_io::counters::syncs(),
            teseladb_io::counters::device_flushes(),
            teseladb_io::counters::device_flush_micros(),
        );
        let outside = (
            crate::backend::served_outside(),
            crate::backend::declined_outside(),
            crate::backend::unchanged_outside(),
        );
        outside_metrics(&mut out, outside);
        let lags = crate::backend::range_lags(crate::clock::wall_ms());
        stream_metrics(&mut out, teseladb_wire::commands::change_stream::stream_metrics(), &lags);
        search_metrics(
            &mut out,
            &teseladb_wire::commands::search_ops::search_metrics(&self.handle.sessions),
        );
        if let Some(m) = &self.memory {
            m.metrics(&mut out);
        }
        crate::tenant_limits::metrics(&self.handle.routes, &mut out);
        machine_metrics(&mut out, self.data_dir.as_deref());
        if let Some(h) = &self.http_metrics {
            out.push_str(&h.lock().unwrap_or_else(std::sync::PoisonError::into_inner).render());
        }
        out
    }

    fn nodes(&self) -> Answer {
        let v = self
            .ask(0, NodeMsg::View)
            .ok_or_else(|| AdminError::unavailable("Group 0 did not answer"))?;
        let nodes: Vec<Value> = v
            .nodes
            .iter()
            .map(|n| {
                Value::Document(
                    Document::new()
                        .with("_id", node_value(*n))
                        .with("voter", v.voters.contains(n))
                        .with("learner", v.learners.contains(n))
                        .with("state", node_state(&v.node_states, *n)),
                )
            })
            .collect();
        Ok(Document::new()
            .with("nodes", Value::Array(nodes))
            .with("group0Leader", node_value(v.leader))
            .with("self", node_value(self.handle.node))
            .with("featureCompatibilityVersion", self.fcv()?))
    }

    fn ranges(&self, ns: Option<&str>) -> Answer {
        let interval = match ns {
            None => None,
            Some(ns) => {
                // `db.c` of the default tenant, the one the administration port works in.
                let c = self.collection(ns)?;
                let (t, id) = (c.tenant, c.id);
                let start = teseladb_keys::KeyBuilder::collection_start(t, id).as_bytes().to_vec();
                let end = teseladb_keys::KeyBuilder::collection_end(t, id).as_bytes().to_vec();
                Some((start, end))
            }
        };
        let ranges: Vec<Value> = self
            .handle
            .routes
            .entries()
            .into_iter()
            .filter(|e| interval.as_ref().is_none_or(|(s, en)| overlaps((&e.start, &e.end), s, en)))
            .map(|e| {
                let voters = e.voters.iter().map(|n| node_value(*n)).collect();
                Value::Document(
                    Document::new()
                        .with("_id", Value::Int64(i64::try_from(e.group.0).unwrap_or(i64::MAX)))
                        .with("start", hex(&e.start))
                        .with("end", hex(&e.end))
                        .with("core", i32::from(e.core))
                        .with("voters", Value::Array(voters))
                        .with("leaderHint", e.leader_hint.map_or(Value::Null, node_value)),
                )
            })
            .collect();
        Ok(Document::new().with("ranges", Value::Array(ranges)))
    }

    fn split(&self, range: GroupId, at: Option<Document>) -> Answer {
        let entry = self
            .handle
            .routes
            .entry(range)
            .ok_or_else(|| AdminError::not_found(format!("no range {}", range.0)))?;
        let key = match at {
            Some(at) => {
                let Some(Value::String(ns)) = at.get("ns") else {
                    return Err(AdminError::bad("--at needs {\"ns\": \"db.c\", \"_id\": …}"));
                };
                let coll = self.collection(ns)?;
                teseladb_wire::commands::sharding::split_point(&coll, &at)
                    .map_err(|e| AdminError::bad(e.errmsg))?
            }
            None => self.cut(range).ok_or_else(|| {
                AdminError::unavailable(
                    "this node has no data of the range to cut it by size: give --at, or ask a node with a replica",
                )
            })?,
        };
        if !strictly_inside(&key, &entry.start, &entry.end) {
            return Err(AdminError::bad(format!("the key is not inside range {}", range.0)));
        }
        self.group0(Group0Command::SplitRange { group: range, at: key.clone() })?;
        Ok(Document::new().with("ok", 1.0).with("at", hex(&key)))
    }

    fn merge(&self, range: GroupId) -> Answer {
        let entries = self.handle.routes.entries();
        let left = entries
            .iter()
            .find(|e| e.group == range)
            .ok_or_else(|| AdminError::not_found(format!("no range {}", range.0)))?;
        let right = entries
            .iter()
            .find(|e| e.start == left.end)
            .ok_or_else(|| AdminError::bad(format!("range {} has no right neighbour", range.0)))?;
        self.group0(Group0Command::MergeRanges { left: range, right: right.group })?;
        let right = Value::Int64(i64::try_from(right.group.0).unwrap_or(i64::MAX));
        Ok(Document::new().with("ok", 1.0).with("right", right))
    }

    fn relocate(&self, range: GroupId, core: u16) -> Answer {
        if self.handle.routes.entry(range).is_none() {
            return Err(AdminError::not_found(format!("no range {}", range.0)));
        }
        self.group0(Group0Command::RelocateRange { group: range, core })?;
        Ok(Document::new().with("ok", 1.0).with("core", Value::Int32(i32::from(core))))
    }

    fn tenants(&self) -> Answer {
        crate::admin_tenants::list(self)
    }

    fn create_tenant(&self, name: &str) -> Answer {
        crate::admin_tenants::create(self, name)
    }

    fn api_keys(&self, cmd: &Document) -> Answer {
        self.command(cmd)
    }

    fn drop_tenant(&self, name: &str) -> Answer {
        crate::admin_tenants::drop(self, name)
    }

    fn tenant_state(&self, name: &str, op: crate::admin_tenant_state::StateOp) -> Answer {
        crate::admin_tenant_state::apply(self, name, op)
    }

    fn drain(&self, node: NodeId, drain: bool) -> Answer {
        let cmd = match self.ask(0, |tx| NodeMsg::Drain(node, drain, tx)).flatten() {
            None => return Err(AdminError::unavailable("Group 0 did not answer")),
            Some(Err(e @ DrainError::UnknownNode(_))) => {
                return Err(AdminError::not_found(e.to_string()));
            }
            Some(Err(e)) => return Err(AdminError::bad(e.to_string())),
            Some(Ok(cmd)) => cmd,
        };
        self.group0(cmd)?;
        Ok(Document::new().with("ok", 1.0).with("node", node_value(node)))
    }

    fn parameter(&self, name: &str) -> Answer {
        self.command(&Document::new().with(GET_CLUSTER_PARAMETER, name))
    }

    fn set_parameter(&self, name: &str, value: Document) -> Answer {
        self.command(
            &Document::new().with(SET_CLUSTER_PARAMETER, Document::new().with(name, value)),
        )
    }

    fn create_token(&self, ttl: Duration) -> Answer {
        let e = self
            .enroller
            .as_ref()
            .ok_or_else(|| AdminError::bad("this node has no PKI (--insecure)"))?;
        let token =
            crate::node::enroll_token(e, &self.core0()?, ttl).map_err(AdminError::unavailable)?;
        Ok(Document::new().with("token", token))
    }

    fn rotate_certificates(&self) -> Answer {
        let c = self
            .certificates
            .as_ref()
            .ok_or_else(|| AdminError::bad("this node has no PKI (--insecure)"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        c.renew(now, &mut |b| teseladb_sys::fill_random(b)).map_err(AdminError::unavailable)?;
        Ok(Document::new().with("rotated", true))
    }

    fn backup(&self, op: crate::backup::Op) -> Answer {
        crate::backup::run_op(self, op)
    }

    fn privileges(&self, user: &UserName) -> Result<Vec<Privilege>, AdminError> {
        let mut backend = ServerBackend::new(self.handle.clone(), self.handle.node.0);
        let unavailable = |e: teseladb_wire::error::CommandError| AdminError::unavailable(e.errmsg);
        let roles = backend.user(user).map_err(unavailable)?.map(|u| u.roles).unwrap_or_default();
        Ok(Graph::new(&mut backend).inherit(&roles, None).map_err(unavailable)?.privileges)
    }
}

/// `teseladb admin topology set <mode> [--name <set>] [--primary <host>]` (018 FR-007): the
/// cluster parameter `topologyPresentation`, as `setClusterParameter` sets it.
fn topology_set(
    mode: &str,
    name: Option<String>,
    primary: Option<String>,
) -> Result<AdminAction, String> {
    use teseladb_wire::commands::presentation::{MODE, PRIMARY, SET_NAME, TOPOLOGY_PRESENTATION};
    if teseladb_topology::replset::PresentationMode::from_name(mode).is_none() {
        return Err(format!("not a presentation: {mode} (refrouter or replicaset)"));
    }
    let mut value = Document::new().with(MODE, mode);
    if let Some(n) = name {
        value.push(SET_NAME, n);
    }
    if let Some(p) = primary {
        value.push(PRIMARY, p);
    }
    let bytes = value.to_bytes().map_err(|e| e.to_string())?;
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).map_err(|e| e.to_string())?;
    Ok(AdminAction::ConfigSet {
        name: TOPOLOGY_PRESENTATION.to_owned(),
        value: teseladb_bson::extjson::to_relaxed(raw),
    })
}

/// The warning of a `topology set` that renames the replica set: the drivers configured with the
/// old name (`replicaSet=`) stop using the cluster. `before` is the stored parameter (alone or
/// under its name), `after` the requested value.
pub fn rename_warning(before: &Document, after: &Document) -> Option<String> {
    use teseladb_wire::commands::presentation::{MODE, SET_NAME, TOPOLOGY_PRESENTATION};
    let stored = match before.get(TOPOLOGY_PRESENTATION) {
        Some(Value::Document(d)) => d,
        _ => before,
    };
    let text = |d: &Document, f: &str| match d.get(f) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    if text(stored, MODE).as_deref() != Some("replicaset") {
        return None;
    }
    let old = text(stored, SET_NAME)?;
    let new = match (text(after, MODE).as_deref(), text(after, SET_NAME)) {
        (Some("replicaset"), Some(n)) if n != old => n,
        _ => return None,
    };
    Some(format!(
        "warning: the replica set {old} becomes {new}; drivers connected with replicaSet={old} stop using the cluster until their URI says replicaSet={new}"
    ))
}

/// What `teseladb admin` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminAction {
    /// `nodes`.
    Nodes,
    /// `metrics`: the node's metrics in Prometheus' text format (the battery scans them for
    /// secrets, 013 FR-023).
    Metrics,
    /// `ranges [--collection db.c]`.
    Ranges {
        /// Only the ranges of this collection.
        collection: Option<String>,
    },
    /// `range split <id> [--at <Extended JSON>]`.
    Split {
        /// Range.
        range: u64,
        /// `{"ns": "db.c", "_id": …}`.
        at: Option<String>,
    },
    /// `range merge <id>`.
    Merge {
        /// Range.
        range: u64,
    },
    /// `range relocate <id> --core <n>` (007 FR-011).
    Relocate {
        /// Range.
        range: u64,
        /// The core it moves to.
        core: u16,
    },
    /// `node drain <id>` (`drain`) or `node remove <id>` (007 FR-015).
    Node {
        /// Node.
        node: u64,
        /// Drain it; else remove it.
        drain: bool,
    },
    /// `config get <parameter>`.
    ConfigGet {
        /// Parameter.
        name: String,
    },
    /// `config set <parameter> <Extended JSON document>`.
    ConfigSet {
        /// Parameter.
        name: String,
        /// Its value.
        value: String,
    },
    /// `cert rotate`.
    CertRotate,
    /// `token create [--ttl 1h]`.
    TokenCreate {
        /// How long it lives.
        ttl: Duration,
    },
    /// `backup --at <mark|now> --out <dir>` (007 FR-021).
    Backup {
        /// The mark (`seconds.increment`), or `now`.
        at: Option<String>,
        /// Where the backup goes.
        out: String,
    },
    /// `restore --from <dir>` (007 FR-021).
    Restore {
        /// Where the backup is.
        from: String,
    },
    /// `tenant create|list|drop` (007 FR-025).
    Tenant(crate::admin_tenants::TenantAction),
    /// `apikey create|list|revoke` (019 FR-016).
    ApiKey(crate::admin_apikeys::ApiKeyAction),
}

/// Options of `teseladb admin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminOpts {
    /// The node's administration port (`--node`, by default `127.0.0.1:27200`).
    pub node: String,
    /// `--format json`: the answer as it comes.
    pub json: bool,
    /// What to do.
    pub action: AdminAction,
    /// The credentials it presents (013 FR-017).
    pub tls: AdminCredentials,
}

/// What `teseladb admin` presents to the port (013 FR-017): the certificate of
/// `--tlsCertificateKeyFile` trusting `--tlsCAFile`, or else the node's own certificate and
/// CA from the PKI of `--dbpath`; with neither, plain HTTP for a node without a PKI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminCredentials {
    /// `--dbpath`, `./teseladb-data` as in `teseladb start`.
    pub dbpath: std::path::PathBuf,
    /// `--tlsCertificateKeyFile`: the client certificate and its key, PEM.
    pub certificate_key_file: Option<std::path::PathBuf>,
    /// `--tlsCAFile`: the CA that signed the node's certificate, PEM.
    pub ca_file: Option<std::path::PathBuf>,
}

/// `--dbpath` by default.
pub const DEFAULT_DBPATH: &str = "./teseladb-data";

fn read_pem(path: &std::path::Path, option: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("--{option} {}: {e}", path.display()))
}

impl AdminCredentials {
    /// The client side of TLS; `None` for plain HTTP.
    pub fn client(&self) -> Result<Option<Arc<ClientConfig>>, String> {
        let (identity, anchors) = match &self.certificate_key_file {
            Some(path) => {
                let text = read_pem(path, TLS_CERTIFICATE_KEY_FILE)?;
                let bad =
                    |e: String| format!("--{TLS_CERTIFICATE_KEY_FILE} {}: {e}", path.display());
                let chain = Certificate::from_pem_all(&text).map_err(|e| bad(e.to_string()))?;
                let key = PrivateKey::from_pem(&text, None).map_err(|e| bad(e.to_string()))?;
                let ca = self
                    .ca_file
                    .as_ref()
                    .ok_or_else(|| format!("--{TLS_CERTIFICATE_KEY_FILE} needs --{TLS_CA_FILE}"))?;
                let anchors = Certificate::from_pem_all(&read_pem(ca, TLS_CA_FILE)?)
                    .map_err(|e| format!("--{TLS_CA_FILE} {}: {e}", ca.display()))?;
                (CertifiedKey { chain, key }, anchors)
            }
            None if crate::pki::absent(&self.dbpath) => return Ok(None),
            None => {
                let pki = crate::pki::load(&self.dbpath).map_err(|e| e.to_string())?;
                let identity = CertifiedKey { chain: pki.node_chain, key: pki.node_key };
                (identity, vec![pki.ca])
            }
        };
        let mut config = ClientConfig::new(Trust::anchors(anchors));
        config.identity = Some(identity);
        Ok(Some(Arc::new(config)))
    }
}

/// The administration port of a node started without `--admin-listen`.
pub const DEFAULT_NODE: &str = "127.0.0.1:27200";

/// Parses the arguments after `admin`.
pub fn parse(args: &[String]) -> Result<AdminOpts, String> {
    let mut node = DEFAULT_NODE.to_owned();
    let mut json = false;
    let mut tls = AdminCredentials {
        dbpath: DEFAULT_DBPATH.into(),
        certificate_key_file: None,
        ca_file: None,
    };
    let mut words = Vec::new();
    let mut opts: Vec<(String, String)> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let Some(name) = a.strip_prefix("--") else {
            words.push(a.as_str());
            continue;
        };
        // `--clear` of `tenant access` takes no value.
        if name == "clear" {
            opts.push((name.to_owned(), String::new()));
            continue;
        }
        let value = it.next().ok_or_else(|| format!("--{name} needs a value"))?.clone();
        match name {
            "node" => node = value,
            "dbpath" => tls.dbpath = value.into(),
            TLS_CERTIFICATE_KEY_FILE => tls.certificate_key_file = Some(value.into()),
            TLS_CA_FILE => tls.ca_file = Some(value.into()),
            "format" => {
                json = match value.as_str() {
                    "json" => true,
                    "text" => false,
                    other => return Err(format!("--format: text or json, not {other}")),
                }
            }
            "collection" | "at" | "ttl" | "core" | "out" | "from" | "user" | "db" | "role"
            | "expires-after-days" | "name" | "primary" | "allow" => {
                opts.push((name.to_owned(), value));
            }
            limit if crate::admin_tenants::LIMIT_OPTIONS.contains(&limit) => {
                opts.push((name.to_owned(), value));
            }
            other => return Err(format!("unknown option --{other}")),
        }
    }
    let opt = |n: &str| opts.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let allow = |allowed: &[&str]| match opts.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(format!("--{k} does not go with this subcommand")),
        None => Ok(()),
    };
    let range_id = |s: &str| s.parse::<u64>().map_err(|_| format!("not a range: {s}"));
    let action = match words.as_slice() {
        ["nodes"] => allow(&[]).map(|()| AdminAction::Nodes)?,
        ["metrics"] => allow(&[]).map(|()| AdminAction::Metrics)?,
        ["ranges"] => allow(&["collection"])
            .map(|()| AdminAction::Ranges { collection: opt("collection") })?,
        ["range", "split", id] => {
            allow(&["at"])?;
            AdminAction::Split { range: range_id(id)?, at: opt("at") }
        }
        ["range", "merge", id] => {
            allow(&[])?;
            AdminAction::Merge { range: range_id(id)? }
        }
        ["range", "relocate", id] => {
            allow(&["core"])?;
            let core = opt("core").ok_or("range relocate needs --core")?;
            let core = core.parse::<u16>().map_err(|_| format!("not a core: {core}"))?;
            AdminAction::Relocate { range: range_id(id)?, core }
        }
        ["node", op @ ("drain" | "remove"), id] => {
            allow(&[])?;
            let node = id.parse::<u64>().map_err(|_| format!("not a node: {id}"))?;
            AdminAction::Node { node, drain: *op == DRAIN }
        }
        ["config", "get", name] => {
            allow(&[]).map(|()| AdminAction::ConfigGet { name: (*name).to_owned() })?
        }
        ["config", "set", name, value] => allow(&[]).map(|()| AdminAction::ConfigSet {
            name: (*name).to_owned(),
            value: (*value).to_owned(),
        })?,
        ["topology", "set", mode] => {
            allow(&["name", "primary"])?;
            topology_set(mode, opt("name"), opt("primary"))?
        }
        ["cert", "rotate"] => allow(&[]).map(|()| AdminAction::CertRotate)?,
        ["backup"] => {
            allow(&["at", "out"])?;
            let out = opt("out").ok_or("backup needs --out <dir>")?;
            AdminAction::Backup { at: opt("at"), out }
        }
        ["restore"] => {
            allow(&["from"])?;
            AdminAction::Restore { from: opt("from").ok_or("restore needs --from <dir>")? }
        }
        ["token", "create"] => {
            allow(&["ttl"])?;
            let ttl = opt("ttl").map_or(Ok(DEFAULT_TTL), |t| duration(&t))?;
            AdminAction::TokenCreate { ttl }
        }
        words => {
            match (
                crate::admin_tenants::parse(words, &opts),
                crate::admin_apikeys::parse(words, &opts),
            ) {
                (Some(t), _) => AdminAction::Tenant(t?),
                (None, Some(k)) => AdminAction::ApiKey(k?),
                (None, None) => return Err(ADMIN_USAGE.to_owned()),
            }
        }
    };
    Ok(AdminOpts { node, json, action, tls })
}

/// `90`, `90s`, `30m`, `1h` or `2d`.
fn duration(text: &str) -> Result<Duration, String> {
    let (n, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None => (text, "s"),
    };
    let n: u64 = n.parse().map_err(|_| format!("not a duration: {text}"))?;
    let secs = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3600),
        "d" => n.saturating_mul(86_400),
        _ => return Err(format!("not a duration: {text}")),
    };
    Ok(Duration::from_secs(secs))
}

/// Percent-encodes a path segment or query value.
pub(crate) fn component(text: &str) -> String {
    text.bytes().fold(String::new(), |mut s, b| {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            s.push(char::from(b));
        } else {
            let _ = write!(s, "%{b:02X}");
        }
        s
    })
}

/// Usage of `teseladb admin`.
pub const ADMIN_USAGE: &str = "\
usage: teseladb admin [--node <ip:port>] [--format text|json] [--dbpath <dir>]
                      [--tlsCertificateKeyFile <pem> --tlsCAFile <pem>] <subcommand>
  nodes
  metrics
  ranges [--collection <db.c>]
  range split <range> [--at '{\"ns\": \"db.c\", \"_id\": <value>}']
  range merge <range>
  range relocate <range> --core <n>
  node drain <node>
  node remove <node>
  config get <parameter>
  config set <parameter> '<Extended JSON document>'
  topology set <refrouter|replicaset> [--name <set>] [--primary <host:port>]
  token create [--ttl <90s|30m|1h|2d>]
  cert rotate
  backup [--at <seconds.increment|now>] --out <dir>
  restore --from <dir>
  tenant create <name>
  tenant list
  tenant drop <name>
  tenant limits <name> [--storage-bytes <n>] [--max-connections <n>] [--ops-per-second <n>]
    [--max-api-keys <n>] [--max-time-ms <n>] [--max-concurrent-ops <n>]
    [--max-write-bytes-per-second <n>] [--max-databases <n>] [--max-collections <n>]
    [--max-search-indexes <n>] [--http on|off] [--mcp on|off] [--service-user <user>]
  tenant access <name> [--allow <cidr>]... [--clear]
  tenant suspend <name>
  tenant resume <name>
  tenant usage <name>
  apikey create --user <u> --db <db> [--role <role>@<db>]... [--expires-after-days <n>]
  apikey list [--user <u>]
  apikey revoke <keyId>";

/// Runs `teseladb admin` against the node's port; the exit code: 0 correct, 1 error.
pub fn run(o: &AdminOpts, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (method, target, body) = match &o.action {
        AdminAction::Nodes => (Method::Get, NODES.to_owned(), String::new()),
        AdminAction::Metrics => (Method::Get, METRICS.to_owned(), String::new()),
        AdminAction::Ranges { collection: None } => (Method::Get, RANGES.to_owned(), String::new()),
        AdminAction::Ranges { collection: Some(c) } => {
            (Method::Get, format!("{RANGES}?collection={}", component(c)), String::new())
        }
        AdminAction::Split { range, at } => {
            (Method::Post, format!("{RANGE}{range}/{SPLIT}"), at.clone().unwrap_or_default())
        }
        AdminAction::Merge { range } => {
            (Method::Post, format!("{RANGE}{range}/{MERGE}"), String::new())
        }
        AdminAction::Relocate { range, core } => {
            (Method::Post, format!("{RANGE}{range}/{RELOCATE}?core={core}"), String::new())
        }
        AdminAction::Node { node, drain } => {
            let op = if *drain { DRAIN } else { REMOVE };
            (Method::Post, format!("{NODE}{node}/{op}"), String::new())
        }
        AdminAction::ConfigGet { name } => {
            (Method::Get, format!("{CONFIG}{}", component(name)), String::new())
        }
        AdminAction::ConfigSet { name, value } => {
            (Method::Post, format!("{CONFIG}{}", component(name)), value.clone())
        }
        AdminAction::CertRotate => (Method::Post, CERT_ROTATE.to_owned(), String::new()),
        AdminAction::TokenCreate { ttl } => {
            (Method::Post, format!("{TOKENS}?ttl={}", ttl.as_secs()), String::new())
        }
        AdminAction::Backup { at, out: dir } => {
            let dir = std::path::Path::new(dir);
            return crate::backup::run(o, Ok(dir), at.as_deref(), out, err);
        }
        AdminAction::Restore { from } => {
            return crate::backup::run(o, Err(std::path::Path::new(from)), None, out, err);
        }
        AdminAction::Tenant(t) => match crate::admin_tenants::request(t) {
            Ok(r) => r,
            Err(e) => {
                let _ = writeln!(err, "teseladb admin: {e}");
                return 1;
            }
        },
        AdminAction::ApiKey(k) => match crate::admin_apikeys::request(k) {
            Ok(r) => r,
            Err(e) => {
                let _ = writeln!(err, "teseladb admin: {e}");
                return 1;
            }
        },
    };
    let tls = match o.tls.client() {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(err, "teseladb admin: {e}");
            return 1;
        }
    };
    if let AdminAction::ConfigSet { name, value } = &o.action
        && name == teseladb_wire::commands::presentation::TOPOLOGY_PRESENTATION
    {
        // A new name leaves the drivers configured with the old one out: say it before applying
        // it (018 spec, edge cases).
        let current = format!("{CONFIG}{}", component(name));
        let before = admin_http::request_with(&o.node, tls.clone(), Method::Get, &current, b"");
        let before = before.ok().and_then(|(_, a)| media::decode(&a, Format::Relaxed).ok());
        let after = media::decode(value.as_bytes(), Format::Relaxed).ok();
        if let (Some(b), Some(a)) = (before, after)
            && let Some(w) = rename_warning(&b, &a)
        {
            let _ = writeln!(err, "teseladb admin: {w}");
        }
    }
    let sent = admin_http::request_with(&o.node, tls, method, &target, body.as_bytes());
    let (status, answer) = match sent {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(err, "teseladb admin: cannot reach {}: {e}", o.node);
            return 1;
        }
    };
    let doc = media::decode(&answer, Format::Relaxed).ok();
    if status != 200 {
        let why = match doc.as_ref().and_then(|d| d.get("error")) {
            Some(Value::String(m)) => m.clone(),
            _ => format!("HTTP {status}"),
        };
        let _ = writeln!(err, "teseladb admin: {why}");
        return 1;
    }
    if o.json {
        let _ = out.write_all(&answer);
        let _ = writeln!(out);
        return 0;
    }
    let _ = out.write_all(text(&o.action, doc.as_ref(), &answer).as_bytes());
    0
}

/// The answer as text: a line per node or range, the token alone, the rest as it comes.
fn text(action: &AdminAction, doc: Option<&Document>, raw: &[u8]) -> String {
    let list = |field: &str| match doc.and_then(|d| d.get(field)) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| match v {
                Value::Document(d) => Some(d),
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let show = |v: Option<&Value>| v.map_or_else(String::new, teseladb_wire::commands::display);
    let mut out = String::new();
    match action {
        AdminAction::Nodes => {
            let leader = show(doc.and_then(|d| d.get("group0Leader")));
            for n in list("nodes") {
                let id = show(n.get("_id"));
                let yes = |k: &str| n.get(k) == Some(&Value::Boolean(true));
                let role = if yes("voter") {
                    "voter"
                } else if yes("learner") {
                    "learner"
                } else {
                    "-"
                };
                let lead = if id == leader { "\tGroup 0 leader" } else { "" };
                let state = match n.get("state") {
                    Some(Value::String(s)) if s != "active" => format!("\t{s}"),
                    _ => String::new(),
                };
                let _ = writeln!(out, "{id}\t{role}{lead}{state}");
            }
        }
        AdminAction::Ranges { .. } => {
            for r in list("ranges") {
                let _ = writeln!(
                    out,
                    "{}\t[{}, {})\tcore {}\tvoters {}",
                    show(r.get("_id")),
                    show(r.get("start")),
                    show(r.get("end")),
                    show(r.get("core")),
                    show(r.get("voters"))
                );
            }
        }
        AdminAction::CertRotate => {
            let _ = writeln!(out, "certificates rotated");
        }
        AdminAction::TokenCreate { .. } => {
            // The bare token, so that `TOKEN=$(teseladb admin token create)` takes it (013
            // quickstart, contract `pki-and-enrollment.md`).
            match doc.and_then(|d| d.get("token")) {
                Some(Value::String(t)) => {
                    let _ = writeln!(out, "{t}");
                }
                other => {
                    let _ = writeln!(out, "{}", show(other));
                }
            }
        }
        AdminAction::Node { node, drain: true } => {
            let _ = writeln!(out, "node {node} draining");
        }
        AdminAction::Node { node, drain: false } => {
            let _ = writeln!(out, "node {node} removed");
        }
        AdminAction::Tenant(t) => out.push_str(&crate::admin_tenants::text(t, doc)),
        AdminAction::Metrics => out.push_str(&String::from_utf8_lossy(raw)),
        AdminAction::ApiKey(k) => out.push_str(&crate::admin_apikeys::text(k, doc)),
        AdminAction::Split { .. }
        | AdminAction::Merge { .. }
        | AdminAction::Relocate { .. }
        | AdminAction::ConfigGet { .. }
        | AdminAction::ConfigSet { .. }
        | AdminAction::Backup { .. }
        | AdminAction::Restore { .. } => {
            let _ = writeln!(out, "{}", String::from_utf8_lossy(raw));
        }
    }
    out
}

/// Whether a range `[start, end)` (`end` empty: no end) holds some key of `[from, to)`.
fn overlaps((start, end): (&[u8], &[u8]), from: &[u8], to: &[u8]) -> bool {
    (end.is_empty() || end > from) && start < to
}

/// Whether `key` is inside `[start, end)` past its first key: a cut there leaves both sides
/// with keys.
fn strictly_inside(key: &[u8], start: &[u8], end: &[u8]) -> bool {
    key > start && (end.is_empty() || key < end)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #[test]
    fn bounds_without_an_end_reach_every_key_past_the_start() {
        assert!(super::overlaps((b"m", b""), b"a", b"z"), "no end");
        assert!(super::overlaps((b"a", b"m"), b"b", b"c"));
        assert!(!super::overlaps((b"a", b"m"), b"m", b"z"), "ends where the interval starts");
        assert!(!super::overlaps((b"m", b""), b"a", b"m"), "starts where the interval ends");
        assert!(super::strictly_inside(b"q", b"m", b""), "no end");
        assert!(!super::strictly_inside(b"m", b"m", b""), "its first key");
        assert!(!super::strictly_inside(b"q", b"a", b"q"), "its end");
    }

    use super::*;

    #[test]
    fn the_change_stream_metrics_give_cursors_events_and_the_lag_of_each_range() {
        let mut out = String::new();
        stream_metrics(&mut out, (2, 7), &[(4, 180), (9, 230)]);
        for line in [
            "teseladb_changestream_cursors 2",
            "teseladb_changestream_events_total 7",
            "teseladb_changestream_range_lag_ms{range=\"4\"} 180",
            "teseladb_changestream_range_lag_ms{range=\"9\"} 230",
        ] {
            assert!(out.lines().any(|l| l == line), "{line} in {out}");
        }
    }

    #[test]
    fn the_search_metrics_name_each_counter() {
        let mut out = String::new();
        let m = teseladb_wire::commands::search_ops::SearchMetrics {
            queries: 5,
            query_micros: 1200,
            indexer_lag_ms: 3,
            segments: 2,
            merges: 1,
            memory_bytes: 4096,
        };
        search_metrics(&mut out, &m);
        for line in [
            "teseladb_search_queries_total 5",
            "teseladb_search_query_micros_total 1200",
            "teseladb_search_indexer_lag_ms 3",
            "teseladb_search_segments 2",
            "teseladb_search_merges_total 1",
            "teseladb_search_segment_bytes 4096",
            "# TYPE teseladb_search_segments gauge",
            "# TYPE teseladb_search_merges_total counter",
        ] {
            assert!(out.lines().any(|l| l == line), "{line} in {out}");
        }
    }

    #[test]
    fn the_read_metrics_give_the_reads_served_outside_the_cores_and_those_sent_back() {
        let mut out = String::new();
        outside_metrics(&mut out, (40, 2, 5));
        assert!(out.lines().any(|l| l == "teseladb_writes_unchanged_outside_total 5"), "{out}");
        assert!(out.lines().any(|l| l == "teseladb_reads_outside_total 40"), "{out}");
        assert!(out.lines().any(|l| l == "teseladb_reads_outside_declined_total 2"), "{out}");
        assert!(out.lines().any(|l| l == "# TYPE teseladb_reads_outside_total counter"), "{out}");
    }

    #[test]
    fn the_disk_metrics_give_the_syncs() {
        let mut out = String::new();
        disk_metrics(&mut out, 7, 3, 9_000);
        assert!(out.lines().any(|l| l == "teseladb_disk_syncs_total 7"), "{out}");
        assert!(out.lines().any(|l| l == "teseladb_disk_device_flush_micros_total 9000"), "{out}");
        assert!(out.lines().any(|l| l == "teseladb_disk_device_flushes_total 3"), "{out}");
        assert!(out.lines().any(|l| l == "# TYPE teseladb_disk_syncs_total counter"), "{out}");
    }

    #[test]
    fn the_transactions_metrics_name_each_counter_and_each_cause() {
        let mut t = teseladb_wire::commands::status::TxnCounts {
            committed: 3,
            pushes: 4,
            uncertainty_restarts: 5,
            unresolved_intents: 6,
            ..Default::default()
        };
        t.abort_causes.insert(teseladb_wire::error::WRITE_CONFLICT.name.to_owned(), 2);
        let mut out = String::new();
        txn_metrics(&mut out, &t);
        let conflicts = format!(
            "teseladb_txn_aborts_total{{cause=\"{}\"}} 2",
            teseladb_wire::error::WRITE_CONFLICT.name
        );
        for line in [
            "teseladb_txn_commits_total 3",
            "teseladb_txn_pushes_total 4",
            "teseladb_txn_uncertainty_restarts_total 5",
            &conflicts,
            "teseladb_txn_unresolved_intents 6",
        ] {
            assert!(out.lines().any(|l| l == line), "{line} in {out}");
        }
    }
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        down: bool,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn called(&self, what: String) -> Answer {
            self.calls.lock().unwrap().push(what.clone());
            Ok(Document::new().with("did", what))
        }
    }

    impl AdminBackend for Fake {
        fn healthy(&self) -> bool {
            !self.down
        }
        fn metrics(&self) -> String {
            "teseladb_up 1\n".into()
        }
        fn nodes(&self) -> Answer {
            self.called("nodes".into())
        }
        fn ranges(&self, ns: Option<&str>) -> Answer {
            self.called(format!("ranges {ns:?}"))
        }
        fn split(&self, range: GroupId, at: Option<Document>) -> Answer {
            if range == GroupId(99) {
                return Err(AdminError::not_found("no range 99"));
            }
            self.called(format!("split {} {}", range.0, at.is_some()))
        }
        fn merge(&self, range: GroupId) -> Answer {
            self.called(format!("merge {}", range.0))
        }
        fn relocate(&self, range: GroupId, core: u16) -> Answer {
            self.called(format!("relocate {} {core}", range.0))
        }
        fn drain(&self, node: NodeId, drain: bool) -> Answer {
            if node == NodeId(99) {
                return Err(AdminError::not_found("node 99 is not in the cluster"));
            }
            self.called(format!("{} {}", if drain { DRAIN } else { REMOVE }, node.0))
        }
        fn parameter(&self, name: &str) -> Answer {
            self.called(format!("get {name}"))
        }
        fn set_parameter(&self, name: &str, value: Document) -> Answer {
            self.called(format!("set {name} {}", value.len()))
        }
        fn create_token(&self, ttl: Duration) -> Answer {
            self.called(format!("token {}", ttl.as_secs()))
        }
        fn rotate_certificates(&self) -> Answer {
            self.called("rotate".into())
        }
        fn backup(&self, op: crate::backup::Op) -> Answer {
            self.called(format!("backup {op:?}"))
        }
        fn api_keys(&self, cmd: &Document) -> Answer {
            self.called(format!("apikeys {}", cmd.iter().next().map_or("", |(k, _)| k)))
        }
        fn tenants(&self) -> Answer {
            self.called("tenants".into())
        }
        fn create_tenant(&self, name: &str) -> Answer {
            self.called(format!("create tenant {name}"))
        }
        fn drop_tenant(&self, name: &str) -> Answer {
            self.called(format!("drop tenant {name}"))
        }
        fn tenant_state(&self, name: &str, op: crate::admin_tenant_state::StateOp) -> Answer {
            self.called(format!("tenant {name} {op:?}"))
        }

        fn privileges(&self, user: &UserName) -> Result<Vec<Privilege>, AdminError> {
            match user.user.as_str() {
                "CN=broken" => Err(AdminError::unavailable("Group 0 did not answer")),
                "CN=monitor" => {
                    Ok(privileges::builtin(&RoleName::new(METRICS_ROLE, "admin")).unwrap())
                }
                "CN=admin" => Ok(privileges::builtin(&RoleName::new("root", "admin")).unwrap()),
                _ => Ok(Vec::new()),
            }
        }
    }

    fn req(method: Method, target: &str, body: &str) -> HttpRequest {
        let text = format!(
            "{} {target} HTTP/1.1\r\nHost: node\r\nContent-Length: {}\r\n\r\n{body}",
            if method == Method::Get { "GET" } else { "POST" },
            body.len()
        );
        admin_http::parse_request(text.as_bytes()).unwrap().unwrap()
    }

    fn did(r: &HttpResponse) -> String {
        assert_eq!(
            (r.status, r.content_type),
            (200, JSON),
            "{:?}",
            String::from_utf8_lossy(&r.body)
        );
        let d = media::decode(&r.body, Format::Relaxed).unwrap();
        let Some(Value::String(s)) = d.get("did") else { panic!("{d:?}") };
        s.clone()
    }

    #[test]
    fn each_route_reaches_its_operation() {
        let b = Fake::default();
        assert_eq!(did(&route(&req(Method::Get, NODES, ""), &b)), "nodes");
        assert_eq!(did(&route(&req(Method::Get, RANGES, ""), &b)), "ranges None");
        let r = route(&req(Method::Get, "/admin/ranges?collection=db.c", ""), &b);
        assert_eq!(did(&r), "ranges Some(\"db.c\")");
        let r = route(&req(Method::Post, "/admin/ranges/7/split", ""), &b);
        assert_eq!(did(&r), "split 7 false");
        let at = r#"{"ns": "db.c", "_id": 5}"#;
        assert_eq!(
            did(&route(&req(Method::Post, "/admin/ranges/7/split", at), &b)),
            "split 7 true"
        );
        assert_eq!(did(&route(&req(Method::Post, "/admin/ranges/7/merge", ""), &b)), "merge 7");
        let relocate = req(Method::Post, "/admin/ranges/7/relocate?core=3", "");
        assert_eq!(did(&route(&relocate, &b)), "relocate 7 3");
        let no_core = route(&req(Method::Post, "/admin/ranges/7/relocate", ""), &b);
        assert_eq!(no_core.status, 400);
        assert_eq!(did(&route(&req(Method::Get, "/admin/config/x", ""), &b)), "get x");
        let r = route(&req(Method::Post, "/admin/config/x", r#"{"a": 1}"#), &b);
        assert_eq!(did(&r), "set x 1");
        assert_eq!(did(&route(&req(Method::Post, TOKENS, ""), &b)), "token 3600");
        assert_eq!(did(&route(&req(Method::Post, "/admin/tokens?ttl=60", ""), &b)), "token 60");
        assert_eq!(did(&route(&req(Method::Post, CERT_ROTATE, ""), &b)), "rotate");
        assert_eq!(did(&route(&req(Method::Post, "/admin/nodes/3/drain", ""), &b)), "drain 3");
        assert_eq!(did(&route(&req(Method::Post, "/admin/nodes/3/remove", ""), &b)), "remove 3");
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let drain = parse(&words(&["node", "drain", "3"])).map(|o| o.action);
        assert_eq!(drain, Ok(AdminAction::Node { node: 3, drain: true }));
        let remove = parse(&words(&["node", "remove", "3"])).map(|o| o.action);
        assert_eq!(remove, Ok(AdminAction::Node { node: 3, drain: false }));
        assert_eq!(parse(&words(&["node", "drain", "x"])).unwrap_err(), "not a node: x");
        assert!(parse(&words(&["node", "stop", "3"])).is_err());
        assert_eq!(route(&req(Method::Get, CERT_ROTATE, ""), &b).status, 405);
        // A tenant is created or dropped with a POST only.
        assert_eq!(did(&route(&req(Method::Get, "/admin/tenants", ""), &b)), "tenants");
        assert_eq!(route(&req(Method::Post, "/admin/tenants", ""), &b).status, 405);
        assert_eq!(route(&req(Method::Get, "/admin/tenants/t1", ""), &b).status, 405);
        assert_eq!(
            did(&route(&req(Method::Post, "/admin/tenants/t1", ""), &b)),
            "create tenant t1"
        );
        let r = route(&req(Method::Post, "/admin/tenants/t1/drop", ""), &b);
        assert_eq!(did(&r), "drop tenant t1");
        assert_eq!(route(&req(Method::Post, "/admin/tenants/t1/x", ""), &b).status, 404);
        let args: Vec<String> = ["cert", "rotate"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse(&args).map(|o| o.action), Ok(AdminAction::CertRotate));
    }

    #[test]
    fn health_and_metrics_have_their_formats() {
        let up = Fake::default();
        let r = route(&req(Method::Get, HEALTHZ, ""), &up);
        assert_eq!((r.status, r.body.len()), (200, 0));
        let down = Fake { down: true, ..Fake::default() };
        assert_eq!(route(&req(Method::Get, HEALTHZ, ""), &down).status, 503);
        let r = route(&req(Method::Get, METRICS, ""), &up);
        assert_eq!(
            (r.status, r.content_type, r.body.as_slice()),
            (200, PROMETHEUS, &b"teseladb_up 1\n"[..])
        );
    }

    #[test]
    fn unreadable_certificate_files_name_their_option() {
        let dir = std::env::temp_dir().join(format!("teseladb-admin-pem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.pem");
        std::fs::write(&bad, "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
            .unwrap();
        let creds = |key: &std::path::Path, ca: &std::path::Path| AdminCredentials {
            dbpath: dir.clone(),
            certificate_key_file: Some(key.to_path_buf()),
            ca_file: Some(ca.to_path_buf()),
        };
        let e = creds(&bad, &bad).client().unwrap_err();
        assert!(e.starts_with(&format!("--{TLS_CERTIFICATE_KEY_FILE} ")), "{e}");
        // A good certificate and key, with a CA file that does not parse.
        let key = teseladb_x509::PrivateKey::ed25519(&[4; 32]);
        let name = teseladb_x509::Name::build(&[(teseladb_x509::oid::COMMON_NAME, "ops")]);
        let params = teseladb_x509::generate::CertificateParams::node([6; 16], name, 0, 1 << 40);
        let cert = teseladb_x509::generate::self_signed(&params, &key).unwrap();
        let good = dir.join("good.pem");
        let pem = teseladb_x509::pem::encode("PRIVATE KEY", &key.to_pkcs8());
        std::fs::write(&good, format!("{}{pem}", cert.to_pem())).unwrap();
        let e = creds(&good, &bad).client().unwrap_err();
        assert!(e.starts_with(&format!("--{TLS_CA_FILE} ")), "{e}");
        assert!(creds(&good, &good).client().unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_api_key_and_tenant_routes_reach_their_commands() {
        let b = Fake::default();
        let keys = crate::admin_apikeys::API_KEYS;
        assert_eq!(did(&route(&req(Method::Get, keys, ""), &b)), "apikeys listApiKeys");
        let mine = format!("{keys}?user=ann");
        assert_eq!(did(&route(&req(Method::Get, &mine, ""), &b)), "apikeys listApiKeys");
        let body = r#"{"user": "ann", "db": "admin", "expiresAfterDays": 3}"#;
        assert_eq!(did(&route(&req(Method::Post, keys, body), &b)), "apikeys createApiKey");
        let revoke = format!("{keys}/k1/revoke");
        assert_eq!(did(&route(&req(Method::Post, &revoke, ""), &b)), "apikeys revokeApiKey");
        let status = |m, t: &str, body: &str| route(&req(m, t, body), &b).status;
        assert_eq!(status(Method::Post, keys, ""), 400);
        assert_eq!(status(Method::Post, keys, r#"{"user": 1}"#), 400);
        assert_eq!(status(Method::Get, &revoke, ""), 405);
        assert_eq!(status(Method::Post, &format!("{keys}/k1"), ""), 404);
        assert_eq!(status(Method::Post, &format!("{keys}/k1/rotate"), ""), 404);
        assert_eq!(status(Method::Get, "/admin/tenants/acme", ""), 405);
        assert_eq!(status(Method::Post, "/admin/tenants", ""), 405);
        let calls = b.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 4, "{calls:?}");
    }

    #[test]
    fn wrong_requests_get_their_status() {
        let b = Fake::default();
        let status = |m, t: &str, body: &str| route(&req(m, t, body), &b).status;
        assert_eq!(status(Method::Get, "/nothing", ""), 404);
        assert_eq!(status(Method::Post, NODES, ""), 405);
        assert_eq!(status(Method::Post, HEALTHZ, ""), 405);
        assert_eq!(status(Method::Post, METRICS, ""), 405);
        assert_eq!(status(Method::Post, RANGES, ""), 405);
        assert_eq!(status(Method::Get, TOKENS, ""), 405);
        assert_eq!(status(Method::Get, CERT_ROTATE, ""), 405);
        assert_eq!(status(Method::Post, "/admin/ranges/7", ""), 404);
        assert_eq!(status(Method::Get, "/admin/ranges/7/split", ""), 405);
        assert_eq!(status(Method::Post, "/admin/ranges/x/split", ""), 400);
        assert_eq!(status(Method::Post, "/admin/ranges/7/cut", ""), 404);
        assert_eq!(status(Method::Post, "/admin/ranges/7/split", "{"), 400);
        assert_eq!(status(Method::Get, "/admin/nodes/3/drain", ""), 405);
        assert_eq!(status(Method::Post, "/admin/nodes/3/stop", ""), 404);
        assert_eq!(status(Method::Post, "/admin/nodes/3", ""), 404);
        assert_eq!(status(Method::Post, "/admin/nodes/x/drain", ""), 400);
        assert_eq!(status(Method::Post, "/admin/nodes/99/drain", ""), 404);
        assert_eq!(status(Method::Post, "/admin/config/x", "[1]"), 400);
        assert_eq!(status(Method::Post, "/admin/tokens?ttl=x", ""), 400);
        let r = route(&req(Method::Post, "/admin/ranges/99/split", ""), &b);
        assert_eq!(r.status, 404);
        let d = media::decode(&r.body, Format::Relaxed).unwrap();
        assert_eq!(d.get("error"), Some(&Value::String("no range 99".into())));
        assert!(b.calls.lock().unwrap().is_empty(), "no operation ran");
    }

    #[test]
    fn each_caller_reaches_what_its_privileges_allow() {
        let b = Fake::default();
        let status = |c: &Caller, t: &str| answer(&req(Method::Get, t, ""), c, &b).status;
        let user = |n: &str| Caller::User(n.to_owned());
        for c in [Caller::Open, Caller::Member, user("CN=admin")] {
            assert_eq!((status(&c, METRICS), status(&c, NODES)), (200, 200), "{c:?}");
        }
        assert_eq!(
            (status(&user("CN=monitor"), METRICS), status(&user("CN=monitor"), NODES)),
            (200, 403)
        );
        assert_eq!(
            (status(&user("CN=nobody"), METRICS), status(&user("CN=nobody"), NODES)),
            (403, 403)
        );
        assert_eq!(
            (status(&Caller::Anonymous, METRICS), status(&Caller::Anonymous, NODES)),
            (401, 401)
        );
        assert_eq!(status(&user("CN=broken"), NODES), 503);
        // `/healthz` needs nothing.
        assert_eq!(status(&Caller::Anonymous, HEALTHZ), 200);
        let r = answer(&req(Method::Get, NODES, ""), &user("CN=monitor"), &b);
        let d = media::decode(&r.body, Format::Relaxed).unwrap();
        assert_eq!(
            d.get("error"),
            Some(&Value::String(
                "$external.CN=monitor is not authorized: /admin/nodes needs the privileges of clusterAdmin".into()
            ))
        );
        assert_eq!(b.calls.lock().unwrap().len(), 3, "only the allowed requests ran");
    }

    #[test]
    fn the_text_form_has_a_line_per_node_or_range_and_the_bare_token() {
        let node = |id: i64, voter: bool, learner: bool, state: &str| {
            Value::Document(
                Document::new()
                    .with("_id", id)
                    .with("voter", voter)
                    .with("learner", learner)
                    .with("state", state),
            )
        };
        let nodes = Document::new().with("group0Leader", 2i64).with(
            "nodes",
            Value::Array(vec![
                node(1, true, false, "active"),
                node(2, true, false, "active"),
                node(3, false, true, "active"),
                node(4, false, false, "draining"),
                node(5, false, false, "removed"),
                7i64.into(),
            ]),
        );
        assert_eq!(
            text(&AdminAction::Nodes, Some(&nodes), b""),
            "1\tvoter\n2\tvoter\tGroup 0 leader\n3\tlearner\n4\t-\tdraining\n5\t-\tremoved\n"
        );
        let range = Document::new()
            .with("_id", 4i64)
            .with("start", "a")
            .with("end", "b")
            .with("core", 1i64)
            .with("voters", Value::Array(vec![1i64.into()]));
        let ranges = Document::new().with("ranges", Value::Array(vec![Value::Document(range)]));
        let line = text(&AdminAction::Ranges { collection: None }, Some(&ranges), b"");
        assert!(line.starts_with("4\t[") && line.contains("core 1"), "{line}");
        assert_eq!(text(&AdminAction::Ranges { collection: None }, None, b""), "");
        let ttl = Duration::from_secs(60);
        let token = Document::new().with("token", "abc");
        assert_eq!(text(&AdminAction::TokenCreate { ttl }, Some(&token), b""), "abc\n");
        let odd = Document::new().with("token", 5i64);
        assert_eq!(text(&AdminAction::TokenCreate { ttl }, Some(&odd), b""), "5\n");
        assert_eq!(text(&AdminAction::CertRotate, None, b""), "certificates rotated\n");
        // Renaming the replica set warns before it is applied (018, edge cases).
        let rs = |name: &str| Document::new().with("mode", "replicaset").with("setName", name);
        let stored = Document::new()
            .with("_id", "topologyPresentation")
            .with("topologyPresentation", rs("rs0"));
        let w = rename_warning(&stored, &rs("rs1")).expect("a warning");
        assert!(w.contains("replicaSet=rs0") && w.contains("replicaSet=rs1"), "{w}");
        assert_eq!(rename_warning(&rs("rs0"), &rs("rs0")), None, "the same name");
        assert_eq!(rename_warning(&rs("rs0"), &Document::new().with("mode", "refrouter")), None);
        let refrouter = Document::new().with("mode", "refrouter");
        assert_eq!(rename_warning(&refrouter, &rs("rs1")), None, "no replica set yet");
        assert_eq!(rename_warning(&Document::new().with("mode", "replicaset"), &rs("a")), None);
        // `topology set` is `setClusterParameter` of `topologyPresentation` (018 FR-007).
        let words = |w: &[&str]| parse(&w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        let set = words(&["topology", "set", "replicaset", "--name", "rs0", "--primary", "h:1"]);
        assert_eq!(
            set.map(|o| o.action),
            Ok(AdminAction::ConfigSet {
                name: "topologyPresentation".into(),
                value: r#"{"mode":"replicaset","setName":"rs0","primary":"h:1"}"#.into(),
            })
        );
        assert!(words(&["topology", "set", "other"]).is_err());
        // `metrics` prints Prometheus' text as the port gives it.
        let metrics = parse(&["metrics".to_owned()]).map(|o| o.action);
        assert_eq!(metrics, Ok(AdminAction::Metrics));
        assert_eq!(text(&AdminAction::Metrics, None, b"a 1\nb 2\n"), "a 1\nb 2\n");
        let drain = AdminAction::Node { node: 3, drain: true };
        assert_eq!(text(&drain, None, b""), "node 3 draining\n");
        let remove = AdminAction::Node { node: 3, drain: false };
        assert_eq!(text(&remove, None, b""), "node 3 removed\n");
        assert_eq!(text(&AdminAction::Merge { range: 3 }, None, b"{\"ok\": 1}"), "{\"ok\": 1}\n");
        let relocate = AdminAction::Relocate { range: 3, core: 1 };
        assert_eq!(text(&relocate, None, b"{\"ok\": 1}"), "{\"ok\": 1}\n");
    }

    #[test]
    fn run_reports_what_keeps_it_from_the_node() {
        let dir = std::env::temp_dir().join(format!("teseladb-admin-run-{}", std::process::id()));
        let tls = AdminCredentials {
            dbpath: dir.join("none"),
            certificate_key_file: None,
            ca_file: None,
        };
        // A port nobody listens on: a listener bound and dropped leaves it free.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let mut o =
            AdminOpts { node: free.to_string(), json: false, action: AdminAction::Nodes, tls };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&o, &mut out, &mut err), 1);
        let e = String::from_utf8(err).unwrap();
        assert!(e.starts_with(&format!("teseladb admin: cannot reach {free}")), "{e}");
        o.tls.certificate_key_file = Some(dir.join("missing.pem"));
        let mut err = Vec::new();
        assert_eq!(run(&o, &mut out, &mut err), 1);
        let e = String::from_utf8(err).unwrap();
        assert!(e.contains("--tlsCertificateKeyFile") && e.contains("missing.pem"), "{e}");
        assert!(out.is_empty());
    }
}
