//! The reapers of abandoned transactions of this node (008 FR-007, T011): one per core, each
//! reading the transaction records of the ranges its core leads (the space `[02, 03)`) and
//! handing them to its [`Reaper`], which pushes the pending ones without a heartbeat for five
//! seconds and resolves the intents of those that end. The node's keeper thread runs a pass every
//! [`SCAN_EVERY`].

use std::collections::BTreeMap;
use std::time::Duration;

use teseladb_hlc::Hlc;
use teseladb_raft::Role;
use teseladb_range::intents::TxnRecord;
use teseladb_range::read::ReadConcern;
use teseladb_router::table::RouteEntry;
use teseladb_txn::reaper::Reaper;
use teseladb_txn::record::HEARTBEAT_EVERY_MS;
use teseladb_types::GroupId;

use super::ServerBackend;
use super::across::{Routes, reply_of};
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;

/// How often the reapers read their records: as often as a coordinator beats.
pub(crate) const SCAN_EVERY: Duration = Duration::from_millis(HEARTBEAT_EVERY_MS);
/// First key of the transaction records.
const RECORDS_START: &[u8] = &[2];
/// First key past them.
const RECORDS_END: &[u8] = &[3];
/// Records read per scan request.
const PAGE: u32 = 256;

/// The reapers of the node's cores, and the ranges they found without records (H-040).
#[derive(Debug, Default)]
pub struct Reapers {
    cores: BTreeMap<u16, Reaper>,
    /// For each range read without records, its transaction record mark then.
    empty: BTreeMap<GroupId, u64>,
}

impl Reapers {
    /// How many cores have a reaper: one is made when its first range is read.
    pub fn cores(&self) -> usize {
        self.cores.len()
    }

    /// Whether range `g`, whose core publishes `mark`, needs reading: no record can appear without
    /// an entry that moves the mark, so a range found empty under the same mark is not read nor
    /// asked about again (008 FR-007).
    fn needs_read(&self, g: GroupId, mark: Option<u64>) -> bool {
        mark.is_none() || self.empty.get(&g) != mark.as_ref()
    }
}

/// Where range `e` and the records overlap, if they do.
fn records_of(e: &RouteEntry) -> Option<(Vec<u8>, Vec<u8>)> {
    let start = e.start.as_slice().max(RECORDS_START).to_vec();
    let end = match e.end.as_slice() {
        [] => RECORDS_END.to_vec(),
        end => end.min(RECORDS_END).to_vec(),
    };
    (start < end).then_some((start, end))
}

impl ServerBackend {
    /// Whether this node's replica of the range of `e` leads it.
    fn leads(&self, e: &RouteEntry) -> bool {
        let until = self.until();
        let msg = |tx| NodeMsg::Data(DataReq::Status(e.group), tx);
        matches!(
            self.ask(usize::from(e.core), msg, until),
            Some(Ok(DataReply::Status(s))) if s.role == Some(Role::Leader)
        )
    }

    /// The records in `[start, end)` of the range of `e`, read by its leader on this node; `None`
    /// if a scan failed (the next pass reads them again).
    fn records(
        &self,
        e: &RouteEntry,
        (mut start, end): (Vec<u8>, Vec<u8>),
    ) -> Option<Vec<TxnRecord>> {
        let mut out = Vec::new();
        loop {
            let scan = RemoteReq::Scan {
                start: start.clone(),
                end: end.clone(),
                limit: PAGE,
                concern: ReadConcern::Local,
                backward: false,
            };
            let msg = |tx| NodeMsg::Data(DataReq::Remote(e.group, scan), tx);
            let Some(Ok(DataReply::Remote(RemoteReply::Rows(rows)))) =
                self.ask(usize::from(e.core), msg, self.until())
            else {
                return None;
            };
            let full = rows.len() == PAGE as usize;
            for (key, value) in rows {
                // A record that does not decode is skipped: no reaper can push it either.
                if let Ok(rec) = TxnRecord::decode_row(&value) {
                    out.push(rec);
                }
                start = [key.as_slice(), &[0]].concat();
            }
            if !full {
                return Some(out);
            }
        }
    }

    /// One pass of the reapers of this node's cores; the transactions they aborted. It ends
    /// between two requests once `stop` is set: a node that stops does not wait for a pass whose
    /// requests may each take their whole deadline.
    pub fn reap_transactions(
        &mut self,
        reapers: &mut Reapers,
        stop: &std::sync::atomic::AtomicBool,
    ) -> u64 {
        let stopping = || stop.load(std::sync::atomic::Ordering::Acquire);
        let routes = self.h.routes.clone();
        let route = Routes(&routes);
        let now = Hlc { physical: self.h.clock.0.peek().physical, logical: 0 };
        let before: u64 = reapers.cores.values().map(Reaper::reaped).sum();
        for e in routes.entries() {
            if stopping() {
                break;
            }
            let Some(span) = records_of(&e) else { continue };
            // Read before the records: an entry applied after it moves it again.
            let mark = self.h.views.get(usize::from(e.core)).and_then(|v| v.txn_mark(e.group));
            if !reapers.needs_read(e.group, mark) {
                continue;
            }
            if !self.leads(&e) {
                continue;
            }
            let Some(records) = self.records(&e, span) else { continue };
            match mark {
                Some(m) if records.is_empty() => reapers.empty.insert(e.group, m),
                _ => reapers.empty.remove(&e.group),
            };
            let reaper = reapers.cores.entry(e.core).or_default();
            reaper.scan(&route, now, records);
            loop {
                let requests = reaper.take_requests();
                if requests.is_empty() {
                    break;
                }
                if stopping() {
                    break;
                }
                for r in requests {
                    let answer = self.ask_range(r.g, r.op).and_then(reply_of);
                    reaper.on_reply(&route, r.id, answer);
                }
            }
        }
        reapers.cores.values().map(Reaper::reaped).sum::<u64>() - before
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn entry(start: &[u8], end: &[u8]) -> RouteEntry {
        RouteEntry {
            group: GroupId(1),
            start: start.to_vec(),
            end: end.to_vec(),
            core: 0,
            voters: Vec::new(),
            generation: 0,
            leader_hint: None,
            moved_ms: 0,
            replica_epoch: 1,
        }
    }

    #[test]
    fn a_range_found_empty_is_read_again_only_when_its_mark_moves() {
        let g = GroupId(2);
        let mut r = Reapers::default();
        assert!(r.needs_read(g, Some(7)), "never read");
        r.empty.insert(g, 7);
        assert!(!r.needs_read(g, Some(7)), "empty under the same mark");
        assert!(r.needs_read(g, Some(9)), "an entry touched the records");
        assert!(r.needs_read(g, None), "its core does not publish it");
        assert!(r.needs_read(GroupId(3), Some(7)), "another range");
    }

    #[test]
    fn a_range_overlaps_the_records_where_their_spaces_meet() {
        let records = |s: &[u8], e: &[u8]| records_of(&entry(s, e));
        assert_eq!(records(&[1], &[]), Some((vec![2], vec![3])));
        assert_eq!(records(&[2, 5], &[2, 9]), Some((vec![2, 5], vec![2, 9])));
        assert_eq!(records(&[1], &[2, 5]), Some((vec![2], vec![2, 5])));
        assert_eq!(records(&[1], &[2]), None);
        assert_eq!(records(&[3], &[]), None);
    }
}
