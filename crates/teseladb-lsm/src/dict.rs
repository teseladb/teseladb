//! The dictionaries of the collections (004 FR-022, research R7).
//!
//! A compaction that writes with [`Codec::ZstdDict`](crate::Codec::ZstdDict) samples the versions
//! of each collection it writes. When a collection has no dictionary yet, or compactions wrote
//! `dict_renew_bytes` of it since its dictionary was trained, the compaction trains one from those
//! samples and adds it to its manifest edit; the blocks written after that edit use it. A
//! dictionary that is not its collection's newest and that no file uses any more leaves the
//! manifest in the same edit. Flushes and exports write zstd alone, so that only compactions,
//! which run one at a time, change which files use a dictionary.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::config::LsmConfig;
use crate::manifest::{Dictionary, State};
use crate::sst::writer::CurrentDictionaries;
use crate::version::Version;

/// Bytes of samples per byte of dictionary, at most [`MAX_SAMPLES`] per collection.
const SAMPLES_PER_BYTE: usize = 100;
/// Most bytes of samples kept for one collection in one compaction.
const MAX_SAMPLES: usize = 8 << 20;
/// Shortest value worth a sample.
const MIN_SAMPLE: usize = 16;

/// Bytes written per collection since its dictionary was trained.
pub(crate) type Written = BTreeMap<Vec<u8>, u64>;

/// What one compaction learns of the collections it writes.
#[derive(Debug)]
pub(crate) struct Training {
    renew: u64,
    size: usize,
    cap: usize,
    /// The newest dictionary of each collection when the compaction started: its outputs use it.
    pub(crate) current: Rc<CurrentDictionaries>,
    /// The collection of each dictionary, by version.
    versions: BTreeMap<u64, Vec<u8>>,
    /// Files that use each dictionary when the compaction started.
    used: BTreeMap<u64, usize>,
    /// Bytes written per collection since its dictionary was trained, before this compaction.
    before: Written,
    written: Written,
    samples: BTreeMap<Vec<u8>, (usize, Vec<Vec<u8>>)>,
    next_version: u64,
}

impl Training {
    /// For a compaction that starts with the manifest at `state`, the files of `version` and
    /// `before` bytes written per collection since its dictionary was trained.
    pub(crate) fn new(cfg: &LsmConfig, state: &State, version: &Version, before: Written) -> Self {
        let mut used = BTreeMap::new();
        for t in version.tables().filter(|t| t.props.dictionary != 0) {
            *used.entry(t.props.dictionary).or_insert(0) += 1;
        }
        let versions = state.dictionaries.iter().map(|(v, d)| (*v, d.prefix.clone())).collect();
        Training {
            renew: cfg.dict_renew_bytes,
            size: cfg.dict_bytes,
            cap: cfg.dict_bytes.saturating_mul(SAMPLES_PER_BYTE).min(MAX_SAMPLES),
            current: Rc::new(state.current_dictionaries()),
            versions,
            used,
            before,
            written: BTreeMap::new(),
            samples: BTreeMap::new(),
            next_version: state.dictionaries.keys().next_back().map_or(1, |v| v + 1),
        }
    }

    /// A version of `logical` with `value` written by the compaction.
    pub(crate) fn note(&mut self, logical: &[u8], value: &[u8]) {
        let Some(prefix) = teseladb_keys::space::collection_of(logical) else { return };
        *self.written.entry(prefix.to_vec()).or_insert(0) += (logical.len() + value.len()) as u64;
        if value.len() < MIN_SAMPLE {
            return;
        }
        let (bytes, samples) = self.samples.entry(prefix.to_vec()).or_default();
        if *bytes + value.len() <= self.cap {
            *bytes += value.len();
            samples.push(value.to_vec());
        }
    }

    /// The dictionaries the compaction's edit adds and the versions it drops, given the
    /// dictionaries of the files it removes and of those it adds; and the bytes written per
    /// collection since its dictionary after the edit.
    pub(crate) fn finish(
        &mut self,
        removed: &[u64],
        added: &[u64],
    ) -> (Vec<Dictionary>, Vec<u64>, Written) {
        let mut written = self.before.clone();
        let mut trained = Vec::new();
        for (prefix, bytes) in std::mem::take(&mut self.written) {
            let since = written.entry(prefix.clone()).or_insert(0);
            *since += bytes;
            let due = !self.current.contains_key(&prefix) || *since >= self.renew;
            let samples = self.samples.remove(&prefix).map(|(_, s)| s).unwrap_or_default();
            if !due || samples.is_empty() {
                continue;
            }
            let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
            let content = teseladb_compress::zstd::train_dict(&refs, self.size);
            if content.is_empty() {
                continue;
            }
            *since = 0;
            let version = self.next_version;
            self.next_version += 1;
            trained.push(Dictionary { version, prefix, content: Arc::from(content) });
        }
        let mut used = self.used.clone();
        for v in removed.iter().filter(|v| **v != 0) {
            used.entry(*v).and_modify(|n| *n = n.saturating_sub(1));
        }
        for v in added.iter().filter(|v| **v != 0) {
            *used.entry(*v).or_insert(0) += 1;
        }
        // The newest of each collection stays, used or not.
        let mut newest: BTreeMap<&[u8], u64> =
            self.current.iter().map(|(p, (v, _))| (p.as_slice(), *v)).collect();
        for d in &trained {
            newest.insert(&d.prefix, d.version);
        }
        let dropped = self
            .versions
            .iter()
            .filter(|(v, prefix)| {
                newest.get(prefix.as_slice()) != Some(v) && used.get(v).is_none_or(|n| *n == 0)
            })
            .map(|(v, _)| *v)
            .collect();
        (trained, dropped, written)
    }
}
