//! The parent's side of a node process: [`NodeProc`] starts it, waits for it, asks it and stops,
//! kills or restarts it.

use super::lines::*;
use super::*;

/// Why a node process did not start: what its error line says, or how it ended, with its log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartError {
    /// The kind of [`NodeError`] (`locked`, `config`, `join`, `io:<ErrorKind>`, `format:<n>`,
    /// `storage`, `lsm`), or `exit` when the process ended without saying why.
    pub kind: String,
    /// The error's message.
    pub message: String,
    /// The process's log.
    pub log: String,
}

impl StartError {
    /// The [`NodeError`] the node process reported, as near as a line carries it: a storage or
    /// LSM error comes back as [`NodeError::Config`] with its message.
    #[must_use]
    pub fn node_error(&self) -> NodeError {
        let m = self.message.clone();
        match self.kind.as_str() {
            "locked" => NodeError::Locked,
            "join" => NodeError::Join(m),
            k => {
                if let Some(kind) = k.strip_prefix("io:") {
                    return NodeError::Io(io::Error::new(error_kind(kind), m));
                }
                match k.strip_prefix("format:").and_then(|n| n.parse().ok()) {
                    Some(found) => NodeError::DataFormat { found },
                    None => NodeError::Config(m),
                }
            }
        }
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}); log:\n{}", self.message, self.kind, self.log)
    }
}

/// The kinds of [`io::Error`] a node reports when it cannot start.
const IO_KINDS: &[io::ErrorKind] = &[
    io::ErrorKind::AddrInUse,
    io::ErrorKind::AddrNotAvailable,
    io::ErrorKind::AlreadyExists,
    io::ErrorKind::ConnectionRefused,
    io::ErrorKind::InvalidData,
    io::ErrorKind::InvalidInput,
    io::ErrorKind::NotFound,
    io::ErrorKind::PermissionDenied,
    io::ErrorKind::StorageFull,
    io::ErrorKind::TimedOut,
    io::ErrorKind::UnexpectedEof,
    io::ErrorKind::Unsupported,
    io::ErrorKind::WouldBlock,
];

pub(super) fn error_kind(name: &str) -> io::ErrorKind {
    IO_KINDS.iter().copied().find(|k| format!("{k:?}") == name).unwrap_or(io::ErrorKind::Other)
}

/// A node in a child process. Dropping it stops the process (`SIGTERM`, then `SIGKILL` past
/// [`STOP_WAIT`]); during a panic it kills it and prints the end of its log.
#[derive(Debug)]
pub struct NodeProc {
    child: Child,
    stdin: Mutex<Option<ChildStdin>>,
    lines: Mutex<Receiver<String>>,
    program: PathBuf,
    args: Vec<String>,
    /// Its log file; `None`: its standard error is this process's.
    log: Option<PathBuf>,
    ports: Ports,
    cores: u16,
    exited: Option<ExitStatus>,
    /// An earlier run [`Self::restart`] replaced: it owns neither the process nor the log.
    replaced: bool,
}

static LOGS: AtomicU64 = AtomicU64::new(0);

/// A log file of its own for a node process of this process.
#[must_use]
pub fn log_path(name: &str) -> PathBuf {
    let n = LOGS.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("teseladb-proc-{}-{n}-{name}.log", std::process::id()))
}

/// Starts this binary (a test's, whose `main` takes [`NODE_PROCESS`]) as a node process of
/// `recipe` joining `join` (none: it creates the cluster), in memory or, with `dir`, on files there;
/// its log goes to a file of its own ([`log_path`]).
pub fn spawn_self(
    recipe: &str,
    join: &[String],
    dir: Option<&Path>,
    params: &[&str],
) -> Result<NodeProc, StartError> {
    let a = NodeArgs {
        recipe: recipe.to_owned(),
        join: join.to_vec(),
        dir: dir.map(Path::to_owned),
        params: params.iter().map(|p| (*p).to_owned()).collect(),
        ports: None,
    };
    let me = std::env::current_exe().map_err(|e| StartError {
        kind: "exit".to_owned(),
        message: format!("the path of this program: {e}"),
        log: String::new(),
    })?;
    NodeProc::spawn(&me, &a.to_args(), Some(&log_path(recipe)))
}

/// [`spawn_self`], which has to start.
///
/// # Panics
///
/// If the node process does not start, with its error and log.
#[must_use]
pub fn start_self(recipe: &str, join: &[String], dir: Option<&Path>, params: &[&str]) -> NodeProc {
    spawn_self(recipe, join, dir, params).unwrap_or_else(|e| panic!("node `{recipe}`: {e}"))
}

impl NodeProc {
    /// Starts `program` with `args` (a node process), its log in `log` (`None`: on this process's
    /// standard error), and waits for its member line.
    pub fn spawn(
        program: &Path,
        args: &[String],
        log: Option<&Path>,
    ) -> Result<NodeProc, StartError> {
        let fail = |kind: &str, message: String| StartError {
            kind: kind.to_owned(),
            message,
            log: log.and_then(|l| fs::read_to_string(l).ok()).unwrap_or_default(),
        };
        let stderr = match log {
            Some(l) => Stdio::from(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(l)
                    .map_err(|e| fail("exit", format!("{}: {e}", l.display())))?,
            ),
            None => Stdio::inherit(),
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|e| fail("exit", format!("{} does not start: {e}", program.display())))?;
        let out = child.stdout.take().expect("piped standard output");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let first = lines.recv_timeout(START_WAIT);
        let reported = |kind: &str, message: String, mut child: Child| {
            let _ = child.kill();
            let _ = child.wait();
            fail(kind, message)
        };
        let line = match first {
            Ok(l) => l,
            Err(_) => {
                let status = child.try_wait().ok().flatten();
                return Err(reported("exit", format!("no member line; ended {status:?}"), child));
            }
        };
        if let Some(rest) = line.strip_prefix(ERROR).map(str::trim_start) {
            let (kind, message) = rest.split_once(' ').unwrap_or((rest, ""));
            return Err(reported(kind, message.to_owned(), child));
        }
        let Some(fields) = line.strip_prefix(MEMBER) else {
            return Err(reported("exit", format!("not a member line: {line}"), child));
        };
        let field =
            |k: &str| fields.split_whitespace().find_map(|f| f.strip_prefix(k)?.strip_prefix('='));
        let parsed = (|| {
            Some(Ports {
                peer: field("peer")?.parse().ok()?,
                clients: parse_addrs(field("clients")?).ok()?,
                admin: parse_addrs(field("admin")?).ok()?.first().copied(),
                http: parse_addrs(field("http")?).ok()?,
            })
        })();
        let (Some(ports), Some(cores)) = (parsed, field("cores").and_then(|c| c.parse().ok()))
        else {
            return Err(reported("exit", format!("a member line without ports: {line}"), child));
        };
        let stdin = child.stdin.take();
        Ok(NodeProc {
            child,
            stdin: Mutex::new(stdin),
            lines: Mutex::new(lines),
            program: program.to_owned(),
            args: args.to_vec(),
            log: log.map(Path::to_owned),
            ports,
            cores,
            exited: None,
            replaced: false,
        })
    }

    /// The process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Inter-node address.
    #[must_use]
    pub fn peer_addr(&self) -> SocketAddr {
        self.ports.peer
    }

    /// Clients' addresses.
    #[must_use]
    pub fn client_addrs(&self) -> &[SocketAddr] {
        &self.ports.clients
    }

    /// Administration port.
    #[must_use]
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.ports.admin
    }

    /// HTTP port's addresses.
    #[must_use]
    pub fn http_addrs(&self) -> Vec<SocketAddr> {
        self.ports.http.clone()
    }

    /// The node's cores.
    #[must_use]
    pub fn cores(&self) -> u16 {
        self.cores
    }

    /// Where its log goes (`None`: this process's standard error).
    #[must_use]
    pub fn log_path(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// What its log holds so far.
    #[must_use]
    pub fn log_text(&self) -> String {
        self.log.as_ref().and_then(|l| fs::read_to_string(l).ok()).unwrap_or_default()
    }

    /// Sends `request` and returns its answer line; `wait` is the request's own deadline.
    ///
    /// # Panics
    ///
    /// If the process does not answer within `wait` and a margin.
    pub fn ask(&self, request: &str, wait: Duration) -> String {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stdin = self.stdin.lock().unwrap_or_else(PoisonError::into_inner);
        let sent = match stdin.as_mut() {
            Some(s) => writeln!(s, "{request}").and_then(|()| s.flush()),
            None => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        };
        if let Err(e) = sent {
            panic!("node process {} takes no `{request}`: {e}", self.pid());
        }
        lines.recv_timeout(wait + ANSWER_MARGIN).unwrap_or_else(|e| {
            panic!("node process {}: no answer to `{request}`: {e}", self.pid())
        })
    }

    /// Whether the node serves within `timeout`: its `/healthz` answers 200 (its `ready` request
    /// when its administration port takes no plain HTTP).
    #[must_use]
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let Some(admin) = self.ports.admin.map(|a| a.to_string()) else {
            return self.ask_ready(timeout);
        };
        loop {
            match admin_http::request(&admin, Method::Get, crate::admin::HEALTHZ, b"") {
                Ok((200, _)) => return true,
                Ok(_) => {}
                Err(_) => return self.ask_ready(end.saturating_duration_since(Instant::now())),
            }
            if Instant::now() >= end {
                return false;
            }
            thread::sleep(POLL);
        }
    }

    fn ask_ready(&self, timeout: Duration) -> bool {
        self.ask(&format!("ready {}", timeout.as_millis()), timeout) == "ok 1"
    }

    fn id_fields(&self) -> Vec<String> {
        let a = self.ask("id", ASK);
        let words: Vec<String> = a.split_whitespace().map(str::to_owned).collect();
        assert!(words.len() == 4 && words[0] == "ok", "id: {a}");
        words
    }

    /// The node's identifier (0 until it joined).
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        NodeId(self.id_fields()[1].parse().expect("a node id"))
    }

    /// The cluster's identifier.
    #[must_use]
    pub fn cluster_id(&self) -> [u8; 16] {
        let bytes = unhex(&self.id_fields()[2]).expect("hexadecimal");
        bytes.try_into().expect("16 bytes")
    }

    /// The inter-node address the node advertises.
    #[must_use]
    pub fn advertised_peer(&self) -> String {
        self.id_fields()[3].clone()
    }

    /// What core 0's copy of Group 0 shows.
    #[must_use]
    pub fn view(&self, timeout: Duration) -> Option<Group0View> {
        let a = self.ask(&format!("view {}", timeout.as_millis()), timeout);
        (a != "none").then(|| parse_view(&a).unwrap_or_else(|| panic!("a view: {a}")))
    }

    /// The node's replica of the data range.
    #[must_use]
    pub fn data_status(&self, timeout: Duration) -> Option<DataStatus> {
        let a = self.ask(&format!("status {}", timeout.as_millis()), timeout);
        (a != "none").then(|| parse_status(&a).unwrap_or_else(|| panic!("a status: {a}")))
    }

    /// The node's replica of range `g`.
    #[must_use]
    pub fn range_status(&self, g: GroupId, timeout: Duration) -> Option<DataStatus> {
        let a = self.ask(&format!("range {} {}", g.0, timeout.as_millis()), timeout);
        (a != "none").then(|| parse_status(&a).unwrap_or_else(|| panic!("a status: {a}")))
    }

    /// The node's routing table.
    #[must_use]
    pub fn ranges(&self) -> Vec<RouteEntry> {
        let a = self.ask("ranges", ASK);
        let rest = a.strip_prefix("ok").unwrap_or_else(|| panic!("ranges: {a}"));
        rest.split_whitespace()
            .map(|e| parse_route(e).unwrap_or_else(|| panic!("a route: {e}")))
            .collect()
    }

    /// Writes `value` at `key` through the node with the majority, as [`Node::put`].
    pub fn put(&self, key: &[u8], value: &[u8], timeout: Duration) -> Result<Hlc, DataError> {
        self.put_with(key, value, WriteConcern { w: W::Majority, j: true }, timeout)
    }

    /// Writes `value` at `key` through the node with `wc`, as [`Node::put_with`]. An error comes
    /// back as [`DataError::NoAnswer`], a [`DataError::Range`] of a range error without fields or
    /// [`RangeError::NotLeader`], or else [`DataError::Unexpected`] with its text.
    pub fn put_with(
        &self,
        key: &[u8],
        value: &[u8],
        wc: WriteConcern,
        timeout: Duration,
    ) -> Result<Hlc, DataError> {
        let req =
            format!("put {} {} {} {}", hex(key), hex(value), wc_text(wc), timeout.as_millis());
        let a = self.ask(&req, timeout);
        let mut w = a.split_whitespace();
        match (w.next(), w.next().map(str::parse), w.next().map(str::parse)) {
            (Some("ok"), Some(Ok(physical)), Some(Ok(logical))) => Ok(Hlc { physical, logical }),
            _ => Err(parse_data_error(&a)),
        }
    }

    /// Reads `key` through the node, as [`Node::get`]; errors as [`Self::put_with`]'s.
    pub fn get(&self, key: &[u8], timeout: Duration) -> Result<Option<Vec<u8>>, DataError> {
        let a = self.ask(&format!("get {} {}", hex(key), timeout.as_millis()), timeout);
        match a.split_once(' ') {
            Some(("ok", v)) => unhex(v).map(Some).ok_or_else(|| parse_data_error(&a)),
            _ if a == "none" => Ok(None),
            _ => Err(parse_data_error(&a)),
        }
    }

    /// A node enrollment token valid for `ttl`.
    pub fn create_enroll_token(&self, ttl: Duration) -> Result<String, String> {
        let a = self.ask(&format!("token {}", ttl.as_secs()), ASK);
        match a.split_once(' ') {
            Some(("ok", t)) => Ok(t.to_owned()),
            Some(("err", e)) => Err(e.to_owned()),
            _ => Err(a),
        }
    }

    /// Stops the node: `SIGTERM`, and `SIGKILL` if it has not ended after [`STOP_WAIT`].
    pub fn terminate(&mut self) -> ExitStatus {
        if let Some(s) = self.exited {
            return s;
        }
        let _ =
            teseladb_sys::process::send_stop(self.pid(), teseladb_sys::process::Stop::Terminate);
        let end = Instant::now() + STOP_WAIT;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() < end => thread::sleep(POLL),
                _ => {
                    let _ = self.child.kill();
                    break self.child.wait().expect("the node process ends");
                }
            }
        };
        self.exited = Some(status);
        status
    }

    /// How the node's process ended, if it did, without waiting.
    pub fn ended(&mut self) -> Option<ExitStatus> {
        if self.exited.is_none() {
            self.exited = self.child.try_wait().ok().flatten();
        }
        self.exited
    }

    /// Stops the node ([`Self::terminate`]) and returns how it ended.
    pub fn stop(mut self) -> ExitStatus {
        self.terminate()
    }

    /// Kills the node with `SIGKILL`, as a crash does.
    pub fn kill(&mut self) {
        if self.exited.is_none() {
            let _ = self.child.kill();
            self.exited = Some(self.child.wait().expect("the node process ends"));
        }
    }

    /// Starts the node again, after it stopped or was killed, on the same ports and data.
    pub fn restart(&mut self) -> Result<(), StartError> {
        self.terminate();
        let mut args: Vec<String> = Vec::with_capacity(self.args.len() + 2);
        let mut rest = self.args.iter();
        while let Some(a) = rest.next() {
            if a == PORTS {
                rest.next();
            } else {
                args.push(a.clone());
            }
        }
        args.extend([PORTS.to_owned(), self.ports.to_arg()]);
        // The new run's log goes after the earlier one's, in the same file.
        let mut next = NodeProc::spawn(&self.program, &args, self.log.as_deref())?;
        std::mem::swap(self, &mut next);
        // `next` is the earlier run, already waited for; the log file is the new run's now.
        next.replaced = true;
        Ok(())
    }
}

impl Drop for NodeProc {
    fn drop(&mut self) {
        if self.replaced {
            return;
        }
        if thread::panicking() {
            self.kill();
            let text = self.log_text();
            let lines: Vec<&str> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(LOG_TAIL)..];
            let at = self.log.as_ref().map_or_else(String::new, |l| l.display().to_string());
            eprintln!("--- log of node process {} ({at}):", self.pid());
            tail.iter().for_each(|l| eprintln!("{l}"));
            return;
        }
        self.terminate();
        if let Some(l) = &self.log {
            let _ = fs::remove_file(l);
        }
    }
}
