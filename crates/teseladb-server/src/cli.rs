//! Our own command line parser (contract `cli-base.md` of 001): `teseladb <subcommand>
//! [--option value]...`, with `--option=value` too. An error in the command line has exit code 2.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use teseladb_wire::compressed::Compressor;

use crate::client_listener::DEFAULT_MAX_CONNECTIONS;
use crate::config::yaml_args;
use crate::log::{Format, Level};
use crate::tls_port::TlsMode;

/// Compressors of the clients' port without `--networkMessageCompressors` (contract
/// `cli-base.md`).
pub const DEFAULT_COMPRESSORS: [Compressor; 3] =
    [Compressor::Zstd, Compressor::Snappy, Compressor::Zlib];

/// Exit code of an error in the command line.
pub const USAGE_EXIT: i32 = 2;

/// Error in the command line (exit code [`USAGE_EXIT`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError(pub String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

fn err<T>(msg: impl Into<String>) -> Result<T, CliError> {
    Err(CliError(msg.into()))
}

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Starts a node.
    Start(Box<StartOpts>),
    /// Starts a cluster of nodes in this process.
    Dev(DevOpts),
    /// Administers a node through its administration port (007 FR-020).
    Admin(crate::admin::AdminOpts),
    /// Prints the version.
    Version,
    /// Prints the usage.
    Help,
}

/// Options of `teseladb start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartOpts {
    /// Data directory (`storage.dbPath`).
    pub dbpath: PathBuf,
    /// Cores (`None` = the data directory's, or the machine's logical cores for a new one).
    pub cores: Option<u16>,
    /// Address of the clients (`net.bindIp`; 006 opens it).
    pub bind_ip: String,
    /// Port of the clients (`net.port`).
    pub port: u16,
    /// Inter-node address.
    pub peer_listen: SocketAddr,
    /// Administration address (`teseladb.adminListen`; 007).
    pub admin_listen: SocketAddr,
    /// Inter-node addresses of nodes of an existing cluster (comma separated).
    pub join: Vec<String>,
    /// The node's tags (`--tags dc=east,rack=r1`), sorted by name, which reads with a
    /// `readPreference` match (007 FR-013).
    pub tags: Vec<(String, String)>,
    /// Memory the node may use (`--memory-limit`, `teseladb.memoryLimit`; 012 FR-013); `None`:
    /// three quarters of the memory the process has.
    pub memory_limit: Option<u64>,
    /// The enrollment token of a node that joins without a PKI yet (013 FR-005).
    pub enroll_token: Option<Password>,
    /// No TLS and no authentication.
    pub insecure: bool,
    /// `--insecure` on addresses that are not loopback.
    pub insecure_allow_remote: bool,
    /// Line format of the log.
    pub log_format: Format,
    /// Log level (`--quiet` = warnings, `--verbose` = debug).
    pub log_level: Level,
    /// YAML configuration file (FR-022).
    pub config: Option<PathBuf>,
    /// Log file (`None` = standard error).
    pub logpath: Option<PathBuf>,
    /// Append to the log file instead of replacing it.
    pub logappend: bool,
    /// Most open client connections (`--maxConns`, `net.maxIncomingConnections`; 006).
    pub max_conns: usize,
    /// Compressors of the clients' port (`--networkMessageCompressors`; 006).
    pub compressors: Vec<Compressor>,
    /// `--setParameter enableTestCommands=1`: `configureFailPoint` (006 FR-034).
    pub test_commands: bool,
    /// `--setParameter useInternalAuthzForX509=<bool>` (013 FR-033).
    pub use_internal_authz_for_x509: bool,
    /// `--keyFile` (`security.keyFile`, 013 FR-034): the key file, read and checked at start.
    pub key_file: Option<PathBuf>,
    /// `--clusterAuthMode` (`security.clusterAuthMode`, 013 FR-034), as given; checked at start.
    pub cluster_auth_mode: Option<String>,
    /// `--setParameter transactionLifetimeLimitSeconds=<n>` (008 FR-010).
    pub transaction_lifetime_secs: i64,
    /// `--setParameter maxConcurrentMultiDocumentTransactions=<n>` (008 FR-020).
    pub max_concurrent_transactions: i64,
    /// `--redactClientLogData`: the clients' documents go redacted in the log (013 FR-027).
    pub redact_client_log_data: bool,
    /// `--noscripting`: no server-side JavaScript (016 FR-009).
    pub no_scripting: bool,
    /// The options as given, by name, for `getCmdLineOpts` (006).
    pub given: Vec<(String, String)>,
    /// The command line, for `getCmdLineOpts`; [`crate::run`] fills it.
    pub argv: Vec<String>,
    /// TLS of the clients' port (013 FR-007).
    pub tls: TlsOpts,
    /// Encryption at rest (013 FR-015).
    pub at_rest: crate::at_rest::AtRestOpts,
    /// The audit log (013 FR-016).
    pub audit: crate::audit_log::AuditOpts,
    /// The LDAP server of PLAIN logins (013 FR-025).
    pub ldap: crate::ldap_client::LdapOpts,
    /// `--setParameter awsSTSUrl`: the STS of REFDB-AWS (013 FR-025).
    pub aws_sts_url: Option<String>,
    /// `--setParameter oidcIdentityProviders`: the providers of REFDB-OIDC (013 FR-025).
    pub oidc_providers: Option<String>,
    /// `--setParameter authenticationMechanisms`; empty, RefDb's default.
    pub mechanisms: Vec<teseladb_auth::sasl::SaslMechanism>,
}

/// `--tlsMode`.
pub const TLS_MODE: &str = "tlsMode";
/// `--tlsCertificateKeyFile`.
pub const TLS_CERTIFICATE_KEY_FILE: &str = "tlsCertificateKeyFile";
/// `--enroll-token`.
pub const ENROLL_TOKEN: &str = "enroll-token";
/// `--tlsCertificateKeyFilePassword`.
pub const TLS_CERTIFICATE_KEY_FILE_PASSWORD: &str = "tlsCertificateKeyFilePassword";
/// `--tlsCAFile`.
pub const TLS_CA_FILE: &str = "tlsCAFile";
/// `--tlsCRLFile`.
pub const TLS_CRL_FILE: &str = "tlsCRLFile";
/// `--tlsAllowConnectionsWithoutCertificates`.
pub const TLS_ALLOW_WITHOUT_CERTIFICATES: &str = "tlsAllowConnectionsWithoutCertificates";
/// `--tlsFIPSMode` (013 FR-029).
pub const TLS_FIPS_MODE: &str = "tlsFIPSMode";
/// What `refnode` 9.0.4 says when `authenticationMechanisms` names SCRAM-SHA-1 in FIPS mode.
pub const SCRAM_SHA1_FIPS: &str = "SCRAM-SHA-1 is not allowed in FIPS mode.";
/// `--redactClientLogData` (013 FR-027).
pub const REDACT_CLIENT_LOG_DATA: &str = "redactClientLogData";
/// `--noscripting` (016 FR-009).
pub const NO_SCRIPTING: &str = "noscripting";
/// `--enableEncryption`.
pub const ENABLE_ENCRYPTION: &str = "enableEncryption";
/// `--encryptionKeyFile`.
pub const ENCRYPTION_KEY_FILE: &str = "encryptionKeyFile";
/// `--encryptionCipherMode`.
pub const ENCRYPTION_CIPHER_MODE: &str = "encryptionCipherMode";
/// `--kmipServerName`.
pub const KMIP_SERVER_NAME: &str = "kmipServerName";
/// `--kmipPort`.
pub const KMIP_PORT: &str = "kmipPort";
/// `--kmipServerCAFile`.
pub const KMIP_SERVER_CA_FILE: &str = "kmipServerCAFile";
/// `--kmipClientCertificateFile`.
pub const KMIP_CLIENT_CERTIFICATE_FILE: &str = "kmipClientCertificateFile";
/// `--kmipClientCertificatePassword`.
pub const KMIP_CLIENT_CERTIFICATE_PASSWORD: &str = "kmipClientCertificatePassword";
/// `--kmipKeyIdentifier`.
pub const KMIP_KEY_IDENTIFIER: &str = "kmipKeyIdentifier";
/// `--kmipRotateMasterKey`.
pub const KMIP_ROTATE_MASTER_KEY: &str = "kmipRotateMasterKey";
/// `--kmipConnectRetries`.
pub const KMIP_CONNECT_RETRIES: &str = "kmipConnectRetries";
/// `--kmipConnectTimeoutMS`.
pub const KMIP_CONNECT_TIMEOUT_MS: &str = "kmipConnectTimeoutMS";
/// `--kmipKeyStatePollingSeconds`.
pub const KMIP_KEY_STATE_POLLING_SECONDS: &str = "kmipKeyStatePollingSeconds";
/// `--kmipActivateKeys`.
pub const KMIP_ACTIVATE_KEYS: &str = "kmipActivateKeys";
/// `--kmipUseLegacyProtocol`.
pub const KMIP_USE_LEGACY_PROTOCOL: &str = "kmipUseLegacyProtocol";
/// `--ldapServers`.
pub const LDAP_SERVERS: &str = "ldapServers";
/// `--ldapTransportSecurity`.
pub const LDAP_TRANSPORT_SECURITY: &str = "ldapTransportSecurity";
/// `--ldapBindMethod`.
pub const LDAP_BIND_METHOD: &str = "ldapBindMethod";
/// `--ldapQueryUser`.
pub const LDAP_QUERY_USER: &str = "ldapQueryUser";
/// `--ldapQueryPassword`.
pub const LDAP_QUERY_PASSWORD: &str = "ldapQueryPassword";
/// `--ldapUserToDNMapping`.
pub const LDAP_USER_TO_DN_MAPPING: &str = "ldapUserToDNMapping";
/// `--ldapTimeoutMS`.
pub const LDAP_TIMEOUT_MS: &str = "ldapTimeoutMS";
/// `--ldapAuthzQueryTemplate`.
pub const LDAP_AUTHZ_QUERY_TEMPLATE: &str = "ldapAuthzQueryTemplate";
/// `--setParameter ldapUserCacheInvalidationInterval=<seconds>`.
pub const LDAP_CACHE_INTERVAL: &str = "ldapUserCacheInvalidationInterval";
/// Its bounds, as RefDb 9.0.4 checks them.
const LDAP_CACHE_INTERVAL_MAX: u64 = 86_400;
/// `--keyFile` (013 FR-034).
pub const KEY_FILE: &str = "keyFile";
/// `--clusterAuthMode` (013 FR-034).
pub const CLUSTER_AUTH_MODE: &str = "clusterAuthMode";
/// `--auditDestination`.
pub const AUDIT_DESTINATION: &str = "auditDestination";
/// `--auditFormat`.
pub const AUDIT_FORMAT: &str = "auditFormat";
/// `--auditPath`.
pub const AUDIT_PATH: &str = "auditPath";
/// `--auditFilter`.
pub const AUDIT_FILTER: &str = "auditFilter";
/// `--auditRuntimeConfiguration` (013 FR-031).
pub const AUDIT_RUNTIME_CONFIGURATION: &str = "auditRuntimeConfiguration";
/// `--auditSchema` (013 FR-031).
pub const AUDIT_SCHEMA: &str = "auditSchema";
/// `--auditLocalKeyFile` (013 FR-031).
pub const AUDIT_LOCAL_KEY_FILE: &str = "auditLocalKeyFile";
/// `--auditEncryptionKeyUID` (013 FR-031).
pub const AUDIT_ENCRYPTION_KEY_UID: &str = "auditEncryptionKeyUID";
/// `--auditCompressionMode` (013 FR-031).
pub const AUDIT_COMPRESSION_MODE: &str = "auditCompressionMode";
/// `--setParameter auditAuthorizationSuccess=<bool>` (013 FR-016).
pub const AUDIT_AUTHORIZATION_SUCCESS: &str = "auditAuthorizationSuccess";
/// What `getCmdLineOpts` shows instead of a password, as `refnode` does.
pub const REDACTED_PASSWORD: &str = "<password>";

/// A password given on the command line, hidden from debug output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Password(pub String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_PASSWORD)
    }
}

impl From<&str> for Password {
    fn from(s: &str) -> Password {
        Password(s.to_owned())
    }
}

/// The TLS options of `teseladb start` (013 FR-007, contract `cli-base.md`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsOpts {
    /// `--tlsMode`; `None` is `requireTLS`.
    pub mode: Option<TlsMode>,
    /// `--tlsCertificateKeyFile`: the port's chain and key; `None` takes the PKI's.
    pub certificate_key_file: Option<PathBuf>,
    /// `--tlsCertificateKeyFilePassword`.
    pub certificate_key_file_password: Option<Password>,
    /// `--tlsCAFile`: with it, clients send certificates signed by these CAs.
    pub ca_file: Option<PathBuf>,
    /// `--tlsCRLFile`.
    pub crl_file: Option<PathBuf>,
    /// `--tlsAllowConnectionsWithoutCertificates`.
    pub allow_without_certificates: bool,
    /// `--tlsFIPSMode`: every port and outbound client keeps only FIPS-approved algorithms.
    pub fips_mode: bool,
    /// `--setParameter ocspEnabled=false`: no OCSP stapling (013 FR-031; on by default).
    pub ocsp_disabled: bool,
}

fn tls_opts(o: &Opts) -> Result<TlsOpts, CliError> {
    let mode = match o.get(TLS_MODE) {
        None => None,
        Some(v) => Some(TlsMode::parse(v).ok_or_else(|| {
            CliError(format!(
                "--{TLS_MODE}: expected disabled, allowTLS, preferTLS or requireTLS, got `{v}`"
            ))
        })?),
    };
    Ok(TlsOpts {
        mode,
        certificate_key_file: o.path(TLS_CERTIFICATE_KEY_FILE),
        certificate_key_file_password: o.get(TLS_CERTIFICATE_KEY_FILE_PASSWORD).map(Password::from),
        ca_file: o.path(TLS_CA_FILE),
        crl_file: o.path(TLS_CRL_FILE),
        allow_without_certificates: o.flag(TLS_ALLOW_WITHOUT_CERTIFICATES)?,
        fips_mode: o.flag(TLS_FIPS_MODE)?,
        ocsp_disabled: !ocsp_enabled(o)?,
    })
}

/// Options of `teseladb dev`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevOpts {
    /// Nodes of the development cluster.
    pub nodes: u16,
    /// Cores of each node.
    pub cores: u16,
    /// Clients' port of the first node; node `k` listens on this one plus `k` (0: ports the
    /// system chooses).
    pub port: u16,
    /// `--setParameter enableTestCommands=1`: `configureFailPoint`, for the drivers'
    /// specification tests (002 T043).
    pub test_commands: bool,
}

/// One option of a subcommand: its name and whether it is a flag (it takes no value).
type Spec = (&'static str, bool);

const VALUE: bool = false;
const FLAG: bool = true;

const START: &[Spec] = &[
    ("dbpath", VALUE),
    ("cores", VALUE),
    ("memory-limit", VALUE),
    ("bind_ip", VALUE),
    ("port", VALUE),
    ("peer-listen", VALUE),
    ("admin-listen", VALUE),
    ("join", VALUE),
    ("tags", VALUE),
    (ENROLL_TOKEN, VALUE),
    ("insecure", FLAG),
    ("insecure-allow-remote", FLAG),
    ("log-format", VALUE),
    ("config", VALUE),
    ("logpath", VALUE),
    ("logappend", FLAG),
    ("quiet", FLAG),
    ("verbose", FLAG),
    ("maxConns", VALUE),
    ("networkMessageCompressors", VALUE),
    (SET_PARAMETER, VALUE),
    (TLS_MODE, VALUE),
    (TLS_CERTIFICATE_KEY_FILE, VALUE),
    (TLS_CERTIFICATE_KEY_FILE_PASSWORD, VALUE),
    (TLS_CA_FILE, VALUE),
    (TLS_CRL_FILE, VALUE),
    (TLS_ALLOW_WITHOUT_CERTIFICATES, FLAG),
    (TLS_FIPS_MODE, FLAG),
    (KEY_FILE, VALUE),
    (CLUSTER_AUTH_MODE, VALUE),
    (REDACT_CLIENT_LOG_DATA, FLAG),
    (NO_SCRIPTING, FLAG),
    (ENABLE_ENCRYPTION, FLAG),
    (ENCRYPTION_KEY_FILE, VALUE),
    (ENCRYPTION_CIPHER_MODE, VALUE),
    (KMIP_SERVER_NAME, VALUE),
    (KMIP_PORT, VALUE),
    (KMIP_SERVER_CA_FILE, VALUE),
    (KMIP_CLIENT_CERTIFICATE_FILE, VALUE),
    (KMIP_CLIENT_CERTIFICATE_PASSWORD, VALUE),
    (KMIP_KEY_IDENTIFIER, VALUE),
    (KMIP_ROTATE_MASTER_KEY, FLAG),
    (KMIP_CONNECT_RETRIES, VALUE),
    (KMIP_CONNECT_TIMEOUT_MS, VALUE),
    (KMIP_KEY_STATE_POLLING_SECONDS, VALUE),
    (KMIP_ACTIVATE_KEYS, VALUE),
    (KMIP_USE_LEGACY_PROTOCOL, VALUE),
    (AUDIT_DESTINATION, VALUE),
    (AUDIT_FORMAT, VALUE),
    (AUDIT_PATH, VALUE),
    (AUDIT_FILTER, VALUE),
    (AUDIT_RUNTIME_CONFIGURATION, VALUE),
    (AUDIT_SCHEMA, VALUE),
    (AUDIT_LOCAL_KEY_FILE, VALUE),
    (AUDIT_COMPRESSION_MODE, VALUE),
    (AUDIT_ENCRYPTION_KEY_UID, VALUE),
    (LDAP_SERVERS, VALUE),
    (LDAP_TRANSPORT_SECURITY, VALUE),
    (LDAP_BIND_METHOD, VALUE),
    (LDAP_QUERY_USER, VALUE),
    (LDAP_QUERY_PASSWORD, VALUE),
    (LDAP_USER_TO_DN_MAPPING, VALUE),
    (LDAP_TIMEOUT_MS, VALUE),
    (LDAP_AUTHZ_QUERY_TEMPLATE, VALUE),
];

/// The LDAP options (013 FR-025).
fn ldap_opts(o: &Opts, fips: bool) -> Result<crate::ldap_client::LdapOpts, CliError> {
    let get = |name| o.get(name).map(str::to_owned);
    let timeout_ms = match o.get(LDAP_TIMEOUT_MS) {
        None => None,
        Some(v) => Some(v.parse().map_err(|_| {
            CliError(format!("--{LDAP_TIMEOUT_MS}: expected milliseconds, got `{v}`"))
        })?),
    };
    let opts = crate::ldap_client::LdapOpts {
        servers: get(LDAP_SERVERS),
        transport: get(LDAP_TRANSPORT_SECURITY),
        bind_method: get(LDAP_BIND_METHOD),
        query_user: get(LDAP_QUERY_USER),
        query_password: o.get(LDAP_QUERY_PASSWORD).map(Password::from),
        mapping: get(LDAP_USER_TO_DN_MAPPING),
        timeout_ms,
        authz_query_template: get(LDAP_AUTHZ_QUERY_TEMPLATE),
        cache_interval_secs: cache_interval(o)?,
        fips,
    };
    Ok(opts)
}

/// `--setParameter ldapUserCacheInvalidationInterval`, with RefDb's messages.
fn cache_interval(o: &Opts) -> Result<Option<u64>, CliError> {
    let mut out = None;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((LDAP_CACHE_INTERVAL, v)) = item.split_once('=') else { continue };
        let secs: u64 = v.parse().map_err(|_| {
            CliError(format!("{LDAP_CACHE_INTERVAL} must be a numeric value, '{v}' provided"))
        })?;
        if !(1..=LDAP_CACHE_INTERVAL_MAX).contains(&secs) {
            return err(format!(
                "{LDAP_CACHE_INTERVAL} must be between 1 and {LDAP_CACHE_INTERVAL_MAX} (24 hours)"
            ));
        }
        out = Some(secs);
    }
    Ok(out)
}

/// The audit options (013 FR-016), refused with RefDb's messages.
fn audit_opts(o: &Opts, fips: bool) -> Result<crate::audit_log::AuditOpts, CliError> {
    let get = |name| o.get(name).map(str::to_owned);
    let opts = crate::audit_log::AuditOpts {
        destination: get(AUDIT_DESTINATION),
        format: get(AUDIT_FORMAT),
        path: get(AUDIT_PATH),
        filter: get(AUDIT_FILTER),
        runtime_configuration: bool_value(o, AUDIT_RUNTIME_CONFIGURATION, false)?,
        schema: get(AUDIT_SCHEMA),
        authorization_success: authorization_success(o)?,
        local_key_file: get(AUDIT_LOCAL_KEY_FILE),
        compression_mode: get(AUDIT_COMPRESSION_MODE),
        encryption_key_uid: get(AUDIT_ENCRYPTION_KEY_UID),
        // The audit log reaches the KMIP server with the options of encryption at rest, which
        // it needs whether or not the data are encrypted.
        kmip: kmip_opts(o, fips)?,
    };
    opts.config().map_err(CliError)?;
    Ok(opts)
}

/// `--setParameter ocspEnabled=<bool>` (013 FR-031): on by default.
pub const OCSP_ENABLED: &str = "ocspEnabled";

fn ocsp_enabled(o: &Opts) -> Result<bool, CliError> {
    let mut on = true;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((OCSP_ENABLED, v)) = item.split_once('=') else { continue };
        on = match v {
            "true" | "1" => true,
            "false" | "0" => false,
            other => {
                return err(format!(
                    "Bad value for parameter \"{OCSP_ENABLED}\": {OCSP_ENABLED} expects bool, got '{other}'"
                ));
            }
        };
    }
    Ok(on)
}

/// `--setParameter useInternalAuthzForX509` (013 FR-033).
const USE_INTERNAL_AUTHZ_FOR_X509: &str =
    teseladb_wire::commands::server_params::USE_INTERNAL_AUTHZ_FOR_X509;

/// `--setParameter useInternalAuthzForX509=<bool>`: `true`, `false`, `1` or `0`, as `refnode`
/// 9.0.4 takes it (measured); false by default.
fn use_internal_authz_for_x509(o: &Opts) -> Result<bool, CliError> {
    let mut on = false;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((USE_INTERNAL_AUTHZ_FOR_X509, v)) = item.split_once('=') else { continue };
        on = match v {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => {
                return err(format!(
                    "Bad value for parameter \"{USE_INTERNAL_AUTHZ_FOR_X509}\": Value is not a valid boolean"
                ));
            }
        };
    }
    Ok(on)
}

/// `--setParameter auditAuthorizationSuccess=<bool>`, refused with RefDb's message when it is
/// not a boolean (measured).
fn authorization_success(o: &Opts) -> Result<Option<bool>, CliError> {
    let mut out = None;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((AUDIT_AUTHORIZATION_SUCCESS, v)) = item.split_once('=') else { continue };
        out = Some(match v {
            "true" | "1" => true,
            "false" | "0" => false,
            other => {
                return err(format!(
                    "Bad value for parameter \"{AUDIT_AUTHORIZATION_SUCCESS}\": {AUDIT_AUTHORIZATION_SUCCESS} expects bool, got '{other}'"
                ));
            }
        });
    }
    Ok(out)
}

/// The options of encryption at rest; a key file or a cipher mode without `--enableEncryption`
/// is refused, as `refnode` refuses them.
fn at_rest_opts(o: &Opts, fips: bool) -> Result<crate::at_rest::AtRestOpts, CliError> {
    use teseladb_crypto::at_rest::CipherMode;
    let enable = o.flag(ENABLE_ENCRYPTION)?;
    let mode = match o.get(ENCRYPTION_CIPHER_MODE) {
        None => CipherMode::Cbc,
        Some(v) => CipherMode::parse(v).ok_or_else(|| {
            CliError(format!(
                "--{ENCRYPTION_CIPHER_MODE}: expected AES256-CBC or AES256-GCM, got `{v}`"
            ))
        })?,
    };
    let key_file = o.path(ENCRYPTION_KEY_FILE);
    if !enable && (key_file.is_some() || o.get(ENCRYPTION_CIPHER_MODE).is_some()) {
        return err(format!(
            "--{ENCRYPTION_KEY_FILE} and --{ENCRYPTION_CIPHER_MODE} need --{ENABLE_ENCRYPTION}"
        ));
    }
    // Without --enableEncryption `refnode` takes the KMIP options and does not use them.
    let kmip = kmip_opts(o, fips)?.filter(|_| enable);
    if key_file.is_some() && kmip.is_some() {
        return err(
            "Must specify either an encryption key file or a KMIP server when enabling file encryption",
        );
    }
    Ok(crate::at_rest::AtRestOpts { enable, key_file, mode, kmip })
}

/// A boolean option with a value, `default` without it.
fn bool_value(o: &Opts, name: &str, default: bool) -> Result<bool, CliError> {
    if o.get(name).is_none() { Ok(default) } else { o.flag(name) }
}

/// The KMIP options (013 FR-028); `None` without `--kmipServerName`.
fn kmip_opts(o: &Opts, fips: bool) -> Result<Option<crate::kmip::KmipOpts>, CliError> {
    use crate::kmip::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_POLLING, DEFAULT_PORT, KmipOpts};
    let servers = o.list(KMIP_SERVER_NAME);
    if servers.is_empty() {
        return Ok(None);
    }
    let polling = match o.parse::<i64>(KMIP_KEY_STATE_POLLING_SECONDS, 0)? {
        0 if o.get(KMIP_KEY_STATE_POLLING_SECONDS).is_none() => Some(DEFAULT_POLLING),
        -1 => None,
        s if s > 0 => Some(std::time::Duration::from_secs(s.unsigned_abs())),
        s => {
            return err(format!(
                "--{KMIP_KEY_STATE_POLLING_SECONDS}: expected -1 or a positive number of seconds, got {s}"
            ));
        }
    };
    let timeout_ms = o.parse::<u64>(KMIP_CONNECT_TIMEOUT_MS, 0)?;
    Ok(Some(KmipOpts {
        servers,
        port: o.parse(KMIP_PORT, DEFAULT_PORT)?,
        ca_file: o.path(KMIP_SERVER_CA_FILE),
        client_certificate_file: o.path(KMIP_CLIENT_CERTIFICATE_FILE),
        client_certificate_password: o.get(KMIP_CLIENT_CERTIFICATE_PASSWORD).map(Password::from),
        key_identifier: o.get(KMIP_KEY_IDENTIFIER).map(str::to_owned),
        rotate: o.flag(KMIP_ROTATE_MASTER_KEY)?,
        connect_retries: o.parse(KMIP_CONNECT_RETRIES, 0)?,
        connect_timeout: if timeout_ms == 0 {
            DEFAULT_CONNECT_TIMEOUT
        } else {
            std::time::Duration::from_millis(timeout_ms)
        },
        polling,
        activate_keys: bool_value(o, KMIP_ACTIVATE_KEYS, true)?,
        legacy: bool_value(o, KMIP_USE_LEGACY_PROTOCOL, false)?,
        fips,
    }))
}

/// `--setParameter <name>=<value>`, which may come several times.
const SET_PARAMETER: &str = "setParameter";
/// The startup parameter of the mechanisms the node takes (013 FR-025).
const AUTHENTICATION_MECHANISMS: &str = "authenticationMechanisms";

/// `--setParameter authenticationMechanisms=<names>`: the mechanisms, refused when one is not
/// RefDb's. In FIPS mode SCRAM-SHA-1, which derives its key with MD5, is refused, and the
/// default leaves it out (RefDb 8.3 and later).
fn mechanisms(o: &Opts, fips: bool) -> Result<Vec<teseladb_auth::sasl::SaslMechanism>, CliError> {
    use teseladb_auth::sasl::SaslMechanism;
    let out = named_mechanisms(o)?;
    if !fips {
        return Ok(out);
    }
    if out.contains(&SaslMechanism::ScramSha1) {
        return err(SCRAM_SHA1_FIPS);
    }
    if out.is_empty() {
        return Ok(vec![SaslMechanism::ScramSha256, SaslMechanism::X509]);
    }
    Ok(out)
}

/// The mechanisms `authenticationMechanisms` names; empty when it is not given.
fn named_mechanisms(o: &Opts) -> Result<Vec<teseladb_auth::sasl::SaslMechanism>, CliError> {
    use teseladb_auth::sasl::SaslMechanism;
    let mut out = Vec::new();
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((AUTHENTICATION_MECHANISMS, names)) = item.split_once('=') else { continue };
        out.clear();
        for name in names.split(',').map(str::trim) {
            let m = SaslMechanism::from_name(name).ok_or_else(|| {
                CliError(format!("Mechanism {name} is not supported in authenticationMechanisms"))
            })?;
            out.push(m);
        }
    }
    Ok(out)
}

/// `--setParameter awsSTSUrl`.
pub const AWS_STS_URL: &str = "awsSTSUrl";

/// `--setParameter awsSTSUrl=<https URL>`, refused with RefDb's message when it is not HTTPS.
fn aws_sts_url(o: &Opts) -> Result<Option<String>, CliError> {
    let mut out = None;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((AWS_STS_URL, url)) = item.split_once('=') else { continue };
        teseladb_auth::aws::sts_host(url).map_err(|e| CliError(e.0))?;
        out = Some(url.to_owned());
    }
    Ok(out)
}

/// `--setParameter oidcIdentityProviders`.
pub const OIDC_IDENTITY_PROVIDERS: &str = "oidcIdentityProviders";

/// `--setParameter oidcIdentityProviders=<JSON array>`, refused when it does not read.
fn oidc_providers(o: &Opts) -> Result<Option<String>, CliError> {
    let mut out = None;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((OIDC_IDENTITY_PROVIDERS, json)) = item.split_once('=') else { continue };
        teseladb_auth::oidc::Providers::parse(json).map_err(|e| CliError(e.0))?;
        out = Some(json.to_owned());
    }
    Ok(out)
}

/// `--setParameter transactionLifetimeLimitSeconds` (008 FR-010).
const TRANSACTION_LIFETIME: &str =
    teseladb_wire::commands::server_params::TRANSACTION_LIFETIME_LIMIT_SECONDS;
const TRANSACTION_LIFETIME_DEFAULT: i64 =
    teseladb_wire::commands::server_params::TRANSACTION_LIFETIME_DEFAULT_SECS;
/// `--setParameter maxConcurrentMultiDocumentTransactions` (008 FR-020).
const MAX_TRANSACTIONS: &str =
    teseladb_wire::commands::server_params::MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS;
const MAX_TRANSACTIONS_DEFAULT: i64 =
    teseladb_wire::commands::server_params::MAX_CONCURRENT_TRANSACTIONS_DEFAULT;

/// The integer of `--setParameter <name>=<n>` (`default` without one), at least `least`, with a
/// `refnode` 9.0.4's messages (measured).
fn integer_parameter(o: &Opts, name: &str, default: i64, least: i64) -> Result<i64, CliError> {
    let mut value = default;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        let Some((n, v)) = item.split_once('=') else { continue };
        if n != name {
            continue;
        }
        let bad = |why: String| err(format!("Bad value for parameter \"{name}\": {why}"));
        value = match v.parse::<i32>() {
            Ok(x) => i64::from(x),
            Err(_) => return bad("Did not consume whole string.".to_owned()),
        };
        if value < least {
            return bad(format!(
                "Invalid value for parameter {name}: {value} is not greater than or equal to {least}"
            ));
        }
    }
    Ok(value)
}

/// The startup parameter that enables the test commands.
const ENABLE_TEST_COMMANDS: &str = "enableTestCommands";
/// A parameter that exists and cannot be set at startup (013 FR-030).
const AUTH_SCHEMA_VERSION: &str = teseladb_wire::commands::server_params::AUTH_SCHEMA_VERSION;

/// `--setParameter`: whether `enableTestCommands` is on; another parameter is refused, as
/// `refnode` refuses one it does not know.
fn test_commands(o: &Opts) -> Result<bool, CliError> {
    let mut on = false;
    for item in o.get(SET_PARAMETER).into_iter().flat_map(|v| v.split('\n')) {
        match item.split_once('=') {
            Some((ENABLE_TEST_COMMANDS, v)) => {
                on = match v {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => {
                        return err(format!(
                            "--setParameter {ENABLE_TEST_COMMANDS}: expected a boolean, got `{v}`"
                        ));
                    }
                };
            }
            Some((
                AUTHENTICATION_MECHANISMS
                | LDAP_CACHE_INTERVAL
                | AWS_STS_URL
                | OIDC_IDENTITY_PROVIDERS
                | AUDIT_AUTHORIZATION_SUCCESS
                | OCSP_ENABLED
                | USE_INTERNAL_AUTHZ_FOR_X509
                | TRANSACTION_LIFETIME
                | MAX_TRANSACTIONS,
                _,
            )) => {}
            Some((AUTH_SCHEMA_VERSION, _)) => {
                let msg = format!(
                    "Cannot use --setParameter to set \"{AUTH_SCHEMA_VERSION}\" at startup"
                );
                return err(msg);
            }
            Some((name, _)) => return err(format!("Illegal --setParameter parameter: \"{name}\"")),
            None => return err(format!("--setParameter: expected <name>=<value>, got `{item}`")),
        }
    }
    Ok(on)
}

const DEV: &[Spec] = &[("nodes", VALUE), ("cores", VALUE), ("port", VALUE), (SET_PARAMETER, VALUE)];

/// Value of `--networkMessageCompressors` that turns compression off.
const COMPRESSION_DISABLED: &str = "disabled";

/// `--networkMessageCompressors`: names of compressors, or `disabled`.
fn compressors(o: &Opts) -> Result<Vec<Compressor>, CliError> {
    let names = match o.get("networkMessageCompressors") {
        None => return Ok(DEFAULT_COMPRESSORS.to_vec()),
        Some(_) => o.list("networkMessageCompressors"),
    };
    if names.len() == 1 && names[0] == COMPRESSION_DISABLED {
        return Ok(Vec::new());
    }
    names
        .iter()
        .map(|n| {
            Compressor::from_name(n).ok_or_else(|| {
                CliError(format!(
                    "--networkMessageCompressors: unknown compressor `{n}` (snappy, zstd, zlib or \
                     disabled)"
                ))
            })
        })
        .collect()
}

/// `--maxConns`: a positive number.
fn max_conns(o: &Opts) -> Result<usize, CliError> {
    match o.get("maxConns") {
        None => Ok(DEFAULT_MAX_CONNECTIONS),
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n > 0 => Ok(n),
            _ => err(format!("--maxConns: `{v}` is not a positive number")),
        },
    }
}

/// Parsed options of one subcommand.
struct Opts {
    values: BTreeMap<&'static str, String>,
    positional: Vec<String>,
}

impl Opts {
    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn flag(&self, name: &str) -> Result<bool, CliError> {
        match self.get(name) {
            None => Ok(false),
            Some("" | "1" | "true" | "yes") => Ok(true),
            Some("0" | "false" | "no") => Ok(false),
            Some(v) => err(format!("--{name}: expected a boolean, got `{v}`")),
        }
    }

    fn parse<T: FromStr>(&self, name: &str, default: T) -> Result<T, CliError>
    where
        T::Err: fmt::Display,
    {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|e| CliError(format!("--{name}: {e}"))),
        }
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.get(name).map(PathBuf::from)
    }

    fn list(&self, name: &str) -> Vec<String> {
        self.get(name).map_or_else(Vec::new, |v| {
            v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
        })
    }

    /// A number in `lo..=hi`.
    fn range(&self, name: &str, default: u16, lo: u16, hi: u16) -> Result<u16, CliError> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => match v.parse::<u16>() {
                Ok(n) if (lo..=hi).contains(&n) => Ok(n),
                _ => err(format!("--{name}: `{v}` ({lo} to {hi})")),
            },
        }
    }
}

fn parse_opts(args: &[String], specs: &[Spec]) -> Result<Opts, CliError> {
    let mut values = BTreeMap::new();
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let Some(body) = a.strip_prefix("--") else {
            positional.push(a.clone());
            continue;
        };
        let (name, inline) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (body, None),
        };
        let Some(&(spec, is_flag)) = specs.iter().find(|(s, _)| *s == name) else {
            return err(format!("unknown option --{name}"));
        };
        let value = match (is_flag, inline) {
            (_, Some(v)) => v,
            (true, None) => String::new(),
            (false, None) => match it.next() {
                Some(v) => v.clone(),
                None => return err(format!("--{name} needs a value")),
            },
        };
        match values.get_mut(spec) {
            // A repeated `--setParameter` adds to the others.
            Some(v) if spec == SET_PARAMETER => {
                let v: &mut String = v;
                v.push('\n');
                v.push_str(&value);
            }
            _ => drop(values.insert(spec, value)),
        }
    }
    Ok(Opts { values, positional })
}

fn addr(o: &Opts, name: &str, default: &str) -> Result<SocketAddr, CliError> {
    o.get(name)
        .unwrap_or(default)
        .parse()
        .map_err(|_| CliError(format!("--{name}: expected <ip>:<port>")))
}

fn no_positional(o: &Opts, sub: &str) -> Result<(), CliError> {
    match o.positional.first() {
        Some(p) => err(format!("`teseladb {sub}` takes no argument `{p}`")),
        None => Ok(()),
    }
}

/// The tags of `--tags`: `name=value` items, each name once, sorted by name.
fn tags(items: &[String]) -> Result<Vec<(String, String)>, CliError> {
    let mut tags = Vec::with_capacity(items.len());
    for item in items {
        let Some((name, value)) = item.split_once('=').filter(|(n, _)| !n.is_empty()) else {
            return err(format!("--tags {item}: a tag is name=value"));
        };
        tags.push((name.to_owned(), value.to_owned()));
    }
    tags.sort();
    if let Some(w) = tags.windows(2).find(|w| w[0].0 == w[1].0) {
        return err(format!("--tags: tag {} given twice", w[0].0));
    }
    Ok(tags)
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn log_level(o: &Opts) -> Result<Level, CliError> {
    match (o.flag("quiet")?, o.flag("verbose")?) {
        (true, true) => err("--quiet and --verbose cannot go together"),
        (true, false) => Ok(Level::Warn),
        (false, true) => Ok(Level::Debug),
        (false, false) => Ok(Level::Info),
    }
}

fn start(o: &Opts) -> Result<StartOpts, CliError> {
    no_positional(o, "start")?;
    let insecure = o.flag("insecure")?;
    let insecure_allow_remote = o.flag("insecure-allow-remote")?;
    if insecure_allow_remote && !insecure {
        return err("--insecure-allow-remote only makes sense with --insecure");
    }
    let tls = tls_opts(o)?;
    let fips = tls.fips_mode;
    if insecure && tls != TlsOpts::default() {
        return err("--tls* options configure TLS, which --insecure turns off");
    }
    if insecure && o.get(KEY_FILE).is_some() {
        return err("--keyFile turns authentication on, which --insecure turns off");
    }
    let bind_ip = o.get("bind_ip").unwrap_or("127.0.0.1").to_string();
    let peer_listen = addr(o, "peer-listen", "127.0.0.1:27100")?;
    let admin_listen = addr(o, "admin-listen", "127.0.0.1:27200")?;
    if insecure && !insecure_allow_remote {
        // `--bind_ip` is a comma separated list: every address in it must be loopback.
        let binds = bind_ip.split(',').map(str::trim).filter(|h| !h.is_empty());
        let hosts = binds.map(|h| ("bind_ip", h.to_owned())).chain([
            ("peer-listen", peer_listen.ip().to_string()),
            ("admin-listen", admin_listen.ip().to_string()),
        ]);
        for (name, host) in hosts {
            if !is_loopback(&host) {
                return err(format!(
                    "--{name} {host}: with --insecure only loopback addresses are allowed \
                     (add --insecure-allow-remote to accept others)"
                ));
            }
        }
    }
    let cores = match o.get("cores") {
        None => None,
        Some(_) => Some(o.range("cores", 1, 1, 256)?),
    };
    Ok(StartOpts {
        dbpath: o.path("dbpath").unwrap_or_else(|| PathBuf::from("./teseladb-data")),
        cores,
        bind_ip,
        port: o.range("port", 27017, 0, u16::MAX)?,
        peer_listen,
        admin_listen,
        join: o.list("join"),
        tags: tags(&o.list("tags"))?,
        memory_limit: o
            .get("memory-limit")
            .map(|v| {
                crate::memory::parse_size(v).map_err(|e| CliError(format!("--memory-limit: {e}")))
            })
            .transpose()?,
        enroll_token: o.get(ENROLL_TOKEN).map(Password::from),
        insecure,
        insecure_allow_remote,
        log_format: o.parse("log-format", Format::Text)?,
        log_level: log_level(o)?,
        config: o.path("config"),
        logpath: o.path("logpath"),
        logappend: o.flag("logappend")?,
        max_conns: max_conns(o)?,
        compressors: compressors(o)?,
        test_commands: test_commands(o)?,
        use_internal_authz_for_x509: use_internal_authz_for_x509(o)?,
        key_file: o.path(KEY_FILE),
        cluster_auth_mode: o.get(CLUSTER_AUTH_MODE).map(str::to_owned),
        transaction_lifetime_secs: integer_parameter(
            o,
            TRANSACTION_LIFETIME,
            TRANSACTION_LIFETIME_DEFAULT,
            1,
        )?,
        max_concurrent_transactions: integer_parameter(
            o,
            MAX_TRANSACTIONS,
            MAX_TRANSACTIONS_DEFAULT,
            0,
        )?,
        redact_client_log_data: o.flag(REDACT_CLIENT_LOG_DATA)?,
        no_scripting: o.flag(NO_SCRIPTING)?,
        // Passwords never stay in the options as given (they show in `Debug`).
        given: o
            .values
            .iter()
            .map(|(k, v)| {
                let secret = [TLS_CERTIFICATE_KEY_FILE_PASSWORD, LDAP_QUERY_PASSWORD].contains(k);
                ((*k).to_owned(), if secret { REDACTED_PASSWORD.to_owned() } else { v.clone() })
            })
            .collect(),
        argv: Vec::new(),
        tls,
        at_rest: at_rest_opts(o, fips)?,
        audit: audit_opts(o, fips)?,
        ldap: ldap_opts(o, fips)?,
        aws_sts_url: aws_sts_url(o)?,
        oidc_providers: oidc_providers(o)?,
        mechanisms: mechanisms(o, fips)?,
    })
}

fn dev(o: &Opts) -> Result<DevOpts, CliError> {
    no_positional(o, "dev")?;
    Ok(DevOpts {
        nodes: o.range("nodes", 3, 1, 7)?,
        cores: o.range("cores", 1, 1, 256)?,
        port: o.range("port", 27017, 0, u16::MAX - 7)?,
        test_commands: test_commands(o)?,
    })
}

/// Parses `args` (without the program name), reading `--config` from the disk.
pub fn parse(args: &[String]) -> Result<Command, CliError> {
    parse_with(args, &|p| std::fs::read(p))
}

/// Parses `args` (without the program name); `read` gives the content of the `--config` file,
/// whose options go before the command line's, so that the command line wins.
pub fn parse_with(
    args: &[String],
    read: &dyn Fn(&Path) -> io::Result<Vec<u8>>,
) -> Result<Command, CliError> {
    let Some((sub, rest)) = args.split_first() else { return err(USAGE) };
    match sub.as_str() {
        "start" => {
            let mut o = parse_opts(rest, START)?;
            if let Some(path) = o.path("config") {
                let in_file = |e: String| CliError(format!("--config {}: {e}", path.display()));
                let text = read(&path).map_err(|e| in_file(e.to_string()))?;
                let mut all = yaml_args(&text).map_err(in_file)?;
                all.extend_from_slice(rest);
                o = parse_opts(&all, START)?;
            }
            Ok(Command::Start(Box::new(start(&o)?)))
        }
        "dev" => Ok(Command::Dev(dev(&parse_opts(rest, DEV)?)?)),
        "admin" => crate::admin::parse(rest).map(Command::Admin).map_err(CliError),
        "--version" => Ok(Command::Version),
        "--help" | "-h" => Ok(Command::Help),
        other => err(format!("unknown subcommand `{other}` (see `teseladb --help`)")),
    }
}

/// Usage text.
pub const USAGE: &str = "\
usage:
  teseladb start [--dbpath <dir>] [--cores <n>] [--memory-limit <bytes[K|M|G|T]>]
                 [--bind_ip <addr>] [--port <n>]
                 [--peer-listen <ip:port>] [--admin-listen <ip:port>] [--join <ip:port>,...]
                 [--enroll-token <token>]
                 [--tlsMode <mode>] [--tlsCertificateKeyFile <file>] [--tlsCAFile <file>]
                 [--insecure [--insecure-allow-remote]] [--log-format text|json] [--config <file.yaml>]
                 [--logpath <file>] [--logappend] [--quiet | --verbose] [--maxConns <n>]
                 [--networkMessageCompressors <zstd,snappy,zlib|disabled>]
                 [--setParameter enableTestCommands=1]
  teseladb dev [--nodes <1-7>] [--cores <n>] [--port <n>] [--setParameter enableTestCommands=1]
  teseladb admin [--node <ip:port>] [--format text|json] <subcommand>   (teseladb admin help)
  teseladb --version
  teseladb --help
";
