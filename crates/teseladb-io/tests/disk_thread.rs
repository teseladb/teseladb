//! The disk thread executes in submission order, answers through the mailbox, returns
//! the disk on shutdown and stops when nobody listens.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use teseladb_io::FileDisk;
use teseladb_io::disk_thread::DiskThread;
use teseladb_io::mailbox::{Mailbox, Notify, channel};
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp, MemDisk};
use teseladb_types::Bytes;

#[derive(Default)]
struct Counter(AtomicUsize);

impl Notify for Counter {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, PartialEq)]
enum Msg {
    Disk(IoDone),
}

fn wait_for(rx: &mut Mailbox<Msg>, n: usize) -> Vec<IoDone> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < n {
        assert!(Instant::now() < deadline, "only {} of {n} results", got.len());
        rx.drain_into(&mut got);
        std::thread::sleep(Duration::from_millis(1));
    }
    got.into_iter().map(|Msg::Disk(d)| d).collect()
}

fn ops(file: FileId) -> Vec<IoOp> {
    let mut v = vec![IoOp::Create { file, path: PathBuf::from("f"), prealloc: 0 }];
    for i in 0..100u64 {
        v.push(IoOp::WriteAt { file, offset: i, data: Bytes::from_vec(vec![i as u8]), batch: i });
    }
    // A sync outside a store's batches keeps its place (a batch's runs on a sync thread).
    v.push(IoOp::Sync { file, batch: 0 });
    v.push(IoOp::ReadAt { file, offset: 0, len: 100, token: 7 });
    v
}

fn check(results: &[IoDone], file: FileId) {
    assert_eq!(results.len(), 103);
    assert!(matches!(&results[0], IoDone::Ok(IoOp::Create { .. })));
    for (i, r) in results[1..101].iter().enumerate() {
        match r {
            IoDone::Ok(IoOp::WriteAt { file: f, offset, data, .. }) => {
                assert_eq!((*f, *offset), (file, i as u64), "out of order");
                assert!(data.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(matches!(&results[101], IoDone::Ok(IoOp::Sync { .. })));
    let want: Vec<u8> = (0..100).collect();
    assert_eq!(results[102], IoDone::Read { token: 7, data: Ok(Bytes::from_vec(want)) });
}

#[test]
fn operations_run_in_order_over_memory_and_the_disk_comes_back() {
    let n = Arc::new(Counter::default());
    let (tx, mut rx) = channel(n.clone());
    let t = DiskThread::spawn("disk-0".into(), MemDisk::new(), tx, Msg::Disk).unwrap();
    let file = FileId(3);
    for op in ops(file) {
        t.submit(op).unwrap();
    }
    check(&wait_for(&mut rx, 103), file);
    assert!(n.0.load(Ordering::SeqCst) >= 1);
    let mut disk = t.shutdown().unwrap();
    assert_eq!(disk.read_all(&PathBuf::from("f")).unwrap().len(), 100);
}

#[test]
fn operations_run_in_order_over_real_files() {
    let dir = std::env::temp_dir().join(format!("teseladb-disk-thread-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (tx, mut rx) = channel(Arc::new(Counter::default()));
    let t =
        DiskThread::spawn("disk-1".into(), FileDisk::open(&dir).unwrap(), tx, Msg::Disk).unwrap();
    let file = FileId(1);
    for op in ops(file) {
        t.submit(op).unwrap();
    }
    check(&wait_for(&mut rx, 103), file);
    // Errors travel back too, in order.
    t.submit(IoOp::Open { file: FileId(2), path: PathBuf::from("missing") }).unwrap();
    match &wait_for(&mut rx, 1)[0] {
        IoDone::Err { op, errno } => {
            assert_eq!(op.name(), "open");
            assert_eq!(*errno, IoErrno::NotFound);
        }
        other => panic!("unexpected {other:?}"),
    }
    drop(t);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_batch_sync_on_a_real_disk_answers_on_its_own_and_other_syncs_stay_in_order() {
    // 012 FR-014: the sync of a store's batch (`batch > 0`) runs on a sync thread, so its result
    // may come after that of a later write; a sync with `batch: 0` keeps its place.
    let dir =
        std::env::temp_dir().join(format!("teseladb-disk-thread-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (tx, mut rx) = channel(Arc::new(Counter::default()));
    let t =
        DiskThread::spawn("disk-3".into(), FileDisk::open(&dir).unwrap(), tx, Msg::Disk).unwrap();
    let file = FileId(1);
    let write = |offset: u64| IoOp::WriteAt {
        file,
        offset,
        data: Bytes::from_vec(vec![1; 4]),
        batch: offset + 1,
    };
    let sent = [
        IoOp::Create { file, path: PathBuf::from("f"), prealloc: 0 },
        write(0),
        IoOp::Sync { file, batch: 1 },
        write(4),
        IoOp::Sync { file, batch: 2 },
        write(8),
        IoOp::Sync { file, batch: 0 },
        IoOp::Close { file },
    ];
    for op in sent.iter().cloned() {
        t.submit(op).unwrap();
    }
    let got = wait_for(&mut rx, sent.len());
    // Every operation answered once, all well.
    let mut answered: Vec<IoOp> = got
        .iter()
        .map(|d| match d {
            IoDone::Ok(op) => op.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    let mut want: Vec<IoOp> = sent.iter().map(IoOp::without_data).collect();
    let key = |op: &IoOp| format!("{op:?}");
    answered.sort_by_key(key);
    want.sort_by_key(key);
    assert_eq!(answered, want);
    // The operations that stay in order: everything but the two batch syncs.
    let in_order: Vec<IoOp> = got
        .iter()
        .filter_map(|d| match d {
            IoDone::Ok(op) if !matches!(op, IoOp::Sync { batch, .. } if *batch > 0) => {
                Some(op.clone())
            }
            _ => None,
        })
        .collect();
    let want_order: Vec<IoOp> = sent
        .iter()
        .filter(|op| !matches!(op, IoOp::Sync { batch, .. } if *batch > 0))
        .map(IoOp::without_data)
        .collect();
    assert_eq!(in_order, want_order);
    let mut disk = t.shutdown().unwrap();
    assert_eq!(disk.read_all(&PathBuf::from("f")).unwrap().len(), 12);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_thread_stops_when_the_core_is_gone() {
    let (tx, rx) = channel(Arc::new(Counter::default()));
    let t = DiskThread::spawn("disk-2".into(), MemDisk::new(), tx, Msg::Disk).unwrap();
    drop(rx);
    let file = FileId(1);
    // The first result cannot be delivered, so the thread ends and later submits are refused.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the thread did not stop");
        if t.submit(IoOp::Close { file }).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(t.shutdown().is_some());
}

#[test]
fn a_shut_down_handle_refuses_work() {
    let (tx, _rx) = channel(Arc::new(Counter::default()));
    let t = DiskThread::spawn("disk-3".into(), MemDisk::new(), tx, Msg::Disk).unwrap();
    assert!(format!("{t:?}").contains("DiskThread"));
    assert!(t.shutdown().is_some());
}
