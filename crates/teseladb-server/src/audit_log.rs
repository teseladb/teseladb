//! The audit log of a node (013 FR-016): `--auditDestination`, `--auditFormat`, `--auditPath`
//! and `--auditFilter`, checked with RefDb's messages, and where the messages go: a file
//! created with permissions 0600 and appended to, the standard output, or the system log.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use teseladb_auth::audit::{
    AuditType, Auditor, Client, Config, Destination, Event, Options, ProcessInfo,
};
use teseladb_auth::audit_crypt::AuditEncryption;
use teseladb_bson::Document;
use teseladb_wire::commands::audit::{Audit, SharedAuditor, Sink};

/// Permissions of the audit file, as RefDb creates it.
const FILE_MODE: u32 = 0o600;
/// The system log's sockets, Linux first.
const SYSLOG_SOCKETS: [&str; 2] = ["/dev/log", "/var/run/syslog"];
/// Priority of a message in the system log: facility `user` (1) and level `info` (6).
const SYSLOG_PRIORITY: u8 = 14;
/// Name the messages carry in the system log.
const SYSLOG_TAG: &str = "teseladb";

/// The audit options as given (`auditLog.*` of the configuration file or the command line).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditOpts {
    /// `--auditDestination`.
    pub destination: Option<String>,
    /// `--auditFormat`.
    pub format: Option<String>,
    /// `--auditPath`.
    pub path: Option<String>,
    /// `--auditFilter`.
    pub filter: Option<String>,
    /// `--auditRuntimeConfiguration` (013 FR-031).
    pub runtime_configuration: bool,
    /// `--auditSchema`.
    pub schema: Option<String>,
    /// `--setParameter auditAuthorizationSuccess`.
    pub authorization_success: Option<bool>,
    /// `--auditLocalKeyFile` (013 FR-031).
    pub local_key_file: Option<String>,
    /// `--auditCompressionMode`.
    pub compression_mode: Option<String>,
    /// `--auditEncryptionKeyUID`.
    pub encryption_key_uid: Option<String>,
    /// The KMIP server of `--kmipServerName` and its options.
    pub kmip: Option<crate::kmip::KmipOpts>,
}

impl AuditOpts {
    fn options(&self) -> Options<'_> {
        Options {
            destination: self.destination.as_deref(),
            format: self.format.as_deref(),
            path: self.path.as_deref(),
            filter: self.filter.as_deref(),
            runtime_configuration: self.runtime_configuration,
            authorization_success_set: self.authorization_success.is_some(),
            schema: self.schema.as_deref(),
            local_key_file: self.local_key_file.as_deref(),
            compression_mode: self.compression_mode.as_deref(),
            encryption_key_uid: self.encryption_key_uid.as_deref(),
            kmip_server_set: self.kmip.is_some(),
        }
    }

    /// The configuration the options describe, `None` without an audit log, or RefDb's
    /// message.
    pub fn config(&self) -> Result<Option<Config>, String> {
        let mut config = Config::from_options(&self.options()).map_err(|e| e.0)?;
        if let Some(c) = config.as_mut() {
            c.authorization_success = self.authorization_success.unwrap_or(false);
        }
        Ok(config)
    }
}

/// Where the bytes go.
enum Target {
    File(File),
    Console,
    Syslog(UnixDatagram),
}

impl Target {
    /// Opens where `config` sends the messages; the system log through the first of `sockets`
    /// that takes a connection.
    fn open(config: &Config, sockets: &[&str]) -> Result<Target, String> {
        match config.destination {
            Destination::File => {
                // The configuration always carries the file's path with this destination.
                let path = config.path.clone().unwrap_or_default();
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(FILE_MODE)
                    .open(PathBuf::from(&path))
                    .map(Target::File)
                    .map_err(|e| format!("auditLog.path {path}: {e}"))
            }
            Destination::Console => Ok(Target::Console),
            Destination::Syslog => sockets
                .iter()
                .find_map(|p| UnixDatagram::unbound().and_then(|s| s.connect(p).map(|()| s)).ok())
                .map(Target::Syslog)
                .ok_or_else(|| "syslog: no system log socket".to_owned()),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        // An audit message that cannot be written is lost with the device; the node goes on, as
        // the log does.
        let _ = match self {
            Target::File(f) => f.write_all(bytes).and_then(|()| f.flush()),
            Target::Console => {
                let mut out = std::io::stdout().lock();
                out.write_all(bytes).and_then(|()| out.flush())
            }
            Target::Syslog(s) => {
                let mut msg = format!("<{SYSLOG_PRIORITY}>{SYSLOG_TAG}: ").into_bytes();
                msg.extend_from_slice(bytes);
                s.send(&msg).map(|_| ())
            }
        };
    }
}

/// The sink of every worker of a node, shared.
#[derive(Clone)]
struct Shared(Arc<Mutex<Target>>);

impl Sink for Shared {
    fn write(&mut self, bytes: &[u8]) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).write(bytes);
    }
}

/// Fills a buffer with random bytes.
type Random = fn(&mut [u8]) -> std::io::Result<()>;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The encryption of an audit file with `--auditEncryptionKeyUID`: the log's key is drawn now
/// and the KMIP server wraps it with Encrypt under the key `uid` (measured on 9.0.4).
fn kmip_encryption(
    config: &Config,
    uid: &str,
    kmip: &crate::kmip::KmipOpts,
    random: Random,
) -> Result<AuditEncryption, String> {
    use teseladb_auth::kmip::{Answer, Request};
    let mut log_key = [0u8; 32];
    random(&mut log_key).map_err(crate::error_text)?;
    let mut session = crate::kmip::Session::connect(kmip)?;
    match session.ask(&Request::Encrypt(uid.to_owned(), log_key.to_vec())) {
        Ok(Answer::Encrypted { data, iv }) => Ok(AuditEncryption::with_kmip(
            log_key,
            uid,
            &kmip.servers,
            kmip.port,
            (&data, &iv),
            config.compression,
        )),
        Ok(_) => {
            Err(format!("KMIP encrypt with key '{uid}' failed: an answer of another operation"))
        }
        Err(e) => Err(format!("KMIP encrypt with key '{uid}' failed: {e}")),
    }
}

/// The encryption of an audit file with `--auditLocalKeyFile`: the key file holds a 32-byte key
/// in Base64; the log's own key and the IV that wraps it are drawn now. RefDb's messages for a
/// file it cannot read or a key it cannot decode (measured).
fn encryption(config: &Config, random: Random) -> Result<Option<AuditEncryption>, String> {
    let Some(path) = &config.local_key_file else { return Ok(None) };
    let text = std::fs::read_to_string(path).map_err(|e| {
        let why = match e.kind() {
            std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
            _ => e.to_string(),
        };
        format!("Error reading file {path}: {why}")
    })?;
    let key = teseladb_x509::pem::base64_decode(text.trim().as_bytes())
        .ok()
        .and_then(|k| <[u8; 32]>::try_from(k).ok())
        .ok_or_else(|| "invalid base64".to_owned())?;
    let mut log_key = [0u8; 32];
    let mut iv = [0u8; 16];
    random(&mut log_key).and_then(|()| random(&mut iv)).map_err(crate::error_text)?;
    Ok(Some(AuditEncryption::new(&key, log_key, iv, path, config.compression)))
}

/// A node's audit log: its options, what it records and the open destination.
#[derive(Clone)]
pub struct AuditLog {
    opts: AuditOpts,
    auditor: SharedAuditor,
    sink: Shared,
}

impl fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuditLog").field("opts", &self.opts).finish()
    }
}

impl PartialEq for AuditLog {
    /// The same open destination.
    fn eq(&self, other: &AuditLog) -> bool {
        Arc::ptr_eq(&self.sink.0, &other.sink.0)
    }
}

impl Eq for AuditLog {}

/// Where Linux names its distribution, and where macOS keeps its version.
const OS_RELEASE: &str = "/etc/os-release";
const MACOS_VERSION: &str = "/System/Library/CoreServices/SystemVersion.plist";
/// The system's name on macOS, as the OCSF messages give it.
const MACOS: &str = "macOS";

/// The process and its system, for the OCSF messages (013 FR-031): `NAME` and `VERSION_ID` of
/// `os_release` on Linux, the `ProductVersion` of `macos` on macOS; empty when neither reads.
fn process_info(os_release: &str, macos: &str) -> ProcessInfo {
    let mut info = ProcessInfo { pid: std::process::id(), ..ProcessInfo::default() };
    if let Ok(text) = std::fs::read_to_string(os_release) {
        for line in text.lines() {
            let unquote = |v: &str| v.trim_matches('"').to_owned();
            if let Some(v) = line.strip_prefix("NAME=") {
                info.os_name = unquote(v);
            } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
                info.os_version = unquote(v);
            }
        }
    } else if let Ok(text) = std::fs::read_to_string(macos) {
        info.os_name = MACOS.to_owned();
        let after = text.split("<key>ProductVersion</key>").nth(1).unwrap_or("");
        let value = after.split("<string>").nth(1).and_then(|v| v.split("</string>").next());
        info.os_version = value.unwrap_or("").to_owned();
    }
    info
}

impl AuditLog {
    /// Opens the destination of `opts`; `None` without an audit log.
    pub fn open(opts: &AuditOpts) -> Result<Option<AuditLog>, String> {
        AuditLog::open_with(opts, &SYSLOG_SOCKETS)
    }

    fn open_with(opts: &AuditOpts, sockets: &[&str]) -> Result<Option<AuditLog>, String> {
        let Some(mut config) = opts.config()? else { return Ok(None) };
        config.process = Some(process_info(OS_RELEASE, MACOS_VERSION));
        config.encryption = match (&config.encryption_key_uid, &opts.kmip) {
            (Some(uid), Some(kmip)) => {
                Some(kmip_encryption(&config, uid, kmip, teseladb_sys::fill_random)?)
            }
            _ => encryption(&config, teseladb_sys::fill_random)?,
        };
        let mut target = Target::open(&config, sockets)?;
        let auditor = Auditor::new(config);
        if let (Target::File(_), Some(header)) = (&target, auditor.encryption_header(now_ms())) {
            target.write(&header);
        }
        Ok(Some(AuditLog {
            opts: opts.clone(),
            auditor: Arc::new(RwLock::new(auditor)),
            sink: Shared(Arc::new(Mutex::new(target))),
        }))
    }

    /// The audit log of the node's commands: the same auditor and destination.
    pub fn for_node(&self) -> Audit {
        Audit::new(Arc::clone(&self.auditor), Box::new(self.sink.clone()))
    }

    /// Records an event of the node itself (`startup`, `shutdown`).
    pub fn system(&self, atype: AuditType, param: Document) {
        self.system_with(atype, param, teseladb_sys::fill_random);
    }

    /// [`AuditLog::system`] with the client's identifier drawn from `random`; without one the
    /// event is not recorded.
    fn system_with(&self, atype: AuditType, param: Document, random: Random) {
        let mut uuid = [0u8; 16];
        if random(&mut uuid).is_err() {
            return;
        }
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let event = Event { atype, ts_ms, param, result: 0 };
        let auditor = self.auditor.read().unwrap_or_else(PoisonError::into_inner);
        let bytes = auditor.render(&event, &Client::system(uuid));
        drop(auditor);
        if let Some(bytes) = bytes {
            self.sink.clone().write(&bytes);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_auth::audit::{Destination, Format};

    fn opts(destination: Destination, path: Option<String>, filter: Option<&str>) -> AuditOpts {
        AuditOpts {
            destination: Some(destination.name().to_owned()),
            format: (destination == Destination::File).then(|| Format::Json.name().to_owned()),
            path,
            filter: filter.map(str::to_owned),
            ..AuditOpts::default()
        }
    }

    #[test]
    fn an_encrypted_file_starts_with_its_header_and_seals_each_message() {
        let dir = std::env::temp_dir().join(format!("teseladb-audit-enc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("audit.key");
        std::fs::write(&key, format!("{}\n", teseladb_x509::pem::base64_encode(&[5; 32]))).unwrap();
        let path = dir.join("audit.json").to_str().unwrap().to_owned();
        let with = |key: &std::path::Path, mode: Option<&str>| AuditOpts {
            local_key_file: Some(key.to_str().unwrap().to_owned()),
            compression_mode: mode.map(str::to_owned),
            ..opts(Destination::File, Some(path.clone()), None)
        };
        let log = AuditLog::open(&with(&key, Some("zstd"))).unwrap().unwrap();
        log.system(AuditType::Startup, Document::new());
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].contains(r#""compressionMode":"zstd""#)
                && lines[0].contains(r#""auditRecordType":"header""#),
            "{text}"
        );
        assert!(lines[1].contains(r#""log":""#) && !text.contains("startup"), "{text}");
        // A key file that is missing or holds no key, RefDb's messages.
        let missing = dir.join("none.key");
        let e = AuditLog::open(&with(&missing, None)).unwrap_err();
        assert_eq!(
            e,
            format!("Error reading file {}: No such file or directory", missing.display())
        );
        let short = dir.join("short.key");
        std::fs::write(&short, "short").unwrap();
        assert_eq!(AuditLog::open(&with(&short, None)).unwrap_err(), "invalid base64");
        let failing: Random = |_| Err(std::io::Error::other("no entropy"));
        let config = with(&key, None).config().unwrap().unwrap();
        assert!(encryption(&config, failing).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_process_and_its_system_are_read_for_ocsf() {
        let dir = std::env::temp_dir().join(format!("teseladb-osrel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rel = dir.join("os-release");
        std::fs::write(&rel, "NAME=\"Ubuntu\"\nVERSION_ID=\"22.04\"\nID=ubuntu\n").unwrap();
        let plist = dir.join("SystemVersion.plist");
        std::fs::write(&plist, "<dict><key>ProductVersion</key>\n<string>26.1</string></dict>")
            .unwrap();
        let none = dir.join("none");
        let (rel, plist, none) =
            (rel.to_str().unwrap(), plist.to_str().unwrap(), none.to_str().unwrap());
        let linux = process_info(rel, none);
        assert_eq!((linux.os_name.as_str(), linux.os_version.as_str()), ("Ubuntu", "22.04"));
        assert_eq!(linux.pid, std::process::id());
        let mac = process_info(none, plist);
        assert_eq!((mac.os_name.as_str(), mac.os_version.as_str()), (MACOS, "26.1"));
        let neither = process_info(none, none);
        assert!(neither.os_name.is_empty() && neither.os_version.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_destination_takes_the_messages_its_filter_lets_through() {
        let dir = std::env::temp_dir().join(format!("teseladb-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.json").to_str().unwrap().to_owned();
        let log =
            AuditLog::open(&opts(Destination::File, Some(path.clone()), None)).unwrap().unwrap();
        assert!(format!("{log:?}").contains("AuditLog") && log == log.clone());
        assert!(format!("{:?}", log.for_node()).contains("Audit"));
        log.system(AuditType::Startup, Document::new());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""atype" : "startup""#), "{text}");
        // A filter that leaves the type out writes nothing.
        let only = Some(r#"{atype: "authenticate"}"#);
        let quiet = dir.join("quiet.json").to_str().unwrap().to_owned();
        let log =
            AuditLog::open(&opts(Destination::File, Some(quiet.clone()), only)).unwrap().unwrap();
        log.system(AuditType::Shutdown, Document::new());
        assert_eq!(std::fs::read_to_string(&quiet).unwrap(), "");
        // A file that cannot be created, and no options at all.
        let bad = dir.join("none/a.json").to_str().unwrap().to_owned();
        assert!(
            AuditLog::open(&opts(Destination::File, Some(bad), None))
                .unwrap_err()
                .contains("auditLog.path")
        );
        assert_eq!(AuditLog::open(&AuditOpts::default()), Ok(None));
        // The standard output.
        let console = AuditLog::open(&opts(Destination::Console, None, None)).unwrap().unwrap();
        console.system(AuditType::Startup, Document::new());
        // A random source that fails: the event is not recorded.
        let failing =
            AuditLog::open(&opts(Destination::File, Some(quiet.clone()), None)).unwrap().unwrap();
        failing.system_with(AuditType::Startup, Document::new(), |_| {
            Err(std::io::Error::other("no entropy"))
        });
        assert_eq!(std::fs::read_to_string(&quiet).unwrap(), "");
        // The system log: no socket takes the connection, then the first one that does.
        let syslog = opts(Destination::Syslog, None, None);
        let missing = dir.join("missing.sock").to_str().unwrap().to_owned();
        assert_eq!(
            AuditLog::open_with(&syslog, &[missing.as_str()]),
            Err("syslog: no system log socket".to_owned())
        );
        let sock = dir.join("log.sock").to_str().unwrap().to_owned();
        let server = UnixDatagram::bind(&sock).unwrap();
        let l = AuditLog::open_with(&syslog, &[missing.as_str(), sock.as_str()]).unwrap().unwrap();
        l.system(AuditType::Startup, Document::new());
        let mut buf = [0u8; 4096];
        let n = server.recv(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
        assert!(msg.starts_with("<14>teseladb: {"), "{msg}");
        assert!(msg.contains(r#""atype" : "startup""#), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
