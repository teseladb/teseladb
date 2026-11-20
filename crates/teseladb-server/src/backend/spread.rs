//! A batch of document keys read by the shards of its ranges' cores at once (010 FR-027). The
//! keys of an index scan's entries are read from the views of their ranges on the command's
//! thread ([`super::lookups`]), one range after the other: an aggregation over a collection
//! spread over the node's cores (D-186) read its documents from each partition in turn, on one
//! shard, while the others had time to spare (002 run p75: 7.1 of the 7.8 ms of an aggregation by
//! an index were that read, a document costing the same in a large batch as in a small one).
//!
//! Here each range's keys go to the shard of its core, where an engine thread reads them from
//! the same views with the same rules and answers what it read; the command's thread reads the
//! keys of its own shard meanwhile and waits for the others. A part that does not come back in
//! time, and each key a part could not read, is read as before.

use std::sync::atomic::Ordering;

use teseladb_io::engine::answer as mpsc;
use teseladb_io::engine::{stacks, wait};
use teseladb_range::read::ReadConcern;
use teseladb_wire::commands::Fetched;

use super::ServerBackend;
use super::lookups::{KEYS_SPREAD, SPREAD_MIN};

/// The keys of one part in one buffer: they cross to another shard, where a buffer for each
/// would be freed far from the shard that made it.
struct Packed {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl Packed {
    fn of(keys: &[Vec<u8>], places: &[usize]) -> Packed {
        let size = places.iter().map(|&i| keys[i].len()).sum();
        let mut packed =
            Packed { bytes: Vec::with_capacity(size), ends: Vec::with_capacity(places.len()) };
        for &i in places {
            packed.bytes.extend_from_slice(&keys[i]);
            packed.ends.push(packed.bytes.len());
        }
        packed
    }

    fn keys(&self) -> Vec<Vec<u8>> {
        let mut from = 0;
        self.ends
            .iter()
            .map(|&end| {
                let key = self.bytes[from..end].to_vec();
                from = end;
                key
            })
            .collect()
    }
}

/// A part another shard reads: the places of its keys and its answer to come.
type Away = (Vec<usize>, mpsc::Receiver<Vec<Option<Fetched>>>);

impl ServerBackend {
    /// The places of `keys` by the core of their range, in key order within each; a key no
    /// range holds is in none.
    fn places_by_core(&mut self, keys: &[Vec<u8>]) -> Vec<(u16, Vec<usize>)> {
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_unstable_by(|a, b| keys[*a].cmp(&keys[*b]));
        let mut by_core: Vec<(u16, Vec<usize>)> = Vec::new();
        let mut at = 0;
        while at < order.len() {
            let Some((core, range_end)) = self.core_and_end_of(&keys[order[at]]) else {
                at += 1;
                continue;
            };
            let in_range = |k: &[u8]| range_end.is_empty() || k < range_end.as_slice();
            let mut to = at + 1;
            while to < order.len() && in_range(keys[order[to]].as_slice()) {
                to += 1;
            }
            match by_core.iter_mut().find(|(c, _)| *c == core) {
                Some((_, places)) => places.extend_from_slice(&order[at..to]),
                None => by_core.push((core, order[at..to].to_vec())),
            }
            at = to;
        }
        by_core
    }

    /// Posts the keys at `places` to `shard`, where an engine thread reads them from the views
    /// as this backend would, with what its session's transactions wrote; `None` when the
    /// shard does not take the work.
    fn read_on_shard(
        &mut self,
        shard: u32,
        (keys, places): (&[Vec<u8>], &[usize]),
        concern: ReadConcern,
    ) -> Option<mpsc::Receiver<Vec<Option<Fetched>>>> {
        let packed = Packed::of(keys, places);
        let (h, across, committed) = (self.h.clone(), self.across.clone(), self.committed);
        let seed = teseladb_wire::commands::Backend::random(self);
        let (tx, rx) = mpsc::channel();
        let job = move || {
            let mut b = ServerBackend::new(h, seed);
            b.across = across;
            b.committed = committed;
            let found = b.fetch_here(&packed.keys(), concern);
            b.report_outside();
            b.release_outside();
            // The command gave up waiting: nobody takes the answer.
            let _ = tx.send(found);
        };
        wait::post(shard, Box::new(move || drop(stacks::run(Box::new(job))))).then_some(rx)
    }

    /// Reads `keys`, when they are [`SPREAD_MIN`] or more and lie in the ranges of several
    /// cores, with each core's on its shard at once, and leaves what was read in `found`;
    /// `false`, with nothing read, when the batch is small, its keys are of one core or this is
    /// no engine thread, which cannot wait for the other shards.
    pub(super) fn fetch_spread(
        &mut self,
        keys: &[Vec<u8>],
        concern: ReadConcern,
        found: &mut [Option<Fetched>],
    ) -> bool {
        if keys.len() < SPREAD_MIN || !wait::in_engine_thread() {
            return false;
        }
        let Some(here) = wait::this_shard() else { return false };
        let by_core = self.places_by_core(keys);
        if by_core.len() < 2 {
            return false;
        }
        let mut away: Vec<Away> = Vec::new();
        let mut mine: Vec<usize> = Vec::new();
        for (core, places) in by_core {
            let sent = match u32::from(core) == here {
                true => None,
                false => self.read_on_shard(u32::from(core), (keys, &places), concern),
            };
            match sent {
                Some(answer) => away.push((places, answer)),
                None => mine.extend(places),
            }
        }
        // This shard's own keys, while the others read theirs.
        let own: Vec<Vec<u8>> = mine.iter().map(|&i| keys[i].clone()).collect();
        for (place, f) in mine.into_iter().zip(self.fetch_here(&own, concern)) {
            found[place] = f;
        }
        let until = self.until();
        for (places, answer) in away {
            // A part that does not come back leaves its keys to be read one by one.
            let Ok(got) = answer.recv_deadline(until) else { continue };
            if got.len() != places.len() {
                continue;
            }
            let n = u64::try_from(places.len()).unwrap_or(u64::MAX);
            KEYS_SPREAD.fetch_add(n, Ordering::Relaxed);
            for (place, f) in places.into_iter().zip(got) {
                found[place] = f;
            }
        }
        true
    }
}
