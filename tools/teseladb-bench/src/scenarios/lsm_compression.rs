//! `lsm-compression` scenario (004 FR-023, SC-006, T036): the same corpus of documents written
//! with zlib, zstd and zstd with the dictionary of each collection, on the real disk through
//! `FileDisk`. For each codec it reports:
//! - `lsm_compression.<codec>.rate`: bytes of the data blocks after compression per byte before,
//!   for the blocks the compactions wrote once the dictionaries existed;
//! - `lsm_compression.<codec>.small_rate`: the same for a collection of documents under 1 KiB,
//!   which the dictionary of SC-006 has to improve by 10 %;
//! - `lsm_compression.<codec>.get_cold`: a point read whose block comes from the disk and is
//!   decompressed (no block cache);
//! - `lsm_compression.<codec>.compaction_mb_s`: one compaction of every file.
//!
//! The corpus has three collections shaped as real ones: customers (about 400 bytes), orders with
//! their lines (1 to 4 KiB) and events (under 200 bytes), BSON-encoded, with values drawn from
//! fixed lists as real data repeats them. Without targets: SC-006 compares the codecs with each
//! other, and T037 takes the default codec from this measurement.

use std::hint::black_box;
use std::time::Instant;

use teseladb_io::FileDisk;
use teseladb_keys::Hlc;
use teseladb_lsm::{Codec, Lookup, Lsm, LsmConfig};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::Disk;
use teseladb_types::{GroupId, Nanos, Rng};

use super::{DataDir, at, msg};
use crate::cli::Args;
use crate::report::Report;

const G: GroupId = GroupId(1);
/// Collections of the corpus.
const CUSTOMERS: u32 = 1;
const ORDERS: u32 = 2;
const EVENTS: u32 = 3;
/// The codecs compared, with the name of their metrics.
const CODECS: [(Codec, &str); 3] =
    [(Codec::Zlib, "zlib"), (Codec::Zstd, "zstd"), (Codec::ZstdDict, "zstd_dict")];
const CITIES: [&str; 8] =
    ["Madrid", "Sevilla", "Bilbao", "Valencia", "Zaragoza", "Málaga", "Murcia", "Vigo"];
const PRODUCTS: [&str; 6] = ["keyboard", "monitor", "mouse", "laptop", "headset", "webcam"];
const KINDS: [&str; 4] = ["login", "logout", "view", "purchase"];

/// The key of document `i` of `coll` (`01 · tenant · collection · index 0 · id`).
fn key(coll: u32, i: u64) -> Vec<u8> {
    let mut k = vec![teseladb_keys::space::DATA];
    k.extend_from_slice(&1u32.to_be_bytes());
    k.extend_from_slice(&coll.to_be_bytes());
    k.extend_from_slice(&0u32.to_be_bytes());
    k.extend_from_slice(&i.to_be_bytes());
    k
}

fn pick<'a>(rng: &mut Rng, from: &[&'a str]) -> &'a str {
    from[rng.range(0, from.len() as u64) as usize]
}

/// Document `i` of `coll`, BSON-encoded.
fn document(rng: &mut Rng, coll: u32, i: u64) -> Vec<u8> {
    let id = i64::try_from(i).unwrap_or(i64::MAX);
    let doc = match coll {
        CUSTOMERS => Document::new()
            .with("_id", id)
            .with("name", format!("customer {}", rng.range(0, 100_000)))
            .with("email", format!("user{}@example.org", rng.range(0, 1_000_000)))
            .with(
                "address",
                Document::new()
                    .with("street", format!("{} Calle Mayor", rng.range(1, 400)))
                    .with("city", pick(rng, &CITIES))
                    .with("zip", format!("{:05}", rng.range(0, 53_000))),
            )
            .with("tier", ["free", "pro", "enterprise"][rng.range(0, 3) as usize])
            .with("active", rng.range(0, 2) == 1),
        ORDERS => {
            let lines: Vec<Value> = (0..rng.range(8, 40))
                .map(|_| {
                    Value::Document(
                        Document::new()
                            .with("product", pick(rng, &PRODUCTS))
                            .with("quantity", rng.range(1, 10) as i64)
                            .with("price", rng.range(100, 99_999) as f64 / 100.0),
                    )
                })
                .collect();
            Document::new()
                .with("_id", id)
                .with("customer", rng.range(0, 100_000) as i64)
                .with("status", ["open", "paid", "shipped"][rng.range(0, 3) as usize])
                .with("lines", Value::Array(lines))
        }
        _ => Document::new()
            .with("_id", id)
            .with("kind", pick(rng, &KINDS))
            .with("user", rng.range(0, 100_000) as i64)
            .with("at", rng.range(1_700_000_000, 1_800_000_000) as i64),
    };
    doc.to_bytes().unwrap_or_default()
}

/// The shape of a run.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// Documents of each collection, written twice: before the dictionaries and after.
    docs: u64,
    /// Point reads timed per codec.
    reads: u64,
}

fn drain(lsm: &mut Lsm, disk: &mut dyn Disk, clock: &Instant) {
    lsm.run_io(disk, Nanos(clock.elapsed().as_nanos() as u64));
}

/// Writes documents `from..to` of every collection and flushes.
fn write(
    lsm: &mut Lsm,
    disk: &mut dyn Disk,
    (from, to): (u64, u64),
    clock: &Instant,
) -> Result<(), String> {
    let mut rng = Rng::new(from + 1);
    for i in from..to {
        for coll in [CUSTOMERS, ORDERS, EVENTS] {
            let doc = document(&mut rng, coll, i);
            let index = i * 3 + u64::from(coll);
            lsm.put(G, index, &key(coll, i), Hlc { physical: index, logical: 0 }, &doc)
                .map_err(msg)?;
        }
        if i % 1_000 == 999 {
            lsm.flush();
            drain(lsm, disk, clock);
        }
    }
    lsm.flush();
    drain(lsm, disk, clock);
    Ok(())
}

fn compact(lsm: &mut Lsm, disk: &mut dyn Disk, clock: &Instant) -> Result<(), String> {
    lsm.compact_all();
    drain(lsm, disk, clock);
    lsm.health().map_err(msg)
}

/// The compression rate of the data blocks written between `before` and `after`.
fn rate(before: [u64; 2], after: [u64; 2]) -> f64 {
    let raw = after[0].saturating_sub(before[0]).max(1);
    after[1].saturating_sub(before[1]) as f64 / raw as f64
}

/// Measures one codec into `report`.
fn measure(
    dir: &DataDir,
    (codec, name): (Codec, &str),
    shape: Shape,
    report: &mut Report,
) -> Result<(), String> {
    let clock = Instant::now();
    let cfg = LsmConfig {
        dir: "lsm".into(),
        codec,
        compaction_bytes_per_sec: 1 << 40,
        ..LsmConfig::default()
    };
    let path = dir.path.join(name);
    let mut disk = FileDisk::open(&path).map_err(at(&path))?;
    let (mut lsm, _) = Lsm::recover(&cfg, &mut disk).map_err(msg)?;
    // The first half trains the dictionaries in its compaction; the second is measured.
    write(&mut lsm, &mut disk, (0, shape.docs), &clock)?;
    compact(&mut lsm, &mut disk, &clock)?;
    let before = lsm.metrics().block_bytes_written;
    write(&mut lsm, &mut disk, (shape.docs, 2 * shape.docs), &clock)?;
    let files: u64 = lsm.metrics().level_bytes.iter().sum();
    let t = Instant::now();
    compact(&mut lsm, &mut disk, &clock)?;
    let mb_s = files as f64 / t.elapsed().as_secs_f64().max(1e-9) / 1e6;
    report.push(
        &format!("lsm_compression.{name}.rate"),
        "ratio",
        rate(before, lsm.metrics().block_bytes_written),
        None,
    );
    report.push(&format!("lsm_compression.{name}.compaction_mb_s"), "MB/s", mb_s, None);
    drop(lsm);
    // The small documents alone, in an LSM of their own, the same way.
    let small = path.join("small");
    let mut sdisk = FileDisk::open(&small).map_err(at(&small))?;
    let (mut s, _) = Lsm::recover(&cfg, &mut sdisk).map_err(msg)?;
    let mut rng = Rng::new(5);
    let mut index = 0;
    let mut small_round = |s: &mut Lsm, d: &mut FileDisk, range: std::ops::Range<u64>| {
        for i in range {
            index += 1;
            let doc = document(&mut rng, EVENTS, i);
            s.put(G, index, &key(EVENTS, i), Hlc { physical: index, logical: 0 }, &doc)
                .map_err(msg)?;
        }
        s.flush();
        drain(s, d, &clock);
        compact(s, d, &clock)
    };
    small_round(&mut s, &mut sdisk, 0..shape.docs)?;
    let before = s.metrics().block_bytes_written;
    small_round(&mut s, &mut sdisk, shape.docs..2 * shape.docs)?;
    report.push(
        &format!("lsm_compression.{name}.small_rate"),
        "ratio",
        rate(before, s.metrics().block_bytes_written),
        None,
    );
    drop(s);
    // Cold reads: the files reopened without a block cache.
    let cold_cfg = LsmConfig { cache_bytes: 0, ..cfg };
    let (mut cold, _) = Lsm::recover(&cold_cfg, &mut disk).map_err(msg)?;
    let mut h = teseladb_types::Histogram::new();
    let mut rng = Rng::new(9);
    for _ in 0..shape.reads {
        let k = key(
            [CUSTOMERS, ORDERS, EVENTS][rng.range(0, 3) as usize],
            rng.range(0, 2 * shape.docs),
        );
        let t = Instant::now();
        let v = cold.view();
        loop {
            match v.get(&mut cold, &k, Hlc { physical: u64::MAX, logical: 0 }) {
                Lookup::Miss(_) => drain(&mut cold, &mut disk, &clock),
                found => {
                    black_box(found);
                    break;
                }
            }
        }
        h.record(t.elapsed().as_nanos() as u64);
    }
    report.latency(&format!("lsm_compression.{name}.get_cold"), &h, None, None);
    Ok(())
}

/// The measurement's shape: 100 000 documents of each collection, twice, and 20 000 reads.
pub const MEASUREMENT: Shape = Shape { docs: 100_000, reads: 20_000 };

/// Runs the scenario with `shape` (`MEASUREMENT` for the measurement; the tests run a smaller one).
pub fn run_with(args: &Args, shape: Shape) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "lsm-compression")?;
    let mut report = Report::new("lsm-compression");
    for codec in CODECS {
        measure(&dir, codec, shape, &mut report)?;
    }
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_small_run_reports_every_codec() {
        let args = Args { scenario: "lsm-compression".into(), ..Args::default() };
        let r = run_with(&args, Shape { docs: 1_500, reads: 50 }).unwrap();
        for (_, name) in CODECS {
            for metric in ["rate", "small_rate", "compaction_mb_s", "get_cold.p50", "get_cold.p99"]
            {
                let full = format!("lsm_compression.{name}.{metric}");
                let m = r.metrics.iter().find(|m| m.name == full);
                assert!(m.is_some_and(|m| m.value > 0.0), "{full}: {m:?}");
            }
        }
        let value = |n: &str| r.metrics.iter().find(|m| m.name == n).map_or(0.0, |m| m.value);
        // Every codec shrinks the corpus.
        for (_, name) in CODECS {
            assert!(value(&format!("lsm_compression.{name}.rate")) < 1.0, "{name}");
        }
    }
}
