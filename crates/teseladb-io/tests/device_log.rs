//! The device log with its writer on shard 0 of the engine (012 FR-035, D-154, 023 FR-013): cores
//! writing at once share its syncs and find their entries again after a restart, on a disk in
//! memory and on the engine's files; the writer counts each part of a sync's wait as its stage;
//! the HLC bound rides in its records.
//!
//! One engine per process (D4): a program with its own `main` that runs the cases one after another
//! in one engine of two shards.

#[cfg(target_os = "linux")]
mod cases {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;

    use teseladb_io::device_log::{DeviceLog, DeviceLogConfig, LogBound, LogDisk};
    use teseladb_io::engine::{EngineConfig, NodeEngine};
    use teseladb_raftlog::format::wal::EntryKind;
    use teseladb_raftlog::{CoreStore, Disk, LogEntry, MemDisk, StorageConfig};
    use teseladb_stages::Stage;
    use teseladb_types::{Bytes, Durability, GroupId, Nanos};

    const G: GroupId = GroupId(5);
    const ENTRIES: u64 = 60;
    const CORES: u16 = 8;
    const LOG: DeviceLogConfig = DeviceLogConfig { file_bytes: 256 << 10 };

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

    fn start(
        e: &NodeEngine,
        disk: LogDisk,
        bound: Arc<LogBound>,
    ) -> (DeviceLog, Vec<teseladb_io::device_log::LogPort>) {
        DeviceLog::start(e, disk, CORES, LOG, bound).expect("the log starts")
    }

    /// Every core writes `ENTRIES` entries at once from a thread of its own; the batches.
    fn write_from_every_core(ports: Vec<teseladb_io::device_log::LogPort>) -> u64 {
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
        writers.into_iter().map(|t| t.join().unwrap()).sum()
    }

    /// Every core finds its entries in the log of `ports`.
    fn every_core_finds_its_entries(ports: Vec<teseladb_io::device_log::LogPort>) {
        for mut port in ports {
            let core = port.core();
            let mut own = MemDisk::new();
            let s = CoreStore::recover(&cfg(), core, &mut port.disk(&mut own)).unwrap().0;
            let got = s.entries(G, 1, ENTRIES + 1, u64::MAX).unwrap();
            assert_eq!(got.len() as u64, ENTRIES, "core {core}");
            assert_eq!(got[9], entry(core, 10));
        }
    }

    fn cores_writing_at_once_share_the_syncs_and_find_their_entries_again(e: &NodeEngine) {
        let parts = [Stage::LogQueueWait, Stage::LogTurnWait, Stage::LogWrite, Stage::LogSync];
        let before: Vec<u64> = parts.iter().map(|s| teseladb_stages::count(*s)).collect();
        let (log, ports) = start(e, LogDisk::Memory(Box::new(MemDisk::new())), Arc::default());
        let batches = write_from_every_core(ports);
        let syncs = log.syncs();
        assert!(syncs > 0 && syncs <= batches, "{syncs} syncs for {batches} batches");
        for (stage, was) in parts.iter().zip(before) {
            assert!(teseladb_stages::count(*stage) > was, "{stage:?}");
        }
        let disk = log.stop().expect("the disk in memory comes back");
        let (log, ports) = start(e, LogDisk::Memory(disk), Arc::default());
        every_core_finds_its_entries(ports);
        assert!(log.stop().is_some());
    }

    fn the_log_on_the_engine_s_files_keeps_every_core_s_entries(e: &NodeEngine) {
        let dir = std::env::temp_dir().join(format!("teseladb-devlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (log, ports) = start(e, LogDisk::Files(dir.clone()), Arc::default());
        assert_eq!(log.recovered_files(), 0);
        let batches = write_from_every_core(ports);
        assert!(log.syncs() > 0 && log.syncs() <= batches);
        assert!(log.stop().is_none(), "files give nothing back");
        let (log, ports) = start(e, LogDisk::Files(dir.clone()), Arc::default());
        assert!(log.recovered_files() > 0, "the log's files are there");
        every_core_finds_its_entries(ports);
        drop(log);
        let mut files = teseladb_io::files::BlockingDisk::open(&dir).unwrap();
        let listed = files.list(&PathBuf::from(teseladb_io::device_log::format::DIR)).unwrap();
        assert!(!listed.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The HLC bound offered to the log rides in its records (005 FR-003, D-172): it is durable
    /// once a sync of the cores' writes ends, and the next start finds it again.
    fn an_offered_bound_is_durable_with_the_cores_syncs_and_found_again(e: &NodeEngine) {
        let bound = Arc::new(LogBound::default());
        bound.offer(7_000);
        let (log, mut ports) = start(e, LogDisk::Memory(Box::new(MemDisk::new())), bound.clone());
        let mut port = ports.remove(0);
        let mut own = MemDisk::new();
        {
            let mut disk = port.disk(&mut own);
            let mut s = CoreStore::recover(&cfg(), 0, &mut disk).unwrap().0;
            s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
            s.append_raft(G, &[entry(0, 1)], None, Durability::Strict).unwrap();
            for (_, r) in s.run_io(&mut disk, Nanos::ZERO).tickets {
                r.unwrap();
            }
        }
        // Answered once durable: the record it went in was synced.
        assert_eq!(bound.durable(), 7_000);
        drop((port, ports));
        let disk = log.stop().expect("the disk in memory comes back");
        let again = Arc::new(LogBound::default());
        let (log, _ports) = start(e, LogDisk::Memory(disk), again.clone());
        assert_eq!(again.durable(), 7_000);
        drop(log);
    }

    /// The writer runs on a stack of its own with a guard below, not on its engine thread's 128 KiB
    /// of the shard's memory, which has no guard page in a release build and which the writer's
    /// frames went past in a debug one.
    fn the_writer_runs_on_a_deep_stack_of_its_own(e: &NodeEngine) {
        let (log, ports) = start(e, LogDisk::Memory(Box::new(MemDisk::new())), Arc::default());
        let room = log.writer_stack_room();
        drop(ports);
        assert!(log.stop().is_some());
        assert!(room >= 1 << 20, "the writer had {room} bytes of stack below it");
    }

    /// Runs every case in one engine of two shards.
    pub fn run() {
        let e = NodeEngine::start(&EngineConfig { shards: 2, ..EngineConfig::default() })
            .expect("the engine starts");
        let cases: [(&str, fn(&NodeEngine)); 4] = [
            (
                "the_writer_runs_on_a_deep_stack_of_its_own",
                the_writer_runs_on_a_deep_stack_of_its_own,
            ),
            (
                "cores_writing_at_once_share_the_syncs_and_find_their_entries_again",
                cores_writing_at_once_share_the_syncs_and_find_their_entries_again,
            ),
            (
                "the_log_on_the_engine_s_files_keeps_every_core_s_entries",
                the_log_on_the_engine_s_files_keeps_every_core_s_entries,
            ),
            (
                "an_offered_bound_is_durable_with_the_cores_syncs_and_found_again",
                an_offered_bound_is_durable_with_the_cores_syncs_and_found_again,
            ),
        ];
        for (name, case) in cases {
            eprintln!("test {name} ...");
            case(&e);
            eprintln!("test {name} ... ok");
        }
        e.stop();
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    cases::run();
}
