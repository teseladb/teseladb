//! The documents of an index scan's entries read together (010 FR-027): a key whose range
//! published a view on this node is read here, outside the cores (005 FR-028); the others go by
//! range, in batches of at most [`BATCH`] keys, all at once to the range's core, which reads them
//! as the leader or sends the batch on to the leader on another node, with the residual filter
//! when the plan has one that may travel. A batch that does not come back, and each key a leader
//! did not answer, is read on its own with every rule of a single read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use teseladb_io::runtime::CoreInput;
use teseladb_range::read::ReadConcern;
use teseladb_wire::commands::Fetched;
use teseladb_wire::error::CommandError;
use teseladb_types::GroupId;

use super::ServerBackend;
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;
use crate::pushed::{Fallback, note_fallback};

/// The most keys of one batch.
const BATCH: usize = 128;

/// Batches of keys sent to the core of their range.
static BATCHES: AtomicU64 = AtomicU64::new(0);
/// Keys those batches carried.
static KEYS_SENT: AtomicU64 = AtomicU64::new(0);
/// Keys sent with the residual filter.
static KEYS_FILTERED: AtomicU64 = AtomicU64::new(0);
/// Keys read on this node outside the cores.
static KEYS_HERE: AtomicU64 = AtomicU64::new(0);

/// Batches of document keys of index scans this node sent to their range since the process
/// started (010 FR-027).
pub fn lookup_batches_sent() -> u64 {
    BATCHES.load(Ordering::Relaxed)
}

/// Document keys of index scans those batches carried since the process started.
pub fn lookup_keys_sent() -> u64 {
    KEYS_SENT.load(Ordering::Relaxed)
}

/// Document keys of index scans sent with the residual filter for the leader to evaluate, since
/// the process started.
pub fn lookup_keys_filtered() -> u64 {
    KEYS_FILTERED.load(Ordering::Relaxed)
}

/// Document keys of index scans read on this node outside the cores since the process started.
pub fn lookup_keys_here() -> u64 {
    KEYS_HERE.load(Ordering::Relaxed)
}

/// A batch sent: the places of its keys and its answer to come.
type Waiting = (Vec<usize>, mpsc::Receiver<Result<DataReply, teseladb_range::host::RangeError>>);

impl ServerBackend {
    /// [`teseladb_wire::commands::Backend::fetch_documents`]: `None` for a read at a command's
    /// mark or with a read preference, which read key by key.
    pub(super) fn fetch_together(
        &mut self,
        keys: &[Vec<u8>],
        filter: Option<&[u8]>,
        concern: ReadConcern,
    ) -> Result<Option<Vec<Fetched>>, CommandError> {
        let primary = self.read_pref.as_ref().is_none_or(|p| p.mode == super::Mode::Primary);
        if self.read_mark.is_some() || !primary {
            return Ok(None);
        }
        let mut found: Vec<Option<Fetched>> = vec![None; keys.len()];
        let mut by_range: Vec<((GroupId, u16), Vec<usize>)> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            if let Some(value) = self.read_outside(key, concern) {
                KEYS_HERE.fetch_add(1, Ordering::Relaxed);
                found[i] = Some(value.map_or(Fetched::Missing, Fetched::Doc));
                continue;
            }
            let Some(e) = self.h.routes.lookup(key) else { continue };
            match by_range.iter_mut().find(|(r, _)| *r == (e.group, e.core)) {
                Some((_, places)) => places.push(i),
                None => by_range.push(((e.group, e.core), vec![i])),
            }
        }
        let waiting: Vec<Waiting> = by_range
            .iter()
            .flat_map(|(range, places)| places.chunks(BATCH).map(move |c| (*range, c)))
            .filter_map(|(range, places)| self.send_batch(range, (keys, places), filter, concern))
            .collect();
        let until = self.until();
        for (places, answer) in waiting {
            let wait = until.saturating_duration_since(Instant::now());
            match answer.recv_timeout(wait) {
                Ok(Ok(DataReply::Remote(RemoteReply::Looked(got))))
                    if got.len() == places.len() =>
                {
                    // A leader that could not evaluate the filter sent the documents to decide here.
                    let undecided = got.iter().any(|f| matches!(f, Some(Fetched::Doc(_))));
                    if filter.is_some() && undecided {
                        note_fallback(Fallback::Lookup);
                    }
                    for (i, f) in places.into_iter().zip(got) {
                        found[i] = f;
                    }
                }
                _ => note_fallback(Fallback::Lookup),
            }
        }
        keys.iter()
            .zip(found)
            .map(|(key, f)| match f {
                Some(f) => Ok(f),
                None => {
                    let doc = self.point_on_leader(key, concern)?;
                    Ok(doc.map_or(Fetched::Missing, Fetched::Doc))
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// Sends the keys of `places` to the core of `range`, with `filter`; `None` when the core
    /// does not take it.
    fn send_batch(
        &self,
        (group, core): (GroupId, u16),
        (keys, places): (&[Vec<u8>], &[usize]),
        filter: Option<&[u8]>,
        concern: ReadConcern,
    ) -> Option<Waiting> {
        let req = RemoteReq::Lookup {
            keys: places.iter().map(|&i| keys[i].clone()).collect(),
            concern,
            filter: filter.map(<[u8]>::to_vec),
        };
        let (tx, rx) = mpsc::channel();
        let msg = NodeMsg::Data(DataReq::Remote(group, req), tx);
        let sent =
            self.cores.get(usize::from(core)).is_some_and(|c| c.send(CoreInput::App(msg)).is_ok());
        if !sent {
            note_fallback(Fallback::Lookup);
            return None;
        }
        BATCHES.fetch_add(1, Ordering::Relaxed);
        KEYS_SENT.fetch_add(places.len() as u64, Ordering::Relaxed);
        if filter.is_some() {
            KEYS_FILTERED.fetch_add(places.len() as u64, Ordering::Relaxed);
        }
        Some((places.to_vec(), rx))
    }
}
