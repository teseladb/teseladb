//! What a core's LSM holds, for an operator who wants to know where its space goes: every
//! version of every key, grouped by the key's prefix (`01`, collection and index for the data;
//! the first byte for a range's local keys), with how many versions, distinct keys, intents and
//! deletions each group has and their bytes. It opens the LSM in place and may write a new
//! manifest, so it is meant for a copy of a core's directory.
//!
//!     cargo run --release -p teseladb-server --example lsm_inspect -- <copy of core-NN> [bytes]
//!
//! `bytes` is how much of a data key names its group (13 by default: collection and index).

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;

#[cfg(target_os = "linux")]
use teseladb_hlc::Hlc;
#[cfg(target_os = "linux")]
use teseladb_io::FileDisk;
#[cfg(target_os = "linux")]
use teseladb_lsm::{Kind, Lsm, LsmConfig, Step};
#[cfg(target_os = "linux")]
use teseladb_types::Nanos;

/// Bytes of the prefix that names a collection and an index in a data key.
#[cfg(target_os = "linux")]
const DATA_PREFIX: usize = 13;
/// First byte of the data keys.
#[cfg(target_os = "linux")]
const DATA: u8 = 1;

#[cfg(target_os = "linux")]
#[derive(Default)]
struct Group {
    versions: u64,
    keys: u64,
    intents: u64,
    deletes: u64,
    bytes: u64,
}

#[cfg(target_os = "linux")]
fn prefix(key: &[u8], data: usize) -> Vec<u8> {
    match key.first() {
        Some(&DATA) => key[..key.len().min(data)].to_vec(),
        _ => key[..key.len().min(1)].to_vec(),
    }
}

#[cfg(target_os = "linux")]
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(target_os = "linux")]
fn main() {
    let dir = std::env::args().nth(1).expect("usage: lsm_inspect <copy of core-NN> [bytes]");
    let data = std::env::args().nth(2).and_then(|b| b.parse().ok()).unwrap_or(DATA_PREFIX);
    let mut disk = FileDisk::open(&dir).expect("the core's directory");
    let cfg = LsmConfig::default();
    let (mut lsm, _) = Lsm::recover(&cfg, &mut disk).expect("the LSM");
    println!("levels (bytes): {:?}", lsm.metrics().level_bytes);
    let view = lsm.view();
    let lowest = Hlc { physical: 0, logical: 0 };
    let highest = Hlc { physical: u64::MAX >> 16, logical: u32::MAX >> 4 };
    let mut scan = view.scan_time(&[], &[0xff; 16], lowest, highest);
    let mut groups: BTreeMap<Vec<u8>, Group> = BTreeMap::new();
    let mut last_key: Option<Vec<u8>> = None;
    loop {
        match scan.next(&mut lsm) {
            Step::Item(i) => {
                let g = groups.entry(prefix(&i.key, data)).or_default();
                g.versions += 1;
                g.bytes += (i.key.len() + i.value.len()) as u64;
                match i.kind {
                    Kind::Intent => g.intents += 1,
                    Kind::Delete => g.deletes += 1,
                    Kind::Put | Kind::Retract => {}
                }
                if last_key.as_deref() != Some(i.key.as_slice()) {
                    g.keys += 1;
                    last_key = Some(i.key);
                }
            }
            Step::Done => break,
            Step::Miss(_) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            Step::Failed(e) => panic!("the LSM failed: {e}"),
        }
    }
    println!("tombstones over the scan: {}", scan.tombstones().len());
    println!(
        "{:<30} {:>9} {:>9} {:>8} {:>8} {:>12}",
        "prefix", "versions", "keys", "intents", "deletes", "bytes"
    );
    for (p, g) in &groups {
        println!(
            "{:<30} {:>9} {:>9} {:>8} {:>8} {:>12}",
            hex(p),
            g.versions,
            g.keys,
            g.intents,
            g.deletes,
            g.bytes
        );
    }
}

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}
