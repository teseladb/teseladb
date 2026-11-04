//! Routing of a request to its command (FR-003, FR-021): the checks every `OP_MSG` command
//! passes (`$db`, a known name), the `OP_QUERY` handshake, and the commands a node answers by
//! itself. Every reply carries `$clusterTime` and `operationTime`, as a `refrouter`'s does.

use teseladb_bson::{Document, Value};

use crate::cluster_time;
use crate::commands::diagnostics::WHATS_MY_URI;
use crate::commands::query::FIND;
use teseladb_auth::users::advertised_mechanisms;

use crate::commands::{
    Backend, Ctx, Shared, audit, auth, authorize, is_data_command, lock, profile, run, txn,
    txn_concern,
};
use crate::concerns::canonical;
use crate::connection::{Connection, Kind, Request};
use crate::error::INTERRUPTED;
use crate::error::{
    COMMAND_NOT_FOUND, CommandError, INVALID_NAMESPACE, MAX_TIME_MS_EXPIRED, OP_QUERY_REMOVED,
    UNSUPPORTED_OP_QUERY_COMMAND,
};
use crate::failpoint::{CONFIGURE_FAIL_POINT, Fault};
pub use crate::hello::{ClusterTime, NodeInfo, Topology};
use crate::hello::{HelloRequest, Next, is_hello, restamp_cluster_time, with_cluster_time};
use crate::ops::{
    DEFAULT_MAX_TIME_COMMANDS, Guarded, INTERRUPTED_MESSAGE, Op, default_read_ms, effective_limit,
    op_ns, op_type,
};
use crate::types::wrong_type;
use teseladb_auth::audit::AuditType;
use teseladb_auth::audit_param;
use teseladb_auth::privileges::Privilege;

/// `ping`.
pub const PING: &str = "ping";
/// Link RefDb's messages about the removed opcodes give.
const LEGACY_OPCODE_DOC: &str = "https://dochub.refdb.org/core/legacy-opcode-removal";

/// What the server does with a request.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Write this reply (unless the request was `moreToCome`).
    Reply(Document),
    /// An awaitable `hello`: answer with [`hello_after_wait`] when the topology changes or after
    /// `max_await_ms`; with `exhaust`, keep answering that way with `moreToCome` set.
    Await {
        /// Longest wait.
        max_await_ms: u64,
        /// The client allowed a stream of replies.
        exhaust: bool,
        /// The request, already validated.
        hello: HelloRequest,
    },
    /// Close the connection without a reply (`failCommand` with `closeConnection`).
    Close,
    /// Hold the connection for `ms`, then dispatch the same request again (`failCommand` with
    /// `blockConnection`); the connection keeps what the failpoint does next.
    Delay {
        /// Milliseconds.
        ms: u64,
    },
}

fn error(e: &CommandError, node: &NodeInfo) -> Action {
    Action::Reply(with_cluster_time(e.to_document(), node))
}

/// The node's data commands for [`dispatch_with`]: its backend and its cursors and sessions.
pub type Commands<'a, B> = Option<(&'a mut B, &'a Shared)>;

/// Runs `req` on connection `conn`, with the handshake and `ping` only.
pub fn dispatch(conn: &mut Connection, req: &Request, node: &NodeInfo) -> Action {
    dispatch_with::<dyn Backend>(conn, req, node, None)
}

/// Runs `req` on connection `conn`; the commands of [`crate::commands`] run on `cmds`.
pub fn dispatch_with<B: Backend + ?Sized>(
    conn: &mut Connection,
    req: &Request,
    node: &NodeInfo,
    mut cmds: Commands<'_, B>,
) -> Action {
    match req.kind {
        Kind::LegacyQuery => {
            let msg = format!(
                "OP_QUERY is no longer supported. The client driver may require an upgrade. For more details see {LEGACY_OPCODE_DOC}"
            );
            let d = Document::new()
                .with("$err", msg)
                .with("code", OP_QUERY_REMOVED.code)
                .with("ok", 0.0);
            Action::Reply(d)
        }
        Kind::Query if !is_hello(req.name()) => {
            let msg = format!(
                "Unsupported OP_QUERY command: {}. The client driver may require an upgrade. For more details see {LEGACY_OPCODE_DOC}",
                req.name()
            );
            error(&CommandError::new(UNSUPPORTED_OP_QUERY_COMMAND, msg), node)
        }
        Kind::Query => match cluster_time::check(&req.command, req.name(), node) {
            Ok(()) => {
                if let Some((_, shared)) = &cmds {
                    conn.auth.members_by_x509 = members_by_x509(shared);
                }
                hello(conn, req, node, cmds.map(|(b, _)| b))
            }
            Err(e) => error(&e, node),
        },
        Kind::Msg => {
            let started = cmds.as_ref().map(|(b, _)| b.now_micros());
            // The tenant it runs in, and the command without the `$tenant` that named it, which
            // no command knows (007 FR-025).
            let own = conn.auth.user.as_ref().map_or(0, |u| u.tenant);
            let resolved = match cmds.as_mut() {
                Some((b, _)) => crate::commands::tenants::of_request(own, &req.command, &mut **b),
                None => Ok((own, None)),
            };
            let stripped;
            let (req, resolved) = match resolved {
                Ok((tenant, cmd)) => {
                    conn.tenant = tenant;
                    stripped = cmd.map(|command| Request { command, ..req.clone() });
                    (stripped.as_ref().unwrap_or(req), Ok(()))
                }
                Err(e) => (req, Err(e)),
            };
            let committed_before = cmds.as_ref().and_then(|(b, _)| b.last_commit());
            let inner = cmds.as_mut().map(|(b, s)| (&mut **b, *s));
            let action = match resolved.and_then(|()| command(conn, req, node, inner)) {
                Ok(a) => a,
                Err(e) => error(&e, node),
            };
            // The cluster time after the command: a write's is at or above its own mark (008
            // FR-013).
            let action = match (action, cmds.as_ref()) {
                (Action::Reply(d), Some((b, _))) => {
                    let d = restamp(d, node, b.cluster_mark());
                    // A command that committed writes answers the last one's mark as its
                    // `operationTime`, the optime of a write (measured on a `refrouter` 9.0.4: a
                    // change stream that starts at it includes the write).
                    let wrote = b.last_commit().filter(|c| Some(*c) > committed_before);
                    Action::Reply(match wrote {
                        Some(c) => with_operation_time(d, c),
                        None => d,
                    })
                }
                (a, _) => a,
            };
            // A member's write replies carry its election and the write's optime (018).
            let action = match (action, &node.replica_set) {
                (Action::Reply(d), Some(view)) => {
                    Action::Reply(crate::commands::replset::member_write_reply(view, req.name(), d))
                }
                (Action::Reply(d), None) => {
                    Action::Reply(crate::commands::txn::router_reply(req.name(), d))
                }
                (a, _) => a,
            };
            if let (Some((b, s)), Some(t0)) = (cmds, started) {
                record(conn, req, &action, s, b.now_micros().saturating_sub(t0));
            }
            action
        }
    }
}

/// `reply` with `operationTime` at `mark`, where it has one.
fn with_operation_time(mut reply: Document, mark: teseladb_hlc::Hlc) -> Document {
    if let Some(v) = reply.get_mut("operationTime") {
        let (time, increment) = teseladb_hlc::to_timestamp(mark);
        *v = Value::Timestamp { time, increment };
    }
    reply
}

/// `reply` with the cluster time `node` had before its command, or the node's `mark` after it
/// when that is later.
fn restamp(mut reply: Document, node: &NodeInfo, mark: teseladb_hlc::Hlc) -> Document {
    let (time, increment) = teseladb_hlc::to_timestamp(mark);
    let had = node.cluster_time;
    if (time, increment) <= (had.time, had.increment) || reply.get("operationTime").is_none() {
        return reply;
    }
    let later = ClusterTime { time, increment };
    if restamp_cluster_time(&mut reply, later, node.keys.as_deref()) {
        return reply;
    }
    reply.remove("$clusterTime");
    reply.remove("operationTime");
    with_cluster_time(reply, &NodeInfo { cluster_time: later, ..node.clone() })
}

/// The mechanism a `saslStart`, `authenticate` or `speculativeAuthenticate` document names.
fn mechanism_of(d: &Document) -> Option<&str> {
    match d.get("mechanism") {
        Some(Value::String(m)) => Some(m.as_str()),
        _ => None,
    }
}

/// The user `after` if the command authenticated it (it was not the connection's user before).
fn newly<'a>(
    before: Option<&auth::Authenticated>,
    after: Option<&'a auth::Authenticated>,
) -> Option<&'a auth::Authenticated> {
    after.filter(|a| before != Some(*a))
}

/// Counts an authentication command for `security.authentication` (006 T046): a `saslStart` or
/// `authenticate` is an attempt with its mechanism; a command that leaves a new user on the
/// connection is a success of that user's mechanism.
fn count_authentication(
    shared: &Shared,
    req: &Request,
    before: Option<&auth::Authenticated>,
    after: Option<&auth::Authenticated>,
    micros: u64,
) {
    let mut g = lock(shared);
    let name = req.name();
    if (name == auth::SASL_START || name == auth::AUTHENTICATE)
        && let Some(m) = mechanism_of(&req.command)
    {
        g.status.authentication_attempt(m, false);
    }
    g.status.authentication_time(micros);
    if let Some(user) = newly(before, after) {
        g.status.authentication_succeeded(user.mechanism.name(), false);
    }
}

/// Counts what a `hello` asked of authentication: `saslSupportedMechs` and a
/// `speculativeAuthenticate` attempt with its outcome.
fn count_hello_auth(
    shared: &Shared,
    req: &Request,
    before: Option<&auth::Authenticated>,
    after: Option<&auth::Authenticated>,
) {
    let mut g = lock(shared);
    if req.command.get("saslSupportedMechs").is_some() {
        g.status.sasl_supported_mechs += 1;
    }
    if let Some(Value::Document(spec)) = req.command.get(auth::SPECULATIVE_AUTHENTICATE)
        && let Some(m) = mechanism_of(spec)
    {
        g.status.authentication_attempt(m, true);
    }
    if let Some(user) = newly(before, after) {
        g.status.authentication_succeeded(user.mechanism.name(), true);
    }
}

/// Counts a command that ran for `serverStatus` (006 T046): its latency, its totals and the API
/// version its application used. A request the failpoint holds counts when it runs again.
fn record(conn: &Connection, req: &Request, action: &Action, shared: &Shared, micros: u64) {
    let (failed, known) = match action {
        Action::Reply(d) => {
            let ok = d.get("ok") == Some(&Value::Double(1.0));
            let unknown = d.get("code") == Some(&Value::Int32(COMMAND_NOT_FOUND.code));
            (!ok, !unknown)
        }
        Action::Await { .. } => (false, true),
        Action::Delay { .. } => return,
        Action::Close => (true, true),
    };
    let version = match req.command.get("apiVersion") {
        Some(Value::String(v)) => Some(v.as_str()),
        _ => None,
    };
    let mut g = lock(shared);
    g.status.command(canonical(req.name()), &req.command, micros, failed, known);
    if let Some((_, Value::String(coll))) = req.command.iter().next()
        && known
    {
        g.status.collection_op(&format!("{}.{coll}", req.db), canonical(req.name()), micros);
    }
    g.status.api_version(conn.hello.app_name.as_deref(), version);
    let Action::Reply(d) = action else { return };
    if d.get("code") == Some(&Value::Int32(MAX_TIME_MS_EXPIRED.code)) {
        // Its own `maxTimeMS`, or the cluster's `defaultMaxTimeMS`.
        let own = req.command.get("maxTimeMS").is_some();
        let k = &mut g.status.killed_max_time;
        if own { k.0 += 1 } else { k.1 += 1 }
    }
    if req.name() == crate::commands::crud::INSERT
        && let Some(Value::Array(errors)) = d.get("writeErrors")
    {
        let duplicate = Value::Int32(crate::error::DUPLICATE_KEY.code);
        let n = errors
            .iter()
            .filter(|e| matches!(e, Value::Document(e) if e.get("code") == Some(&duplicate)))
            .count();
        g.status.duplicate_inserts += i64::try_from(n).unwrap_or(i64::MAX);
    }
}

/// The commands a node answers, each by the name `listCommands` gives it (without the aliases
/// `ismaster` and `findandmodify`); `configureFailPoint` only with `enableTestCommands` (`tests`).
pub fn command_names(tests: bool) -> Vec<&'static str> {
    let mut names = vec![crate::hello::HELLO, crate::hello::IS_MASTER, PING, WHATS_MY_URI];
    names.extend(auth::NAMES);
    names.extend(crate::commands::data_command_names().map(canonical));
    if tests {
        names.push(CONFIGURE_FAIL_POINT);
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Whether a node takes command `name`: one that runs commands (`node`), with
/// `enableTestCommands` or not (`tests`). A connection without a node answers only the handshake,
/// `ping` and `whatsmyuri`.
pub fn accepts(name: &str, node: bool, tests: bool) -> bool {
    is_hello(name)
        || name == PING
        || name == WHATS_MY_URI
        || (node && (is_data_command(name) || auth::is_auth_command(name)))
        || (tests && name == CONFIGURE_FAIL_POINT)
}

/// Checks of every `OP_MSG` command, then the command.
fn command<B: Backend + ?Sized>(
    conn: &mut Connection,
    req: &Request,
    node: &NodeInfo,
    mut cmds: Commands<'_, B>,
) -> Result<Action, CommandError> {
    if let Some(p) = &req.problem {
        return Err(p.clone());
    }
    let name = req.name();
    let not_found = || not_found_in(node, name);
    let shared = cmds.as_ref().map(|(_, s)| *s);
    let tests = shared.is_some_and(|s| lock(s).failpoint.is_some());
    if !accepts(name, shared.is_some(), tests) {
        return Err(not_found());
    }
    match req.command.get("$db") {
        None => {
            let msg = format!("BSON field '{name}.$db' is missing but a required field");
            return Err(CommandError::new(crate::error::IDL_FAILED_TO_PARSE, msg));
        }
        Some(Value::String(db)) if db.is_empty() => {
            return Err(CommandError::new(INVALID_NAMESPACE, "Invalid database name: ''"));
        }
        Some(Value::String(_)) => {}
        Some(other) => return Err(wrong_type(&format!("{name}.$db"), other, "string")),
    }
    if !conn.auth.may_run(name) {
        if let Some((backend, shared)) = cmds.as_mut() {
            // Without arguments and with the database as namespace (measured).
            let p = audit_param::auth_check(name, &req.db, None);
            let code = crate::error::UNAUTHORIZED.code;
            audit::record(shared, conn, &mut **backend, vec![(AuditType::AuthCheck, p)], code);
        }
        return Err(auth::requires_authentication(name));
    }
    let generic = crate::generic::parse(&req.command, name)?;
    let own_limit = generic.max_time_ms;
    if let Some((backend, _)) = cmds.as_mut() {
        backend.set_read_preference(generic.read_pref);
    }
    // A `refrouter` reads and checks `$clusterTime` after the command's arguments and before it
    // authorizes (measured).
    cluster_time::check(&req.command, name, node)?;
    if let Some((backend, _)) = cmds.as_mut() {
        cluster_time::take(&req.command, name, &mut **backend)?;
    }
    let target = match req.command.iter().next() {
        Some((_, teseladb_bson::Value::String(c))) => Some(c.as_str()),
        _ => None,
    };
    crate::commands::local_db::check_reachable(&req.db, canonical(name), target)?;
    if let Some(view) = &node.replica_set {
        let tv = Document::new()
            .with("processId", Value::ObjectId(node.topology.process_id))
            .with("counter", node.topology.counter);
        crate::commands::replset::gate(&view.set, view.me, canonical(name), tv)?;
    }
    let privileges = match (cmds.as_mut(), auth::allowed_unauthenticated(name)) {
        (Some((backend, shared)), false) => {
            let r = authorize::authorize(conn, req, canonical(name), &mut **backend, shared);
            audit_auth_check(conn, req, &mut **backend, shared, &r);
            r?
        }
        _ => None,
    };
    // A command a transaction cannot run, after its arguments and its authorization (008 T019,
    // measured on a `refrouter` 9.0.4).
    if let (Some(_), Some(e)) = (cmds.as_ref(), txn::refused(name, &req.command)) {
        let as_op = !(is_hello(name)
            || name == PING
            || name == WHATS_MY_URI
            || name == CONFIGURE_FAIL_POINT
            || auth::is_auth_command(name));
        // The others read their arguments first, in `run_op`.
        if !as_op || txn::refused_at_once(name) {
            return Err(e);
        }
    }
    // The times of the read concern against the cluster time, and the mark of the reads (008
    // FR-012, FR-013).
    let in_txn = matches!(txn::fields(name, &req.command), Ok(Some(_)));
    let mark = match cmds.as_mut() {
        Some((backend, _)) => {
            let rc = generic.read.as_ref();
            let at = (name, req.db.as_str(), &req.command);
            let mark = txn_concern::causal_mark(&mut **backend, at, rc, in_txn)?;
            backend.set_read_mark(mark);
            mark
        }
        None => None,
    };
    let fault = match conn.fault.take() {
        Some(f) => Some(f),
        None => shared.and_then(|s| {
            let app = conn.hello.app_name.clone();
            lock(s).failpoint.as_mut().and_then(|fp| fp.check(name, app.as_deref()))
        }),
    };
    let mut write_concern_error = None;
    if let Some(f) = fault {
        if f.block_ms > 0 && !f.blocked {
            let ms = f.block_ms;
            conn.fault = Some(Fault { blocked: true, ..f });
            return Ok(Action::Delay { ms });
        }
        if f.close {
            return Ok(Action::Close);
        }
        if let Some(e) = f.error {
            return Ok(Action::Reply(with_cluster_time(e, node)));
        }
        write_concern_error = f.write_concern_error;
    }
    if let (Some(s), true) = (shared, is_hello(name) || name == PING || name == WHATS_MY_URI) {
        // The commands that answer without the node count here; the others in `run_op`.
        lock(s).counters.count(name, &req.command, false);
    }
    let action = run_command(conn, req, node, cmds, (own_limit, privileges), mark)?;
    Ok(match (action, write_concern_error) {
        (Action::Reply(mut d), Some(wce)) if d.get("ok") == Some(&Value::Double(1.0)) => {
            d.insert("writeConcernError", wce);
            Action::Reply(d)
        }
        (a, _) => a,
    })
}

/// Fields of `hello` that read the users of Group 0 (013).
const HELLO_AUTH_FIELDS: [&str; 2] = ["saslSupportedMechs", "speculativeAuthenticate"];

/// Whether `req` is answered without waiting for the node's cores: `ping`, `whatsmyuri` and a
/// `hello` that does not ask for authentication. A client worker runs these itself; the others
/// go to its executors, since they may wait.
pub fn answers_without_waiting(req: &Request) -> bool {
    let name = req.name();
    name == PING
        || name == WHATS_MY_URI
        || (is_hello(name) && HELLO_AUTH_FIELDS.iter().all(|f| req.command.get(f).is_none()))
}

/// The command of a request that passed the checks.
fn run_command<B: Backend + ?Sized>(
    conn: &mut Connection,
    req: &Request,
    node: &NodeInfo,
    mut cmds: Commands<'_, B>,
    (own_limit, privileges): (Option<u64>, Option<Vec<Privilege>>),
    mark: Option<teseladb_txn::causal::ReadMark>,
) -> Result<Action, CommandError> {
    let name = req.name();
    let not_found = || not_found_in(node, name);
    if is_hello(name) {
        let before = conn.auth.user.clone();
        let greeted = conn.hello.metadata.is_some();
        let pending_attempt = conn.auth.attempt.clone();
        let Some((backend, shared)) = cmds else {
            return Ok(hello::<B>(conn, req, node, None));
        };
        conn.auth.members_by_x509 = members_by_x509(shared);
        let action = hello(conn, req, node, Some(&mut *backend));
        count_hello_auth(shared, req, before.as_ref(), conn.auth.user.as_ref());
        if let (false, Some(metadata)) = (greeted, conn.hello.metadata.clone()) {
            let local = endpoint_doc(conn.auth.server_addr.as_deref());
            let p = audit_param::client_metadata(&local, &metadata);
            audit::record(shared, conn, &mut *backend, vec![(AuditType::ClientMetadata, p)], 0);
        }
        if pending_attempt != conn.auth.attempt {
            // `speculativeAuthenticate` ran: a failure leaves the field out of the reply.
            let answered = matches!(&action, Action::Reply(d) if d.get(auth::SPECULATIVE_AUTHENTICATE).is_some());
            let code = (!answered).then_some(crate::error::AUTHENTICATION_FAILED.code);
            audit_authentication(conn, &mut *backend, shared, before.as_ref(), code);
        }
        return Ok(action);
    }
    if let (Some((backend, shared)), true) = (cmds.as_mut(), auth::is_auth_command(name)) {
        let (before, started) = (conn.auth.user.clone(), backend.now_micros());
        conn.auth.members_by_x509 = members_by_x509(shared);
        let r = auth::run(&mut conn.auth, &req.db, &req.command, &mut **backend);
        let micros = backend.now_micros().saturating_sub(started);
        count_authentication(shared, req, before.as_ref(), conn.auth.user.as_ref(), micros);
        let code = r.as_ref().err().map(|e| e.code.code);
        audit_authentication(conn, &mut **backend, shared, before.as_ref(), code);
        return Ok(Action::Reply(with_cluster_time(r?, node)));
    }
    if let (Some((backend, shared)), false) = (cmds, name == PING || name == WHATS_MY_URI) {
        if name == CONFIGURE_FAIL_POINT {
            let mut g = lock(shared);
            let fp = g.failpoint.as_mut().ok_or_else(not_found)?;
            let doc = fp.configure(&req.db, &req.command)?;
            return Ok(Action::Reply(with_cluster_time(doc, node)));
        }
        let doc =
            run_op(conn, req, backend, shared, own_limit, privileges).ok_or_else(not_found)?;
        // A `getMore` reads at its cursor's mark, which it set.
        let at = if name == crate::commands::query::GET_MORE { backend.read_mark() } else { mark };
        backend.set_read_mark(None);
        let doc = txn_concern::stamp(doc, at)?;
        return Ok(Action::Reply(with_cluster_time(doc, node)));
    }
    match name {
        PING => Ok(Action::Reply(with_cluster_time(Document::new().with("ok", 1.0), node))),
        _ => {
            // `whatsmyuri`: the client's address as the server saw it.
            let you = conn.peer.clone().unwrap_or_default();
            let doc = Document::new().with("you", you).with("ok", 1.0);
            Ok(Action::Reply(with_cluster_time(doc, node)))
        }
    }
}

/// Runs a command of [`crate::commands`] as an operation of the node's table: with its time
/// limit, visible to `currentOp` and stopped by `killOp`.
fn run_op<B: Backend + ?Sized>(
    conn: &mut Connection,
    req: &Request,
    backend: &mut B,
    shared: &Shared,
    own_limit: Option<u64>,
    privileges: Option<Vec<Privilege>>,
) -> Option<Result<Document, CommandError>> {
    let name = req.name();
    let default_ms = match (own_limit, conn.bypass_default_max_time) {
        (None, false) if DEFAULT_MAX_TIME_COMMANDS.contains(&name) => {
            match backend.cluster_parameters() {
                Ok(p) => default_read_ms(&p),
                Err(e) => return Some(Err(e)),
            }
        }
        _ => 0,
    };
    let limit = effective_limit(name, own_limit, default_ms, conn.bypass_default_max_time);
    let now = backend.now_ms();
    // A command a transaction cannot run reads its arguments with every request to the node
    // refused: an argument error is its answer, anything else the refusal.
    let refusal = txn::refused(name, &req.command);
    let op = Op {
        conn: conn.id(),
        client: conn.peer.clone(),
        metadata: conn.hello.metadata.clone(),
        app_name: conn.hello.app_name.clone(),
        op: op_type(name),
        ns: op_ns(&req.db, &req.command),
        command: req.command.clone(),
        started_ms: now,
        // A length of time: on the monotonic clock, which the cluster time is not (it jumps
        // ahead with a mark from a node ahead, and stops while this one is out of margin).
        deadline_ms: limit.map(|ms| (backend.now_micros() / 1000).saturating_add(ms)),
        killed: refusal.is_some(),
        interrupt: None,
        user: conn.auth.user.as_ref().map(|u| u.name.clone()),
        tenant: conn.tenant,
        privileges,
        roles: conn.auth.user.as_ref().and_then(|u| u.roles.clone()),
        desc: None,
        progress: None,
        metrics: None,
    };
    let id = lock(shared).ops.begin(op);
    let started_micros = backend.now_micros();
    let started_cpu = backend.thread_cpu_nanos();
    let (result, mut events) = {
        let mut guarded = Guarded::new(backend, shared, id);
        let (db, cmd) = (req.db.as_str(), &req.command);
        // A statement of a client transaction runs inside it (008 FR-009).
        let result = match (refusal, txn::fields(name, cmd)) {
            // The commit and the abort are no command of the module: nothing to read first.
            (Some(refused), _) if txn::ends(name) => Some(Err(refused)),
            (Some(refused), _) => match run(&mut Ctx { backend: &mut guarded, shared, db, cmd }) {
                Some(Err(e)) if e.code != INTERRUPTED => Some(Err(e)),
                Some(_) | None => Some(Err(refused)),
            },
            (None, Err(e)) => Some(Err(e)),
            (None, Ok(None)) => {
                // A write on encrypted fields runs in a transaction of the node (017 FR-003).
                let internal = crate::commands::fle::in_internal_txn(
                    &mut guarded,
                    shared,
                    (name, db, cmd),
                    |backend, cmd| run(&mut Ctx { backend, shared, db, cmd }),
                );
                match internal {
                    Some(r) => Some(r),
                    None => run(&mut Ctx { backend: &mut guarded, shared, db, cmd }),
                }
            }
            (None, Ok(Some(f))) => {
                txn::run_in_txn(&mut guarded, shared, (name, db, cmd), f, |backend| {
                    run(&mut Ctx { backend, shared, db, cmd })
                })
            }
        };
        (result, guarded.take_audit())
    };
    if let Some(Ok(reply)) = &result
        && reply.get("ok") == Some(&Value::Double(1.0))
        && let Some(e) = audit::user_role_event(&req.db, &req.command)
    {
        events.push(e);
    }
    let report = {
        let mut g = lock(shared);
        let report = g.ops.take_report(id);
        g.ops.end(id);
        g.counters.count(name, &req.command, matches!(result, Some(Err(_))));
        report
    };
    audit::record(shared, conn, backend, events, 0);
    let ns = op_ns(&req.db, &req.command);
    if !report.used.is_empty() {
        let now = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
        let mut g = lock(shared);
        for name in &report.used {
            g.index_usage.used(&ns, name, now);
        }
    }
    let ended = profile::Ended {
        db: &req.db,
        command: &req.command,
        op: op_type(name),
        ns: &ns,
        reply: result.as_ref(),
        micros: backend.now_micros().saturating_sub(started_micros),
        cpu_nanos: started_cpu
            .zip(backend.thread_cpu_nanos())
            .map(|(start, end)| end.saturating_sub(start)),
        report,
        client: conn.peer.as_deref(),
        app_name: conn.hello.app_name.as_deref(),
        user: conn.auth.user.as_ref().map(|u| format!("{}@{}", u.name.user, u.name.db)),
        opid: id,
    };
    profile::after_command(backend, shared, &ended);
    result.map(|r| r.map_err(|e| interrupted_find(e, name, &req.db, &req.command)))
}

/// A `find` that `killOp` interrupted says so as RefDb's executor does.
fn interrupted_find(e: CommandError, name: &str, db: &str, cmd: &Document) -> CommandError {
    if name != FIND || e.code != INTERRUPTED {
        return e;
    }
    let msg = format!(
        "Executor error during find command: {} :: caused by :: {INTERRUPTED_MESSAGE}",
        op_ns(db, cmd)
    );
    CommandError::new(INTERRUPTED, msg)
}

/// The handshake; with the node's `backend`, the mechanisms of the user `saslSupportedMechs`
/// names and the first step of `speculativeAuthenticate` (013).
/// Whether the node's `clusterAuthMode` lets a member's certificate log in as `__system`
/// (013 FR-034).
fn members_by_x509(shared: &Shared) -> bool {
    crate::commands::lock(shared).params.cluster_auth_mode.accepts_member_certificates()
}

fn hello<B: Backend + ?Sized>(
    conn: &mut Connection,
    req: &Request,
    node: &NodeInfo,
    backend: Option<&mut B>,
) -> Action {
    let parsed = HelloRequest::parse(req.name(), &req.command, &conn.hello)
        .and_then(|h| h.accept(&mut conn.hello, &node.topology).map(|next| (h, next)));
    let (mut h, next) = match parsed {
        Err(e) => return error(&e, node),
        Ok(p) => p,
    };
    let mut speculative = None;
    if let Some(b) = backend {
        if let Some(name) = h.sasl_user().cloned() {
            match auth::acquire(b, &name) {
                Ok(user) => h.set_mechanisms(advertised_mechanisms(
                    &name,
                    user.as_ref(),
                    &conn.auth.mechanisms,
                )),
                Err(e) => return error(&e, node),
            }
        }
        if let Some(spec) = h.speculative() {
            speculative = auth::speculative(&mut conn.auth, spec, b);
        }
    }
    match next {
        Next::Wait(ms) => {
            Action::Await { max_await_ms: ms, exhaust: req.exhaust_allowed(), hello: h }
        }
        Next::Now => {
            let mut d = h.response(&conn.hello, conn.id(), node);
            if let Some(spec) = speculative {
                // Before `ok`, as RefDb places it.
                d.remove("ok");
                d.push(auth::SPECULATIVE_AUTHENTICATE, spec);
                d.push("ok", 1.0);
            }
            Action::Reply(with_cluster_time(d, node))
        }
    }
}

/// `CommandNotFound` of `name`: as a `refrouter` names it, or as a `refnode` in the replica set
/// presentation (measured on 9.0.4).
fn not_found_in(node: &NodeInfo, name: &str) -> CommandError {
    match &node.replica_set {
        Some(view) if name.starts_with(crate::commands::replset::CONFIG_SERVER_PREFIX) => {
            crate::commands::replset::config_server_refusal(view, name, node)
        }
        Some(_) => CommandError::new(COMMAND_NOT_FOUND, format!("no such command: '{name}'")),
        None => CommandError::new(COMMAND_NOT_FOUND, format!("no such cmd: {name}")),
    }
}

/// The reply to an awaitable `hello` once its wait ends (a topology change or its deadline).
pub fn hello_after_wait(conn: &Connection, hello: &HelloRequest, node: &NodeInfo) -> Document {
    with_cluster_time(hello.response(&conn.hello, conn.id(), node), node)
}

/// The audit event of an authorization check: a refusal always, a success with
/// `auditAuthorizationSuccess` (013 FR-016).
fn audit_auth_check<B: Backend + ?Sized, T>(
    conn: &mut Connection,
    req: &Request,
    backend: &mut B,
    shared: &Shared,
    r: &Result<T, CommandError>,
) {
    let code = match r {
        Ok(_) => 0,
        Err(e) if e.code.code == crate::error::UNAUTHORIZED.code => e.code.code,
        Err(_) => return,
    };
    if !conn.auth.required || !audit::records_auth_check(shared, code) {
        return;
    }
    let name = req.name();
    let ns = audit_param::auth_check_ns(&req.db, &req.command);
    let p = audit_param::auth_check(name, &ns, Some(&req.command));
    audit::record(shared, conn, backend, vec![(AuditType::AuthCheck, p)], code);
}

/// The audit event of an authentication step: a login, or the failure (`code`) of the attempt in
/// progress; nothing while a conversation goes on.
fn audit_authentication<B: Backend + ?Sized>(
    conn: &mut Connection,
    backend: &mut B,
    shared: &Shared,
    before: Option<&auth::Authenticated>,
    code: Option<i32>,
) {
    let after = conn.auth.user.as_ref().map(|u| (u.name.clone(), u.mechanism.name()));
    let logged_in = after.as_ref().is_some_and(|(n, _)| before.is_none_or(|b| &b.name != n));
    let (event, result) = match (logged_in, after, code, conn.auth.attempt.take()) {
        (true, Some((name, mech)), _, _) => (audit_param::authenticate(&name, mech), 0),
        (_, _, Some(c), Some((name, mech))) => (audit_param::authenticate(&name, &mech), c),
        (_, _, _, attempt) => {
            conn.auth.attempt = attempt;
            return;
        }
    };
    audit::record(shared, conn, backend, vec![(AuditType::Authenticate, event)], result);
}

/// Logs `conn` in with `login` outside a command (the credentials of the HTTP API, 019 FR-015),
/// with the `authenticate` event of the audit log that an authentication command writes.
pub fn login<B: Backend + ?Sized, T>(
    conn: &mut Connection,
    backend: &mut B,
    shared: &Shared,
    login: impl FnOnce(&mut auth::AuthState, &mut B) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    let before = conn.auth.user.clone();
    let r = login(&mut conn.auth, backend);
    let code = r.as_ref().err().map(|e| e.code.code);
    audit_authentication(conn, backend, shared, before.as_ref(), code);
    r
}

/// `{ip, port}` of an address the connection knows, for `clientMetadata`.
fn endpoint_doc(addr: Option<&str>) -> Document {
    addr.and_then(teseladb_auth::audit::Endpoint::parse)
        .unwrap_or(teseladb_auth::audit::Endpoint::SystemUser)
        .to_document()
}
