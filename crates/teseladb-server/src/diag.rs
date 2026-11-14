//! What the node reports of itself to the diagnostic commands (006 FR-023): the counters of its
//! client port, its command line and what the operating system says of the host and the process.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use teseladb_bson::{Document, RawDoc};
use teseladb_wire::commands::diagnostics::{
    ConnectionCounts, HostFacts, NetworkCounts, ProcessFacts, ResourceUse, ServerCertificate,
};

use crate::log;

/// What `getCmdLineOpts` shows and where the data lives, as the command line gave them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessConfig {
    /// The command line.
    pub argv: Vec<String>,
    /// The options in the reference server's configuration names, as a BSON document.
    pub parsed: Vec<u8>,
    /// The data directory (`dbStats`' file system sizes).
    pub dbpath: Option<PathBuf>,
}

/// Counters of the client port, updated by its threads.
#[derive(Debug, Default)]
pub struct PortStats {
    /// Connections accepted since the start.
    pub total_created: AtomicI64,
    /// Connections refused because the port was full.
    pub rejected: AtomicI64,
    /// Connections running a command now.
    pub active: AtomicI64,
    /// Bytes of the requests.
    pub bytes_in: AtomicI64,
    /// Bytes of the replies.
    pub bytes_out: AtomicI64,
    /// Bytes read from the sockets.
    pub physical_in: AtomicI64,
    /// Bytes written to the sockets.
    pub physical_out: AtomicI64,
    /// Requests.
    pub requests: AtomicI64,
    /// TLS handshakes completed (all TLS 1.3, the only version the port speaks).
    pub tls_handshakes: AtomicI64,
    /// Milliseconds they took, summed.
    pub tls_handshake_ms: AtomicI64,
    /// Microseconds they took, summed.
    pub tls_handshake_micros: AtomicI64,
}

impl PortStats {
    /// Adds `n` to `counter`.
    pub fn add(counter: &AtomicI64, n: usize) {
        counter.fetch_add(i64::try_from(n).unwrap_or(i64::MAX), Ordering::Relaxed);
    }
}

/// The node's facts for the diagnostic commands.
#[derive(Debug)]
pub struct Diag {
    /// The port's counters.
    pub stats: PortStats,
    open: Arc<AtomicUsize>,
    max_connections: usize,
    port: u16,
    /// `host:port`, read once at the start (the reference server caches its host name the same
    /// way).
    host_port: String,
    start_ms: u64,
    process: ProcessConfig,
    certificates: Option<Arc<crate::certs::Certificates>>,
    tls: Option<crate::tls_port::TlsPort>,
    ldap: Option<crate::ldap_client::SharedLdap>,
    sts: Option<Arc<crate::sts_client::Sts>>,
    oidc: Option<Arc<crate::oidc_client::Oidc>>,
    kerberos: Option<Arc<Vec<teseladb_auth::kerberos::ServiceKey>>>,
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Diag {
    /// The facts of a port on `port` whose open connections `open` counts.
    pub fn new(
        process: ProcessConfig,
        port: u16,
        open: Arc<AtomicUsize>,
        max_connections: usize,
    ) -> Diag {
        Diag {
            stats: PortStats::default(),
            open,
            max_connections,
            port,
            host_port: format!("{}:{port}", teseladb_sys::host::hostname().unwrap_or_default()),
            start_ms: wall_ms(),
            process,
            certificates: None,
            tls: None,
            ldap: None,
            sts: None,
            oidc: None,
            kerberos: None,
        }
    }

    /// The facts with the node's LDAP server (013 FR-025).
    pub fn with_ldap(self, ldap: Option<crate::ldap_client::SharedLdap>) -> Diag {
        Diag { ldap, ..self }
    }

    /// The facts with the node's STS (013 FR-025).
    pub fn with_sts(self, sts: Option<Arc<crate::sts_client::Sts>>) -> Diag {
        Diag { sts, ..self }
    }

    /// The node's STS, if AWS IAM authentication is on.
    pub fn sts(&self) -> Option<&crate::sts_client::Sts> {
        self.sts.as_deref()
    }

    /// The facts with the node's identity providers (013 FR-025).
    pub fn with_oidc(self, oidc: Option<Arc<crate::oidc_client::Oidc>>) -> Diag {
        Diag { oidc, ..self }
    }

    /// The node's identity providers, if OIDC authentication is on.
    pub fn oidc(&self) -> Option<&crate::oidc_client::Oidc> {
        self.oidc.as_deref()
    }

    /// The facts with the node's keytab (013 FR-025).
    pub fn with_kerberos(
        self,
        kerberos: Option<Arc<Vec<teseladb_auth::kerberos::ServiceKey>>>,
    ) -> Diag {
        Diag { kerberos, ..self }
    }

    /// The keys of the node's keytab, if GSSAPI is on.
    pub fn kerberos(&self) -> &[teseladb_auth::kerberos::ServiceKey] {
        self.kerberos.as_deref().map_or(&[], Vec::as_slice)
    }

    /// The node's LDAP server, if it has one.
    pub fn ldap(&self) -> Option<&crate::ldap_client::LdapServer> {
        self.ldap.as_ref().map(|l| &*l.0)
    }

    /// The facts with what `rotateCertificates` loads again.
    pub fn with_certificates(self, certificates: Option<Arc<crate::certs::Certificates>>) -> Diag {
        Diag { certificates, ..self }
    }

    /// The facts with the client port's TLS, whose certificate `serverStatus` names.
    pub fn with_tls(self, tls: crate::tls_port::TlsPort) -> Diag {
        Diag { tls: Some(tls), ..self }
    }

    /// The certificate the client port presents now (after a rotation, the new one); `None`
    /// without TLS.
    fn server_certificate(&self) -> Option<ServerCertificate> {
        let config = self.tls.as_ref()?.config()?;
        let cert = config.identity.chain.first()?;
        Some(ServerCertificate {
            subject: cert.subject.to_rfc4514(),
            expires_ms: cert.not_after.saturating_mul(1000),
        })
    }

    /// What `rotateCertificates` loads again; `None` on a node without a PKI.
    pub fn certificates(&self) -> Option<&crate::certs::Certificates> {
        self.certificates.as_deref()
    }

    /// The host as the operating system describes it; a fact it does not give stays empty.
    fn host() -> HostFacts {
        use teseladb_sys::host;
        let kernel = host::kernel().unwrap_or_default();
        let (os_name, os_version) = host::distribution();
        let page = host::page_size().unwrap_or(0);
        let pages = host::physical_pages().unwrap_or(0);
        let cores = host::online_cpus().map_or(0, |n| i32::try_from(n).unwrap_or(i32::MAX));
        HostFacts {
            hostname: host::hostname().unwrap_or_default(),
            cpu_arch: std::env::consts::ARCH.to_owned(),
            num_cores: cores,
            num_cores_available: cores,
            mem_size_mb: int(page.saturating_mul(pages) >> 20),
            page_size: int(page),
            num_pages: int(pages),
            max_open_files: host::max_open_files().map_or(0, int),
            os_type: kernel.sysname.clone(),
            os_name,
            os_version,
            kernel_version: kernel.release,
            version_string: kernel.version,
        }
    }

    fn resources() -> ResourceUse {
        use teseladb_sys::host;
        let u = host::usage().unwrap_or_default();
        let m = host::memory().unwrap_or_default();
        ResourceUse {
            resident_bytes: m.resident,
            virtual_bytes: m.virtual_size,
            user_time_us: u.user_us,
            system_time_us: u.system_us,
            max_resident_kb: u.max_rss_kb,
            input_blocks: u.in_blocks,
            output_blocks: u.out_blocks,
            page_reclaims: u.minor_faults,
            page_faults: u.major_faults,
            voluntary_switches: u.voluntary_switches,
            involuntary_switches: u.involuntary_switches,
            threads: int(m.threads),
            pageins: m.pageins,
        }
    }

    /// The node's `host:port`, as it was at the start.
    pub fn host_port(&self) -> &str {
        &self.host_port
    }

    /// Everything the diagnostic commands report, read now.
    pub fn facts(&self) -> ProcessFacts {
        let s = &self.stats;
        let load = |c: &AtomicI64| c.load(Ordering::Relaxed);
        let current = int(self.open.load(Ordering::Acquire) as u64);
        let (fs_used, fs_total) = self
            .process
            .dbpath
            .as_deref()
            .and_then(|p| teseladb_sys::host::fs_usage(p).ok())
            .unwrap_or((0, 0));
        let (log_total, log_lines) = log::recent();
        let (warn_total, warn_lines) = log::startup_warnings();
        ProcessFacts {
            port: self.port,
            pid: i64::from(std::process::id()),
            start_ms: self.start_ms,
            now_ms: wall_ms(),
            argv: self.process.argv.clone(),
            parsed: RawDoc::from_bytes(&self.process.parsed)
                .map(Document::from)
                .unwrap_or_default(),
            build: crate::VERSION.to_owned(),
            build_target: (std::env::consts::ARCH.to_owned(), std::env::consts::OS.to_owned()),
            host: Diag::host(),
            connections: ConnectionCounts {
                current,
                available: int(self.max_connections as u64).saturating_sub(current),
                total_created: load(&s.total_created),
                rejected: load(&s.rejected),
                active: load(&s.active),
            },
            network: NetworkCounts {
                bytes_in: load(&s.bytes_in),
                bytes_out: load(&s.bytes_out),
                physical_bytes_in: load(&s.physical_in),
                physical_bytes_out: load(&s.physical_out),
                requests: load(&s.requests),
                tls_handshakes: load(&s.tls_handshakes),
                tls_handshake_ms: load(&s.tls_handshake_ms),
                tls_handshake_micros: load(&s.tls_handshake_micros),
            },
            resources: Diag::resources(),
            fs_used,
            fs_total,
            log: teseladb_wire::commands::diagnostics::LogTail {
                total: log_total,
                lines: log_lines,
            },
            startup_warnings: teseladb_wire::commands::diagnostics::LogTail {
                total: warn_total,
                lines: warn_lines,
            },
            server_certificate: self.server_certificate(),
        }
    }
}
