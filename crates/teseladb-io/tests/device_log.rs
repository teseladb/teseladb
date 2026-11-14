//! The device log on the node's writer thread (012 FR-035, D-154): cores writing at once share its
//! syncs and find their entries again after a restart, and the writer counts each part of a sync's
//! wait as its stage.

use std::thread;

use teseladb_io::device_log::{DeviceLog, DeviceLogConfig};
use teseladb_stages::Stage;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, LogEntry, MemDisk, StorageConfig};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

const G: GroupId = GroupId(5);
const ENTRIES: u64 = 60;
const CORES: u16 = 8;

fn entry(core: u16, index: u64) -> LogEntry {
    let data = Bytes::from_vec(format!("{core}:{index}").into_bytes());
    LogEntry { index, term: 1, kind: EntryKind::Normal, data }
}

fn cfg() -> StorageConfig {
    StorageConfig {
        wal_segment_size: 1 << 20,
        max_batch_bytes: 64 << 10,
        ..StorageConfig::default()
    }
}

#[test]
fn cores_writing_at_once_share_the_syncs_and_find_their_entries_again() {
    let parts = [Stage::LogQueueWait, Stage::LogTurnWait, Stage::LogWrite, Stage::LogSync];
    let before: Vec<u64> = parts.iter().map(|s| teseladb_stages::count(*s)).collect();
    let (log, ports) =
        DeviceLog::start(MemDisk::new(), CORES, DeviceLogConfig { file_bytes: 256 << 10 }).unwrap();
    let writers: Vec<_> = ports
        .into_iter()
        .map(|mut port| {
            thread::spawn(move || {
                let core = port.core();
                let mut own = MemDisk::new();
                let mut disk = port.disk(&mut own);
                let mut s = CoreStore::recover(&cfg(), core, &mut disk).unwrap().0;
                s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
                for i in 1..=ENTRIES {
                    s.append_raft(G, &[entry(core, i)], None, Durability::Strict).unwrap();
                    for (_, r) in s.run_io(&mut disk, Nanos::ZERO).tickets {
                        r.unwrap();
                    }
                }
                s.metrics().batches
            })
        })
        .collect();
    let batches: u64 = writers.into_iter().map(|t| t.join().unwrap()).sum();
    let syncs = log.syncs();
    assert!(syncs > 0 && syncs <= batches, "{syncs} syncs for {batches} batches");
    for (stage, was) in parts.iter().zip(before) {
        assert!(teseladb_stages::count(*stage) > was, "{stage:?}");
    }
    let disk = log.stop().unwrap();
    let (log, ports) =
        DeviceLog::start(disk, CORES, DeviceLogConfig { file_bytes: 256 << 10 }).unwrap();
    for mut port in ports {
        let core = port.core();
        let mut own = MemDisk::new();
        let s = CoreStore::recover(&cfg(), core, &mut port.disk(&mut own)).unwrap().0;
        let got = s.entries(G, 1, ENTRIES + 1, u64::MAX).unwrap();
        assert_eq!(got.len() as u64, ENTRIES, "core {core}");
        assert_eq!(got[9], entry(core, 10));
    }
    drop(log);
}
