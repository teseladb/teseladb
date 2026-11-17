//! The contract of a shard's disk executor on the engine (023 FR-010, FR-012, FR-017): each class
//! answers in submission order and with its class, what `call` runs is answered to its caller
//! alone, `pending` and `idle` follow what runs, the bytes of a file for an answer to another node
//! come from the executor's read, the classes are scheduling groups with their shares, and a model
//! of a disk comes back when the executor goes. Linux only: the engine runs on Linux.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use teseladb::engine::core::future::Future;
use teseladb::engine::core::thread::async_ as in_thread;
use teseladb::engine_test_case;
use teseladb_io::IoClass;
use teseladb_io::engine::CoreDisk;
use teseladb_io::engine::disk::{BACKGROUND_SHARES, DATA_SHARES, IoGroups, LOG_SHARES, ShardDisk};
use teseladb_raftlog::{Disk, FileId, IoDone, IoOp, MemDisk};
use teseladb_types::Bytes;

/// A directory of its own, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir()
            .join(format!("teseladb-engine-disk-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn on_engine(body: impl FnOnce(IoGroups) + 'static) -> Future<()> {
    IoGroups::get_or_create().then(move |g: IoGroups| in_thread(move || body(g)))
}

fn create(file: u32, path: &str) -> IoOp {
    IoOp::Create { file: FileId(file), path: path.into(), prealloc: 0 }
}

fn write(file: u32, offset: u64, data: &[u8]) -> IoOp {
    IoOp::WriteAt { file: FileId(file), offset, data: Bytes::copy_from_slice(data), batch: 0 }
}

/// Waits for `n` results of `d` and gives them.
fn collect(d: &ShardDisk, n: usize) -> Vec<(IoDone, IoClass)> {
    let mut got = Vec::new();
    while got.len() < n {
        d.ready().get().expect("ready");
        d.take_done(&mut got);
    }
    got
}

engine_test_case!(each_class_answers_in_submission_order_with_its_class, {
    on_engine(|g| {
        let dir = Dir::new("classes");
        let d = ShardDisk::open(&dir.0, g).get().unwrap();
        let mut sent = Vec::new();
        for (i, class) in [IoClass::Log, IoClass::Data, IoClass::Background].into_iter().enumerate()
        {
            let f = i as u32 + 1;
            sent.push((create(f, &format!("c{f}")), class));
            for k in 0..20u64 {
                sent.push((write(f, k * 4096, &[k as u8; 4096]), class));
            }
            sent.push((IoOp::Sync { file: FileId(f), batch: 0 }, class));
        }
        for (op, class) in &sent {
            d.submit(op.clone(), *class);
        }
        assert!(d.pending() > 0);
        let got = collect(&d, sent.len());
        for class in [IoClass::Log, IoClass::Data, IoClass::Background] {
            let want: Vec<IoDone> = sent
                .iter()
                .filter(|(_, c)| *c == class)
                .map(|(op, _)| IoDone::Ok(op.without_data()))
                .collect();
            let had: Vec<IoDone> =
                got.iter().filter(|(_, c)| *c == class).map(|(d, _)| d.clone()).collect();
            assert_eq!(want, had, "{class:?}");
        }
        assert_eq!(d.pending(), 0);
    })
});

engine_test_case!(a_call_is_answered_to_its_caller_and_pending_follows_what_runs, {
    on_engine(|g| {
        let dir = Dir::new("calls");
        let d = ShardDisk::open(&dir.0, g).get().unwrap();
        // Nothing runs: idle at once.
        d.idle().get().unwrap();
        let op = create(1, "a");
        assert_eq!(d.call(op.clone(), IoClass::Data).get().unwrap(), IoDone::Ok(op));
        let mut none = Vec::new();
        d.take_done(&mut none);
        assert!(none.is_empty(), "a call's result is its caller's");
        d.submit(write(1, 0, b"x"), IoClass::Data);
        d.submit(IoOp::Sync { file: FileId(1), batch: 0 }, IoClass::Data);
        assert_eq!(d.pending(), 2);
        d.idle().get().unwrap();
        // Done but not taken: still pending for the core.
        assert_eq!(d.pending(), 2);
        assert!(CoreDisk::ready(&d), "results wait for the core");
        let mut got = Vec::new();
        d.take_done(&mut got);
        assert_eq!(got.len(), 2);
        assert_eq!(d.pending(), 0);
        assert!(!CoreDisk::ready(&d));
        assert_eq!(d.root(), Some(dir.0.clone()));
        assert!(format!("{d:?}").contains("pending"));
    })
});

engine_test_case!(the_bytes_of_a_file_for_an_answer_come_from_the_executor_s_read, {
    on_engine(|g| {
        let dir = Dir::new("sends");
        let mut d = ShardDisk::open(&dir.0, g).get().unwrap();
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        assert!(matches!(d.execute(create(1, "piece")), IoDone::Ok(_)));
        assert!(matches!(d.execute(write(1, 0, &bytes)), IoDone::Ok(_)));
        // Any offset and length within the file.
        let got = d.read_for_send(FileId(1), 100, 5000).get().unwrap().expect("the bytes");
        assert_eq!(got.as_slice(), &bytes[100..5100]);
        let got = d.read_for_send(FileId(1), 8192, 1808).get().unwrap().expect("the tail");
        assert_eq!(got.as_slice(), &bytes[8192..]);
        // Fewer bytes than asked, past the end, or a file not open: the answer falls back.
        assert!(d.read_for_send(FileId(1), 9000, 2000).get().unwrap().is_none());
        assert!(d.read_for_send(FileId(1), 20_000, 10).get().unwrap().is_none());
        assert!(d.read_for_send(FileId(7), 0, 10).get().unwrap().is_none());
        assert!(CoreDisk::sends_files(&d));
        let via_trait = CoreDisk::read_for_send(&d, FileId(1), 0, 10).expect("it reads");
        assert_eq!(via_trait.get().unwrap().expect("the bytes").as_slice(), &bytes[..10]);
        // A disk in memory has no files of the engine to read.
        let m = ShardDisk::mem(MemDisk::new());
        assert!(m.read_for_send(FileId(1), 0, 1).get().unwrap().is_none());
    })
});

engine_test_case!(the_classes_are_scheduling_groups_with_their_shares, {
    on_engine(|g| {
        const { assert!(LOG_SHARES > DATA_SHARES && DATA_SHARES > BACKGROUND_SHARES) };
        assert_eq!(g.of(IoClass::Log).get_shares(), LOG_SHARES);
        assert_eq!(g.of(IoClass::Data).get_shares(), DATA_SHARES);
        assert_eq!(g.of(IoClass::Background).get_shares(), BACKGROUND_SHARES);
        // Made once per engine.
        let again = IoGroups::get_or_create().get().unwrap();
        assert_eq!(again, g);
    })
});

engine_test_case!(a_model_of_a_disk_comes_back_with_what_it_holds, {
    on_engine(|_| {
        let mut d = ShardDisk::mem(MemDisk::new());
        assert!(matches!(d.execute(create(1, "m")), IoDone::Ok(_)));
        assert!(matches!(d.execute(write(1, 0, b"kept")), IoDone::Ok(_)));
        assert_eq!(d.root(), None);
        let mut back = d.into_model().expect("the model");
        assert_eq!(back.read_all(Path::new("m")).unwrap(), b"kept");
        // A second handle keeps the executor: nothing comes back while it lives.
        let d = ShardDisk::mem(MemDisk::new());
        let other = d.clone();
        assert!(d.into_model().is_none());
        drop(other);
    })
});
