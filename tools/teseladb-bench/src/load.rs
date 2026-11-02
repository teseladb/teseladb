//! Load driver of the network scenarios (002 FR-017): `--concurrency` workers, each with its own
//! connection, a warm-up that does not count, then `--duration` of timed operations, in a closed
//! loop (each worker sends its next operation when the last one answers) or, with `--rate`, in an
//! open loop at a fixed pace. In the open loop an operation's latency counts from the instant it
//! was due, not from when it was sent, so that a server that falls behind is not measured as if
//! the clients had waited for it (coordinated omission).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use teseladb_types::Histogram;

/// How a load runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Load {
    /// Untimed phase.
    pub warmup: Duration,
    /// Timed phase.
    pub duration: Duration,
    /// Workers.
    pub concurrency: usize,
    /// Operations per second of all the workers together (open loop), or `None` (closed loop).
    pub rate: Option<f64>,
}

/// What the timed phase gave.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// Latencies in nanoseconds by kind of operation.
    pub latencies: BTreeMap<&'static str, Histogram>,
    /// Operations that answered.
    pub ops: u64,
    /// Operations that failed.
    pub errors: u64,
    /// The first failure, for the report.
    pub first_error: Option<String>,
    /// Length of the timed phase.
    pub elapsed: Duration,
}

impl Outcome {
    fn merge(&mut self, other: Outcome) {
        for (k, h) in other.latencies {
            self.latencies.entry(k).or_default().merge(&h);
        }
        self.ops += other.ops;
        self.errors += other.errors;
        if self.first_error.is_none() {
            self.first_error = other.first_error;
        }
    }

    /// Answered operations per second.
    pub fn throughput(&self) -> f64 {
        self.ops as f64 / self.elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
    }
}

/// Runs `op` with `load.concurrency` workers; `open(worker)` gives each worker its state (its
/// connection) and `op` returns the kind of operation it did.
pub fn drive<C: Send>(
    load: &Load,
    open: impl Fn(usize) -> Result<C, String> + Sync,
    op: impl Fn(&mut C) -> Result<&'static str, String> + Sync,
) -> Result<Outcome, String> {
    let workers = load.concurrency.max(1);
    let mut states = Vec::with_capacity(workers);
    for w in 0..workers {
        states.push(open(w)?);
    }
    let start = Instant::now() + Duration::from_millis(1);
    let timed = start + load.warmup;
    let end = timed + load.duration;
    let pace =
        load.rate.map(|r| Duration::from_secs_f64(workers as f64 / r.max(f64::MIN_POSITIVE)));
    let total = Mutex::new(Outcome::default());
    std::thread::scope(|s| {
        for (w, mut state) in states.into_iter().enumerate() {
            let (op, total) = (&op, &total);
            s.spawn(move || {
                let mut mine = Outcome::default();
                // Each worker starts its schedule a fraction of the pace later than the previous one.
                let offset = pace.map_or(Duration::ZERO, |p| p.mul_f64(w as f64 / workers as f64));
                let mut due = start + offset;
                loop {
                    if pace.is_some() {
                        let now = Instant::now();
                        if due > now {
                            std::thread::sleep(due - now);
                        }
                    } else {
                        due = Instant::now();
                    }
                    if due >= end {
                        break;
                    }
                    let result = op(&mut state);
                    if due >= timed {
                        match result {
                            Ok(kind) => {
                                let ns =
                                    Instant::now().saturating_duration_since(due).as_nanos() as u64;
                                mine.latencies.entry(kind).or_default().record(ns);
                                mine.ops += 1;
                            }
                            Err(e) => {
                                mine.errors += 1;
                                mine.first_error.get_or_insert(e);
                            }
                        }
                    }
                    if let Some(p) = pace {
                        due += p;
                    }
                }
                total.lock().unwrap_or_else(std::sync::PoisonError::into_inner).merge(mine);
            });
        }
    });
    let mut out = total.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    out.elapsed = load.duration;
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn a_merge_keeps_the_first_error_it_had() {
        let failed =
            |e: &str| Outcome { errors: 1, first_error: Some(e.into()), ..Outcome::default() };
        let mut out = Outcome::default();
        out.merge(failed("first"));
        out.merge(failed("second"));
        assert_eq!((out.errors, out.first_error.as_deref()), (2, Some("first")));
    }

    fn load(rate: Option<f64>, concurrency: usize) -> Load {
        Load {
            warmup: Duration::from_millis(50),
            duration: Duration::from_millis(300),
            concurrency,
            rate,
        }
    }

    #[test]
    fn a_closed_loop_times_every_worker_and_kind() {
        let calls = AtomicU64::new(0);
        let out = drive(&load(None, 3), Ok, |_| {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_micros(200));
            Ok(if n.is_multiple_of(2) { "even" } else { "odd" })
        })
        .unwrap();
        assert!(out.ops > 100, "{}", out.ops);
        assert!(calls.load(Ordering::Relaxed) > out.ops, "the warm-up ran too, untimed");
        assert_eq!(out.latencies.keys().copied().collect::<Vec<_>>(), ["even", "odd"]);
        assert!(out.latencies["even"].percentile(50.0) >= 200_000);
        assert!(out.throughput() > 0.0);
        assert_eq!((out.errors, out.first_error), (0, None));
    }

    #[test]
    fn an_open_loop_keeps_its_pace_and_counts_from_the_due_instant() {
        let out = drive(&load(Some(1000.0), 2), |_| Ok(()), |_| Ok("op")).unwrap();
        // 1 000 per second for 300 ms.
        assert!((240..=330).contains(&out.ops), "{}", out.ops);
        // A server slower than the pace is measured with the time its requests waited to go out.
        let slow = drive(
            &load(Some(400.0), 1),
            |_| Ok(()),
            |_| {
                std::thread::sleep(Duration::from_millis(5));
                Ok("op")
            },
        )
        .unwrap();
        assert!(
            slow.latencies["op"].percentile(99.0) > 50_000_000,
            "{}",
            slow.latencies["op"].percentile(99.0)
        );
    }

    #[test]
    fn failures_are_counted_and_a_worker_that_cannot_start_stops_the_load() {
        let out =
            drive(&load(None, 1), |_| Ok(()), |_| Err::<&'static str, _>("refused".to_string()))
                .unwrap();
        assert!(out.errors > 0 && out.ops == 0);
        assert_eq!(out.first_error.as_deref(), Some("refused"));
        let failed = drive(
            &load(None, 2),
            |w| if w == 1 { Err("no connection".to_string()) } else { Ok(()) },
            |_| Ok("op"),
        );
        assert_eq!(failed.unwrap_err(), "no connection");
    }
}
