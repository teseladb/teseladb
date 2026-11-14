//! Diagnostic commands with RefDb's shapes and TeselaDB's data (FR-023): `buildInfo`,
//! `hostInfo`, `serverStatus`, `connectionStatus`, `listCommands`, `getCmdLineOpts`, `getLog`,
//! `dbStats` and `collStats` (`whatsmyuri` answers in the dispatch, which knows the connection).
//! Shapes, types and messages measured against a `refrouter` 9.0.4 on 30-10-2026; the sections
//! that describe parts TeselaDB does not have (RefEngine, anotherproduct, shards) are left out, as
//! the divergences of 006 record.

use teseladb_auth::privileges::Privilege;
use teseladb_auth::users::RoleName;
use teseladb_keys::KeyBuilder;
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::indexes::IndexState;
use teseladb_range::read::ReadConcern;
use teseladb_bson::{Document, Value};

use super::admin::admin_only;
use super::args::uuid_value;
use super::catalog::collection_size;
use super::coll_stats_shards as shards;
use super::command_list::COMMANDS;
use super::names::{self, full};
use super::query::{SCAN_CHUNK, documents_of};
use super::{Backend, Ctx, lock, new_uuid};
use crate::MAX_BSON_OBJECT_SIZE;
use crate::api_version;
use crate::error::{BAD_VALUE, CommandError, INVALID_NAMESPACE, OPERATION_FAILED};
use crate::failpoint::CONFIGURE_FAIL_POINT;
use crate::ingress::IngressCounts;
use crate::types::{type_name, wrong_type, wrong_types};

/// `buildInfo`.
pub const BUILD_INFO: &str = "buildInfo";
/// `hostInfo`.
pub const HOST_INFO: &str = "hostInfo";
/// `serverStatus`.
pub const SERVER_STATUS: &str = "serverStatus";
/// `connectionStatus`.
pub const CONNECTION_STATUS: &str = "connectionStatus";
/// `whatsmyuri`.
pub const WHATS_MY_URI: &str = "whatsmyuri";
/// `listCommands`.
pub const LIST_COMMANDS: &str = "listCommands";
/// `getCmdLineOpts`.
pub const GET_CMD_LINE_OPTS: &str = "getCmdLineOpts";
/// `getLog`.
pub const GET_LOG: &str = "getLog";
/// `dbStats`.
pub const DB_STATS: &str = "dbStats";
/// `collStats`.
pub const COLL_STATS: &str = "collStats";

/// The RefDb version TeselaDB announces (D-024), the one its behaviour was measured against.
pub const ANNOUNCED_VERSION: &str = "9.0.4";
/// The announced version as `buildInfo`'s `versionArray`.
const VERSION_ARRAY: [i32; 4] = [9, 0, 4, 0];
/// What `serverStatus` calls the process: TeselaDB presents itself as a `refrouter` (D-024).
const PROCESS_NAME: &str = "refrouter";
/// The port after which RefDb writes `host:port` instead of the bare host name.
const DEFAULT_PORT: u16 = 27017;
/// The log `getLog` names with `'*'`, as a `refrouter` 9.0.4 lists them.
const GLOBAL_LOG: &str = "global";
/// The log of the warnings written at startup.
const STARTUP_WARNINGS: &str = "startupWarnings";
/// Bytes in a megabyte, for `mem`.
const MB: u64 = 1 << 20;

/// Counters of the connections of the node's client port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectionCounts {
    /// Open now.
    pub current: i64,
    /// More it may still open (`--maxConns` minus `current`).
    pub available: i64,
    /// Opened since the start.
    pub total_created: i64,
    /// Refused because the port was full.
    pub rejected: i64,
    /// Running a command now.
    pub active: i64,
}

/// Bytes and messages of the client port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkCounts {
    /// Bytes received, after decompression.
    pub bytes_in: i64,
    /// Bytes sent, before compression.
    pub bytes_out: i64,
    /// Bytes read from the sockets.
    pub physical_bytes_in: i64,
    /// Bytes written to the sockets.
    pub physical_bytes_out: i64,
    /// Requests received.
    pub requests: i64,
    /// TLS handshakes completed with clients (TLS 1.3).
    pub tls_handshakes: i64,
    /// Milliseconds they took, summed.
    pub tls_handshake_ms: i64,
    /// Microseconds they took, summed: their average goes in `metrics.network`.
    pub tls_handshake_micros: i64,
}

/// The certificate the client port presents with TLS (`security` of `serverStatus`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerCertificate {
    /// Its subject, as RFC 4514 writes it (`CN=localhost`).
    pub subject: String,
    /// Its `notAfter`, in milliseconds since the Unix epoch.
    pub expires_ms: i64,
}

/// What `getrusage(2)` and the operating system say of the process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceUse {
    /// Resident memory, in bytes.
    pub resident_bytes: u64,
    /// Virtual memory, in bytes.
    pub virtual_bytes: u64,
    /// CPU time in user mode, in microseconds.
    pub user_time_us: i64,
    /// CPU time in system mode, in microseconds.
    pub system_time_us: i64,
    /// Largest resident set, in kilobytes.
    pub max_resident_kb: i64,
    /// Blocks read.
    pub input_blocks: i64,
    /// Blocks written.
    pub output_blocks: i64,
    /// Page faults served without I/O.
    pub page_reclaims: i64,
    /// Page faults that needed I/O.
    pub page_faults: i64,
    /// Context switches the process asked for.
    pub voluntary_switches: i64,
    /// Context switches it was forced into.
    pub involuntary_switches: i64,
    /// Threads of the process.
    pub threads: i64,
    /// The task's page-ins on macOS, where `extra_info` has only them; `None` elsewhere.
    pub pageins: Option<i32>,
}

/// The host as `hostInfo` describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostFacts {
    /// Host name.
    pub hostname: String,
    /// Processor architecture (`aarch64`, `x86_64`).
    pub cpu_arch: String,
    /// Logical processors.
    pub num_cores: i32,
    /// Logical processors the process may use.
    pub num_cores_available: i32,
    /// Physical memory, in megabytes.
    pub mem_size_mb: i64,
    /// Bytes of a memory page.
    pub page_size: i64,
    /// Pages of physical memory.
    pub num_pages: i64,
    /// Most open files of the process.
    pub max_open_files: i64,
    /// Operating system kind (`Linux`, `Darwin`).
    pub os_type: String,
    /// Its name.
    pub os_name: String,
    /// Its version.
    pub os_version: String,
    /// The kernel's release.
    pub kernel_version: String,
    /// The kernel's full version string.
    pub version_string: String,
}

/// The node's log as `getLog` shows it: its most recent lines and how many it ever wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogTail {
    /// Lines written since the start.
    pub total: i64,
    /// The last lines, oldest first.
    pub lines: Vec<String>,
}

/// What the diagnostic commands report of the process and its host; the server fills it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessFacts {
    /// The client port's number.
    pub port: u16,
    /// Process identifier.
    pub pid: i64,
    /// Wall clock at the start, in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// Wall clock now, in milliseconds since the Unix epoch.
    pub now_ms: u64,
    /// The command line.
    pub argv: Vec<String>,
    /// The options the node runs with, as `getCmdLineOpts` names them.
    pub parsed: Document,
    /// TeselaDB's own version.
    pub build: String,
    /// The architecture and operating system TeselaDB was built for (`aarch64`, `linux`).
    pub build_target: (String, String),
    /// The host.
    pub host: HostFacts,
    /// The client port's connections.
    pub connections: ConnectionCounts,
    /// Its traffic.
    pub network: NetworkCounts,
    /// The process's use of the machine.
    pub resources: ResourceUse,
    /// Bytes used and total of the file system of the data directory.
    pub fs_used: u64,
    /// See `fs_used`.
    pub fs_total: u64,
    /// The `global` log.
    pub log: LogTail,
    /// The `startupWarnings` log.
    pub startup_warnings: LogTail,
    /// The client port's certificate; `None` without TLS.
    pub server_certificate: Option<ServerCertificate>,
}

impl ProcessFacts {
    /// The host as RefDb names it: `host`, or `host:port` off the default port.
    pub fn host_name(&self) -> String {
        match self.port {
            0 | DEFAULT_PORT => self.host.hostname.clone(),
            p => format!("{}:{p}", self.host.hostname),
        }
    }
}

/// What the node counted of its commands since it started (`serverStatus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpCounters {
    /// Documents inserted.
    pub insert: i64,
    /// `find` commands.
    pub query: i64,
    /// Update statements.
    pub update: i64,
    /// Delete statements.
    pub delete: i64,
    /// `getMore` commands.
    pub getmore: i64,
    /// Other commands.
    pub command: i64,
    /// `aggregate` commands.
    pub aggregate: i64,
    /// Commands that failed (`asserts.user`).
    pub user_asserts: i64,
}

/// [`OpCounters`] that every thread adds to without a lock (H-013 of the native tests), as
/// RefDb keeps its `opcounters` in atomics.
#[derive(Debug, Default)]
pub struct AtomicOpCounters {
    insert: std::sync::atomic::AtomicI64,
    query: std::sync::atomic::AtomicI64,
    update: std::sync::atomic::AtomicI64,
    delete: std::sync::atomic::AtomicI64,
    getmore: std::sync::atomic::AtomicI64,
    command: std::sync::atomic::AtomicI64,
    aggregate: std::sync::atomic::AtomicI64,
    user_asserts: std::sync::atomic::AtomicI64,
}

impl AtomicOpCounters {
    /// [`OpCounters::count`].
    pub fn count(&self, name: &str, cmd: &Document, failed: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let statements = |field: &str| match cmd.get(field) {
            Some(Value::Array(a)) => i64::try_from(a.len()).unwrap_or(i64::MAX),
            _ => 0,
        };
        let (counter, n) = match name {
            "insert" => (&self.insert, statements("documents")),
            "update" => (&self.update, statements("updates")),
            "delete" => (&self.delete, statements("deletes")),
            "find" => (&self.query, 1),
            "getMore" => (&self.getmore, 1),
            "aggregate" => (&self.aggregate, 1),
            _ => (&self.command, 1),
        };
        counter.fetch_add(n, Relaxed);
        if failed {
            self.user_asserts.fetch_add(1, Relaxed);
        }
    }

    /// The counters now.
    pub fn snapshot(&self) -> OpCounters {
        use std::sync::atomic::Ordering::Relaxed;
        OpCounters {
            insert: self.insert.load(Relaxed),
            query: self.query.load(Relaxed),
            update: self.update.load(Relaxed),
            delete: self.delete.load(Relaxed),
            getmore: self.getmore.load(Relaxed),
            command: self.command.load(Relaxed),
            aggregate: self.aggregate.load(Relaxed),
            user_asserts: self.user_asserts.load(Relaxed),
        }
    }
}

impl OpCounters {
    /// Counts `cmd`, named `name`, as RefDb's `opcounters` do, and whether it failed.
    pub fn count(&mut self, name: &str, cmd: &Document, failed: bool) {
        let statements = |field: &str| match cmd.get(field) {
            Some(Value::Array(a)) => i64::try_from(a.len()).unwrap_or(i64::MAX),
            _ => 0,
        };
        match name {
            "insert" => self.insert += statements("documents"),
            "update" => self.update += statements("updates"),
            "delete" => self.delete += statements("deletes"),
            "find" => self.query += 1,
            "getMore" => self.getmore += 1,
            "aggregate" => self.aggregate += 1,
            _ => self.command += 1,
        }
        if failed {
            self.user_asserts += 1;
        }
    }
}

/// A whole number as RefDb's `appendNumber` writes it: `int` when it fits, `long` otherwise.
fn number(n: i64) -> Value {
    i32::try_from(n).map_or(Value::Int64(n), Value::Int32)
}

/// `buildInfo`.
pub fn build_info<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    Ok(build_info_of(&ctx.backend.process()).with("ok", 1.0))
}

/// The fields of `buildInfo` for a node with `facts`.
pub fn build_info_of(facts: &ProcessFacts) -> Document {
    let env = Document::new()
        .with("distmod", "")
        .with("distarch", facts.build_target.0.as_str())
        .with("cc", "rustc")
        .with("ccflags", "")
        .with("cxx", "")
        .with("cxxflags", "")
        .with("linkflags", "")
        .with("target_arch", facts.build_target.0.as_str())
        .with("target_os", facts.build_target.1.as_str())
        .with("cppdefines", "");
    let version_array: Vec<Value> = VERSION_ARRAY.iter().map(|n| Value::Int32(*n)).collect();
    Document::new()
        .with("version", ANNOUNCED_VERSION)
        .with("versionArray", version_array)
        .with("gitVersion", facts.build.as_str())
        .with("modules", Vec::<Value>::new())
        .with("allocator", "system")
        .with("javascriptEngine", "none")
        .with("sysinfo", "deprecated")
        .with("buildEnvironment", env)
        .with("bits", 64)
        .with("debug", cfg!(debug_assertions))
        .with("maxBsonObjectSize", Value::Int64(MAX_BSON_OBJECT_SIZE as i64))
        .with("opentelemetry", false)
}

/// `hostInfo`.
pub fn host_info<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let facts = ctx.backend.process();
    let h = &facts.host;
    let system = Document::new()
        .with("currentTime", Value::DateTime(i64::try_from(facts.now_ms).unwrap_or(i64::MAX)))
        .with("hostname", facts.host_name())
        .with("cpuAddrSize", 64)
        .with("memSizeMB", Value::Int64(h.mem_size_mb))
        .with("memLimitMB", Value::Int64(h.mem_size_mb))
        .with("numCores", h.num_cores)
        .with("numCoresAvailableToProcess", h.num_cores_available)
        .with("numPhysicalCores", h.num_cores)
        .with("numCpuSockets", 1)
        .with("cpuArch", h.cpu_arch.as_str())
        .with("numaEnabled", false)
        .with("numNumaNodes", 1);
    let os = Document::new()
        .with("type", h.os_type.as_str())
        .with("name", h.os_name.as_str())
        .with("version", h.os_version.as_str());
    let extra = Document::new()
        .with("versionString", h.version_string.as_str())
        .with("kernelVersion", h.kernel_version.as_str())
        .with("pageSize", Value::Int64(h.page_size))
        .with("numPages", number(h.num_pages))
        .with("maxOpenFiles", number(h.max_open_files));
    Ok(Document::new().with("system", system).with("os", os).with("extra", extra).with("ok", 1.0))
}

/// Whether `serverStatus` shows `section`: every section unless the command sets it to a false
/// value.
fn shown(cmd: &Document, section: &str) -> bool {
    cmd.get(section).and_then(crate::types::truthy).unwrap_or(true)
}

/// `serverStatus`.
pub fn server_status<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let facts = ctx.backend.process();
    // Before the node's lock: `hot::of` may take it to find the counters.
    let counters = super::hot::of(ctx.shared).counters.snapshot();
    let (status, sessions, cursors, ingress) = {
        let g = lock(ctx.shared);
        let ingress = g.ingress.counts();
        (super::txn_stats::status(&g), g.sessions.len(), g.cursors.len(), ingress)
    };
    let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    let started = i64::try_from(facts.start_ms).unwrap_or(i64::MAX);
    let uptime_ms = facts.now_ms.saturating_sub(facts.start_ms);
    let mut d = Document::new()
        .with("host", facts.host_name())
        .with("version", ANNOUNCED_VERSION)
        .with("process", PROCESS_NAME)
        .with("pid", Value::Int64(facts.pid))
        .with("uptime", (uptime_ms / 1000) as f64)
        .with("uptimeMillis", Value::Int64(i64::try_from(uptime_ms).unwrap_or(i64::MAX)))
        .with("uptimeEstimate", Value::Int64(i64::try_from(uptime_ms / 1000).unwrap_or(i64::MAX)))
        .with("localTime", Value::DateTime(i64::try_from(facts.now_ms).unwrap_or(i64::MAX)));
    // In a `refrouter` 9.0.4's order; the sections that cannot apply to TeselaDB are in DV-038.
    let sections = [
        ("asserts", asserts(&counters)),
        ("connections", connections(&facts.connections, &ingress)),
        ("defaultRWConcern", super::cluster_admin::server_status_section(ctx.backend)?),
        ("extra_info", extra_info(&facts.resources)),
        (
            "health",
            Document::new()
                .with("state", "Ok")
                .with("enteredStateAtTime", Value::DateTime(started)),
        ),
        ("internalTransactions", status.internal_transactions()),
        ("logicalSessionRecordCache", status.session_cache(count(sessions))),
        ("network", network(&facts.network)),
        ("opLatencies", status.latencies()),
        ("opWorkingTime", status.latencies()),
        ("opcounters", opcounters(&counters)),
        ("security", security(status.security(), facts.server_certificate.as_ref())),
        ("transactions", status.transactions()),
        ("transportSecurity", transport_security(&facts.network)),
        ("mem", mem(&facts.resources)),
        ("metrics", metrics(&status, &facts.network, count(cursors), self_tests(ctx.shared))),
    ];
    for (name, section) in sections {
        if shown(ctx.cmd, name) {
            d.push(name, section);
        }
    }
    d.push("ok", 1.0);
    Ok(d)
}

/// Whether the node runs with `enableTestCommands` (`configureFailPoint` is one of its commands).
fn self_tests(shared: &super::Shared) -> bool {
    lock(shared).failpoint.is_some()
}

/// `security`: the authentications, then the client port's certificate when it has TLS (measured
/// on a `refrouter` 9.0.4 with `--tlsMode allowTLS` on 03-11-2026).
fn security(authentication: Document, cert: Option<&ServerCertificate>) -> Document {
    match cert {
        None => authentication,
        Some(c) => authentication
            .with("SSLServerSubjectName", c.subject.as_str())
            .with("SSLServerCertificateExpirationDate", Value::DateTime(c.expires_ms)),
    }
}

/// `transportSecurity`: connections by TLS version; the client port speaks TLS 1.3 only.
fn transport_security(n: &NetworkCounts) -> Document {
    Document::new()
        .with("1.0", Value::Int64(0))
        .with("1.1", Value::Int64(0))
        .with("1.2", Value::Int64(0))
        .with("1.3", Value::Int64(n.tls_handshakes))
        .with("unknown", Value::Int64(0))
}

/// `metrics`: the subsections TeselaDB has data for, in a `refrouter` 9.0.4's order.
fn metrics(
    s: &super::status::StatusCounters,
    n: &NetworkCounts,
    open_cursors: i64,
    tests: bool,
) -> Document {
    let names =
        COMMANDS.iter().map(|c| c.name).filter(|name| tests || *name != CONFIGURE_FAIL_POINT);
    let mut network = Document::new();
    // Once a client completed a handshake (measured on a `refrouter` 9.0.4). RefDb's value is its
    // own running average; TeselaDB gives the mean of every handshake.
    if n.tls_handshakes > 0 {
        let mean = n.tls_handshake_micros as f64 / n.tls_handshakes as f64;
        network.push("averageTimeToCompletedTLSHandshakeMicros", mean);
    }
    let network = network
        .with("totalIngressTLSConnections", Value::Int64(n.tls_handshakes))
        .with("totalIngressTLSHandshakeTimeMillis", Value::Int64(n.tls_handshake_ms));
    Document::new()
        .with("apiVersions", s.api_versions_section())
        .with("cursor", s.cursor_section(open_cursors))
        .with("commands", s.commands_section(names))
        .with("getLastError", s.write_concern_section())
        .with("network", network)
        .with("operation", s.operation_section())
        .with("queryExecutor", s.query_executor_section())
}

fn asserts(c: &OpCounters) -> Document {
    Document::new()
        .with("regular", 0)
        .with("warning", 0)
        .with("msg", 0)
        .with("user", number(c.user_asserts))
        .with("tripwire", 0)
        .with("rollovers", 0)
}

/// The `connections` section: the port's counters, and those of the limiter of new connections
/// (006 FR-036), whose closes count among the rejected ones too (measured).
fn connections(c: &ConnectionCounts, ingress: &IngressCounts) -> Document {
    let rate = Document::new()
        .with("rejected", number(ingress.rejected))
        .with("exempted", number(ingress.exempted))
        .with("interruptedDueToClientDisconnect", number(ingress.interrupted));
    Document::new()
        .with("current", number(c.current))
        .with("available", number(c.available))
        .with("totalCreated", number(c.total_created))
        .with("rejected", number(c.rejected.saturating_add(ingress.rejected)))
        .with("active", number(c.active))
        .with("queuedForEstablishment", number(ingress.queued))
        .with("establishmentRateLimit", rate)
        .with("threaded", number(c.current))
        .with("exhaustIsMaster", Value::Int64(0))
        .with("exhaustHello", Value::Int64(0))
        .with("awaitingTopologyChanges", Value::Int64(0))
        .with("loadBalanced", 0)
        .with("priority", 0)
}

fn network(n: &NetworkCounts) -> Document {
    Document::new()
        .with("bytesIn", Value::Int64(n.bytes_in))
        .with("bytesOut", Value::Int64(n.bytes_out))
        .with("physicalBytesIn", Value::Int64(n.physical_bytes_in))
        .with("physicalBytesOut", Value::Int64(n.physical_bytes_out))
        .with("numSlowDNSOperations", Value::Int64(0))
        .with("numSlowSSLOperations", Value::Int64(0))
        .with("numRequests", Value::Int64(n.requests))
}

fn opcounters(c: &OpCounters) -> Document {
    Document::new()
        .with("insert", Value::Int64(c.insert))
        .with("query", Value::Int64(c.query))
        .with("update", Value::Int64(c.update))
        .with("delete", Value::Int64(c.delete))
        .with("getmore", Value::Int64(c.getmore))
        .with("command", Value::Int64(c.command))
        .with("aggregate", Value::Int64(c.aggregate))
}

fn mem(r: &ResourceUse) -> Document {
    let mb = |bytes: u64| number(i64::try_from(bytes / MB).unwrap_or(i64::MAX));
    Document::new()
        .with("bits", 64)
        .with("resident", mb(r.resident_bytes))
        .with("virtual", mb(r.virtual_bytes))
        .with("supported", true)
}

/// What RefDb 9.0.4 says of the process in `extra_info`, which varies by platform: on macOS the
/// task's page-ins as `page_faults` (`processinfo_osx.cpp`), on Linux `getrusage(2)` and the
/// threads.
fn extra_info(r: &ResourceUse) -> Document {
    let note = Document::new().with("note", "fields vary by platform");
    if let Some(pageins) = r.pageins {
        return note.with("page_faults", pageins);
    }
    note.with("user_time_us", Value::Int64(r.user_time_us))
        .with("system_time_us", Value::Int64(r.system_time_us))
        .with("maximum_resident_set_kb", Value::Int64(r.max_resident_kb))
        .with("input_blocks", Value::Int64(r.input_blocks))
        .with("output_blocks", Value::Int64(r.output_blocks))
        .with("page_reclaims", Value::Int64(r.page_reclaims))
        .with("page_faults", Value::Int64(r.page_faults))
        .with("voluntary_context_switches", Value::Int64(r.voluntary_switches))
        .with("involuntary_context_switches", Value::Int64(r.involuntary_switches))
        .with("threads", Value::Int64(r.threads))
}

/// The roles of the running command's user: its direct roles, then those they inherit, as
/// RefDb 9.0.4 lists them in `connectionStatus`; none without a user.
pub(crate) fn authenticated_roles<B: Backend + ?Sized>(
    backend: &mut B,
) -> Result<Vec<RoleName>, CommandError> {
    Ok(authenticated_grants(backend)?.0)
}

/// The roles of the running command's user ([`authenticated_roles`]) and the privileges they
/// give, merged by resource; none without a user.
fn authenticated_grants<B: Backend + ?Sized>(
    backend: &mut B,
) -> Result<(Vec<RoleName>, Vec<Privilege>), CommandError> {
    let Some(name) = backend.authenticated_user() else { return Ok((Vec::new(), Vec::new())) };
    let direct = match backend.authenticated_roles() {
        // `__system` lists no role (measured on `refnode` 9.0.4, 013 FR-034).
        _ if super::auth::is_internal(&name) => Vec::new(),
        Some(r) => r,
        None => backend.user(&name)?.map(|u| u.roles).unwrap_or_default(),
    };
    let inherited = super::roles::Graph::new(&mut *backend).inherit(&direct, None)?;
    let mut all = direct;
    for r in inherited.roles {
        if !all.contains(&r) {
            all.push(r);
        }
    }
    Ok((all, inherited.privileges))
}

/// `connectionStatus`: the user, its roles and, with `showPrivileges`, their privileges (013); a
/// new UUID on each call, as 9.0.4.
pub fn connection_status<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let privileges = ctx.cmd.get("showPrivileges").and_then(crate::types::truthy) == Some(true);
    let (mut users, mut roles, mut granted) =
        (Vec::<Value>::new(), Vec::<Value>::new(), Vec::<Value>::new());
    if let Some(name) = ctx.backend.authenticated_user() {
        let (r, p) = authenticated_grants(&mut *ctx.backend)?;
        roles = r
            .iter()
            .map(|r| Value::Document(Document::new().with("role", &*r.role).with("db", &*r.db)))
            .collect();
        granted = p.iter().map(Privilege::to_value).collect();
        users.push(Value::Document(Document::new().with("user", name.user).with("db", name.db)));
    }
    let mut auth =
        Document::new().with("authenticatedUsers", users).with("authenticatedUserRoles", roles);
    if privileges {
        auth.push("authenticatedUserPrivileges", granted);
    }
    let uuid = new_uuid(ctx.backend);
    Ok(Document::new()
        .with("authInfo", auth)
        .with("uuid", uuid_value(u128::from_be_bytes(uuid)))
        .with("ok", 1.0))
}

/// `getCmdLineOpts`.
pub fn get_cmd_line_opts<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(GET_CMD_LINE_OPTS, ctx.db)?;
    let facts = ctx.backend.process();
    let argv: Vec<Value> = facts.argv.iter().map(|a| Value::from(a.as_str())).collect();
    Ok(Document::new().with("argv", argv).with("parsed", facts.parsed).with("ok", 1.0))
}

/// `getLog`: `'*'` names the logs, `global` and `startupWarnings` give their last lines.
pub fn get_log<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(GET_LOG, ctx.db)?;
    let Value::String(which) = ctx.first().clone() else {
        return Err(wrong_type(&format!("{GET_LOG}.{GET_LOG}"), ctx.first(), "string"));
    };
    let facts = ctx.backend.process();
    if which == "*" {
        // RefDb's in-memory logs are created on first use: `startupWarnings` exists once a
        // warning was logged at start (as the warning of the insecure mode).
        let mut names = vec![Value::from(GLOBAL_LOG)];
        if facts.startup_warnings.total > 0 {
            names.push(Value::from(STARTUP_WARNINGS));
        }
        return Ok(Document::new().with("names", names).with("ok", 1.0));
    }
    let tail = match which.as_str() {
        GLOBAL_LOG => facts.log,
        STARTUP_WARNINGS => facts.startup_warnings,
        other => {
            let msg = format!("No log named '{other}'");
            return Err(CommandError::new(OPERATION_FAILED, msg));
        }
    };
    let lines: Vec<Value> = tail.lines.into_iter().map(Value::from).collect();
    Ok(Document::new()
        .with("totalLinesWritten", number(tail.total))
        .with("log", lines)
        .with("ok", 1.0))
}

/// The numeric types of `scale`, in a `refrouter` 9.0.4's order.
const SCALE_TYPES: [&str; 4] = ["int", "decimal", "double", "long"];

/// The `scale` of `dbStats` and `collStats`: a positive whole number, 1 by default.
fn scale(cmd: &Document, name: &str) -> Result<i64, CommandError> {
    let s = match cmd.get("scale") {
        None => return Ok(1),
        Some(v @ (Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_))) => {
            crate::types::number(v).map_or(0, |x| x as i64)
        }
        Some(v) => return Err(wrong_types(&format!("{name}.scale"), v, &SCALE_TYPES)),
    };
    if s <= 0 {
        return Err(CommandError::new(BAD_VALUE, "Scale must be greater than zero"));
    }
    Ok(s)
}

/// What a collection holds: its documents, their bytes and the bytes of their keys.
pub(super) struct Tally {
    pub(super) count: i64,
    pub(super) bytes: u64,
    pub(super) key_bytes: u64,
}

/// Key intervals `[start, end)`: the ranges of one node, for the part of a collection it holds.
pub(super) type Spans = [(Vec<u8>, Vec<u8>)];

/// The parts of `[start, end)` inside `within`, or the whole interval without it.
pub(super) fn clip(
    (start, end): (&[u8], &[u8]),
    within: Option<&Spans>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let Some(within) = within else { return vec![(start.to_vec(), end.to_vec())] };
    within
        .iter()
        .map(|(s, e)| {
            (s.as_slice().max(start), if e.is_empty() { end } else { e.as_slice().min(end) })
        })
        .filter(|(s, e)| s < e)
        .map(|(s, e)| (s.to_vec(), e.to_vec()))
        .collect()
}

/// The stored bytes of `[start, end)` inside `within`.
pub(super) fn stored_within<B: Backend + ?Sized>(
    backend: &mut B,
    span: (&[u8], &[u8]),
    within: Option<&Spans>,
) -> Result<u64, CommandError> {
    let mut total = 0;
    for (s, e) in clip(span, within) {
        total += backend.stored_bytes(&s, &e)?;
    }
    Ok(total)
}

/// Documents, their bytes and the bytes of their keys in a collection.
fn tally<B: Backend + ?Sized>(
    backend: &mut B,
    c: &teseladb_meta::catalog::CollectionEntry,
) -> Result<Tally, CommandError> {
    tally_within(backend, c, None)
}

/// [`tally`] of the part of the collection inside `within`.
pub(super) fn tally_within<B: Backend + ?Sized>(
    backend: &mut B,
    c: &teseladb_meta::catalog::CollectionEntry,
    within: Option<&Spans>,
) -> Result<Tally, CommandError> {
    let (docs_start, docs_end) = documents_of(c);
    let mut t = Tally { count: 0, bytes: 0, key_bytes: 0 };
    for (mut start, end) in clip((&docs_start, &docs_end), within) {
        loop {
            let chunk = backend.scan(&start, &end, SCAN_CHUNK, ReadConcern::Local)?;
            let Some((last, _)) = chunk.last() else { break };
            let next = [last.as_slice(), &[0]].concat();
            for (key, doc) in &chunk {
                t.count += 1;
                // The size of its BSON, as it came in: the table of offsets is not data.
                t.bytes += teseladb_keys::stored::bson(doc).map_or(doc.len(), <[u8]>::len) as u64;
                t.key_bytes += key.len() as u64;
            }
            if chunk.len() < SCAN_CHUNK {
                break;
            }
            start = next;
        }
    }
    Ok(t)
}

/// Bytes `stored` takes in whole storage blocks, at least one (an empty index takes 4096 bytes
/// in RefEngine, measured on 9.0.4; DV-037).
pub(super) fn in_blocks(stored: u64) -> u64 {
    stored.div_ceil(INDEX_BYTES).max(1) * INDEX_BYTES
}

/// The indexes of collection `c` with the bytes of their keys, in the order of `listIndexes`:
/// `_id_` (the keys of the documents, or the `_id` entries of a capped collection) unless the
/// collection is clustered or a time series, then each secondary index (010 FR-018).
fn index_sizes<B: Backend + ?Sized>(
    backend: &mut B,
    (ns, c): (&teseladb_meta::catalog::Namespace, &teseladb_meta::catalog::CollectionEntry),
    kind: &CollectionKind,
    key_bytes: u64,
) -> Result<Vec<(String, u64)>, CommandError> {
    index_sizes_within(backend, (ns, c), kind, key_bytes, None)
}

/// [`index_sizes`] of the part of the indexes inside `within`.
pub(super) fn index_sizes_within<B: Backend + ?Sized>(
    backend: &mut B,
    (ns, c): (&teseladb_meta::catalog::Namespace, &teseladb_meta::catalog::CollectionEntry),
    kind: &CollectionKind,
    key_bytes: u64,
    within: Option<&Spans>,
) -> Result<Vec<(String, u64)>, CommandError> {
    let prefix = |id: u32| KeyBuilder::index_prefix(c.tenant, c.id, id).as_bytes().to_vec();
    let mut out = Vec::new();
    match kind {
        _ if !super::catalog::has_id_index(ns) => {}
        CollectionKind::Clustered { .. } | CollectionKind::TimeSeries(_) => {}
        CollectionKind::Capped { .. } => {
            let start = prefix(teseladb_range::capped::ID_INDEX);
            let end = KeyBuilder::collection_end(c.tenant, c.id).as_bytes().to_vec();
            let stored = stored_within(backend, (&start, &end), within)?;
            out.push(("_id_".to_owned(), in_blocks(stored)));
        }
        _ => out.push(("_id_".to_owned(), in_blocks(key_bytes))),
    }
    for e in backend.indexes(c.id)? {
        if e.state == IndexState::Dropping {
            continue;
        }
        let bytes = stored_within(backend, (&prefix(e.id), &prefix(e.id + 1)), within)?;
        out.push((e.name, in_blocks(bytes)));
    }
    Ok(out)
}

/// The `nindexes` of `collStats` for a collection with `sized` indexes in `indexSizes`: a
/// clustered collection's index counts and has no size of its own (measured on a `refrouter` 9.0.4
/// on 05-11-2026: `nindexes: 1`, `indexSizes: {}`).
fn index_count(kind: &CollectionKind, sized: usize) -> i32 {
    let clustered = usize::from(matches!(kind, CollectionKind::Clustered { .. }));
    i32::try_from(sized + clustered).unwrap_or(i32::MAX)
}

/// `dbStats`. The sizes are TeselaDB's (DV-037); views arrive with 010.
pub fn db_stats<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let scale = scale(ctx.cmd, DB_STATS)?;
    let db = ctx.db.to_owned();
    let (mut collections, mut objects, mut data, mut storage) = (0i64, 0i64, 0u64, 0u64);
    let (mut indexes, mut index) = (0i64, 0u64);
    for (ns, c) in ctx.backend.collections()? {
        if ns.db != db {
            continue;
        }
        let t = tally(ctx.backend, &c)?;
        let (start, end) = documents_of(&c);
        collections += 1;
        objects += t.count;
        data += t.bytes;
        storage += collection_size(ctx.backend.stored_bytes(&start, &end)?) - INDEX_BYTES;
        let kind = super::kinds::kind_of(&c)?;
        let sizes = index_sizes(ctx.backend, (&ns, &c), &kind, t.key_bytes)?;
        indexes += i64::try_from(sizes.len()).unwrap_or(0);
        index += sizes.iter().map(|(_, b)| b).sum::<u64>();
    }
    let facts = ctx.backend.process();
    let scaled = |b: u64| b as f64 / scale as f64;
    let avg = if objects > 0 { data as f64 / objects as f64 } else { 0.0 };
    let (fs_used, fs_total) = match collections {
        0 => (0, 0),
        _ => (facts.fs_used, facts.fs_total),
    };
    // The shard's own reply goes first under `raw`, its counts as 64-bit integers (measured).
    let stats = |count: &dyn Fn(i64) -> Value| {
        Document::new()
            .with("db", db.as_str())
            .with("collections", count(collections))
            .with("views", count(0))
            .with("objects", count(objects))
            .with("avgObjSize", avg)
            .with("dataSize", scaled(data))
            .with("storageSize", scaled(storage))
            .with("indexes", count(indexes))
            .with("indexSize", scaled(index))
            .with("totalSize", scaled(storage + index))
            .with("scaleFactor", count(scale))
            .with("fsUsedSize", scaled(fs_used))
            .with("fsTotalSize", scaled(fs_total))
            .with("ok", 1.0)
    };
    let raw = stats(&Value::Int64);
    let mut out =
        Document::new().with("raw", Document::new().with(super::raw_name(ctx.backend), raw));
    for (k, v) in stats(&number).iter() {
        out.push(k, v.clone());
    }
    Ok(out)
}

/// The average size of `count` documents (buckets) of `bytes` bytes, none without documents.
fn average(bytes: u64, count: i64) -> Option<i64> {
    u64::try_from(count)
        .ok()
        .filter(|n| *n > 0)
        .map(|n| i64::try_from(bytes / n).unwrap_or(i64::MAX))
}

/// The `storageStats` of `$collStats` for collection `c` of `ns`, sizes divided by `scale`, in
/// the order of a `refrouter` 9.0.4 (measured 31-10-2026): a capped collection shows its limits, a
/// time series its buckets instead of documents, and the index of a time series is on its
/// `metaField` and `timeField`. Like `collStats` it has no `refengine`, `indexDetails` nor
/// `indexBuilds` (DV-038).
pub(crate) fn storage_stats<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    ns: &teseladb_meta::catalog::Namespace,
    c: &teseladb_meta::catalog::CollectionEntry,
    scale: i64,
) -> Result<Document, CommandError> {
    let Tally { count, bytes, key_bytes } = tally(backend, c)?;
    let (start, end) = documents_of(c);
    let storage = collection_size(backend.stored_bytes(&start, &end)?) - INDEX_BYTES;
    let scaled =
        |b: u64| number(i64::try_from(b / u64::try_from(scale).unwrap_or(1)).unwrap_or(i64::MAX));
    let kind = super::kinds::kind_of(c)?;
    let sizes = index_sizes(backend, (ns, c), &kind, key_bytes)?;
    let mut d = Document::new().with("size", scaled(bytes));
    match &kind {
        // Without documents (buckets) there is no average, and before its first insert a time
        // series has no counters (measured on a `refnode` 9.0.4 on 03-11-2026).
        CollectionKind::TimeSeries(_) => {
            let stats = lock(shared).timeseries.get(&c.id).cloned();
            let mut series = Document::new().with("bucketsNs", full(ns));
            let avg = average(bytes, count);
            let fields = match stats {
                Some(stats) => stats.fields(count, avg.unwrap_or(0)),
                None => vec![("bucketCount", count)],
            };
            for (k, v) in fields.into_iter().filter(|(k, _)| avg.is_some() || *k != "avgBucketSize")
            {
                series.push(k, number(v));
            }
            d.push("timeseries", series);
        }
        _ => {
            d.push("count", number(count));
            if let Some(avg) = average(bytes, count) {
                d.push("avgObjSize", number(avg));
            }
        }
    }
    let index_bytes: u64 = sizes.iter().map(|(_, b)| b).sum();
    let mut index_sizes = Document::new();
    for (name, b) in sizes {
        index_sizes.push(name, scaled(b));
    }
    d.push("numOrphanDocs", 0);
    d.push("storageSize", scaled(storage));
    d.push("freeStorageSize", 0);
    d.push("capped", matches!(kind, CollectionKind::Capped { .. }));
    if let CollectionKind::Capped { size, max } = &kind {
        d.push("max", number(max.map_or(0, |m| i64::try_from(m).unwrap_or(i64::MAX))));
        d.push("maxSize", scaled(*size));
    }
    let nindexes = i64::from(index_count(&kind, index_sizes.len()));
    Ok(d.with("nindexes", number(nindexes))
        .with("totalIndexSize", scaled(index_bytes))
        .with("indexSizes", index_sizes)
        .with("totalSize", scaled(storage + index_bytes))
        .with("scaleFactor", number(scale)))
}

/// Bytes TeselaDB counts for the `_id` index of a collection (DV-037).
pub(super) const INDEX_BYTES: u64 = super::catalog::BLOCK_BYTES;

/// `collStats` (obsolete in 9.0 but still sent by tools, DV-021).
pub fn coll_stats<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let coll = match ctx.first() {
        Value::String(s) => s.clone(),
        other => {
            let msg = format!("Collection name has invalid type {}", type_name(other));
            return Err(CommandError::new(INVALID_NAMESPACE, msg));
        }
    };
    let scale = scale(ctx.cmd, COLL_STATS)?;
    if coll.is_empty() {
        let msg = format!("Invalid namespace specified '{}'", ctx.db);
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    let ns = names::ns(ctx.db, &coll);
    names::check(&ns)?;
    let scaled =
        |b: u64| number(i64::try_from(b / u64::try_from(scale).unwrap_or(1)).unwrap_or(i64::MAX));
    let Some(c) = ctx.backend.collection(&ns)? else {
        // In a database the catalog does not have, `refrouter` 9.0.4 fails (measured).
        if !super::catalog::has_database(ctx.backend, &ns.db)? {
            return Err(super::catalog::missing_namespace(ctx.backend, &ns, String::new())?);
        }
        let primary = shards::primary_of(ctx.backend, &ns.db)?;
        let one =
            Document::new().with(primary.as_str(), shards::missing_shard_doc(&full(&ns), scale));
        return Ok(Document::new()
            .with("sharded", false)
            .with("primary", primary)
            .with("ns", full(&ns))
            .with("count", 0)
            .with("size", 0)
            .with("storageSize", 0)
            .with("totalIndexSize", 0)
            .with("totalSize", 0)
            .with("indexSizes", Document::new())
            .with("avgObjSize", 0.0)
            .with("maxSize", Value::Int64(0))
            .with("nindexes", 0)
            .with("scaleFactor", number(scale))
            .with("nchunks", 1)
            .with("shards", one)
            .with("ok", 1.0));
    };
    super::kinds::not_a_view(&ns, &c)?;
    // A collection with a shard key, or the primary shard of its database (T055).
    let key = super::sharding::presented_key(&ns, &c)?;
    let mut d = Document::new().with("sharded", key.is_some());
    if key.is_none() {
        d.push("primary", shards::primary_of(ctx.backend, &ns.db)?);
    }
    let Tally { count, bytes, key_bytes } = tally(ctx.backend, &c)?;
    let (start, end) = documents_of(&c);
    let storage = collection_size(ctx.backend.stored_bytes(&start, &end)?) - INDEX_BYTES;
    let avg = if count > 0 { (bytes / u64::try_from(count).unwrap_or(1)) as f64 } else { 0.0 };
    // Each kind with its shape (015 FR-010, measured on a `refrouter` 9.0.4): a capped collection
    // shows its limits, a clustered one has no `_id_` index of its own, and a time series counts
    // buckets instead of documents.
    let kind = super::kinds::kind_of(&c)?;
    d.push("capped", matches!(kind, CollectionKind::Capped { .. }));
    let mut max_size = 0;
    if let CollectionKind::Capped { size, max } = &kind {
        d.push("max", number(max.map_or(0, |m| i64::try_from(m).unwrap_or(i64::MAX))));
        max_size = i64::try_from(*size).unwrap_or(i64::MAX);
    }
    d.push("ns", full(&ns));
    let series = matches!(kind, CollectionKind::TimeSeries(_));
    if !series {
        d.push("count", number(count));
    }
    d.push("size", scaled(bytes));
    d.push("storageSize", scaled(storage));
    // A clustered collection and a time series have no `_id_` index of their own; a time series
    // has one on its `metaField` and `timeField` when it has a `metaField` (measured).
    let sizes = index_sizes(ctx.backend, (&ns, &c), &kind, key_bytes)?;
    let index_bytes: u64 = sizes.iter().map(|(_, b)| b).sum();
    d.push("totalIndexSize", scaled(index_bytes));
    d.push("totalSize", scaled(storage + index_bytes));
    if series {
        let avg_bucket = if count > 0 { bytes / u64::try_from(count).unwrap_or(1) } else { 0 };
        // The bucket counters of this node since it started, as RefDb's catalog counts them.
        let stats = lock(ctx.shared).timeseries.get(&c.id).cloned().unwrap_or_default();
        let mut ts = Document::new().with("bucketsNs", full(&ns));
        let avg = i64::try_from(avg_bucket).unwrap_or(i64::MAX);
        for (k, v) in stats.fields(count, avg) {
            ts.push(k, number(v));
        }
        d.push("timeseries", ts);
    }
    let nindexes = index_count(&kind, sizes.len());
    let mut index_sizes = Document::new();
    for (name, b) in &sizes {
        index_sizes.push(name, scaled(*b));
    }
    let avg_value = if series { Value::Int32(0) } else { Value::Double(avg) };
    let nchunks = match &key {
        // The record of sessions is one chunk (006 FR-038).
        Some(_) if super::sharding::is_sessions_record(&ns) => 1,
        Some(k) => {
            let entries = ctx.backend.route_entries()?;
            super::chunks::collection_chunks(&entries, &c, k).len().max(1)
        }
        None => 1,
    };
    let down = |b: u64| i64::try_from(b / u64::try_from(scale).unwrap_or(1)).unwrap_or(i64::MAX);
    let whole = shards::Part {
        count,
        size: down(bytes),
        storage: down(storage),
        index_sizes: sizes.iter().map(|(n, b)| (n.clone(), down(*b))).collect(),
    };
    let shown = shards::Kind {
        capped: match &kind {
            CollectionKind::Capped { max, .. } => {
                Some((number(max.map_or(0, |m| i64::try_from(m).unwrap_or(i64::MAX))), max_size))
            }
            _ => None,
        },
        series: match d.get("timeseries") {
            Some(Value::Document(ts)) => Some(ts.clone()),
            _ => None,
        },
    };
    let parts = shard_parts(ctx.backend, (&ns, &c), &kind, whole, scale)?;
    let mut per_shard = Document::new();
    for (name, p) in parts {
        per_shard.push(name, shards::shard_doc(&full(&ns), &p, &shown, nindexes, scale));
    }
    Ok(d.with("indexSizes", index_sizes)
        .with("avgObjSize", avg_value)
        .with("maxSize", Value::Int64(max_size))
        .with("nindexes", nindexes)
        .with("scaleFactor", number(scale))
        .with("nchunks", i32::try_from(nchunks).unwrap_or(i32::MAX))
        .with("shards", per_shard)
        .with("ok", 1.0))
}

/// What each shard holds of collection `c` for `collStats`: a node per name. With one node, or
/// none known, the whole collection under it (or under its database's primary); with several,
/// each node's ranges measured on their own.
fn shard_parts<B: Backend + ?Sized>(
    backend: &mut B,
    (ns, c): (&teseladb_meta::catalog::Namespace, &teseladb_meta::catalog::CollectionEntry),
    kind: &CollectionKind,
    whole: shards::Part,
    scale: i64,
) -> Result<Vec<(String, shards::Part)>, CommandError> {
    let holders = shards::holders(backend, c)?;
    if holders.len() <= 1 {
        let name = match holders.keys().next() {
            Some(node) => super::shard_name(*node),
            None => shards::primary_of(backend, &ns.db)?,
        };
        return Ok(vec![(name, whole)]);
    }
    let down = |b: u64| i64::try_from(b / u64::try_from(scale).unwrap_or(1)).unwrap_or(i64::MAX);
    let mut out = Vec::new();
    for (node, spans) in holders {
        let t = tally_within(backend, c, Some(&spans))?;
        let (start, end) = documents_of(c);
        let stored = stored_within(backend, (&start, &end), Some(&spans))?;
        let storage = collection_size(stored) - INDEX_BYTES;
        let sizes = index_sizes_within(backend, (ns, c), kind, t.key_bytes, Some(&spans))?;
        let part = shards::Part {
            count: t.count,
            size: down(t.bytes),
            storage: down(storage),
            index_sizes: sizes.into_iter().map(|(n, b)| (n, down(b))).collect(),
        };
        out.push((super::shard_name(node), part));
    }
    Ok(out)
}

/// The help of `getParameter`, as 9.0.4 writes it, with TeselaDB's parameters.
fn get_parameter_help() -> String {
    let mut h = String::from(
        "get administrative option(s)\nexample:\n{ getParameter:1, notablescan:1 }\npass a document as the value for getParameter to request options\nexample:\n{ getParameter:{showDetails: true}, notablescan:1}\nsupported:\n",
    );
    for name in super::server_params::names() {
        h.push_str(&format!("  {name}\n"));
    }
    h.push_str(
        "{ getParameter:'*' } or { getParameter:{allParameters: true} } to get everything\n",
    );
    h
}

/// The help of `setParameter`, as 9.0.4 writes it, with the parameters settable at runtime.
fn set_parameter_help() -> String {
    let mut h = String::from(
        "set administrative option(s)\n{ setParameter:1, <param>:<value> }\nsupported:\n",
    );
    for name in super::server_params::runtime_names() {
        h.push_str(&format!("  {name}\n"));
    }
    h
}

/// `listCommands`: every command the node answers ([`crate::dispatch::command_names`]), in
/// byte order of their names as a `refrouter` 9.0.4 lists them, with the fields measured there;
/// `configureFailPoint` only on a node with `enableTestCommands`.
pub fn list_commands<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let tests = lock(ctx.shared).failpoint.is_some();
    let mut entries: Vec<(&str, Document)> = Vec::new();
    for c in COMMANDS.iter().filter(|c| tests || c.name != CONFIGURE_FAIL_POINT) {
        let help = match c.name {
            "getParameter" => get_parameter_help(),
            "setParameter" => set_parameter_help(),
            _ => c.help.to_owned(),
        };
        let mut entry = Document::new()
            .with("help", help)
            .with("requiresAuth", c.requires_auth)
            .with("requiresAuthzChecks", c.requires_authz_checks)
            .with("secondaryOk", c.secondary_ok)
            .with("adminOnly", c.admin_only);
        if c.secondary_override_ok {
            entry.push("secondaryOverrideOk", true);
        }
        let api: Vec<Value> = if c.api_v1 { vec![Value::from("1")] } else { Vec::new() };
        entry.push("apiVersions", api);
        entry.push("deprecatedApiVersions", Vec::<Value>::new());
        entries.push((c.name, entry));
    }
    if tests {
        entries.extend(api_version::TEST_COMMANDS.iter().map(|t| (t.name, t.entry())));
    }
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut commands = Document::new();
    for (name, entry) in entries {
        commands.push(name, entry);
    }
    Ok(Document::new().with("commands", commands).with("ok", 1.0))
}

/// The names `listCommands` gives, `configureFailPoint` included, in its order: tests check them
/// against the commands the dispatch takes.
pub fn listed_names() -> impl Iterator<Item = &'static str> {
    let mut names: Vec<&'static str> = COMMANDS.iter().map(|c| c.name).collect();
    names.extend(api_version::TEST_COMMANDS.iter().map(|t| t.name));
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    names.into_iter()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod op_counter_tests {
    use super::{AtomicOpCounters, OpCounters};
    use teseladb_bson::{Document, Value};

    #[test]
    fn the_counters_add_up_the_commands_of_every_thread() {
        let c = std::sync::Arc::new(AtomicOpCounters::default());
        let insert = Document::new().with("documents", Value::Array(vec![Value::Int32(1); 3]));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (c, insert) = (c.clone(), insert.clone());
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        c.count("find", &Document::new(), false);
                        c.count("insert", &insert, true);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        let want =
            OpCounters { query: 800, insert: 2_400, user_asserts: 800, ..OpCounters::default() };
        assert_eq!(c.snapshot(), want);
    }
}
