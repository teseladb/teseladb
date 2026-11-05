//! The `teseladb` binary as a library (specification 001, FR-017 and FR-018): command line,
//! logging, a core's logic over the I/O runtime, node start-up and the in-process development
//! cluster. `main.rs` only calls [`run`].
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

/// The `User-Agent` of the node's own HTTP requests (OCSP, OIDC, STS).
pub const USER_AGENT: &str = "teseladb";

pub mod admin;
pub mod admin_apikeys;
pub mod admin_http;
pub mod admin_tenant_state;
pub mod admin_tenants;
pub mod affinity;
pub mod at_rest;
pub mod audit_log;
pub mod authz_lease;
pub mod backend;
pub mod backup;
pub mod catalog_ops;
pub mod certs;
pub mod checksums;
pub mod cli;
pub mod client_listener;
pub mod clock;
pub mod cluster_auth;
pub mod clustertime_keys;
pub mod config;
pub mod config_db;
pub mod coresm;
pub mod data;
pub mod data_wire;
pub mod dev;
pub mod diag;
pub mod diagnostic_capture;
pub mod enroll;
pub mod forward;
pub mod frames;
pub mod hosting;
pub mod http_listener;
pub mod http_service;
pub mod https;
mod idle_flush;
pub mod kmip;
pub mod ldap_client;
pub mod leaders;
pub mod listen;
pub mod liveness;
pub mod local_zone;
pub mod log;
pub mod logic;
pub mod mcp_stdio;
pub mod memory;
pub mod merges;
pub mod meta2_ops;
pub mod node;
pub mod node_versions;
pub mod ocsp_staple;
pub mod oidc_client;
pub mod pki;
pub mod ranges;
pub mod relocations;
pub mod replica_port;
pub mod replicas;
pub mod rest_executor;
pub mod rollback;
pub mod routes;
pub mod search_builder;
pub mod search_segments;
pub mod sessions_ops;
pub mod spill;
pub mod sts_client;
pub mod tenant_limits;
pub mod tls_port;
pub mod tls_stream;
pub mod transfer;
pub mod txn_wire;

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use teseladb_io::NodeDisk;

use crate::cli::{Command, StartOpts, USAGE, USAGE_EXIT};
use crate::client_listener::ClientConfig;
use crate::diag::ProcessConfig;
use crate::log::{Format, Logger, json_string};
use crate::node::{Node, NodeConfig, lock_dir};
use teseladb_auth::audit::AuditType;
use teseladb_bson::{Document, Value};

/// Version of the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Longest a node waits for Group 0 to have the cluster created before giving up.
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a running node checks whether it was asked to stop.
pub(crate) const STOP_POLL: Duration = Duration::from_millis(50);

/// Output of `teseladb --version`.
pub fn version_text() -> String {
    format!("teseladb {VERSION}\n")
}

/// Text of the ready line: `ready node_id=<n> cluster_id=<hex> peer=<addr>`.
pub fn ready_line(format: Format, node_id: u64, cluster_id: &[u8; 16], peer: &str) -> String {
    let hex = cluster_id.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    match format {
        Format::Text => format!("ready node_id={node_id} cluster_id={hex} peer={peer}\n"),
        Format::Json => {
            let mut p = String::new();
            json_string(&mut p, peer);
            format!(
                "{{\"msg\":\"ready\",\"node_id\":{node_id},\"cluster_id\":\"{hex}\",\"peer\":{p}}}\n"
            )
        }
    }
}

/// The program `teseladb`: installs the stop handler with `install`, then runs `args` (see [`run`]);
/// a handler that cannot be installed ends it with 1.
pub fn program(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    install: &dyn Fn() -> io::Result<()>,
    stop: &dyn Fn() -> bool,
) -> i32 {
    if let Err(e) = install() {
        let _ = writeln!(err, "teseladb: cannot handle stop signals: {e}");
        return 1;
    }
    run(args, out, err, stop)
}

/// Runs the command line `args` (without the program name) and returns the exit code: 0 correct,
/// 1 execution error, 2 usage error. `out` and `err` are standard output and error; the node runs
/// until `stop()` says so (a stop signal in the binary).
pub fn run(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    stop: &dyn Fn() -> bool,
) -> i32 {
    let cmd = match cli::parse(args) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return USAGE_EXIT;
        }
    };
    match cmd {
        Command::Help => {
            let _ = out.write_all(USAGE.as_bytes());
            0
        }
        Command::Version => {
            let _ = out.write_all(version_text().as_bytes());
            0
        }
        Command::Dev(o) => dev::run(&o, out, err, stop),
        Command::Admin(o) => admin::run(&o, out, err),
        Command::Mcp(o) => {
            let key = std::env::var(mcp_stdio::API_KEY_VAR).ok().filter(|k| !k.is_empty());
            mcp_stdio::run(&o, &mut io::stdin().lock(), out, err, key)
        }
        Command::Start(o) => {
            let argv = std::iter::once(PROGRAM.to_owned()).chain(redact_password(args)).collect();
            start(&StartOpts { argv, ..*o }, out, err, stop)
        }
    }
}

/// Where the log goes: standard error, or `--logpath` (replaced unless `--logappend`).
fn log_output(o: &StartOpts) -> io::Result<Box<dyn Write + Send>> {
    let Some(path) = &o.logpath else { return Ok(Box::new(io::stderr())) };
    let file: File = if o.logappend {
        OpenOptions::new().create(true).append(true).open(path)?
    } else {
        File::create(path)?
    };
    Ok(Box::new(file))
}

/// Node configuration of `teseladb start`.
pub fn node_config(o: &StartOpts) -> NodeConfig {
    NodeConfig {
        cores: o.cores,
        listen_peer: o.peer_listen,
        advertise: None,
        admin_addr: o.admin_listen.to_string(),
        admin_listen: Some(o.admin_listen),
        native_addr: format!("{}:{}", o.bind_ip, o.port),
        http: http_config(o),
        http_advertise: http_advertise(&o.http_bind, o.http_port),
        join: o.join.clone(),
        client: Some(client_config(o)),
        data_dir: Some(o.dbpath.clone()),
        tags: o.tags.clone(),
        memory_limit: o.memory_limit,
        affinity: match &o.cpu_list {
            Some(l) => affinity::Affinity::List(l.clone()),
            None => affinity::Affinity::Allowed,
        },
        // The block cache takes its whole part of the node's memory (012 FR-013).
        lsm: teseladb_lsm::LsmConfig {
            cache_bytes: u64::MAX,
            ..teseladb_lsm::LsmConfig::default()
        },
        ..NodeConfig::default()
    }
}

/// The HTTP port of `teseladb start` (019 FR-002): each address of `--http-bind` with
/// `--http-port`, without TLS until [`secure`] gives it the clients' port's; `None` with port 0.
pub fn http_config(o: &StartOpts) -> Option<crate::http_listener::HttpPortConfig> {
    use std::net::ToSocketAddrs;
    if o.http_port == 0 {
        return None;
    }
    let addrs = o
        .http_bind
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .flat_map(|h| {
            (h, o.http_port).to_socket_addrs().map(Iterator::collect).unwrap_or_else(|_| Vec::new())
        })
        .collect::<Vec<_>>();
    Some(crate::http_listener::HttpPortConfig {
        addrs,
        tls: None,
        settings: teseladb_http::server::Settings::default(),
    })
}

/// The `host:port` clients reach the HTTP port by (019 FR-027): the first name or address of
/// `bind` (`--http-bind`) that is not a wildcard, with `port`. `None` when every one is a
/// wildcard (`0.0.0.0`, `::`): the node cannot know the name it is reached by.
#[must_use]
pub fn http_advertise(bind: &str, port: u16) -> Option<String> {
    let host = bind.split(',').map(str::trim).find(|h| {
        let ip = h.trim_start_matches('[').trim_end_matches(']');
        !h.is_empty() && !ip.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_unspecified())
    })?;
    let bracketed = host.contains(':') && !host.starts_with('[');
    Some(if bracketed { format!("[{host}]:{port}") } else { format!("{host}:{port}") })
}

/// The clients' port of `teseladb start`: each address of `--bind_ip` (comma separated, names
/// resolved) with `--port`. A name that does not resolve is left out; with none left the
/// listener fails to start and says so.
pub fn client_config(o: &StartOpts) -> ClientConfig {
    use std::net::ToSocketAddrs;
    let addrs = o
        .bind_ip
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .flat_map(|h| {
            (h, o.port).to_socket_addrs().map(Iterator::collect).unwrap_or_else(|_| Vec::new())
        })
        .collect::<Vec<_>>();
    let process = ProcessConfig {
        argv: o.argv.clone(),
        parsed: parsed_options(o).to_bytes().unwrap_or_default(),
        dbpath: Some(o.dbpath.clone()),
    };
    ClientConfig {
        max_connections: o.max_conns,
        compressors: o.compressors.clone(),
        test_commands: o.test_commands,
        redact_client_log_data: o.redact_client_log_data,
        javascript_enabled: !o.no_scripting,
        audit_authorization_success: o.audit.authorization_success.unwrap_or(false),
        ocsp_enabled: !o.tls.ocsp_disabled,
        use_internal_authz_for_x509: o.use_internal_authz_for_x509,
        cluster_auth_mode: cluster_auth::mode_or_undefined(o),
        tls_outgoing: cluster_auth::outgoing_tls(o),
        transaction_lifetime_secs: o.transaction_lifetime_secs,
        max_concurrent_transactions: o.max_concurrent_transactions,
        startup: o.startup_parameters.clone(),
        process,
        mechanisms: o.mechanisms.clone(),
        ..ClientConfig::new(addrs)
    }
}

/// The arguments with the value of `--tlsCertificateKeyFilePassword` hidden, as `refnode` shows
/// them in `getCmdLineOpts`.
pub fn redact_password(args: &[String]) -> Vec<String> {
    let flags = [cli::TLS_CERTIFICATE_KEY_FILE_PASSWORD, cli::LDAP_QUERY_PASSWORD]
        .map(|f| format!("--{f}"));
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for a in args {
        let inline = flags.iter().find(|f| a.starts_with(&format!("{f}=")));
        if hide_next {
            out.push(cli::REDACTED_PASSWORD.to_owned());
            hide_next = false;
        } else if flags.contains(a) {
            out.push(a.clone());
            hide_next = true;
        } else if let Some(flag) = inline {
            out.push(format!("{flag}={}", cli::REDACTED_PASSWORD));
        } else {
            out.push(a.clone());
        }
    }
    out
}

/// The program's name in `getCmdLineOpts`' `argv`.
const PROGRAM: &str = "teseladb";

/// The options given to `teseladb start`, under RefDb's configuration names where RefDb has
/// the option (`net.port`, `storage.dbPath`, …) and under `teseladb` the ones of TeselaDB alone.
pub fn parsed_options(o: &StartOpts) -> Document {
    let mut out = Document::new();
    for (name, raw) in &o.given {
        let text = || Value::from(raw.as_str());
        let entries: Vec<(Vec<&str>, Value)> = match name.as_str() {
            "bind_ip" => vec![(vec!["net", "bindIp"], text())],
            "port" => vec![(vec!["net", "port"], Value::Int32(i32::from(o.port)))],
            "maxConns" => vec![(
                vec!["net", "maxIncomingConnections"],
                Value::Int64(i64::try_from(o.max_conns).unwrap_or(i64::MAX)),
            )],
            "networkMessageCompressors" => {
                vec![(vec!["net", "compression", "compressors"], text())]
            }
            "dbpath" => {
                vec![(vec!["storage", "dbPath"], Value::from(o.dbpath.display().to_string()))]
            }
            "logpath" => vec![
                (vec!["systemLog", "destination"], Value::from("file")),
                (vec!["systemLog", "path"], text()),
            ],
            "logappend" => vec![(vec!["systemLog", "logAppend"], Value::Boolean(true))],
            "quiet" => vec![(vec!["systemLog", "quiet"], Value::Boolean(true))],
            "verbose" => vec![(vec!["systemLog", "verbosity"], Value::Int32(1))],
            "config" => vec![(vec!["config"], text())],
            cli::TLS_MODE => vec![(vec!["net", "tls", "mode"], text())],
            cli::TLS_CERTIFICATE_KEY_FILE => {
                vec![(vec!["net", "tls", "certificateKeyFile"], text())]
            }
            cli::TLS_CERTIFICATE_KEY_FILE_PASSWORD => {
                vec![(
                    vec!["net", "tls", "certificateKeyFilePassword"],
                    Value::from(cli::REDACTED_PASSWORD),
                )]
            }
            cli::TLS_CA_FILE => vec![(vec!["net", "tls", "CAFile"], text())],
            cli::ENABLE_ENCRYPTION => {
                vec![(vec!["security", "enableEncryption"], Value::Boolean(true))]
            }
            cli::ENCRYPTION_KEY_FILE => vec![(vec!["security", "encryptionKeyFile"], text())],
            cli::ENCRYPTION_CIPHER_MODE => {
                vec![(vec!["security", "encryptionCipherMode"], text())]
            }
            cli::TLS_CRL_FILE => vec![(vec!["net", "tls", "CRLFile"], text())],
            name if name.starts_with(KMIP_PREFIX) => {
                vec![(vec!["security", "kmip", kmip_key(name)], kmip_value(name, raw))]
            }
            cli::LDAP_SERVERS => vec![(vec!["security", "ldap", "servers"], text())],
            cli::LDAP_TRANSPORT_SECURITY => {
                vec![(vec!["security", "ldap", "transportSecurity"], text())]
            }
            cli::LDAP_BIND_METHOD => vec![(vec!["security", "ldap", "bind", "method"], text())],
            cli::LDAP_QUERY_USER => vec![(vec!["security", "ldap", "bind", "queryUser"], text())],
            cli::LDAP_QUERY_PASSWORD => vec![(
                vec!["security", "ldap", "bind", "queryPassword"],
                Value::from(cli::REDACTED_PASSWORD),
            )],
            cli::LDAP_USER_TO_DN_MAPPING => {
                vec![(vec!["security", "ldap", "userToDNMapping"], text())]
            }
            cli::LDAP_TIMEOUT_MS => vec![(vec!["security", "ldap", "timeoutMS"], text())],
            cli::AUDIT_DESTINATION => vec![(vec!["auditLog", "destination"], text())],
            cli::AUDIT_FORMAT => vec![(vec!["auditLog", "format"], text())],
            cli::AUDIT_PATH => vec![(vec!["auditLog", "path"], text())],
            cli::AUDIT_FILTER => vec![(vec!["auditLog", "filter"], text())],
            cli::AUDIT_RUNTIME_CONFIGURATION => {
                vec![(vec!["auditLog", "runtimeConfiguration"], Value::Boolean(raw == "true"))]
            }
            cli::AUDIT_SCHEMA => vec![(vec!["auditLog", "schema"], text())],
            cli::AUDIT_LOCAL_KEY_FILE => vec![(vec!["auditLog", "localAuditKeyFile"], text())],
            cli::AUDIT_COMPRESSION_MODE => vec![(vec!["auditLog", "compressionMode"], text())],
            cli::AUDIT_ENCRYPTION_KEY_UID => {
                vec![(vec!["auditLog", "auditEncryptionKeyIdentifier"], text())]
            }
            cli::TLS_ALLOW_WITHOUT_CERTIFICATES => {
                vec![(
                    vec!["net", "tls", "allowConnectionsWithoutCertificates"],
                    Value::Boolean(true),
                )]
            }
            cli::TLS_FIPS_MODE => vec![(vec!["net", "tls", "FIPSMode"], Value::Boolean(true))],
            cli::REDACT_CLIENT_LOG_DATA => {
                vec![(vec!["security", "redactClientLogData"], Value::Boolean(true))]
            }
            cli::NO_SCRIPTING => {
                vec![(vec!["security", "javascriptEnabled"], Value::Boolean(false))]
            }
            cli::JAVASCRIPT_ENABLED => {
                vec![(vec!["security", "javascriptEnabled"], Value::Boolean(raw == "true"))]
            }
            "setParameter" => {
                let (k, v) = raw.split_once('=').unwrap_or((raw.as_str(), ""));
                vec![(vec!["setParameter", k], Value::from(v))]
            }
            // A flag of TeselaDB alone (`--insecure`) is `true`; an option, its text.
            other if raw.is_empty() => vec![(vec!["teseladb", other], Value::Boolean(true))],
            other => vec![(vec!["teseladb", other], text())],
        };
        for (path, v) in entries {
            out = with_path(out, &path, v);
        }
    }
    out
}

/// `d` with `v` at `path`, creating the documents on the way.
fn with_path(mut d: Document, path: &[&str], v: Value) -> Document {
    match path {
        [] => d,
        [last] => {
            d.insert(*last, v);
            d
        }
        [first, rest @ ..] => {
            let inner = match d.get(first) {
                Some(Value::Document(inner)) => inner.clone(),
                _ => Document::new(),
            };
            d.insert(*first, with_path(inner, rest, v));
            d
        }
    }
}

/// The monotonic clock in milliseconds since the process started: the clock of a JavaScript
/// call's deadline (016 FR-007).
fn js_clock_ms() -> u64 {
    u64::try_from(backend::process_start().elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The wait of `sleep(ms)` in a JavaScript call: its thread sleeps, as on a `refnode`.
fn js_sleep(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// What the JavaScript calls of a server read from the process: the clock of their deadline and
/// the wait of `sleep` (016 FR-007), for `teseladb start` and `teseladb dev` alike.
pub(crate) fn install_js_host() {
    teseladb_wire::commands::js::set_clock(js_clock_ms);
    teseladb_wire::commands::js::set_sleeper(js_sleep);
}

/// The user id of `root`.
const ROOT_UID: u32 = 0;
/// RefDb's startup warning for a process run as `root`, and its identifier.
pub const RUNNING_AS_ROOT: &str =
    "You are running this process as the root user, which is not recommended";
const RUNNING_AS_ROOT_ID: i32 = 22138;

/// `teseladb start`: runs until `stop()` says so.
pub fn start(
    o: &StartOpts,
    out: &mut dyn Write,
    err: &mut dyn Write,
    stop: &dyn Fn() -> bool,
) -> i32 {
    install_js_host();
    let log = match log_output(o) {
        Ok(w) => std::sync::Arc::new(Logger::new(o.log_level, o.log_format, w)),
        Err(e) => {
            let _ = writeln!(err, "teseladb: --logpath {}: {e}", display(o.logpath.as_deref()));
            return 1;
        }
    };
    if o.insecure {
        log.warn("insecure mode: no TLS nor authentication", &[]);
    }
    // As `refnode` and `refrouter` 9.0.4 warn at start (measured on 03-11-2026, `getLog`
    // `startupWarnings`).
    if teseladb_sys::effective_uid() == ROOT_UID {
        log.warn(RUNNING_AS_ROOT, &[("id", &RUNNING_AS_ROOT_ID)]);
    }
    // `--keyFile` and `--clusterAuthMode`, checked as `refnode` checks them (013 FR-034).
    if let Err(e) = cluster_auth::mode(o) {
        let _ = writeln!(err, "teseladb: {e}");
        return 1;
    }
    for (option, effect) in cluster_auth::relaxations(o) {
        log.warn("not secure by default", &[("option", &option), ("effect", &effect)]);
    }
    let lock = match lock_dir(&o.dbpath) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(err, "teseladb: {}: {e}", o.dbpath.display());
            return 1;
        }
    };
    // The master key from the KMIP server (013 FR-028); a rotation stops here, as in `refnode`.
    let kmip = match o.at_rest.kmip.as_ref() {
        None => None,
        Some(k) => match kmip::master_key(k, &o.dbpath, &mut |b| teseladb_sys::fill_random(b)) {
            Ok(m) => {
                for line in &m.log {
                    log.info(line, &[]);
                }
                if m.rotated {
                    return 0;
                }
                Some(m)
            }
            Err(e) => {
                let _ = writeln!(err, "teseladb: {e}");
                return 1;
            }
        },
    };
    let kmip_key = kmip.as_ref().map(|m| &m.key);
    let mut cfg = node_config(o);
    let steps = log.clone();
    cfg.startup_log = node::StartupLog::new(move |msg, fields| steps.info(msg, fields));
    if o.insecure {
        match at_rest::from_opts(&o.at_rest, kmip_key, None) {
            Ok(e) => encrypt(&mut cfg, e),
            Err(e) => {
                let _ = writeln!(err, "teseladb: {e}");
                return 1;
            }
        }
    } else {
        match secure(o, &mut cfg, kmip_key, &log) {
            Ok(summary) => log.info(&summary, &[]),
            Err(e) => {
                let _ = writeln!(err, "teseladb: {e}");
                return 1;
            }
        }
    }
    let audit = match audit_log::AuditLog::open(&o.audit) {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(err, "teseladb: {e}");
            return 1;
        }
    };
    let ldap = match ldap_client::LdapServer::from_opts(&o.ldap) {
        Ok(l) => l.map(|l| ldap_client::SharedLdap(std::sync::Arc::new(l))),
        Err(e) => {
            let _ = writeln!(err, "teseladb: {e}");
            return 1;
        }
    };
    // The STS only when REFDB-AWS is on: the system's authorities are read then.
    let sts = if o.mechanisms.contains(&teseladb_auth::sasl::SaslMechanism::Aws) {
        match sts_client::Sts::from_url(o.aws_sts_url.as_deref(), o.tls.fips_mode) {
            Ok(s) => Some(std::sync::Arc::new(s)),
            Err(e) => {
                let _ = writeln!(err, "teseladb: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let kerberos = if o.mechanisms.contains(&teseladb_auth::sasl::SaslMechanism::Gssapi) {
        let name = std::env::var("KRB5_KTNAME").unwrap_or_else(|_| DEFAULT_KEYTAB.to_owned());
        match keytab(&name) {
            Ok(k) => Some(std::sync::Arc::new(k)),
            Err(e) => {
                let _ = writeln!(err, "teseladb: gssapi could not acquire server credential: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let oidc = if o.mechanisms.contains(&teseladb_auth::sasl::SaslMechanism::Oidc) {
        let json = o.oidc_providers.as_deref().unwrap_or("[]");
        match oidc_client::Oidc::from_json(json, o.tls.fips_mode) {
            Ok(p) => Some(std::sync::Arc::new(p)),
            Err(e) => {
                let _ = writeln!(err, "teseladb: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    if let Some(client) = cfg.client.as_mut() {
        client.audit.clone_from(&audit);
        client.ldap = ldap;
        client.sts = sts;
        client.oidc = oidc;
        client.kerberos = kerberos;
    }
    let data = o.dbpath.clone();
    // The disk executor and O_DIRECT (012 FR-003, FR-007), decided once and logged.
    let choice = match teseladb_io::node_disk::choose(o.io_executor, o.direct_io, &data) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "teseladb: {e}");
            return 1;
        }
    };
    log.info(&choice.to_string(), &[]);
    cfg.direct_io = choice.direct;
    let node = match Node::start(cfg, || NodeDisk::open(&data, &choice), Some(lock)) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(err, "teseladb: {e}");
            return 1;
        }
    };
    if let Some(a) = &audit {
        let options = parsed_options(o);
        a.system(AuditType::Startup, teseladb_auth::audit_param::startup(&options));
    }
    // A KMIP key that stops being active stops the node (`--kmipKeyStatePollingSeconds`).
    let polling = kmip.as_ref().zip(o.at_rest.kmip.as_ref()).and_then(|(m, k)| {
        let every = k.polling.filter(|_| k.activate_keys)?;
        Some((m.id.clone(), k.clone(), every))
    });
    let lost = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller =
        polling.clone().map(|(id, k, every)| kmip::poll(k, id, every, done.clone(), lost.clone()));
    let lost_or_stop = || stop() || lost.load(std::sync::atomic::Ordering::Relaxed);
    let mut code = serve(node, o.log_format, &log, out, &lost_or_stop, READY_TIMEOUT);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(p) = poller {
        let _ = p.join();
    }
    if let Some((id, _, _)) = polling.filter(|_| lost.load(std::sync::atomic::Ordering::Relaxed)) {
        log.error(&kmip::lost_message(&id), &[]);
        code = 1;
    }
    if let Some(a) = &audit {
        a.system(AuditType::Shutdown, teseladb_auth::audit_param::shutdown());
    }
    code
}

/// The prefix of the KMIP options, `--kmip<Name>` for `security.kmip.<name>`.
const KMIP_PREFIX: &str = "kmip";

/// `security.kmip.<name>` of `--kmip<Name>`, as `refnode` names them.
fn kmip_key(option: &str) -> &str {
    match option {
        cli::KMIP_SERVER_NAME => "serverName",
        cli::KMIP_PORT => "port",
        cli::KMIP_SERVER_CA_FILE => "serverCAFile",
        cli::KMIP_CLIENT_CERTIFICATE_FILE => "clientCertificateFile",
        cli::KMIP_CLIENT_CERTIFICATE_PASSWORD => "clientCertificatePassword",
        cli::KMIP_KEY_IDENTIFIER => "keyIdentifier",
        cli::KMIP_ROTATE_MASTER_KEY => "rotateMasterKey",
        cli::KMIP_CONNECT_RETRIES => "connectRetries",
        cli::KMIP_CONNECT_TIMEOUT_MS => "connectTimeoutMS",
        cli::KMIP_KEY_STATE_POLLING_SECONDS => "keyStatePollingSeconds",
        cli::KMIP_ACTIVATE_KEYS => "activateKeys",
        _ => "useLegacyProtocol",
    }
}

/// The value `getCmdLineOpts` shows for KMIP option `option` given as `raw`: numbers as
/// numbers, booleans as booleans, the password hidden.
fn kmip_value(option: &str, raw: &str) -> Value {
    match option {
        cli::KMIP_PORT
        | cli::KMIP_CONNECT_RETRIES
        | cli::KMIP_CONNECT_TIMEOUT_MS
        | cli::KMIP_KEY_STATE_POLLING_SECONDS => {
            raw.parse::<i32>().map_or_else(|_| Value::from(raw), Value::Int32)
        }
        cli::KMIP_ROTATE_MASTER_KEY | cli::KMIP_ACTIVATE_KEYS | cli::KMIP_USE_LEGACY_PROTOCOL => {
            Value::Boolean(!matches!(raw, "0" | "false" | "no"))
        }
        cli::KMIP_CLIENT_CERTIFICATE_PASSWORD => Value::from(cli::REDACTED_PASSWORD),
        _ => Value::from(raw),
    }
}

/// The secure start (013 FR-004, FR-007): the PKI of the data directory, created on the first
/// start; TLS on the clients' port with the node certificate or the `--tls*` options; clients that
/// authenticate; the `root` user of the credentials file, which core 0 creates while the
/// cluster has no user; and the master key that seals the `$clusterTime` signing keys. Returns
/// the line to log.
fn secure(
    o: &StartOpts,
    cfg: &mut NodeConfig,
    kmip_key: Option<&[u8; at_rest::KEY_LEN]>,
    log: &Logger,
) -> Result<String, String> {
    let mut random = |b: &mut [u8]| teseladb_sys::fill_random(b);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let params = pki::PkiParams { now, node_names: pki::node_names(&o.bind_ip) };
    if let Some(token) = o.enroll_token.as_ref().filter(|_| pki::absent(&o.dbpath)) {
        let at = o.join.first().map_or("", String::as_str);
        log.info("enrolling with the cluster", &[("join", &at)]);
        enroll_node(o, &token.0, &mut random)?;
        log.info("enrolled", &[]);
    }
    let pki = pki::load_or_create(&o.dbpath, &params, &mut random).map_err(|e| e.to_string())?;
    let tls = tls_port::build(&o.tls, &pki)?;
    // A certificate that names an OCSP responder gets its answer stapled (013 FR-031).
    if !o.tls.ocsp_disabled {
        // The issuer: the next certificate of the chain, an authority of --tlsCAFile or the
        // cluster's CA, whichever signed the leaf.
        let config = tls.config();
        let chain = config.as_ref().map(|c| c.identity.chain.clone()).unwrap_or_default();
        let anchors = config.as_ref().map(|c| c.trust.anchors.clone()).unwrap_or_default();
        let candidates = chain.iter().skip(1).chain(anchors.iter()).chain(std::iter::once(&pki.ca));
        let issuer = chain.first().and_then(|leaf| {
            candidates.clone().find(|c| leaf.verify_signed_by(&c.public_key).is_ok()).cloned()
        });
        if let Some(issuer) = issuer {
            ocsp_staple::spawn(tls.clone(), issuer, teseladb_sys::signal::stop_requested);
        }
    }
    let peer_tls = pki::peer_tls(&pki, o.tls.fips_mode);
    let admin_tls = tls_port::admin(&o.tls, &pki)?;
    let certificates = certs::Certificates::new(
        o.dbpath.clone(),
        o.tls.clone(),
        tls.clone(),
        peer_tls.clone(),
        params.node_names.clone(),
    )
    .with_admin(admin_tls.clone());
    cfg.admin_tls = Some(admin_http::AdminTls {
        port: admin_tls,
        ca: pki.ca.clone(),
        node: pki.node_chain.first().map(|c| c.subject.clone()).unwrap_or_default(),
    });
    // The HTTP port speaks TLS as the clients' port does (019 FR-002).
    if let Some(h) = cfg.http.as_mut() {
        h.tls = Some(tls.clone());
    }
    cfg.root_user = pki::root_user(&o.dbpath, &mut random).map_err(|e| e.to_string())?;
    cfg.client = Some(ClientConfig {
        tls,
        auth: true,
        certificates: Some(std::sync::Arc::new(certificates)),
        ..client_config(o)
    });
    cfg.clustertime_sealer = teseladb_auth::clustertime::KeySealer::new(&pki.master_key.0);
    encrypt(cfg, at_rest::from_opts(&o.at_rest, kmip_key, Some(&pki.master_key.0))?);
    cfg.peer_tls = Some(peer_tls);
    cfg.enroller = Some(enroll::Enroller::new(&pki));
    Ok(pki.summary())
}

/// The text of an error, for the messages of the start and of the clients of the node's
/// services (LDAP, HTTPS, TLS).
pub(crate) fn error_text<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// The text of an error that only has its `Debug` form (the TLS and HTTP state machines).
pub(crate) fn error_debug<E: std::fmt::Debug>(e: E) -> String {
    format!("{e:?}")
}

/// The keytab MIT Kerberos and `refnode` read without `KRB5_KTNAME`.
const DEFAULT_KEYTAB: &str = "/etc/krb5.keytab";

/// The keytab of GSSAPI (013 FR-025) named `name` (`KRB5_KTNAME`, with or without `FILE:`, else
/// [`DEFAULT_KEYTAB`]), as MIT Kerberos and `refnode` read it; at least one AES key.
fn keytab(name: &str) -> Result<Vec<teseladb_auth::kerberos::ServiceKey>, String> {
    let path = name.strip_prefix("FILE:").unwrap_or(name);
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let keys = teseladb_auth::kerberos::parse_keytab(&data).map_err(|e| format!("{path}: {e}"))?;
    if keys.is_empty() {
        return Err(format!("{path}: no AES key"));
    }
    Ok(keys)
}

/// The storage of every core sealed with `encryption` (013 FR-015).
fn encrypt(cfg: &mut NodeConfig, encryption: Option<teseladb_crypto::at_rest::Encryption>) {
    cfg.lsm.encryption = encryption.clone();
    cfg.storage.encryption = encryption;
}

/// Enrolls this node (013 FR-005): steps 1 to 3 of `pki-and-enrollment.md` against the first
/// address of `--join`, then the PKI goes to the data directory. A failure names its step and
/// leaves the directory without a PKI.
fn enroll_node(
    o: &StartOpts,
    text: &str,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<(), String> {
    use teseladb_tls::{ClientConfig, Trust};
    let token = enroll::Token::parse(text).map_err(|e| e.to_string())?;
    let addr = o.join.first().ok_or("--enroll-token needs --join <ip:port>")?;
    let mut seed = [0u8; enroll::KEY_RANDOM_LEN];
    random(&mut seed).map_err(|e| e.to_string())?;
    let node_key = teseladb_x509::PrivateKey::generate_ec(teseladb_crypto::ec::NamedCurve::P256, &seed)
        .map_err(|_| enroll::EnrollError::Key.to_string())?;
    seed.fill(0);
    let names: Vec<String> =
        o.bind_ip.split(',').map(str::trim).filter(|h| !h.is_empty()).map(str::to_owned).collect();
    let req = teseladb_io::peer::EnrollRequest {
        token_id: token.id,
        secret: token.secret,
        name: names.join(","),
        public_key: node_key.public_key().to_spki(),
    };
    let trust = Trust { pinned: Some(token.fingerprint), ..Trust::anchors(Vec::new()) };
    let client = ClientConfig::new(trust);
    let client = std::sync::Arc::new(if o.tls.fips_mode { client.fips() } else { client });
    let accepted = teseladb_io::handshake::request_enroll(addr, &req, &client)
        .map_err(|e| format!("enrollment, steps 1-2 (TLS with {addr} and the request): {e}"))?
        .map_err(|e| format!("enrollment, step 3 ({addr} refused it): {e}"))?;
    let bad = |what: &str| format!("enrollment, step 3: the answer {what}");
    let ca = teseladb_x509::Certificate::from_der(&accepted.ca).map_err(|_| bad("has no CA"))?;
    if enroll::fingerprint(&ca) != token.fingerprint {
        return Err(bad("names another CA"));
    }
    let cert = teseladb_x509::Certificate::from_der(&accepted.certificate)
        .map_err(|_| bad("has no certificate"))?;
    if cert.public_key != node_key.public_key() || cert.verify_signed_by(&ca.public_key).is_err() {
        return Err(bad("has a certificate that is not this node's"));
    }
    let (master, ca_key) =
        enroll::open_sealed(&node_key, &accepted.sealed).map_err(|e| e.to_string())?;
    pki::write_enrolled(&o.dbpath, &master, &ca, &ca_key, &node_key, &cert, random)
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn display(p: Option<&Path>) -> String {
    p.map_or_else(String::new, |p| p.display().to_string())
}

/// Waits for the node to be ready, prints the ready line and runs until `stop()`; gives up if
/// the cluster is not ready within `ready_timeout`.
pub fn serve<D: teseladb_raftlog::Disk + Send + 'static>(
    node: Node<D>,
    format: Format,
    log: &Logger,
    out: &mut dyn Write,
    stop: &dyn Fn() -> bool,
    ready_timeout: Duration,
) -> i32 {
    log.info("waiting for the cluster to be ready", &[]);
    let report = |v: Option<&logic::Group0View>| {
        log.info("not ready yet", &[("missing", &node::not_ready(v))]);
    };
    if !node.wait_ready_reporting(ready_timeout, node::READY_REPORT, report) {
        log.error("the cluster did not become ready", &[]);
        node.stop();
        return 1;
    }
    let id = node.node_id().0;
    let peer = node.peer_addr().to_string();
    let _ = out.write_all(ready_line(format, id, &node.cluster_id(), &peer).as_bytes());
    let _ = out.flush();
    log.info("node ready", &[("node_id", &id), ("cores", &node.cores())]);
    // Where the cores run (012 FR-006): every core is pinned or none is (`affinity::core_cpus`).
    let cpus: Vec<String> = node.cpus().iter().flatten().map(ToString::to_string).collect();
    if !cpus.is_empty() {
        log.info("cores' CPUs", &[("cpus", &cpus.join(","))]);
    }
    log::mark_started();
    for a in node.client_addrs() {
        log.info("clients' port", &[("addr", &a.to_string())]);
    }
    if let Some(a) = node.admin_addr() {
        log.info("administration port", &[("addr", &a.to_string())]);
    }
    // Whether this node votes in Group 0, and whether it leads the data range, are logged when
    // they change, and so is its role in the data range with its term, which shows how an
    // election went (005 SC-004).
    let (mut voter, mut leading) = (false, false);
    let mut role = None;
    while !stop() && !node.shutdown_requested() {
        if let Some(v) = node.view(STOP_POLL) {
            let now_voter = v.voters.contains(&node.node_id());
            if now_voter != voter {
                voter = now_voter;
                log.info("Group 0 membership", &[("voter", &voter), ("leader", &v.leader.0)]);
            }
        }
        if let Some(s) = node.data_status(STOP_POLL) {
            if role != Some((s.role, s.term)) {
                role = Some((s.role, s.term));
                let name = s.role.map_or_else(|| "none".to_string(), |r| format!("{r:?}"));
                log.info("data range role", &[("role", &name), ("term", &s.term)]);
            }
            let now_leading = s.role == Some(teseladb_raft::Role::Leader);
            if now_leading != leading {
                leading = now_leading;
                log.info("data range leadership", &[("leader", &leading), ("term", &s.term)]);
            }
        }
        crate::log::drain(log);
        std::thread::sleep(STOP_POLL);
    }
    crate::log::drain(log);
    log.info("stopping", &[]);
    node.stop();
    0
}

#[cfg(test)]
mod tests {
    use super::keytab;

    /// A keytab of MIT's format with one entry of `etype`.
    fn keytab_bytes(etype: u16) -> Vec<u8> {
        let mut e = 1u16.to_be_bytes().to_vec();
        for t in ["TESELA.TEST", "refdb"] {
            e.extend_from_slice(&u16::try_from(t.len()).unwrap().to_be_bytes());
            e.extend_from_slice(t.as_bytes());
        }
        e.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 2]);
        e.extend_from_slice(&etype.to_be_bytes());
        e.extend_from_slice(&16u16.to_be_bytes());
        e.extend_from_slice(&[7; 16]);
        let mut out = vec![0x05, 0x02];
        out.extend_from_slice(&i32::try_from(e.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&e);
        out
    }

    #[test]
    fn the_keytab_is_read_with_or_without_file_and_needs_an_aes_key() {
        let dir = std::env::temp_dir().join(format!("teseladb-keytab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.keytab");
        let p = path.to_str().unwrap();
        std::fs::write(&path, keytab_bytes(17)).unwrap();
        assert_eq!(keytab(p).unwrap()[0].principal, "refdb@TESELA.TEST");
        assert_eq!(keytab(&format!("FILE:{p}")).unwrap().len(), 1);
        std::fs::write(&path, keytab_bytes(23)).unwrap();
        assert_eq!(keytab(p).unwrap_err(), format!("{p}: no AES key"));
        std::fs::write(&path, [0x05, 0x01]).unwrap();
        assert!(keytab(p).unwrap_err().starts_with(&format!("{p}: ")));
        std::fs::remove_dir_all(&dir).unwrap();
        let missing = dir.join("missing.keytab");
        assert!(keytab(missing.to_str().unwrap()).unwrap_err().contains("missing.keytab: "));
    }
}
