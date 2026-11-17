//! The writers of the node's log lines and audit messages on the engine (023 FR-018, 013
//! FR-016): every shard's messages reach the destination whole and in that shard's order, all
//! written by shard 0; a flush from an engine thread waits until its message is written; past
//! the bound messages are dropped and counted; a file is appended to through the engine's files;
//! a pipe whose reader stalls does not hold the shard; the audit log keeps each shard's events; the
//! node's log keeps each shard's lines in order and counts the lines it dropped past its bound.
#![cfg(target_os = "linux")]

mod engine_util;

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_util::{off_shard, on_engine};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::Smp;
use teseladb_auth::audit::{AuditType, Destination, Format};
use teseladb_bson::Document;
use teseladb_io::engine::stacks;
use teseladb_server::audit_log::{AuditLog, AuditOpts};
use teseladb_server::outlet::dest::{FileDest, StreamDest};
use teseladb_server::outlet::{Batch, Dest, HOME, Outlet, ShardWriter};

const T: Duration = Duration::from_secs(10);
/// Messages each shard pushes.
const EACH: usize = 300;
/// Lines of the pipe case: far more than a pipe holds.
const LINES: usize = 2000;

/// What a destination was handed, and on which shards.
#[derive(Default)]
struct Seen {
    bytes: Mutex<Vec<u8>>,
    off_home: AtomicUsize,
    batches: AtomicUsize,
}

/// A destination that keeps what it is handed, a little slowly, so that batches queue up.
struct Recorder(Arc<Seen>);

impl Dest for Recorder {
    fn writer(&self) -> Rc<dyn ShardWriter> {
        Rc::new(Slow(Arc::clone(&self.0)))
    }
    fn write_now(&self, batch: &Batch) {
        self.0.bytes.lock().unwrap().extend_from_slice(batch.bytes());
    }
}

struct Slow(Arc<Seen>);

impl ShardWriter for Slow {
    fn write(self: Rc<Self>, batch: Batch) -> Future<()> {
        if this_shard_id() != HOME {
            self.0.off_home.fetch_add(1, Ordering::AcqRel);
        }
        self.0.batches.fetch_add(1, Ordering::AcqRel);
        self.0.bytes.lock().unwrap().extend_from_slice(batch.bytes());
        sleep_steady(Duration::from_millis(1))
    }
}

/// Waits on the shard, without blocking it, until `done` holds or `T` passes.
async fn until(done: impl Fn() -> bool) -> bool {
    let end = Instant::now() + T;
    while !done() && Instant::now() < end {
        let tick: Future<()> = sleep_steady(Duration::from_millis(2));
        tick.await.unwrap();
    }
    done()
}

fn shards() -> usize {
    usize::try_from(Smp::count()).unwrap()
}

/// Every line of `text` as `(shard, number)`; each line must be whole.
fn numbered(text: &str) -> Vec<(u32, usize)> {
    text.lines()
        .map(|l| {
            let (s, i) = l.strip_prefix('s').and_then(|l| l.split_once(' ')).expect("a whole line");
            (s.parse().unwrap(), i.parse().unwrap())
        })
        .collect()
}

/// Whether each shard's numbers come in order, all of them once.
fn in_order(lines: &[(u32, usize)], each: usize) -> bool {
    let mut next: HashMap<u32, usize> = HashMap::new();
    for (s, i) in lines {
        let n = next.entry(*s).or_default();
        if *i != *n {
            return false;
        }
        *n += 1;
    }
    next.values().all(|n| *n == each)
}

/// Pushes `EACH` numbered lines from every shard.
fn push_everywhere(o: &Outlet) -> Future<()> {
    let o = o.clone();
    Smp::invoke_on_all(move || {
        let s = this_shard_id();
        for i in 0..EACH {
            o.push(format!("s{s} {i}\n").as_bytes());
        }
    })
}

#[test]
fn every_shard_s_lines_arrive_whole_in_order_and_from_shard_0() {
    let (in_order_all, off_home, batches, flushed) = on_engine(|| {
        make_coroutine(async move {
            let seen = Arc::new(Seen::default());
            let o = Outlet::new(Box::new(Recorder(Arc::clone(&seen))), None);
            push_everywhere(&o).await?;
            let want = EACH * shards();
            let s = Arc::clone(&seen);
            assert!(
                until(
                    move || s.bytes.lock().unwrap().iter().filter(|b| **b == b'\n').count() >= want
                )
                .await
            );
            let text = String::from_utf8(seen.bytes.lock().unwrap().clone()).unwrap();
            let ordered = in_order(&numbered(&text), EACH);
            // A flush from an engine thread of the last shard returns once its line is written.
            let flushed = Arc::new(AtomicBool::new(false));
            let (o2, s2, f2) = (o.clone(), Arc::clone(&seen), Arc::clone(&flushed));
            let last = Smp::count() - 1;
            Smp::submit_to(last, move || {
                stacks::run(Box::new(move || {
                    o2.push(b"flushed\n");
                    o2.flush();
                    let text = s2.bytes.lock().unwrap().clone();
                    f2.store(text.ends_with(b"flushed\n"), Ordering::Release);
                }))
            })
            .await?;
            Ok::<_, ExceptionPtr>((
                ordered,
                seen.off_home.load(Ordering::Acquire),
                seen.batches.load(Ordering::Acquire),
                flushed.load(Ordering::Acquire),
            ))
        })
    });
    assert!(in_order_all, "every line whole, once, in its shard's order");
    assert_eq!(off_home, 0, "every batch written on shard 0");
    assert!(batches < EACH * shards(), "lines go out in batches: {batches}");
    assert!(flushed, "a flush from an engine thread waits until its line is written");
}

#[test]
fn past_the_bound_lines_are_dropped_and_counted() {
    let (dropped, kept) = on_engine(|| {
        make_coroutine(async move {
            let seen = Arc::new(Seen::default());
            // Room for six lines of ten bytes in the shard's buffer.
            let o = Outlet::new(Box::new(Recorder(Arc::clone(&seen))), Some(64));
            for i in 0..100 {
                o.push(format!("line {i:04}\n").as_bytes());
            }
            let s = Arc::clone(&seen);
            assert!(until(move || !s.bytes.lock().unwrap().is_empty()).await);
            sleep_steady(Duration::from_millis(20)).await?;
            let kept = seen.bytes.lock().unwrap().iter().filter(|b| **b == b'\n').count();
            Ok::<_, ExceptionPtr>((o.take_dropped(), kept))
        })
    });
    assert_eq!(kept, 6, "the buffer held what fits");
    assert_eq!(dropped, 94, "the rest was dropped and counted");
}

#[test]
fn a_file_is_appended_to_through_the_engine() {
    let dir = std::env::temp_dir().join(format!("teseladb-outlet-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lines.log");
    std::fs::write(&path, b"before\n").unwrap();
    let file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    let dest = FileDest::new(&path, file).unwrap();
    let o = Outlet::new(Box::new(dest), None);
    let p = path.clone();
    let text = on_engine(move || {
        make_coroutine(async move {
            push_everywhere(&o).await?;
            let lines = EACH * shards() + 1;
            let q = p.clone();
            // Every line there and whole: the file is read while the last write may still be
            // landing, whose blocks a reader can see out of order.
            let whole = |b: &[u8]| {
                b.ends_with(b"\n")
                    && !b.contains(&0)
                    && b.iter().filter(|c| **c == b'\n').count() >= lines
            };
            assert!(until(move || std::fs::read(&q).is_ok_and(|b| whole(&b))).await);
            Ok::<_, ExceptionPtr>(std::fs::read_to_string(&p).unwrap())
        })
    });
    let rest = text.strip_prefix("before\n").expect("what the file held stays first");
    assert!(in_order(&numbered(rest), EACH));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pipe_whose_reader_stalls_does_not_hold_the_shard() {
    let (mut reader, writer) = std::io::pipe().unwrap();
    let o = Outlet::new(Box::new(StreamDest::from_fd(OwnedFd::from(writer))), None);
    let body = "y".repeat(500);
    let (ticks, text) = on_engine(move || {
        make_coroutine(async move {
            for i in 0..LINES {
                o.push(format!("s0 {i} {body}\n").as_bytes());
            }
            // The reader waits 300 ms before reading: meanwhile the shard keeps running.
            let read = off_shard(move || {
                std::thread::sleep(Duration::from_millis(300));
                let mut text = String::new();
                let mut buf = vec![0u8; 1 << 16];
                while text.matches('\n').count() < LINES {
                    let n = reader.read(&mut buf).unwrap();
                    assert!(n > 0, "the pipe closed early");
                    text.push_str(std::str::from_utf8(&buf[..n]).unwrap());
                }
                text
            });
            let mut ticks = 0u32;
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(200) {
                sleep_steady(Duration::from_millis(5)).await?;
                ticks += 1;
            }
            let text = read.await?;
            Ok::<_, ExceptionPtr>((ticks, text))
        })
    });
    assert!(ticks >= 10, "the shard ran while the pipe was full: {ticks} ticks");
    let lines: Vec<(u32, usize)> = text
        .lines()
        .map(|l| {
            let mut parts = l.split(' ');
            assert_eq!(parts.next(), Some("s0"));
            let i = parts.next().unwrap().parse().unwrap();
            assert_eq!(parts.next().map(str::len), Some(500), "a whole line");
            (0, i)
        })
        .collect();
    assert!(in_order(&lines, LINES));
}

#[test]
fn the_audit_log_keeps_every_shard_s_events() {
    let dir = std::env::temp_dir().join(format!("teseladb-outlet-audit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("audit.json").to_str().unwrap().to_owned();
    let opts = AuditOpts {
        destination: Some(Destination::File.name().to_owned()),
        format: Some(Format::Json.name().to_owned()),
        path: Some(path.clone()),
        ..AuditOpts::default()
    };
    let log = AuditLog::open(&opts).unwrap().unwrap();
    let count = on_engine(move || {
        make_coroutine(async move {
            let l = log.clone();
            Smp::invoke_on_all(move || {
                for _ in 0..10 {
                    l.system(AuditType::Startup, Document::new());
                }
            })
            .await?;
            // From a thread that is not a shard: it waits until its event is written.
            let l = log.clone();
            off_shard(move || l.system(AuditType::Shutdown, Document::new())).await?;
            let want = 10 * shards() + 1;
            let p = path.clone();
            let lines = move || std::fs::read_to_string(&p).map_or(0, |t| t.lines().count());
            let l2 = lines.clone();
            assert!(until(move || l2() >= want).await);
            Ok::<_, ExceptionPtr>((lines(), want))
        })
    });
    assert_eq!(count.0, count.1, "every event once");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A buffer the logger writes to.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

// The node's log on the shards: every shard's lines whole and in its order; a shard that logs
// more than the bound in one go loses the rest, and the next line says how many.
#[test]
fn the_log_keeps_each_shard_s_order_and_counts_what_it_dropped() {
    use teseladb_server::log::{DROPPED, Format as LogFormat, LOG_BOUND, Level, Logger};
    let buf = Buffer::default();
    let log = Arc::new(Logger::new(Level::Info, LogFormat::Text, Box::new(buf.clone())));
    let shown = buf.clone();
    let (ordered, dropped_line) = on_engine(move || {
        make_coroutine(async move {
            let l = Arc::clone(&log);
            Smp::invoke_on_all(move || {
                let s = this_shard_id();
                for i in 0..EACH {
                    l.info("line", &[("s", &s), ("i", &i)]);
                }
            })
            .await?;
            let want = EACH * shards();
            let b = shown.clone();
            assert!(until(move || b.text().lines().count() >= want).await);
            let lines: Vec<(u32, usize)> = shown
                .text()
                .lines()
                .map(|l| {
                    let field = |k: &str| {
                        l.split(' ').find_map(|f| f.strip_prefix(k)).expect("a whole line")
                    };
                    (field("s=").parse().unwrap(), field("i=").parse().unwrap())
                })
                .collect();
            let ordered = in_order(&lines, EACH);
            // Twice the bound in one task: about half is dropped.
            let long = "z".repeat(1000);
            for _ in 0..(2 * LOG_BOUND / 1000) {
                log.info(&long, &[]);
            }
            sleep_steady(Duration::from_millis(50)).await?;
            log.info("after", &[]);
            let b = shown.clone();
            assert!(until(move || b.text().contains(" info after")).await);
            let text = shown.text();
            let dropped = text.lines().find(|l| l.contains(DROPPED)).map(str::to_owned);
            Ok::<_, ExceptionPtr>((ordered, dropped))
        })
    });
    assert!(ordered, "every line whole, once, in its shard's order");
    let line = dropped_line.expect("a line counts the dropped ones");
    let count: usize = line.rsplit("count=").next().unwrap().parse().unwrap();
    assert!(count > 0 && count < 2 * LOG_BOUND / 1000, "{line}");
}
