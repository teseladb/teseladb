//! Rollback files (FR-010, D-034, model `WriteConcernW1.tla`): a write acknowledged with `w: 1` may
//! be lost if its leader falls before the majority has it. The replica whose log a later leader's
//! log overwrites hands the dropped entries out (`MultiRaftCore::take_rolled_back`), and the
//! embedder writes them under `<data>/rollback/`, as the reference server writes the documents a
//! rollback takes back to `<dbpath>/rollback/<collectionUUID>/removed.<timestamp>.bson`.
//!
//! The range layer does not know collections (006): a file holds what one range lost in one
//! rollback, at `rollback/range-<group>/removed.<YYYY-MM-DDTHH-MM-SS>.<term>.<index>.bson` (UTC
//! time of the rollback; the term and index of the first entry lost make the name unique). Its
//! content is a sequence of BSON documents, readable with `bsondump`: one per version a lost write
//! wrote, with its range, entry index and term, its mark as a BSON timestamp (FR-024) and its key,
//! value type and value, so that the layer of 006 can map each key to its collection and write the
//! reference server's files. As in the reference server, deletions are not written. An entry that
//! does not decode as a command of the range is kept whole as `raw`. This module is sans-I/O: it
//! builds and reads the bytes; the embedder creates the directory, writes the file and syncs it.

use std::collections::BTreeMap;

use teseladb_hlc::{Hlc, from_timestamp, to_timestamp};
use teseladb_multiraft::envelope;
use teseladb_raftlog::LogEntry;
use teseladb_types::GroupId;

use crate::command::Command;

/// Directory of the rollback files, under the node's data directory.
pub const ROLLBACK_DIR: &str = "rollback";

const FIELD_RANGE: &str = "range";
const FIELD_INDEX: &str = "index";
const FIELD_TERM: &str = "term";
const FIELD_TS: &str = "ts";
const FIELD_KEY: &str = "key";
const FIELD_VALUE_TYPE: &str = "valueType";
const FIELD_VALUE: &str = "value";
const FIELD_RAW: &str = "raw";

/// BSON element types used here (bsonspec.org).
const BSON_BINARY: u8 = 0x05;
const BSON_INT32: u8 = 0x10;
const BSON_TIMESTAMP: u8 = 0x11;
const BSON_INT64: u8 = 0x12;
/// Generic binary subtype.
const BINARY_GENERIC: u8 = 0x00;

const MS_PER_DAY: u64 = 86_400_000;

/// A file to write under the data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollbackFile {
    /// Path relative to the data directory.
    pub path: String,
    /// Content.
    pub bytes: Vec<u8>,
}

/// A document of a rollback file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RollbackDoc {
    /// A version a lost write wrote.
    Version {
        /// Range.
        range: GroupId,
        /// Index of the lost entry.
        index: u64,
        /// Its term.
        term: u64,
        /// Mark of the write.
        ts: Hlc,
        /// Logical key.
        key: Vec<u8>,
        /// Value type.
        value_type: u8,
        /// Value.
        value: Vec<u8>,
    },
    /// A lost entry that does not decode as a command of the range.
    Raw {
        /// Range.
        range: GroupId,
        /// Index of the lost entry.
        index: u64,
        /// Its term.
        term: u64,
        /// The entry's data.
        data: Vec<u8>,
    },
}

/// A rollback file that is not what [`rollback_files`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRollbackFile {
    /// Offset of the document or element that does not read.
    pub offset: usize,
}

/// The files for the entries a core rolled back (`MultiRaftCore::take_rolled_back`) at wall time
/// `wall_ms`: one per range, in range order. Entries that wrote no version give no document, and
/// a range without documents no file.
pub fn rollback_files(rolled: &[(GroupId, LogEntry)], wall_ms: u64) -> Vec<RollbackFile> {
    let mut by_range: BTreeMap<GroupId, Vec<&LogEntry>> = BTreeMap::new();
    for (g, e) in rolled {
        by_range.entry(*g).or_default().push(e);
    }
    let stamp = utc_stamp(wall_ms);
    by_range
        .into_iter()
        .filter_map(|(g, entries)| {
            let bytes: Vec<u8> = entries.iter().flat_map(|e| entry_docs(g, e)).flatten().collect();
            let first = entries.first()?;
            let name = format!("removed.{stamp}.{}.{}.bson", first.term, first.index);
            let path = format!("{ROLLBACK_DIR}/range-{}/{name}", g.0);
            (!bytes.is_empty()).then_some(RollbackFile { path, bytes })
        })
        .collect()
}

/// The BSON documents of one lost entry.
fn entry_docs(g: GroupId, e: &LogEntry) -> Vec<Vec<u8>> {
    let decoded = envelope::decode(&e.data)
        .ok()
        .and_then(|env| Command::decode(env.payload).ok().map(|c| (env.ts, c)));
    let head = |d: &mut Doc| {
        d.int64(FIELD_RANGE, g.0).int64(FIELD_INDEX, e.index).int64(FIELD_TERM, e.term);
    };
    match decoded {
        Some((ts, Command::Write { puts, .. })) => puts
            .iter()
            .map(|p| {
                let mut d = Doc::default();
                head(&mut d);
                d.timestamp(FIELD_TS, ts)
                    .binary(FIELD_KEY, &p.key)
                    .int32(FIELD_VALUE_TYPE, i32::from(p.value_type))
                    .binary(FIELD_VALUE, &p.value);
                d.finish()
            })
            .collect(),
        // Commands of the range itself: no document changed.
        Some(_) => Vec::new(),
        None => {
            let mut d = Doc::default();
            head(&mut d);
            d.binary(FIELD_RAW, &e.data);
            vec![d.finish()]
        }
    }
}

/// `YYYY-MM-DDTHH-MM-SS` of a UTC time in milliseconds since the epoch.
fn utc_stamp(ms: u64) -> String {
    let (y, m, d) = civil_from_days(ms / MS_PER_DAY);
    let secs = (ms % MS_PER_DAY) / 1000;
    let (hh, mm, ss) = (secs / 3600, secs / 60 % 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}-{mm:02}-{ss:02}")
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (H. Hinnant, «chrono-Compatible
/// Low-Level Date Algorithms», `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

/// A BSON document being written.
#[derive(Default)]
struct Doc(Vec<u8>);

impl Doc {
    fn name(&mut self, ty: u8, name: &str) -> &mut Doc {
        self.0.push(ty);
        self.0.extend_from_slice(name.as_bytes());
        self.0.push(0);
        self
    }

    fn int32(&mut self, name: &str, v: i32) -> &mut Doc {
        self.name(BSON_INT32, name).0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn int64(&mut self, name: &str, v: u64) -> &mut Doc {
        let v = i64::try_from(v).unwrap_or(i64::MAX);
        self.name(BSON_INT64, name).0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn timestamp(&mut self, name: &str, ts: Hlc) -> &mut Doc {
        let (t, i) = to_timestamp(ts);
        let v = (u64::from(t) << 32) | u64::from(i);
        self.name(BSON_TIMESTAMP, name).0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn binary(&mut self, name: &str, v: &[u8]) -> &mut Doc {
        let len = i32::try_from(v.len()).unwrap_or(i32::MAX);
        self.name(BSON_BINARY, name).0.extend_from_slice(&len.to_le_bytes());
        self.0.push(BINARY_GENERIC);
        self.0.extend_from_slice(v);
        self
    }

    fn finish(&mut self) -> Vec<u8> {
        let body = std::mem::take(&mut self.0);
        let len = i32::try_from(body.len() + 5).unwrap_or(i32::MAX);
        let mut out = Vec::with_capacity(body.len() + 5);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&body);
        out.push(0);
        out
    }
}

/// A field of a document read back.
enum Value {
    Int(i64),
    Ts(Hlc),
    Bin(Vec<u8>),
}

/// The documents of a file [`rollback_files`] wrote.
pub fn read_rollback_file(bytes: &[u8]) -> Result<Vec<RollbackDoc>, BadRollbackFile> {
    let mut docs = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let bad = BadRollbackFile { offset: at };
        let len = read_i32(bytes, at).ok_or(bad)?;
        let end = usize::try_from(len).ok().and_then(|l| at.checked_add(l)).ok_or(bad)?;
        if len < 5 || end > bytes.len() || bytes[end - 1] != 0 {
            return Err(bad);
        }
        let fields = read_fields(bytes, at + 4, end - 1)?;
        docs.push(to_doc(&fields).ok_or(bad)?);
        at = end;
    }
    Ok(docs)
}

fn read_i32(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn read_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

/// The elements of a document between `at` and its terminating zero at `end`.
fn read_fields(
    b: &[u8],
    mut at: usize,
    end: usize,
) -> Result<BTreeMap<String, Value>, BadRollbackFile> {
    let mut fields = BTreeMap::new();
    while at < end {
        let bad = BadRollbackFile { offset: at };
        let ty = b[at];
        let name_end = b[at + 1..end].iter().position(|&c| c == 0).ok_or(bad)? + at + 1;
        let name = String::from_utf8(b[at + 1..name_end].to_vec()).map_err(|_| bad)?;
        let v = name_end + 1;
        let (value, next) = match ty {
            BSON_INT32 => (Value::Int(i64::from(read_i32(b, v).ok_or(bad)?)), v + 4),
            BSON_INT64 => (Value::Int(read_u64(b, v).ok_or(bad)? as i64), v + 8),
            BSON_TIMESTAMP => {
                let raw = read_u64(b, v).ok_or(bad)?;
                (Value::Ts(from_timestamp(((raw >> 32) as u32, raw as u32))), v + 8)
            }
            BSON_BINARY => {
                let len = usize::try_from(read_i32(b, v).ok_or(bad)?).map_err(|_| bad)?;
                let data = b.get(v + 5..v + 5 + len).ok_or(bad)?.to_vec();
                (Value::Bin(data), v + 5 + len)
            }
            _ => return Err(bad),
        };
        if next > end {
            return Err(bad);
        }
        fields.insert(name, value);
        at = next;
    }
    Ok(fields)
}

fn to_doc(f: &BTreeMap<String, Value>) -> Option<RollbackDoc> {
    let int = |name: &str| match f.get(name)? {
        Value::Int(v) => u64::try_from(*v).ok(),
        Value::Ts(_) | Value::Bin(_) => None,
    };
    let bin = |name: &str| match f.get(name)? {
        Value::Bin(v) => Some(v.clone()),
        Value::Int(_) | Value::Ts(_) => None,
    };
    let (range, index, term) = (GroupId(int(FIELD_RANGE)?), int(FIELD_INDEX)?, int(FIELD_TERM)?);
    if let Some(data) = bin(FIELD_RAW) {
        return Some(RollbackDoc::Raw { range, index, term, data });
    }
    let ts = match f.get(FIELD_TS)? {
        Value::Ts(ts) => *ts,
        Value::Int(_) | Value::Bin(_) => return None,
    };
    let value_type = u8::try_from(int(FIELD_VALUE_TYPE)?).ok()?;
    let (key, value) = (bin(FIELD_KEY)?, bin(FIELD_VALUE)?);
    Some(RollbackDoc::Version { range, index, term, ts, key, value_type, value })
}
