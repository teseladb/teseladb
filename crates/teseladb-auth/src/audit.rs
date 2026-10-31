//! The audit log of RefDb Enterprise 9.0 (013 FR-016): its action types, the options
//! `--auditDestination`, `--auditFormat`, `--auditPath` and `--auditFilter` with RefDb's errors,
//! the message of each event and the filter with the semantics of `auditLog.filter` (a query on
//! the message). Without I/O: [`Auditor::render`] gives the bytes to write and the server writes
//! them. The shapes and messages were measured on `refdb-enterprise-server:9.0.4`.
//!
//! The parameters of each action type are built in [`crate::audit_param`].

use std::fmt;

use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_mql::{Filter, FilterUse, MatchContext, NoExtensions};

use crate::users::{RoleName, UserName};

macro_rules! audit_types {
    ($($variant:ident => $name:literal,)*) => {
        /// An action type (`atype`).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum AuditType {
            $(
                #[doc = concat!("`", $name, "`.")]
                $variant,
            )*
        }

        impl AuditType {
            /// Every action type.
            pub const ALL: &'static [AuditType] = &[$(AuditType::$variant,)*];

            /// Its name in `atype`.
            pub fn name(self) -> &'static str {
                match self {
                    $(AuditType::$variant => $name,)*
                }
            }
        }
    };
}

audit_types! {
    AddShard => "addShard",
    ApplicationMessage => "applicationMessage",
    AuditConfigure => "auditConfigure",
    AuthCheck => "authCheck",
    Authenticate => "authenticate",
    ClientMetadata => "clientMetadata",
    CreateCollection => "createCollection",
    CreateDatabase => "createDatabase",
    CreateIndex => "createIndex",
    CreateRole => "createRole",
    CreateUser => "createUser",
    DirectAuthMutation => "directAuthMutation",
    DropAllRolesFromDatabase => "dropAllRolesFromDatabase",
    DropAllUsersFromDatabase => "dropAllUsersFromDatabase",
    DropCollection => "dropCollection",
    DropDatabase => "dropDatabase",
    DropIndex => "dropIndex",
    DropRole => "dropRole",
    DropUser => "dropUser",
    EnableSharding => "enableSharding",
    GetClusterParameter => "getClusterParameter",
    GrantPrivilegesToRole => "grantPrivilegesToRole",
    GrantRolesToRole => "grantRolesToRole",
    GrantRolesToUser => "grantRolesToUser",
    Logout => "logout",
    RefineCollectionShardKey => "refineCollectionShardKey",
    RemoveShard => "removeShard",
    RenameCollection => "renameCollection",
    ReplSetReconfig => "replSetReconfig",
    RevokePrivilegesFromRole => "revokePrivilegesFromRole",
    RevokeRolesFromRole => "revokeRolesFromRole",
    RevokeRolesFromUser => "revokeRolesFromUser",
    RotateLog => "rotateLog",
    SetClusterParameter => "setClusterParameter",
    ShardCollection => "shardCollection",
    Shutdown => "shutdown",
    Startup => "startup",
    UpdateCachedClusterServerParameter => "updateCachedClusterServerParameter",
    UpdateRole => "updateRole",
    UpdateUser => "updateUser",
}

impl AuditType {
    /// The action type named `name`.
    pub fn from_name(name: &str) -> Option<AuditType> {
        AuditType::ALL.iter().copied().find(|t| t.name() == name)
    }
}

/// Where the messages go (`--auditDestination`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    /// A file (`--auditPath`).
    File,
    /// The system log.
    Syslog,
    /// The standard output.
    Console,
}

impl Destination {
    /// Every destination.
    pub const ALL: [Destination; 3] =
        [Destination::Console, Destination::Syslog, Destination::File];

    /// Its name in the option.
    pub fn name(self) -> &'static str {
        match self {
            Destination::File => "file",
            Destination::Syslog => "syslog",
            Destination::Console => "console",
        }
    }
}

/// How a file holds the messages (`--auditFormat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// One line of JSON per message.
    Json,
    /// One BSON document per message.
    Bson,
}

impl Format {
    /// Every format.
    pub const ALL: [Format; 2] = [Format::Bson, Format::Json];

    /// Its name in the option.
    pub fn name(self) -> &'static str {
        match self {
            Format::Json => "JSON",
            Format::Bson => "BSON",
        }
    }
}

/// RefDb's code for every error of the options (`BadValue`).
pub const BAD_VALUE: i32 = 2;

/// Options that do not configure an audit log: the message RefDb stops with (code
/// [`BAD_VALUE`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditError(pub String);

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuditError {}

fn bad(message: impl Into<String>) -> AuditError {
    AuditError(message.into())
}

/// The options as given (command line or `auditLog.*` of the configuration file).
#[derive(Clone, Copy, Debug, Default)]
pub struct Options<'a> {
    /// `--auditDestination`.
    pub destination: Option<&'a str>,
    /// `--auditFormat`.
    pub format: Option<&'a str>,
    /// `--auditPath`.
    pub path: Option<&'a str>,
    /// `--auditFilter`, as JSON text (keys may go without quotes, as RefDb reads them).
    pub filter: Option<&'a str>,
    /// `--auditRuntimeConfiguration`: the filter and `auditAuthorizationSuccess` come from the
    /// cluster parameter `auditConfig` (013 FR-031).
    pub runtime_configuration: bool,
    /// Whether `--setParameter auditAuthorizationSuccess` was given.
    pub authorization_success_set: bool,
    /// `--auditSchema`: `ref` or `OCSF`.
    pub schema: Option<&'a str>,
    /// `--auditLocalKeyFile`: the file's messages go encrypted under its key.
    pub local_key_file: Option<&'a str>,
    /// `--auditCompressionMode`: `zstd`, only with encryption.
    pub compression_mode: Option<&'a str>,
    /// `--auditEncryptionKeyUID`: the KMIP key that wraps the log's key.
    pub encryption_key_uid: Option<&'a str>,
    /// Whether `--kmipServerName` was given.
    pub kmip_server_set: bool,
}

/// What the OCSF messages of the process say about it: its identifier and its system.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    /// The process identifier.
    pub pid: u32,
    /// The system's name (`Ubuntu`, `macOS`).
    pub os_name: String,
    /// Its version.
    pub os_version: String,
}

/// The shape of the messages (`auditLog.schema`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Schema {
    /// RefDb's own (`ref`, the default).
    #[default]
    Ref,
    /// The Open Cybersecurity Schema Framework (`OCSF`).
    Ocsf,
}

impl Schema {
    /// Every schema.
    pub const ALL: [Schema; 2] = [Schema::Ref, Schema::Ocsf];

    /// Its name in `auditLog.schema`.
    pub fn name(self) -> &'static str {
        match self {
            Schema::Ref => "ref",
            Schema::Ocsf => "OCSF",
        }
    }
}

/// A configured audit log.
#[derive(Debug)]
pub struct Config {
    /// Where the messages go.
    pub destination: Destination,
    /// How a file holds them (JSON for the console and the system log).
    pub format: Format,
    /// The file, for [`Destination::File`].
    pub path: Option<String>,
    /// The filter as given.
    pub filter_doc: Option<Document>,
    filter: Option<Filter>,
    /// `auditAuthorizationSuccess`: successful authorization checks are recorded too.
    pub authorization_success: bool,
    /// `auditLog.runtimeConfiguration`.
    pub runtime: bool,
    /// `auditLog.schema`.
    pub schema: Schema,
    /// The process, for the OCSF messages of `startup` and `shutdown`.
    pub process: Option<ProcessInfo>,
    /// `auditLog.localAuditKeyFile`.
    pub local_key_file: Option<String>,
    /// `auditLog.compressionMode`.
    pub compression: crate::audit_crypt::Compression,
    /// `auditLog.auditEncryptionKeyIdentifier`.
    pub encryption_key_uid: Option<String>,
    /// The encryption of a file's messages, once the server read the key file.
    pub encryption: Option<crate::audit_crypt::AuditEncryption>,
}

impl Config {
    /// The audit log the options configure, `None` without any, or RefDb's error (in the order
    /// RefDb checks them).
    pub fn from_options(o: &Options<'_>) -> Result<Option<Config>, AuditError> {
        let Some(dest) = o.destination else {
            let runtime = o.runtime_configuration.then_some("true");
            let given = [
                ("path", o.path),
                ("format", o.format),
                ("filter", o.filter),
                ("runtimeConfiguration", runtime),
                ("schema", o.schema),
                ("localAuditKeyFile", o.local_key_file),
            ];
            return match given.iter().find(|(_, v)| v.is_some()) {
                Some((name, _)) => Err(bad(format!(
                    "auditLog.destination is required when auditLog.{name} is specified"
                ))),
                None => Ok(None),
            };
        };
        let destination = match Destination::ALL.iter().find(|d| d.name() == dest) {
            Some(d) => *d,
            None if Destination::ALL.iter().any(|d| d.name().eq_ignore_ascii_case(dest)) => {
                return Err(bad("invalid auditLog destination"));
            }
            None => {
                return Err(bad("auditDestination must be one of 'console', 'syslog', or 'file'"));
            }
        };
        let format = match o.format {
            None => None,
            Some(f) => Some(match Format::ALL.iter().find(|x| x.name() == f) {
                Some(x) => *x,
                None if Format::ALL.iter().any(|x| x.name().eq_ignore_ascii_case(f)) => {
                    return Err(bad("Invalid value for auditLog.format"));
                }
                None => return Err(bad("auditFormat must be one of 'BSON' or 'JSON'")),
            }),
        };
        let format = match (destination, format, o.path) {
            (Destination::File, None, _) => {
                return Err(bad(
                    "auditLog.format must be specified when auditLog.destination is to a file",
                ));
            }
            (Destination::File, Some(_), None) => {
                return Err(bad(
                    "auditLog.path must be specified when auditLog.destination is to a file",
                ));
            }
            (Destination::File, Some(f), Some(_)) => f,
            (_, None, None) => Format::Json,
            (_, _, _) => {
                return Err(bad(
                    "auditLog.format and auditLog.path are only allowed when auditLog.destination is 'file'",
                ));
            }
        };
        if o.runtime_configuration && o.filter.is_some() {
            return Err(bad(
                "auditLog.filter must not be configured when runtime audit configuration is enabled",
            ));
        }
        if o.runtime_configuration && o.authorization_success_set {
            return Err(bad(
                "setParameter.auditAuthorizationSuccess must not be configured when runtime audit configuration is enabled",
            ));
        }
        if o.encryption_key_uid.is_some() && !o.kmip_server_set {
            return Err(bad(
                "security.kmip.serverName is required when auditLog.auditEncryptionKeyIdentifier is specified",
            ));
        }
        if o.encryption_key_uid.is_some() && o.local_key_file.is_some() {
            return Err(bad(
                "auditLog.auditEncryptionKeyIdentifier is not allowed when auditLog.localAuditKeyFile is specified",
            ));
        }
        let encrypted_key = o.local_key_file.or(o.encryption_key_uid);
        let compression = match (o.compression_mode, encrypted_key) {
            (None, _) => crate::audit_crypt::Compression::None,
            (Some(_), None) => {
                return Err(bad(
                    "auditLog.compressionMode is only allowed if audit log encryption is enabled",
                ));
            }
            (Some("zstd"), Some(_)) => crate::audit_crypt::Compression::Zstd,
            (Some(_), Some(_)) => return Err(bad("auditLog.compressionMode must be set as zstd")),
        };
        let schema = match o.schema {
            None => Schema::Ref,
            Some(name) => Schema::ALL
                .into_iter()
                .find(|s| s.name() == name)
                .ok_or_else(|| bad("auditSchema must be one of 'ref' or 'OCSF'"))?,
        };
        let (filter_doc, filter) = match o.filter {
            None => (None, None),
            Some(text) => {
                let doc = filter_document(text)?;
                let filter = parse_filter(&doc)?;
                (Some(doc), Some(filter))
            }
        };
        Ok(Some(Config {
            destination,
            format,
            path: o.path.map(str::to_owned),
            filter_doc,
            filter,
            authorization_success: false,
            runtime: o.runtime_configuration,
            schema,
            process: None,
            local_key_file: o.local_key_file.map(str::to_owned),
            compression,
            encryption_key_uid: o.encryption_key_uid.map(str::to_owned),
            encryption: None,
        }))
    }
}

/// The filter text as a document: Extended JSON, with keys without quotes and strings in single
/// quotes as RefDb also reads them; an array counts as the document of its positions.
fn filter_document(text: &str) -> Result<Document, AuditError> {
    // Inside a field, the text may be a document or an array; anything else is refused.
    let wrapped = format!("{{\"f\":{}}}", crate::shell_json::quote_keys(text));
    let mut doc = extjson::parse(&wrapped).map_err(bad_filter)?;
    match doc.remove("f") {
        Some(Value::Document(d)) => Ok(d),
        Some(Value::Array(items)) => {
            let mut d = Document::new();
            for (i, item) in items.into_iter().enumerate() {
                d.push(i.to_string(), item);
            }
            Ok(d)
        }
        _ => Err(bad_filter("not a document")),
    }
}

fn bad_filter(e: impl fmt::Display) -> AuditError {
    bad(format!("Bad auditFilter: {e}"))
}

/// Checks `doc` as RefDb checks an audit filter (the cluster parameter `auditConfig`).
pub fn check_filter(doc: &Document) -> Result<(), AuditError> {
    parse_filter(doc).map(|_| ())
}

fn parse_filter(doc: &Document) -> Result<Filter, AuditError> {
    let bytes = doc.to_bytes().map_err(bad_filter)?;
    let raw = RawDoc::from_bytes(&bytes).map_err(bad_filter)?;
    Filter::parse(raw, FilterUse::Validator)
        .map_err(|e| bad(format!("Failed to parse auditFilter :: caused by :: {}", e.message)))
}

/// One end of the connection of an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// An address and port.
    Address {
        /// The IP address, without brackets.
        ip: String,
        /// The port.
        port: u16,
    },
    /// A Unix socket.
    Unix(String),
    /// The node itself (startup, shutdown, internal work).
    SystemUser,
}

impl Endpoint {
    /// `host:port`, `[v6]:port` or a socket path as the server knows a connection's ends.
    pub fn parse(text: &str) -> Option<Endpoint> {
        if text.starts_with('/') {
            return Some(Endpoint::Unix(text.to_owned()));
        }
        let (host, port) = text.rsplit_once(':')?;
        let port = port.parse().ok()?;
        let ip = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        if ip.is_empty() {
            return None;
        }
        Some(Endpoint::Address { ip: ip.to_owned(), port })
    }

    /// The document of `local` or `remote`.
    pub fn to_document(&self) -> Document {
        match self {
            Endpoint::Address { ip, port } => {
                Document::new().with("ip", ip.as_str()).with("port", i32::from(*port))
            }
            Endpoint::Unix(path) => Document::new().with("unix", path.as_str()),
            Endpoint::SystemUser => Document::new().with("isSystemUser", true),
        }
    }
}

/// Who an event is about: the connection and its authenticated users.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    /// The connection's identifier (a UUID).
    pub uuid: [u8; 16],
    /// The node's end.
    pub local: Endpoint,
    /// The client's end.
    pub remote: Endpoint,
    /// The users authenticated on the connection.
    pub users: Vec<UserName>,
    /// Their roles, as granted to them.
    pub roles: Vec<RoleName>,
}

impl Client {
    /// The node itself, for the events no client causes.
    pub fn system(uuid: [u8; 16]) -> Client {
        Client {
            uuid,
            local: Endpoint::SystemUser,
            remote: Endpoint::SystemUser,
            users: Vec::new(),
            roles: Vec::new(),
        }
    }
}

/// One audit message.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// The action type.
    pub atype: AuditType,
    /// When, in milliseconds since the epoch.
    pub ts_ms: i64,
    /// The parameters of the action type ([`crate::audit_param`]).
    pub param: Document,
    /// 0 or RefDb's error code.
    pub result: i32,
}

impl Event {
    /// The message in RefDb's order of fields: `atype`, `ts`, `uuid`, `local`, `remote`,
    /// `directRemote` (for a client), `users`, `roles`, `param`, `result`.
    pub fn to_document(&self, client: &Client) -> Document {
        let users = client
            .users
            .iter()
            .map(|u| {
                Value::Document(
                    Document::new().with("user", u.user.as_str()).with("db", u.db.as_str()),
                )
            })
            .collect();
        let mut doc = Document::new()
            .with("atype", self.atype.name())
            .with("ts", Value::DateTime(self.ts_ms))
            .with("uuid", Value::Binary { subtype: 4, bytes: client.uuid.to_vec() })
            .with("local", client.local.to_document())
            .with("remote", client.remote.to_document());
        if client.remote != Endpoint::SystemUser {
            doc.push("directRemote", client.remote.to_document());
        }
        doc.push("users", Value::Array(users));
        doc.push("roles", Value::Array(client.roles.iter().map(RoleName::to_value).collect()));
        doc.push("param", self.param.clone());
        doc.push("result", self.result);
        doc
    }
}

/// The audit log of a node: which events it records and their bytes.
#[derive(Debug)]
pub struct Auditor {
    config: Config,
    /// The time of the `auditConfig` it applies, as (seconds, increment).
    runtime_version: Option<(u32, u32)>,
}

impl Auditor {
    /// The audit log `config` describes.
    pub fn new(config: Config) -> Auditor {
        Auditor { config, runtime_version: None }
    }

    /// Its configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The cluster parameter `auditConfig` while the node runs: its filter, which RefDb checks
    /// as an audit filter (the old one stays on error), and `auditAuthorizationSuccess`.
    pub fn configure(
        &mut self,
        filter: &Document,
        authorization_success: bool,
        version: (u32, u32),
    ) -> Result<(), AuditError> {
        let parsed = parse_filter(filter)?;
        self.runtime_version = Some(version);
        self.config.filter = Some(parsed);
        self.config.filter_doc = Some(filter.clone());
        self.config.authorization_success = authorization_success;
        Ok(())
    }

    /// The header an encrypted file starts with when it opens, if it is encrypted.
    pub fn encryption_header(&self, ts_ms: i64) -> Option<Vec<u8>> {
        let enc = self.config.encryption.as_ref()?;
        let header = enc.header(ts_ms);
        Some(match self.config.format {
            Format::Bson => header.to_bytes().ok()?,
            Format::Json => {
                let mut line = crate::audit_crypt::to_json(&header);
                line.push('\n');
                line.into_bytes()
            }
        })
    }

    /// The time of the `auditConfig` applied, if one was.
    pub fn runtime_version(&self) -> Option<(u32, u32)> {
        self.runtime_version
    }

    /// Sets `auditAuthorizationSuccess`.
    pub fn set_authorization_success(&mut self, on: bool) {
        self.config.authorization_success = on;
    }

    /// Whether an authorization check with `result` is recorded at all (failures always,
    /// successes with `auditAuthorizationSuccess`).
    pub fn records_auth_check(&self, result: i32) -> bool {
        result != 0 || self.config.authorization_success
    }

    /// The bytes to write for `event` of `client`, or `None` when the filter leaves it out: a
    /// JSON line with its newline for a file or the console, the JSON alone for the system log,
    /// the BSON document for a BSON file. A message the filter cannot evaluate is recorded.
    pub fn render(&self, event: &Event, client: &Client) -> Option<Vec<u8>> {
        let doc = match self.config.schema {
            Schema::Ref => event.to_document(client),
            Schema::Ocsf => {
                crate::audit_ocsf::document(event, client, self.config.process.as_ref())
            }
        };
        let bytes = doc.to_bytes().ok()?;
        let raw = RawDoc::from_bytes(&bytes).ok()?;
        let encryption = self
            .config
            .encryption
            .as_ref()
            .filter(|_| self.config.destination == Destination::File);
        // `auditConfigure` goes through every filter (measured on 9.0.4).
        if let Some(filter) =
            self.config.filter.as_ref().filter(|_| event.atype != AuditType::AuditConfigure)
        {
            let ctx = MatchContext { collation: None, ext: &NoExtensions, vars: None };
            if !filter.matches(raw, &ctx).unwrap_or(true) {
                return None;
            }
        }
        if let Some(enc) = encryption {
            let record = enc.record(event.ts_ms, &bytes);
            return Some(match self.config.format {
                Format::Bson => record.to_bytes().ok()?,
                Format::Json => {
                    let mut line = crate::audit_crypt::to_json(&record);
                    line.push('\n');
                    line.into_bytes()
                }
            });
        }
        let json = |raw| match self.config.schema {
            Schema::Ref => extjson::to_legacy(raw),
            Schema::Ocsf => extjson::to_relaxed_uuid(raw),
        };
        Some(match (self.config.format, self.config.destination) {
            (Format::Bson, _) => bytes,
            (Format::Json, Destination::Syslog) => json(raw).into_bytes(),
            (Format::Json, _) => {
                let mut line = json(raw);
                line.push('\n');
                line.into_bytes()
            }
        })
    }
}
