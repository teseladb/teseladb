//! The `local` database (018 FR-009, FR-010): `oplog.rs` ([`super::oplog`]), `startup_log` and,
//! in the replica set presentation, `system.replset`, all read-only. A `refrouter` refuses the
//! whole database; TeselaDB serves these collections in both presentations (DV-015) and refuses
//! the rest as a `refrouter` does.
//!
//! Measured on a replica set of RefDb 9.0.4 (02-11-2026): `listCollections` gives `oplog.rs`
//! capped with `autoIndexId: false` and no `idIndex`, and `startup_log` capped with 10 MiB; a
//! `startup_log` document has `_id` (`<hostname>-<start in ms>`), `hostname`, `startTime`,
//! `startTimeLocal` (`Mon Nov  2 15:44:32.433`), `cmdLine`, `pid` and `buildinfo`.

use teseladb_bson::{Document, Value};
use teseladb_meta::catalog::Namespace;

use super::Backend;
use super::diagnostics::{ProcessFacts, build_info_of};
use crate::error::{CommandError, ILLEGAL_OPERATION};

pub use super::names::LOCAL_DB;
/// `startup_log`.
pub const STARTUP_LOG: &str = "startup_log";
/// `system.replset`.
pub const SYSTEM_REPLSET: &str = "system.replset";
/// The collections served, in `listCollections` order.
pub const SERVED: [&str; 3] = [super::oplog::OPLOG_COLL, STARTUP_LOG, SYSTEM_REPLSET];
/// The commands on `local` that name no collection of it and are served.
const UNNAMED: [&str; 4] = ["getMore", "killCursors", "listCollections", "dbStats"];
/// The commands that write a collection, which the oplog refuses (measured on 9.0.4).
const OPLOG_WRITES: [&str; 4] = ["insert", "update", "delete", "createIndexes"];
/// Size of the capped `startup_log` (RefDb 9.0.4).
const STARTUP_LOG_SIZE: i32 = 10 * 1024 * 1024;
/// Size `listCollections` gives the capped oplog: the 5 % of the disk a `refnode` takes, capped
/// at 50 GiB (RefDb's default; TeselaDB keeps its history window instead, 014).
const OPLOG_MAX_SIZE: i64 = 50 * 1024 * 1024 * 1024;
/// Percent of the disk RefDb gives the oplog.
const OPLOG_DISK_PERCENT: u64 = 5;

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MS_PER_DAY: i64 = 86_400_000;

/// Whether command `name` on database `db` with collection `coll` may run: anything outside
/// `local`, and on `local` the served collections.
pub fn check_reachable(db: &str, name: &str, coll: Option<&str>) -> Result<(), CommandError> {
    if db != LOCAL_DB {
        return Ok(());
    }
    // RefDb 9.0.4 refuses a write to the oplog before it reads the command's documents.
    if coll == Some(super::oplog::OPLOG_COLL) && OPLOG_WRITES.contains(&name) {
        return Err(super::oplog::write_refused());
    }
    let served = match coll {
        Some(c) => SERVED.contains(&c),
        None => UNNAMED.contains(&name),
    };
    match served {
        true => Ok(()),
        false => {
            Err(CommandError::new(ILLEGAL_OPERATION, teseladb_names::ERR_LOCAL_THROUGH_ROUTER))
        }
    }
}

/// Whether `ns` is a served collection of `local` other than the oplog.
pub fn generated(ns: &Namespace) -> bool {
    ns.db == LOCAL_DB && [STARTUP_LOG, SYSTEM_REPLSET].contains(&ns.coll.as_str())
}

/// The refusal of a write to `ns` of `local` (divergence DV-088: a replica set's member takes
/// it).
pub fn not_writable(ns: &Namespace) -> CommandError {
    CommandError::new(
        ILLEGAL_OPERATION,
        format!("{}.{} shows the node and cannot be written", ns.db, ns.coll),
    )
}

/// `Mon Nov  2 15:44:32.433` of `ms` (UTC).
pub fn local_time_text(ms: i64) -> String {
    let days = ms.div_euclid(MS_PER_DAY);
    let in_day = ms.rem_euclid(MS_PER_DAY);
    let (_, month, day) = teseladb_bson::extjson::civil_from_days(days);
    let weekday = WEEKDAYS[usize::try_from(days.rem_euclid(7)).unwrap_or(0)];
    let month = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
    let (h, m, s, frac) =
        (in_day / 3_600_000, in_day / 60_000 % 60, in_day / 1000 % 60, in_day % 1000);
    format!("{weekday} {month} {day:>2} {h:02}:{m:02}:{s:02}.{frac:03}")
}

/// The `startup_log` document of a node with `facts`.
pub fn startup_document(facts: &ProcessFacts) -> Document {
    let start = i64::try_from(facts.start_ms).unwrap_or(i64::MAX);
    let hostname = facts.host.hostname.clone();
    Document::new()
        .with("_id", format!("{hostname}-{start}"))
        .with("hostname", hostname)
        .with("startTime", Value::DateTime(start - start % 1000))
        .with("startTimeLocal", local_time_text(start))
        .with("cmdLine", facts.parsed.clone())
        .with("pid", Value::Int64(facts.pid))
        .with("buildinfo", build_info_of(facts))
}

/// The documents of `ns`, a served collection of `local` other than the oplog ([`generated`]).
pub fn documents<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<Vec<Document>, CommandError> {
    Ok(match ns.coll.as_str() {
        STARTUP_LOG => vec![startup_document(&backend.process())],
        _ => super::replset::system_replset(backend)?.into_iter().collect(),
    })
}

/// The `listCollections` entries of `local`.
pub fn listed<B: Backend + ?Sized>(
    backend: &mut B,
    name_only: bool,
) -> Result<Vec<Document>, CommandError> {
    let facts = backend.process();
    let oplog_size = i64::try_from(facts.fs_total / 100 * OPLOG_DISK_PERCENT)
        .unwrap_or(OPLOG_MAX_SIZE)
        .min(OPLOG_MAX_SIZE);
    let replset = super::replset::system_replset(backend)?.is_some();
    let mut out = Vec::new();
    for (i, name) in SERVED.iter().enumerate() {
        if *name == SYSTEM_REPLSET && !replset {
            continue;
        }
        let mut d = Document::new().with("name", *name).with("type", "collection");
        if name_only {
            out.push(d);
            continue;
        }
        let options = match *name {
            super::oplog::OPLOG_COLL => Document::new()
                .with("capped", true)
                .with("size", Value::Int64(oplog_size))
                .with("autoIndexId", false),
            STARTUP_LOG => Document::new().with("capped", true).with("size", STARTUP_LOG_SIZE),
            _ => Document::new(),
        };
        let mut uuid = [0x6c; 16];
        uuid[6] = 0x40;
        uuid[8] = 0x80;
        uuid[15] = u8::try_from(i).unwrap_or(0);
        d.push("options", options);
        d.push(
            "info",
            Document::new()
                .with("readOnly", false)
                .with("uuid", Value::Binary { subtype: 4, bytes: uuid.to_vec() }),
        );
        if *name != super::oplog::OPLOG_COLL {
            let id = Document::new()
                .with("v", 2)
                .with("key", Document::new().with("_id", 1))
                .with("name", "_id_");
            d.push("idIndex", id);
        }
        out.push(d);
    }
    Ok(out)
}
