//! Backups at a mark and their restore into an empty cluster (007 FR-021, D-028, plan «Copias de
//! seguridad», contract `admin-cli.md`).
//!
//! `teseladb admin backup --at <mark|now> --out <dir>` asks the node for the backup's mark, the
//! catalog and the data ranges ([`Op::Start`]), then fetches the files of each range piece by
//! piece from the range's leader through that node ([`Op::Fetch`], [`source`]) and writes them in
//! `<dir>/ranges/<range>/` with `manifest.bson`, which lists every file with its SHA-256.
//!
//! `teseladb admin restore --from <dir>` checks every file against the manifest first, then asks
//! the node whether the cluster is empty ([`Op::Prepare`]: no collections and one data range, with
//! its replicas), sends the files to every replica of that range ([`Op::Piece`]), each of which
//! ingests them ([`Op::Ingest`]) before the catalog makes the collections visible
//! ([`Op::Finish`]). A replica added later takes the data with a snapshot, as any new replica
//! does (005, `transfer`).

pub mod source;

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_hlc::Hlc;
use teseladb_hlc::{from_timestamp, to_timestamp};
use teseladb_meta::command::Group0Command;
use teseladb_types::{Decoder, GroupId, NodeId};

use crate::admin::{AdminError, AdminOpts, Answer, NodeAdmin};
use crate::admin_http::{HttpRequest, HttpResponse, Method};
use crate::catalog_ops::{CatalogAnswer, CatalogReq};
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;

/// The first key a backup copies: documents and index entries (`01`) and transaction records
/// (`02`), whose intents the documents may still carry.
pub const BACKUP_START: [u8; 1] = [teseladb_keys::space::DATA];
/// The first key past what a backup copies: the system tables (`03`) belong to the cluster that
/// holds them.
pub const BACKUP_END: [u8; 1] = [teseladb_keys::space::SYSTEM];
/// Path of the port that starts a backup.
pub const BACKUP: &str = "/admin/backup";
/// Path of the port under which a restore goes.
pub const RESTORE: &str = "/admin/restore";
/// The manifest's file in a backup's directory.
pub const MANIFEST: &str = "manifest.bson";
/// The manifest's format.
pub const FORMAT: i32 = 1;
fn node_value(n: NodeId) -> Value {
    Value::Int64(i64::try_from(n.0).unwrap_or(i64::MAX))
}

/// Bytes of a restore's piece: its body, in Extended JSON, stays under the port's limit.
pub const RESTORE_PIECE: usize = 512 * 1024;
/// How long the CLI waits for a range's export, or for the restore's range to keep one replica.
pub const WAIT: Duration = Duration::from_secs(120);
/// Pause between two questions while waiting.
const PAUSE: Duration = Duration::from_millis(100);

/// What the port does for a backup or a restore.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// The mark (`None`: now), the cluster, its catalog and its data ranges.
    Start(Option<Hlc>),
    /// A piece of range `range`'s export at `at`.
    Fetch {
        /// Range.
        range: GroupId,
        /// The backup's mark.
        at: Hlc,
        /// File.
        file: u32,
        /// Where in the file.
        offset: u64,
    },
    /// Checks that the cluster is empty: its data range and the range's replicas.
    Prepare,
    /// A piece of a restore's file for the replica of range `range` on node `node`.
    Piece {
        /// Range.
        range: GroupId,
        /// The replica's node.
        node: NodeId,
        /// File.
        file: u32,
        /// Where in the file.
        offset: u64,
        /// The bytes.
        data: Vec<u8>,
    },
    /// The replica of range `range` on node `node` ingests the `files` files of the backup at
    /// mark `at`.
    Ingest {
        /// Range.
        range: GroupId,
        /// The replica's node.
        node: NodeId,
        /// How many files.
        files: u32,
        /// The backup's mark.
        at: Hlc,
    },
    /// The catalog of the backup, encoded as `RestoreCatalog` carries it.
    Finish {
        /// Catalog and index definitions.
        catalog: Vec<u8>,
    },
}

fn mark_text(h: Hlc) -> String {
    let (t, i) = to_timestamp(h);
    format!("{t}.{i}")
}

fn parse_mark(text: &str) -> Result<Hlc, String> {
    let bad = || format!("not a mark: {text} (seconds.increment of a timestamp, or now)");
    let (t, i) = text.split_once(['.', ',']).ok_or_else(bad)?;
    Ok(from_timestamp((t.parse().map_err(|_| bad())?, i.parse().map_err(|_| bad())?)))
}

fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        Value::Double(f) if f.fract() == 0.0 => Some(*f as i64),
        _ => None,
    }
}

fn binary(v: Option<&Value>) -> Option<Vec<u8>> {
    match v? {
        Value::Binary { bytes, .. } => Some(bytes.clone()),
        _ => None,
    }
}

fn timestamp(v: Option<&Value>) -> Option<Hlc> {
    match v? {
        Value::Timestamp { time, increment } => Some(from_timestamp((*time, *increment))),
        _ => None,
    }
}

fn body(req: &HttpRequest) -> Result<Document, AdminError> {
    teseladb_rest::media::decode(&req.body, teseladb_rest::media::Format::Relaxed)
        .map_err(|e| AdminError::bad(format!("the body is not an Extended JSON document: {e:?}")))
}

/// The operation of a request under [`BACKUP`] or [`RESTORE`]: `POST /admin/backup?at=`,
/// `GET /admin/backup/<range>/<file>?at=&offset=`, `POST /admin/restore/prepare`,
/// `POST /admin/restore/<range>/<node>/<file>?offset=` with `{data}`,
/// `POST /admin/restore/<range>/<node>/ingest` with `{files, at}` and
/// `POST /admin/restore/finish` with `{catalog}`. `None`: no such route.
pub fn parse(req: &HttpRequest) -> Option<Result<Op, AdminError>> {
    let post = req.method == Method::Post;
    let range = |s: &str| {
        s.parse::<u64>().map(GroupId).map_err(|_| AdminError::bad(format!("not a range: {s}")))
    };
    let number = |name: &str| -> Result<u64, AdminError> {
        req.param(name)
            .unwrap_or("0")
            .parse()
            .map_err(|_| AdminError::bad(format!("{name}: a number")))
    };
    let path = req.path.as_str();
    if path == BACKUP && post {
        return Some(match req.param("at") {
            None | Some("now") => Ok(Op::Start(None)),
            Some(t) => parse_mark(t).map(|h| Op::Start(Some(h))).map_err(AdminError::bad),
        });
    }
    if let Some(rest) = path.strip_prefix(BACKUP).and_then(|r| r.strip_prefix('/')) {
        let (r, file) = rest.split_once('/')?;
        return Some((|| {
            let at = parse_mark(req.param("at").unwrap_or_default()).map_err(AdminError::bad)?;
            let file =
                u32::try_from(file.parse::<u64>().map_err(|_| AdminError::bad("not a file"))?)
                    .map_err(|_| AdminError::bad("not a file"))?;
            Ok(Op::Fetch { range: range(r)?, at, file, offset: number("offset")? })
        })());
    }
    let rest = path.strip_prefix(RESTORE).and_then(|r| r.strip_prefix('/'))?;
    if !post {
        return None;
    }
    if rest == "prepare" {
        return Some(Ok(Op::Prepare));
    }
    if rest == "finish" {
        return Some(body(req).and_then(|d| {
            let catalog =
                binary(d.get("catalog")).ok_or_else(|| AdminError::bad("catalog: binary data"))?;
            Ok(Op::Finish { catalog })
        }));
    }
    let (r, rest) = rest.split_once('/')?;
    let (n, what) = rest.split_once('/')?;
    Some((|| {
        let range = range(r)?;
        let node = NodeId(n.parse().map_err(|_| AdminError::bad(format!("not a node: {n}")))?);
        let d = body(req)?;
        if what == "ingest" {
            let files = int(d.get("files")).and_then(|n| u32::try_from(n).ok());
            let files = files.ok_or_else(|| AdminError::bad("files: a number"))?;
            let at = timestamp(d.get("at")).ok_or_else(|| AdminError::bad("at: a timestamp"))?;
            return Ok(Op::Ingest { range, node, files, at });
        }
        let file =
            what.parse::<u32>().map_err(|_| AdminError::bad(format!("not a file: {what}")))?;
        let data = binary(d.get("data")).ok_or_else(|| AdminError::bad("data: binary data"))?;
        Ok(Op::Piece { range, node, file, offset: number("offset")?, data })
    })())
}

/// The answer of the port to a request under [`BACKUP`] or [`RESTORE`], if it is one of theirs.
pub fn route(req: &HttpRequest, run: impl FnOnce(Op) -> Answer) -> Option<HttpResponse> {
    let op = parse(req)?;
    Some(crate::admin::json(op.and_then(run)))
}

/// The ranges of the node's table a backup copies, with the core of each.
fn data_ranges(a: &NodeAdmin) -> Vec<teseladb_router::table::RouteEntry> {
    a.handle
        .routes
        .entries()
        .into_iter()
        .filter(|e| {
            let (s, en) = source::backed_up(&e.start, &e.end);
            s < en
        })
        .collect()
}

/// The nodes a restore of range `entry` sends its files to (007 FR-021): every replica of Group
/// 0's descriptor `desc` of it, learners too, since a replica the routing table does not list as a
/// voter yet would otherwise keep the range as it was before the restore.
fn restore_nodes(
    entry: &teseladb_router::table::RouteEntry,
    desc: Option<&teseladb_meta::group0::RangeDesc>,
) -> Vec<NodeId> {
    match desc.filter(|d| d.group == entry.group) {
        Some(d) => d.replicas.iter().map(|(n, _)| *n).collect(),
        None => entry.voters.clone(),
    }
}

/// A request of a backup for range `range`'s leader (`node` `None`), or of a restore for its
/// replica on `node`, through the core of this node that holds the range or forwards it.
fn remote(
    a: &NodeAdmin,
    (range, node): (GroupId, Option<NodeId>),
    req: RemoteReq,
) -> Result<RemoteReply, AdminError> {
    let core = a.handle.routes.entry(range).map_or(0, |e| usize::from(e.core));
    let msg = match node {
        Some(n) => DataReq::At(n, range, req),
        None => DataReq::Remote(range, req),
    };
    match a.ask(core, |tx| NodeMsg::Data(msg, tx)) {
        Some(Ok(DataReply::Remote(r))) => Ok(r),
        Some(Err(teseladb_range::host::RangeError::Mutation(m))) => Err(AdminError::bad(m.0)),
        Some(Err(e)) => Err(AdminError::unavailable(format!("range {}: {e:?}", range.0))),
        Some(Ok(_)) | None => {
            Err(AdminError::unavailable(format!("range {} did not answer", range.0)))
        }
    }
}

/// The `RestoreCatalog` of a backup's catalog and index definitions, with its tenants and
/// security state when it has them (007 FR-037).
fn restore_catalog(bytes: &[u8]) -> Result<Group0Command, AdminError> {
    let mut d = Decoder::new(bytes);
    let read = teseladb_meta::command::decode_restore(&mut d).and_then(|r| d.finish().map(|()| r));
    let (catalog, indexes, security) =
        read.map_err(|e| AdminError::bad(format!("the backup's catalog does not read: {e:?}")))?;
    Ok(Group0Command::RestoreCatalog {
        catalog: Box::new(catalog),
        indexes: Box::new(indexes),
        security: security.map(Box::new),
    })
}

/// The text of an error that has no other.
fn debug_text(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}

/// Runs `op` on node `a`.
pub fn run_op(a: &NodeAdmin, op: Op) -> Answer {
    match op {
        Op::Start(at) => {
            let Some(view) = a.ask(0, NodeMsg::View).filter(|v| v.bootstrapped) else {
                return Err(AdminError::unavailable("Group 0 did not answer"));
            };
            let catalog = match a.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Dump, tx)) {
                Some(CatalogAnswer::Dump(d)) => d,
                _ => return Err(AdminError::unavailable("the catalog did not answer")),
            };
            let now = Hlc { physical: crate::clock::wall_ms(), logical: 0 };
            let at = at.unwrap_or_else(|| a.handle.clock.0.peek().max(now));
            let (time, increment) = to_timestamp(at);
            let ranges: Vec<Value> = data_ranges(a)
                .into_iter()
                .map(|e| {
                    let (s, en) = source::backed_up(&e.start, &e.end);
                    Value::Document(
                        Document::new()
                            .with(
                                "range",
                                Value::Int64(i64::try_from(e.group.0).unwrap_or(i64::MAX)),
                            )
                            .with("start", Value::Binary { subtype: 0, bytes: s })
                            .with("end", Value::Binary { subtype: 0, bytes: en }),
                    )
                })
                .collect();
            Ok(Document::new()
                .with("format", FORMAT)
                .with("clusterId", Value::Binary { subtype: 4, bytes: view.cluster_id.to_vec() })
                .with("at", Value::Timestamp { time, increment })
                .with("catalog", Value::Binary { subtype: 0, bytes: catalog })
                .with("ranges", Value::Array(ranges)))
        }
        Op::Fetch { range, at, file, offset } => {
            match remote(a, (range, None), RemoteReq::BackupFetch { at, file, offset })? {
                RemoteReply::Piece { ready, sizes, data } => {
                    let sizes: Vec<Value> = sizes
                        .into_iter()
                        .map(|s| Value::Int64(i64::try_from(s).unwrap_or(i64::MAX)))
                        .collect();
                    Ok(Document::new()
                        .with("ready", ready)
                        .with("sizes", Value::Array(sizes))
                        .with("data", Value::Binary { subtype: 0, bytes: data }))
                }
                other => Err(AdminError::unavailable(format!("unexpected answer {other:?}"))),
            }
        }
        Op::Prepare => {
            no_collections(a)?;
            // A node that has just started may not know its data range yet.
            let until = Instant::now() + crate::admin::CORE_WAIT;
            let mut ranges = data_ranges(a);
            while ranges.is_empty() && Instant::now() < until {
                std::thread::sleep(PAUSE);
                ranges = data_ranges(a);
            }
            let [only] = ranges.as_slice() else {
                return Err(AdminError::bad(format!(
                    "the cluster has {} data ranges, and a restore needs an empty cluster with one",
                    ranges.len()
                )));
            };
            let view = a.ask(0, NodeMsg::View);
            let desc = view.as_ref().and_then(|v| v.data_range.as_ref());
            let nodes: Vec<Value> = restore_nodes(only, desc).into_iter().map(node_value).collect();
            Ok(Document::new()
                .with("range", Value::Int64(i64::try_from(only.group.0).unwrap_or(i64::MAX)))
                .with("nodes", Value::Array(nodes)))
        }
        Op::Piece { range, node, file, offset, data } => {
            remote(a, (range, Some(node)), RemoteReq::RestorePiece { file, offset, data })?;
            Ok(Document::new().with("ok", 1.0))
        }
        Op::Ingest { range, node, files, at } => {
            // The replica clears what a restore cut before this one left in its range, which
            // only a cluster without collections may lose.
            no_collections(a)?;
            // The node's clock passes the backup's mark: the frames it sends carry it to the
            // range's replicas, so every read after the restore sees the data.
            a.handle.clock.0.update(at);
            match remote(a, (range, Some(node)), RemoteReq::RestoreIngest { files })? {
                RemoteReply::Restored { done } => Ok(Document::new().with("done", done)),
                other => Err(AdminError::unavailable(format!("unexpected answer {other:?}"))),
            }
        }
        Op::Finish { catalog } => {
            a.group0(restore_catalog(&catalog)?)?;
            Ok(Document::new().with("ok", 1.0))
        }
    }
}

/// `BadValue` unless the cluster has no collections, the only one a restore goes into.
fn no_collections(a: &NodeAdmin) -> Result<(), AdminError> {
    match a.catalog()?.collections.is_empty() {
        true => Ok(()),
        false => Err(AdminError::bad(
            "the cluster is not empty: it has collections, and a restore needs an empty cluster",
        )),
    }
}

/// The CLI's side: `backup` writes the backup of the node's cluster into `out`.
pub struct Cli<'a> {
    /// What to call the node with.
    pub call: &'a dyn Fn(Method, &str, &[u8]) -> Result<Document, String>,
    /// How long it waits for an export or an ingestion ([`WAIT`]).
    pub wait: Duration,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn sizes_of(d: &Document) -> Vec<u64> {
    match d.get("sizes") {
        Some(Value::Array(a)) => {
            a.iter().filter_map(|v| int(Some(v))).filter_map(|n| u64::try_from(n).ok()).collect()
        }
        _ => Vec::new(),
    }
}

impl Cli<'_> {
    fn post(&self, target: &str, body: &Document) -> Result<Document, String> {
        let body = teseladb_rest::media::encode(body, teseladb_rest::media::Format::Relaxed)
            .map_err(debug_text)?;
        (self.call)(Method::Post, target, &body)
    }

    /// Writes the backup at `at` (`None`: now) into `out`; the manifest's document.
    pub fn backup(&self, at: Option<&str>, out: &Path) -> Result<Document, String> {
        let target = match at {
            None | Some("now") => BACKUP.to_owned(),
            Some(t) => format!("{BACKUP}?at={}", mark_text(parse_mark(t)?)),
        };
        let mut manifest = (self.call)(Method::Post, &target, b"")?;
        let at = timestamp(manifest.get("at")).ok_or("the node's answer has no mark")?;
        let Some(Value::Array(ranges)) = manifest.get("ranges").cloned() else {
            return Err("the node's answer has no ranges".to_owned());
        };
        let mut with_files = Vec::new();
        for r in ranges {
            let Value::Document(mut r) = r else { continue };
            let range = int(r.get("range")).ok_or("a range without its number")?;
            let files = self.fetch_range(range, at, out)?;
            r.insert("files", Value::Array(files));
            with_files.push(Value::Document(r));
        }
        manifest.insert("ranges", Value::Array(with_files));
        let bytes = manifest.to_bytes().map_err(debug_text)?;
        std::fs::write(out.join(MANIFEST), bytes).map_err(|e| format!("{MANIFEST}: {e}"))?;
        Ok(manifest)
    }

    /// Fetches every file of range `range` at `at` into `out/ranges/<range>/`.
    fn fetch_range(&self, range: i64, at: Hlc, out: &Path) -> Result<Vec<Value>, String> {
        let dir = PathBuf::from("ranges").join(range.to_string());
        std::fs::create_dir_all(out.join(&dir)).map_err(|e| format!("{}: {e}", dir.display()))?;
        let piece = |file: usize, offset: u64| {
            let t = format!("{BACKUP}/{range}/{file}?at={}&offset={offset}", mark_text(at));
            (self.call)(Method::Get, &t, b"")
        };
        // The first piece waits for the export.
        let until = Instant::now() + self.wait;
        let first = loop {
            let d = piece(0, 0)?;
            if d.get("ready") == Some(&Value::Boolean(true)) {
                break d;
            }
            if Instant::now() >= until {
                return Err(format!("range {range} did not export its data in time"));
            }
            std::thread::sleep(PAUSE);
        };
        let sizes = sizes_of(&first);
        let mut files = Vec::new();
        let mut next = Some(first);
        for (n, size) in sizes.iter().enumerate() {
            let name = dir.join(format!("{n:06}.sst"));
            let mut bytes = Vec::new();
            while (bytes.len() as u64) < *size {
                let d = match next.take() {
                    Some(d) => d,
                    None => piece(n, bytes.len() as u64)?,
                };
                let data = binary(d.get("data")).ok_or("a piece without data")?;
                if data.is_empty() {
                    return Err(format!("range {range} sent an empty piece"));
                }
                bytes.extend_from_slice(&data);
            }
            std::fs::write(out.join(&name), &bytes)
                .map_err(|e| format!("{}: {e}", name.display()))?;
            files.push(Value::Document(
                Document::new()
                    .with("name", name.to_string_lossy().into_owned())
                    .with("size", Value::Int64(i64::try_from(bytes.len()).unwrap_or(i64::MAX)))
                    .with("sha256", hex(&teseladb_hash::sha256(&bytes))),
            ));
        }
        Ok(files)
    }

    /// Restores the backup in `from` into the node's cluster, which must be empty.
    pub fn restore(&self, from: &Path) -> Result<(), String> {
        let bytes = std::fs::read(from.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
        let raw =
            RawDoc::from_bytes(&bytes).map_err(|e| format!("{MANIFEST} does not read: {e:?}"))?;
        let manifest = Document::from(raw);
        if int(manifest.get("format")) != Some(i64::from(FORMAT)) {
            return Err(format!("{MANIFEST}: not a backup of format {FORMAT}"));
        }
        let at = manifest.get("at").cloned().ok_or("the manifest has no mark")?;
        let catalog = binary(manifest.get("catalog")).ok_or("the manifest has no catalog")?;
        // Every file is checked before anything changes.
        let mut files = Vec::new();
        let Some(Value::Array(ranges)) = manifest.get("ranges") else {
            return Err("the manifest has no ranges".to_owned());
        };
        for r in ranges {
            let Value::Document(r) = r else { continue };
            let Some(Value::Array(list)) = r.get("files") else { continue };
            for f in list {
                let Value::Document(f) = f else { continue };
                let Some(Value::String(name)) = f.get("name") else {
                    return Err("a file of the manifest has no name".to_owned());
                };
                let data = std::fs::read(from.join(name)).map_err(|e| format!("{name}: {e}"))?;
                let sum = hex(&teseladb_hash::sha256(&data));
                if f.get("sha256") != Some(&Value::String(sum)) {
                    return Err(format!("{name}: its SHA-256 is not the manifest's"));
                }
                files.push(data);
            }
        }
        let prepared = self.post(&format!("{RESTORE}/prepare"), &Document::new())?;
        let range = int(prepared.get("range")).ok_or("the node named no range")?;
        let nodes: Vec<i64> = match prepared.get("nodes") {
            Some(Value::Array(a)) => a.iter().filter_map(|v| int(Some(v))).collect(),
            _ => Vec::new(),
        };
        // A backup of no data leaves nothing to ingest.
        if !files.is_empty() {
            for node in &nodes {
                self.load((range, *node), &files, &at)?;
            }
        }
        let finish = Document::new().with("catalog", Value::Binary { subtype: 0, bytes: catalog });
        self.post(&format!("{RESTORE}/finish"), &finish).map(|_| ())
    }

    /// Sends the files to the replica of `range` on `node` and has them ingested, from the first
    /// piece again while [`Cli::wait`] lasts if a step fails: a replica just added may not have
    /// opened the range yet, and one whose core started again lost the pieces it had.
    fn load(&self, (range, node): (i64, i64), files: &[Vec<u8>], at: &Value) -> Result<(), String> {
        let until = Instant::now() + self.wait;
        loop {
            match self.load_once((range, node), files, at, until) {
                Err(_) if Instant::now() < until => std::thread::sleep(PAUSE),
                done => return done,
            }
        }
    }

    fn load_once(
        &self,
        (range, node): (i64, i64),
        files: &[Vec<u8>],
        at: &Value,
        until: Instant,
    ) -> Result<(), String> {
        for (n, data) in files.iter().enumerate() {
            let mut offset = 0;
            // An empty file still sends its one piece, so that the replica knows it.
            loop {
                let end = (offset + RESTORE_PIECE).min(data.len());
                let piece = Document::new()
                    .with("data", Value::Binary { subtype: 0, bytes: data[offset..end].to_vec() });
                self.post(&format!("{RESTORE}/{range}/{node}/{n}?offset={offset}"), &piece)?;
                offset = end;
                if offset >= data.len() {
                    break;
                }
            }
        }
        let count = i64::try_from(files.len()).unwrap_or(i64::MAX);
        let ingest = Document::new().with("files", Value::Int64(count)).with("at", at.clone());
        loop {
            let d = self.post(&format!("{RESTORE}/{range}/{node}/ingest"), &ingest)?;
            if d.get("done") == Some(&Value::Boolean(true)) {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(format!(
                    "the replica of range {range} on node {node} did not ingest the files in time"
                ));
            }
            std::thread::sleep(PAUSE);
        }
    }
}

/// Runs `backup` (`Ok(out)`) or `restore` (`Err(from)`) of `teseladb admin` against the node's
/// port, with the mark of `--at`; the exit code: 0 correct, 1 error.
pub fn run(
    o: &AdminOpts,
    what: Result<&Path, &Path>,
    at: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let tls = match o.tls.client() {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(err, "teseladb admin: {e}");
            return 1;
        }
    };
    let call = |method: Method, target: &str, body: &[u8]| -> Result<Document, String> {
        let (status, answer) =
            crate::admin_http::request_with(&o.node, tls.clone(), method, target, body)
                .map_err(|e| format!("cannot reach {}: {e}", o.node))?;
        let doc = teseladb_rest::media::decode(&answer, teseladb_rest::media::Format::Relaxed)
            .map_err(|e| format!("the node's answer does not read: {e:?}"))?;
        match (status, doc.get("error")) {
            (200, _) => Ok(doc),
            (_, Some(Value::String(m))) => Err(m.clone()),
            _ => Err(format!("HTTP {status}")),
        }
    };
    let cli = Cli { call: &call, wait: WAIT };
    let done = match what {
        Ok(dir) => std::fs::create_dir_all(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))
            .and_then(|()| cli.backup(at, dir))
            .map(|m| {
                format!(
                    "backup at {} in {}",
                    m.get("at").map_or_else(String::new, teseladb_wire::commands::display),
                    dir.display()
                )
            }),
        Err(dir) => cli.restore(dir).map(|()| format!("restored from {}", dir.display())),
    };
    match done {
        Ok(text) => {
            let _ = writeln!(out, "{text}");
            0
        }
        Err(e) => {
            let _ = writeln!(err, "teseladb admin: {e}");
            1
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn a_restore_goes_to_every_replica_of_the_range_learners_too() {
        use teseladb_meta::group0::{RangeDesc, ReplicaRole};
        let roles = [(1, ReplicaRole::Voter), (2, ReplicaRole::Learner), (3, ReplicaRole::Voter)];
        let desc = RangeDesc {
            group: GroupId(1),
            start: Vec::new(),
            end: Vec::new(),
            core: 0,
            replicas: roles.iter().map(|(n, r)| (NodeId(*n), *r)).collect(),
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: true,
            moved_ms: 0,
        };
        let entry = teseladb_router::table::RouteEntry::from_desc(&desc);
        assert_eq!(restore_nodes(&entry, Some(&desc)), vec![NodeId(1), NodeId(2), NodeId(3)]);
        // Another range's descriptor, or none: the routing table's voters.
        let other = RangeDesc { group: GroupId(9), ..desc.clone() };
        assert_eq!(restore_nodes(&entry, Some(&other)), vec![NodeId(1), NodeId(3)]);
        assert_eq!(restore_nodes(&entry, None), vec![NodeId(1), NodeId(3)]);
    }

    fn req(method: Method, target: &str, body: Document) -> HttpRequest {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let query = query
            .split('&')
            .filter(|q| !q.is_empty())
            .map(|q| {
                let (k, v) = q.split_once('=').unwrap_or((q, ""));
                (k.to_owned(), v.to_owned())
            })
            .collect();
        let body =
            teseladb_rest::media::encode(&body, teseladb_rest::media::Format::Relaxed).unwrap();
        HttpRequest { method, path: path.to_owned(), query, body }
    }

    fn op(method: Method, target: &str, body: Document) -> Option<Result<Op, AdminError>> {
        parse(&req(method, target, body))
    }

    fn bin(b: &[u8]) -> Value {
        Value::Binary { subtype: 0, bytes: b.to_vec() }
    }

    const MARK: Hlc = Hlc { physical: 1_700_000_000_123, logical: 4 };

    #[test]
    fn each_route_reads_its_operation() {
        let none = Document::new;
        assert_eq!(op(Method::Post, BACKUP, none()), Some(Ok(Op::Start(None))));
        assert_eq!(op(Method::Post, "/admin/backup?at=now", none()), Some(Ok(Op::Start(None))));
        let at = mark_text(MARK);
        let start = op(Method::Post, &format!("/admin/backup?at={at}"), none());
        assert_eq!(start, Some(Ok(Op::Start(Some(MARK)))));
        let fetch = op(Method::Get, &format!("/admin/backup/7/2?at={at}&offset=9"), none());
        assert_eq!(fetch, Some(Ok(Op::Fetch { range: GroupId(7), at: MARK, file: 2, offset: 9 })));
        assert_eq!(op(Method::Post, "/admin/restore/prepare", none()), Some(Ok(Op::Prepare)));
        let finish = op(Method::Post, "/admin/restore/finish", none().with("catalog", bin(&[1])));
        assert_eq!(finish, Some(Ok(Op::Finish { catalog: vec![1] })));
        let piece =
            op(Method::Post, "/admin/restore/7/2/3?offset=5", none().with("data", bin(&[9])));
        let want =
            Op::Piece { range: GroupId(7), node: NodeId(2), file: 3, offset: 5, data: vec![9] };
        assert_eq!(piece, Some(Ok(want)));
        let (t, i) = to_timestamp(MARK);
        let ingest = none().with("files", 4).with("at", Value::Timestamp { time: t, increment: i });
        let want = Op::Ingest { range: GroupId(7), node: NodeId(2), files: 4, at: MARK };
        assert_eq!(op(Method::Post, "/admin/restore/7/2/ingest", ingest), Some(Ok(want)));
    }

    #[test]
    fn what_is_not_a_route_of_theirs_goes_elsewhere_and_bad_values_are_refused() {
        let none = Document::new;
        // Not theirs.
        assert_eq!(op(Method::Get, "/admin/ranges", none()), None);
        assert_eq!(op(Method::Get, BACKUP, none()), None);
        assert_eq!(op(Method::Get, "/admin/restore/prepare", none()), None);
        assert_eq!(op(Method::Post, "/admin/restore/7", none()), None);
        assert_eq!(op(Method::Post, "/admin/restore/7/2", none()), None);
        assert_eq!(op(Method::Get, "/admin/backup/7", none()), None);
        let bad = |m: Method, t: &str, b: Document| matches!(op(m, t, b), Some(Err(_)));
        assert!(bad(Method::Post, "/admin/backup?at=x", none()));
        assert!(bad(Method::Post, "/admin/backup?at=1.x", none()));
        assert!(bad(Method::Post, "/admin/backup?at=x.1", none()));
        assert!(bad(Method::Get, "/admin/backup/x/0?at=1.1", none()));
        assert!(bad(Method::Get, "/admin/backup/7/x?at=1.1", none()));
        assert!(bad(Method::Get, "/admin/backup/7/99999999999?at=1.1", none()));
        assert!(bad(Method::Get, "/admin/backup/7/0", none()));
        assert!(bad(Method::Get, "/admin/backup/7/0?at=1.1&offset=x", none()));
        assert!(bad(Method::Post, "/admin/restore/finish", none()));
        let mut broken = req(Method::Post, "/admin/restore/finish", none());
        broken.body = b"{".to_vec();
        assert!(matches!(parse(&broken), Some(Err(_))));
        let data = || none().with("data", bin(&[1]));
        assert!(bad(Method::Post, "/admin/restore/x/2/0", data()));
        assert!(bad(Method::Post, "/admin/restore/7/x/0", data()));
        assert!(bad(Method::Post, "/admin/restore/7/2/x", data()));
        assert!(bad(Method::Post, "/admin/restore/7/2/0", none()));
        assert!(bad(Method::Post, "/admin/restore/7/2/ingest", none().with("files", 1)));
        assert!(bad(Method::Post, "/admin/restore/7/2/ingest", none().with("files", "a")));
        let mut broken = req(Method::Post, "/admin/restore/7/2/0", none());
        broken.body = b"[".to_vec();
        assert!(matches!(parse(&broken), Some(Err(_))));
        // The answer of a route of theirs.
        let r = route(&req(Method::Post, BACKUP, none()), |_| Ok(none().with("ok", 1)));
        assert_eq!(r.map(|r| r.status), Some(200));
        let r = route(&req(Method::Post, "/admin/backup?at=x", none()), |_| Ok(none()));
        assert_eq!(r.map(|r| r.status), Some(400));
        assert!(route(&req(Method::Get, "/admin/nodes", none()), |_| Ok(none())).is_none());
    }

    #[test]
    fn a_backups_catalog_reads_or_is_refused() {
        let mut e = teseladb_types::Encoder::new();
        teseladb_meta::catalog::Catalog::default().encode(&mut e);
        teseladb_meta::indexes::Indexes::default().encode(&mut e);
        assert!(matches!(restore_catalog(&e.finish()), Ok(Group0Command::RestoreCatalog { .. })));
        assert!(restore_catalog(&[9]).unwrap_err().message.contains("does not read"));
        assert_eq!(debug_text(1), "1");
    }

    #[test]
    fn the_cli_reports_a_node_it_cannot_reach_and_a_folder_it_cannot_make() {
        let dir = Dir::new("cli");
        let file = dir.0.join("plain");
        std::fs::write(&file, b"").unwrap();
        let words = |w: &[&str]| -> Vec<String> { w.iter().map(|s| (*s).to_owned()).collect() };
        let run_it = |w: &[&str]| {
            let o = crate::admin::parse(&words(w)).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = crate::admin::run(&o, &mut out, &mut err);
            (code, String::from_utf8(err).unwrap())
        };
        let out = dir.0.join("out");
        let (code, err) =
            run_it(&["--node", "127.0.0.1:1", "backup", "--out", out.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains("cannot reach"), "{err}");
        let inside = file.join("x");
        let (code, err) =
            run_it(&["--node", "127.0.0.1:1", "backup", "--out", inside.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains("plain"), "{err}");
        let (code, err) =
            run_it(&["--node", "127.0.0.1:1", "restore", "--from", out.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains(MANIFEST), "{err}");
        let creds = [
            "--node",
            "127.0.0.1:1",
            "--tlsCertificateKeyFile",
            "/nonexistent.pem",
            "--tlsCAFile",
            "/nonexistent.pem",
            "restore",
            "--from",
            "x",
        ];
        let (code, _) = run_it(&creds);
        assert_eq!(code, 1);
        // A file of the backup that cannot be written.
        let node = FakeNode::new(vec![b"ab".to_vec()]);
        let call = |m: Method, t: &str, b: &[u8]| node.call(m, t, b);
        let blocked = dir.0.join("blocked");
        std::fs::create_dir_all(blocked.join("ranges/5/000000.sst")).unwrap();
        assert!(cli(&call).backup(None, &blocked).unwrap_err().contains("000000.sst"));
    }

    #[test]
    fn values_read_as_numbers_binaries_and_marks() {
        assert_eq!(int(Some(&Value::Int32(3))), Some(3));
        assert_eq!(int(Some(&Value::Int64(4))), Some(4));
        assert_eq!(int(Some(&Value::Double(5.0))), Some(5));
        assert_eq!(int(Some(&Value::Double(5.5))), None);
        assert_eq!(int(Some(&Value::Null)), None);
        assert_eq!(int(None), None);
        assert_eq!(binary(Some(&Value::Null)), None);
        assert_eq!(timestamp(Some(&Value::Null)), None);
        assert_eq!(parse_mark(&mark_text(MARK)), Ok(MARK));
        assert_eq!(parse_mark("1,2"), Ok(from_timestamp((1, 2))));
        let d = Document::new().with("sizes", Value::Array(vec![Value::Int64(3), Value::Null]));
        assert_eq!(sizes_of(&d), vec![3]);
        assert_eq!(sizes_of(&Document::new()), Vec::<u64>::new());
        assert_eq!(hex(&[0, 255]), "00ff");
        assert_eq!(node_value(NodeId(3)), Value::Int64(3));
    }

    /// A node that answers the CLI from what it keeps: one range with `files`, each piece at most
    /// `piece` bytes; the export is ready from the `ready`-th question.
    struct FakeNode {
        files: Vec<Vec<u8>>,
        piece: usize,
        ready: usize,
        asked: RefCell<usize>,
        received: RefCell<BTreeMap<(i64, u32), Vec<u8>>>,
        log: RefCell<Vec<String>>,
        fail: Option<&'static str>,
    }

    impl FakeNode {
        fn new(files: Vec<Vec<u8>>) -> FakeNode {
            FakeNode {
                files,
                piece: 3,
                ready: 2,
                asked: RefCell::new(0),
                received: RefCell::new(BTreeMap::new()),
                log: RefCell::new(Vec::new()),
                fail: None,
            }
        }

        fn call(&self, method: Method, target: &str, body: &[u8]) -> Result<Document, String> {
            self.log.borrow_mut().push(target.to_owned());
            if self.fail.is_some_and(|f| target.starts_with(f)) {
                return Err(format!("refused {target}"));
            }
            let doc = match body.is_empty() {
                true => Document::new(),
                false => teseladb_rest::media::decode(body, teseladb_rest::media::Format::Relaxed)
                    .map_err(|e| format!("{e:?}"))?,
            };
            let r = req(method, target, Document::new());
            let r = HttpRequest { body: body.to_vec(), ..r };
            let (t, i) = to_timestamp(MARK);
            let answer = match parse(&r).expect("a route").map_err(|e| e.message)? {
                Op::Start(_) => Document::new()
                    .with("format", FORMAT)
                    .with("at", Value::Timestamp { time: t, increment: i })
                    .with("catalog", bin(&[1, 2]))
                    .with(
                        "ranges",
                        Value::Array(vec![Value::Document(
                            Document::new().with("range", 5).with("start", bin(&[1])),
                        )]),
                    ),
                Op::Fetch { file, offset, .. } => {
                    *self.asked.borrow_mut() += 1;
                    if *self.asked.borrow() < self.ready {
                        return Ok(Document::new().with("ready", false));
                    }
                    let sizes: Vec<Value> =
                        self.files.iter().map(|f| Value::Int64(f.len() as i64)).collect();
                    let f = self.files.get(file as usize).cloned().unwrap_or_default();
                    let from = (offset as usize).min(f.len());
                    let to = (from + self.piece).min(f.len());
                    Document::new()
                        .with("ready", true)
                        .with("sizes", Value::Array(sizes))
                        .with("data", bin(&f[from..to]))
                }
                Op::Prepare => Document::new()
                    .with("range", 5)
                    .with("nodes", Value::Array(vec![Value::Int64(1), Value::Int64(2)])),
                Op::Piece { node, file, offset, data, .. } => {
                    let mut rec = self.received.borrow_mut();
                    let buf = rec.entry((node.0 as i64, file)).or_default();
                    assert_eq!(buf.len() as u64, offset);
                    buf.extend_from_slice(&data);
                    Document::new().with("ok", 1)
                }
                Op::Ingest { .. } => {
                    *self.asked.borrow_mut() += 1;
                    Document::new().with("done", (*self.asked.borrow()).is_multiple_of(2))
                }
                Op::Finish { catalog } => {
                    assert_eq!(catalog, vec![1, 2]);
                    Document::new().with("ok", 1)
                }
            };
            let _ = doc;
            Ok(answer)
        }
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let p = std::env::temp_dir()
                .join(format!("teseladb-backup-unit-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cli<'a>(call: &'a dyn Fn(Method, &str, &[u8]) -> Result<Document, String>) -> Cli<'a> {
        Cli { call, wait: Duration::from_secs(5) }
    }

    #[test]
    fn a_backup_writes_each_file_and_the_manifest_and_a_restore_sends_them_to_every_replica() {
        let dir = Dir::new("round");
        let node = FakeNode::new(vec![b"abcdefgh".to_vec(), b"xy".to_vec()]);
        let call = |m: Method, t: &str, b: &[u8]| node.call(m, t, b);
        let c = cli(&call);
        let manifest = c.backup(Some(&mark_text(MARK)), &dir.0).unwrap();
        assert_eq!(timestamp(manifest.get("at")), Some(MARK));
        let first = std::fs::read(dir.0.join("ranges/5/000000.sst")).unwrap();
        assert_eq!(first, b"abcdefgh");
        assert!(node.log.borrow()[0].contains("at="), "the mark goes to the node");
        // The restore sends every file to each replica, then the catalog.
        *node.asked.borrow_mut() = 0;
        c.restore(&dir.0).unwrap();
        let rec = node.received.borrow();
        assert_eq!(rec.get(&(1, 0)).map(Vec::as_slice), Some(&b"abcdefgh"[..]));
        assert_eq!(rec.get(&(2, 1)).map(Vec::as_slice), Some(&b"xy"[..]));
        assert!(node.log.borrow().last().unwrap().ends_with("/finish"));
    }

    /// A replica that lost the pieces (its range opened or moved after they came) answers that no
    /// restore is under way: it gets every piece again and ingests them.
    #[test]
    fn a_replica_that_lost_its_pieces_gets_them_again() {
        let dir = Dir::new("again");
        let node = FakeNode::new(vec![b"abcd".to_vec()]);
        let failed = std::cell::Cell::new(false);
        let call = |m: Method, t: &str, b: &[u8]| {
            if t.ends_with("/2/ingest") && !failed.replace(true) {
                node.received.borrow_mut().remove(&(2, 0));
                return Err("no restore of this range is under way".to_owned());
            }
            node.call(m, t, b)
        };
        let c = cli(&call);
        c.backup(None, &dir.0).unwrap();
        *node.asked.borrow_mut() = 0;
        c.restore(&dir.0).unwrap();
        assert!(failed.get());
        assert_eq!(node.received.borrow().get(&(2, 0)).map(Vec::as_slice), Some(&b"abcd"[..]));
        let pieces = node.log.borrow().iter().filter(|t| t.contains("/5/2/0?offset=")).count();
        assert_eq!(pieces, 2, "{:?}", node.log.borrow());
    }

    #[test]
    fn a_restore_sends_a_file_larger_than_a_piece_in_several() {
        let dir = Dir::new("pieces");
        let big: Vec<u8> = (0..RESTORE_PIECE + 5).map(|i| (i % 251) as u8).collect();
        let mut node = FakeNode::new(vec![big.clone()]);
        node.piece = RESTORE_PIECE;
        let call = |m: Method, t: &str, b: &[u8]| node.call(m, t, b);
        let c = cli(&call);
        c.backup(None, &dir.0).unwrap();
        *node.asked.borrow_mut() = 0;
        c.restore(&dir.0).unwrap();
        assert_eq!(node.received.borrow().get(&(1, 0)), Some(&big));
        let pieces = node.log.borrow().iter().filter(|t| t.contains("/5/1/0?offset=")).count();
        assert_eq!(pieces, 2, "{:?}", node.log.borrow());
    }

    #[test]
    fn a_restore_checks_every_file_before_anything_changes() {
        let dir = Dir::new("check");
        let node = FakeNode::new(vec![b"abcd".to_vec()]);
        let call = |m: Method, t: &str, b: &[u8]| node.call(m, t, b);
        let c = cli(&call);
        // No manifest.
        assert!(c.restore(&dir.0).unwrap_err().contains(MANIFEST));
        std::fs::write(dir.0.join(MANIFEST), b"x").unwrap();
        assert!(c.restore(&dir.0).unwrap_err().contains("does not read"));
        let write =
            |d: Document| std::fs::write(dir.0.join(MANIFEST), d.to_bytes().unwrap()).unwrap();
        write(Document::new().with("format", 2));
        assert!(c.restore(&dir.0).unwrap_err().contains("format"));
        write(Document::new().with("format", FORMAT));
        assert!(c.restore(&dir.0).unwrap_err().contains("mark"));
        let (t, i) = to_timestamp(MARK);
        let base = || {
            Document::new()
                .with("format", FORMAT)
                .with("at", Value::Timestamp { time: t, increment: i })
        };
        write(base());
        assert!(c.restore(&dir.0).unwrap_err().contains("catalog"));
        write(base().with("catalog", bin(&[1, 2])));
        assert!(c.restore(&dir.0).unwrap_err().contains("ranges"));
        let file = |f: Document| {
            Value::Array(vec![
                Value::Null,
                Value::Document(Document::new()),
                Value::Document(
                    Document::new()
                        .with("files", Value::Array(vec![Value::Null, Value::Document(f)])),
                ),
            ])
        };
        write(base().with("catalog", bin(&[1, 2])).with("ranges", file(Document::new())));
        assert!(c.restore(&dir.0).unwrap_err().contains("no name"));
        let named = || Document::new().with("name", "f.sst");
        write(base().with("catalog", bin(&[1, 2])).with("ranges", file(named())));
        assert!(c.restore(&dir.0).unwrap_err().contains("f.sst"));
        std::fs::write(dir.0.join("f.sst"), b"abcd").unwrap();
        write(
            base().with("catalog", bin(&[1, 2])).with("ranges", file(named().with("sha256", "00"))),
        );
        assert!(c.restore(&dir.0).unwrap_err().contains("SHA-256"));
        assert!(node.log.borrow().is_empty(), "nothing reached the node");
        // A backup of no data restores the catalog alone.
        write(base().with("catalog", bin(&[1, 2])).with("ranges", Value::Array(vec![])));
        c.restore(&dir.0).unwrap();
        assert_eq!(node.log.borrow().len(), 2, "{:?}", node.log.borrow());
    }

    #[test]
    fn the_cli_reports_what_the_node_refuses_or_never_finishes() {
        let dir = Dir::new("refused");
        // The node's answers lack what the backup needs.
        let empty = |_: Method, _: &str, _: &[u8]| Ok(Document::new());
        assert!(cli(&empty).backup(None, &dir.0).unwrap_err().contains("mark"));
        let (t, i) = to_timestamp(MARK);
        let at = Document::new().with("at", Value::Timestamp { time: t, increment: i });
        let no_ranges = |_: Method, _: &str, _: &[u8]| Ok(at.clone());
        assert!(cli(&no_ranges).backup(None, &dir.0).unwrap_err().contains("ranges"));
        assert!(cli(&no_ranges).backup(Some("x"), &dir.0).unwrap_err().contains("mark"));
        let ranges = |v: Value| at.clone().with("ranges", Value::Array(vec![Value::Null, v]));
        let nameless = |_: Method, _: &str, _: &[u8]| Ok(ranges(Value::Document(Document::new())));
        assert!(cli(&nameless).backup(None, &dir.0).unwrap_err().contains("number"));
        // An export that is never ready, a piece without data, an empty piece.
        let mut node = FakeNode::new(vec![b"ab".to_vec()]);
        node.ready = usize::MAX;
        let call = |m: Method, t: &str, b: &[u8]| node.call(m, t, b);
        let c = Cli { call: &call, wait: Duration::ZERO };
        assert!(c.backup(None, &dir.0).unwrap_err().contains("in time"));
        let start = |d: Document| {
            move |_: Method, t: &str, _: &[u8]| match t.starts_with("/admin/backup/") {
                true => Ok(d.clone()),
                false => Ok(ranges(Value::Document(Document::new().with("range", 5)))),
            }
        };
        let sizes = Value::Array(vec![Value::Int64(2)]);
        let no_data = start(Document::new().with("ready", true).with("sizes", sizes.clone()));
        assert!(cli(&no_data).backup(None, &dir.0).unwrap_err().contains("without data"));
        let empty_piece =
            start(Document::new().with("ready", true).with("sizes", sizes).with("data", bin(&[])));
        assert!(cli(&empty_piece).backup(None, &dir.0).unwrap_err().contains("empty piece"));
        // Files that cannot be written.
        let file = dir.0.join("plain");
        std::fs::write(&file, b"").unwrap();
        let good = FakeNode::new(vec![b"ab".to_vec()]);
        let call = |m: Method, t: &str, b: &[u8]| good.call(m, t, b);
        assert!(cli(&call).backup(None, &file).is_err());
        // A restore whose node names no range, or whose ingestion never ends.
        let dir2 = Dir::new("refused2");
        let node = FakeNode::new(vec![b"ab".to_vec()]);
        let call = |m: Method, t: &str, b: &[u8]| node.call(m, t, b);
        cli(&call).backup(None, &dir2.0).unwrap();
        let mut stuck = FakeNode::new(vec![b"ab".to_vec()]);
        stuck.fail = Some("/admin/restore/prepare");
        let call = |m: Method, t: &str, b: &[u8]| stuck.call(m, t, b);
        assert!(cli(&call).restore(&dir2.0).unwrap_err().contains("refused"));
        let anon = |m: Method, t: &str, b: &[u8]| match t.ends_with("/prepare") {
            true => Ok(Document::new()),
            false => node.call(m, t, b),
        };
        assert!(cli(&anon).restore(&dir2.0).unwrap_err().contains("no range"));
        let never = |m: Method, t: &str, b: &[u8]| match t.ends_with("/ingest") {
            true => Ok(Document::new().with("done", false)),
            false => node.call(m, t, b),
        };
        let c = Cli { call: &never, wait: Duration::ZERO };
        assert!(c.restore(&dir2.0).unwrap_err().contains("in time"));
        let nodeless = |m: Method, t: &str, b: &[u8]| match t.ends_with("/prepare") {
            true => Ok(Document::new().with("range", 5)),
            false => node.call(m, t, b),
        };
        cli(&nodeless).restore(&dir2.0).unwrap();
    }
}
