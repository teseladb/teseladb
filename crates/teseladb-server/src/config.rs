//! The configuration file of `teseladb start --config` (001 FR-021): the keys of the `refnode` file
//! for the options that have one in RefDb and the `teseladb.*` section for TeselaDB's own,
//! turned into command line options that go before the real command line, which therefore wins.

use teseladb_json::{Value, parse_yaml, to_string};

/// What a key of the file becomes.
#[derive(Clone, Copy)]
enum Kind {
    /// `--option=<value>`.
    Option(&'static str),
    /// `--option=<value>`, a list joined with commas.
    List(&'static str),
    /// `--verbose` when the verbosity is above 0.
    Verbosity,
    /// `systemLog.destination`: only `file`, with `systemLog.path`.
    Destination,
    /// `--setParameter <name>=<value>`.
    Parameter(&'static str),
}

/// Keys of the file in 001 (contract `cli-base.md`).
const KEYS: &[(&str, Kind)] = &[
    ("storage.dbPath", Kind::Option("dbpath")),
    ("net.bindIp", Kind::Option("bind_ip")),
    ("net.port", Kind::Option("port")),
    ("net.maxIncomingConnections", Kind::Option("maxConns")),
    ("net.compression.compressors", Kind::Option("networkMessageCompressors")),
    ("systemLog.destination", Kind::Destination),
    ("systemLog.path", Kind::Option("logpath")),
    ("systemLog.logAppend", Kind::Option("logappend")),
    ("systemLog.quiet", Kind::Option("quiet")),
    ("systemLog.verbosity", Kind::Verbosity),
    ("teseladb.cores", Kind::Option("cores")),
    ("teseladb.memoryLimit", Kind::Option("memory-limit")),
    ("teseladb.peerListen", Kind::Option("peer-listen")),
    ("teseladb.adminListen", Kind::Option("admin-listen")),
    ("teseladb.http.port", Kind::Option(crate::cli::HTTP_PORT)),
    ("teseladb.http.bind", Kind::Option(crate::cli::HTTP_BIND)),
    ("teseladb.join", Kind::List("join")),
    ("teseladb.enrollToken", Kind::Option(crate::cli::ENROLL_TOKEN)),
    ("teseladb.insecure", Kind::Option("insecure")),
    ("teseladb.insecureAllowRemote", Kind::Option("insecure-allow-remote")),
    ("teseladb.logFormat", Kind::Option("log-format")),
    ("setParameter.enableTestCommands", Kind::Parameter("enableTestCommands")),
    ("net.tls.mode", Kind::Option(crate::cli::TLS_MODE)),
    ("net.tls.certificateKeyFile", Kind::Option(crate::cli::TLS_CERTIFICATE_KEY_FILE)),
    (
        "net.tls.certificateKeyFilePassword",
        Kind::Option(crate::cli::TLS_CERTIFICATE_KEY_FILE_PASSWORD),
    ),
    ("net.tls.CAFile", Kind::Option(crate::cli::TLS_CA_FILE)),
    ("net.tls.CRLFile", Kind::Option(crate::cli::TLS_CRL_FILE)),
    (
        "net.tls.allowConnectionsWithoutCertificates",
        Kind::Option(crate::cli::TLS_ALLOW_WITHOUT_CERTIFICATES),
    ),
    ("net.tls.FIPSMode", Kind::Option(crate::cli::TLS_FIPS_MODE)),
    ("security.redactClientLogData", Kind::Option(crate::cli::REDACT_CLIENT_LOG_DATA)),
    ("security.keyFile", Kind::Option(crate::cli::KEY_FILE)),
    ("security.clusterAuthMode", Kind::Option(crate::cli::CLUSTER_AUTH_MODE)),
    ("security.enableEncryption", Kind::Option(crate::cli::ENABLE_ENCRYPTION)),
    ("security.encryptionKeyFile", Kind::Option(crate::cli::ENCRYPTION_KEY_FILE)),
    ("security.encryptionCipherMode", Kind::Option(crate::cli::ENCRYPTION_CIPHER_MODE)),
    ("security.kmip.serverName", Kind::Option(crate::cli::KMIP_SERVER_NAME)),
    ("security.kmip.port", Kind::Option(crate::cli::KMIP_PORT)),
    ("security.kmip.serverCAFile", Kind::Option(crate::cli::KMIP_SERVER_CA_FILE)),
    ("security.kmip.clientCertificateFile", Kind::Option(crate::cli::KMIP_CLIENT_CERTIFICATE_FILE)),
    (
        "security.kmip.clientCertificatePassword",
        Kind::Option(crate::cli::KMIP_CLIENT_CERTIFICATE_PASSWORD),
    ),
    ("security.kmip.keyIdentifier", Kind::Option(crate::cli::KMIP_KEY_IDENTIFIER)),
    ("security.kmip.rotateMasterKey", Kind::Option(crate::cli::KMIP_ROTATE_MASTER_KEY)),
    ("security.kmip.connectRetries", Kind::Option(crate::cli::KMIP_CONNECT_RETRIES)),
    ("security.kmip.connectTimeoutMS", Kind::Option(crate::cli::KMIP_CONNECT_TIMEOUT_MS)),
    (
        "security.kmip.keyStatePollingSeconds",
        Kind::Option(crate::cli::KMIP_KEY_STATE_POLLING_SECONDS),
    ),
    ("security.kmip.activateKeys", Kind::Option(crate::cli::KMIP_ACTIVATE_KEYS)),
    ("security.kmip.useLegacyProtocol", Kind::Option(crate::cli::KMIP_USE_LEGACY_PROTOCOL)),
    ("auditLog.destination", Kind::Option(crate::cli::AUDIT_DESTINATION)),
    ("auditLog.format", Kind::Option(crate::cli::AUDIT_FORMAT)),
    ("auditLog.path", Kind::Option(crate::cli::AUDIT_PATH)),
    ("auditLog.filter", Kind::Option(crate::cli::AUDIT_FILTER)),
    ("auditLog.runtimeConfiguration", Kind::Option(crate::cli::AUDIT_RUNTIME_CONFIGURATION)),
    ("auditLog.schema", Kind::Option(crate::cli::AUDIT_SCHEMA)),
    ("auditLog.localAuditKeyFile", Kind::Option(crate::cli::AUDIT_LOCAL_KEY_FILE)),
    ("auditLog.compressionMode", Kind::Option(crate::cli::AUDIT_COMPRESSION_MODE)),
    ("auditLog.auditEncryptionKeyIdentifier", Kind::Option(crate::cli::AUDIT_ENCRYPTION_KEY_UID)),
    ("security.ldap.servers", Kind::Option(crate::cli::LDAP_SERVERS)),
    ("security.ldap.transportSecurity", Kind::Option(crate::cli::LDAP_TRANSPORT_SECURITY)),
    ("security.ldap.bind.method", Kind::Option(crate::cli::LDAP_BIND_METHOD)),
    ("security.ldap.bind.queryUser", Kind::Option(crate::cli::LDAP_QUERY_USER)),
    ("security.ldap.bind.queryPassword", Kind::Option(crate::cli::LDAP_QUERY_PASSWORD)),
    ("security.ldap.userToDNMapping", Kind::Option(crate::cli::LDAP_USER_TO_DN_MAPPING)),
    ("security.ldap.timeoutMS", Kind::Option(crate::cli::LDAP_TIMEOUT_MS)),
    ("security.ldap.authz.queryTemplate", Kind::Option(crate::cli::LDAP_AUTHZ_QUERY_TEMPLATE)),
    ("setParameter.awsSTSUrl", Kind::Parameter("awsSTSUrl")),
    ("setParameter.oidcIdentityProviders", Kind::Parameter("oidcIdentityProviders")),
    (
        "setParameter.ldapUserCacheInvalidationInterval",
        Kind::Parameter("ldapUserCacheInvalidationInterval"),
    ),
    ("setParameter.authenticationMechanisms", Kind::Parameter("authenticationMechanisms")),
    (
        "setParameter.transactionLifetimeLimitSeconds",
        Kind::Parameter("transactionLifetimeLimitSeconds"),
    ),
    (
        "setParameter.maxConcurrentMultiDocumentTransactions",
        Kind::Parameter("maxConcurrentMultiDocumentTransactions"),
    ),
];

/// Most verbose level of `systemLog.verbosity`, as in `refnode`.
const MAX_VERBOSITY: u64 = 5;

/// A scalar as the command line writes it.
fn scalar(key: &str, v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(_) | Value::Number(_) => to_string(v).map_err(|e| format!("`{key}`: {e:?}")),
        _ => Err(format!("`{key}` takes one value")),
    }
}

/// Every leaf of the mapping with its dotted key.
fn leaves<'a>(prefix: &str, v: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    match v.as_object() {
        Some(obj) if !obj.is_empty() || prefix.is_empty() => {
            for (k, child) in obj.iter() {
                let key = if prefix.is_empty() { k.to_owned() } else { format!("{prefix}.{k}") };
                leaves(&key, child, out);
            }
        }
        _ => out.push((prefix.to_owned(), v)),
    }
}

/// The command line options of a configuration file, in the order of the file.
pub fn yaml_args(text: &[u8]) -> Result<Vec<String>, String> {
    let doc = parse_yaml(text).map_err(|e| e.to_string())?;
    if doc.as_object().is_none() {
        return Err("the file is not a mapping".into());
    }
    let mut found = Vec::new();
    leaves("", &doc, &mut found);
    let mut args = Vec::new();
    let mut file_destination = false;
    let mut has_path = false;
    for (key, v) in found {
        let Some(&(_, kind)) = KEYS.iter().find(|(k, _)| *k == key) else {
            return Err(format!("unknown key `{key}`"));
        };
        if v.is_null() {
            continue;
        }
        match kind {
            Kind::Option(name) => {
                has_path |= name == "logpath";
                args.push(format!("--{name}={}", scalar(&key, v)?));
            }
            Kind::List(name) => {
                let items = match v.as_array() {
                    Some(items) => {
                        items.iter().map(|i| scalar(&key, i)).collect::<Result<Vec<_>, _>>()?
                    }
                    None => vec![scalar(&key, v)?],
                };
                args.push(format!("--{name}={}", items.join(",")));
            }
            Kind::Parameter(name) => {
                args.push(format!("--setParameter={name}={}", scalar(&key, v)?));
            }
            Kind::Verbosity => match v.as_u64() {
                Some(0) => {}
                Some(n) if n <= MAX_VERBOSITY => args.push("--verbose".into()),
                _ => return Err(format!("`{key}`: a number from 0 to {MAX_VERBOSITY}")),
            },
            Kind::Destination => match v.as_str() {
                Some("file") => file_destination = true,
                _ => {
                    return Err(format!(
                        "`{key}`: only `file` (the log goes to standard error without it)"
                    ));
                }
            },
        }
    }
    if file_destination && !has_path {
        return Err("`systemLog.destination: file` needs `systemLog.path`".into());
    }
    Ok(args)
}
