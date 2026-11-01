//! Training of raw content dictionaries (RFC 8878 §5) from samples, by covering: the samples are
//! cut into as many epochs as the dictionary has segments, and each epoch gives the segment whose
//! 8-byte substrings appear in the most other samples. Once a segment is taken, its substrings
//! count no more, so the next ones bring something new. The best segments go last, nearest to the
//! data they will precede, where offsets are shortest.

use std::collections::{HashMap, HashSet};

/// Length of the substrings counted.
const DMER: usize = 8;
/// Length of a segment of the dictionary.
const SEGMENT: usize = 128;

fn dmer(s: &[u8], i: usize) -> u64 {
    let mut b = [0u8; DMER];
    b.copy_from_slice(&s[i..i + DMER]);
    u64::from_le_bytes(b)
}

/// A raw content dictionary of at most `capacity` bytes for data like `samples`; empty when the
/// samples repeat nothing among them.
pub fn train_dict(samples: &[&[u8]], capacity: usize) -> Vec<u8> {
    let seg = SEGMENT.min(capacity);
    if seg < DMER {
        return Vec::new();
    }
    // In how many samples each substring appears, beyond the first.
    let mut weight: HashMap<u64, u32> = HashMap::new();
    for s in samples {
        let seen: HashSet<u64> = (0..=s.len().saturating_sub(DMER))
            .filter(|&i| i + DMER <= s.len())
            .map(|i| dmer(s, i))
            .collect();
        for d in seen {
            *weight.entry(d).or_insert(0) += 1;
        }
    }
    weight.values_mut().for_each(|w| *w = w.saturating_sub(1));
    let total: usize = samples.iter().map(|s| s.len()).sum();
    let epochs = capacity.div_ceil(seg).max(1);
    let per_epoch = total.div_ceil(epochs).max(1);
    let mut segments: Vec<&[u8]> = Vec::new();
    let mut size = 0;
    let mut epoch: Vec<&[u8]> = Vec::new();
    let mut epoch_bytes = 0;
    for (k, s) in samples.iter().enumerate() {
        epoch.push(s);
        epoch_bytes += s.len();
        if epoch_bytes < per_epoch && k + 1 < samples.len() {
            continue;
        }
        if let Some(best) = best_segment(&epoch, seg, &weight) {
            for i in 0..=best.len() - DMER {
                weight.insert(dmer(best, i), 0);
            }
            size += best.len();
            segments.push(best);
        }
        epoch.clear();
        epoch_bytes = 0;
        if size >= capacity {
            break;
        }
    }
    // The first epochs' best segments go last.
    let dict: Vec<u8> = segments.iter().rev().flat_map(|s| s.iter().copied()).collect();
    dict[dict.len().saturating_sub(capacity)..].to_vec()
}

/// The window of at most `seg` bytes of the samples of `epoch` whose substrings weigh most;
/// `None` if none weighs anything.
fn best_segment<'a>(
    epoch: &[&'a [u8]],
    seg: usize,
    weight: &HashMap<u64, u32>,
) -> Option<&'a [u8]> {
    let mut best: Option<(u64, &[u8])> = None;
    for s in epoch.iter().filter(|s| s.len() >= DMER) {
        let len = seg.min(s.len());
        let w: Vec<u64> = (0..=s.len() - DMER)
            .map(|i| u64::from(weight.get(&dmer(s, i)).copied().unwrap_or(0)))
            .collect();
        // The window at `i` holds the substrings starting in `i..=i + len - DMER`.
        let span = len - DMER + 1;
        let mut score: u64 = w[..span].iter().sum();
        for i in 0..=s.len() - len {
            if i > 0 {
                score = score - w[i - 1] + w[i + span - 1];
            }
            if score > 0 && best.is_none_or(|(b, _)| score > b) {
                best = Some((score, &s[i..i + len]));
            }
        }
    }
    best.map(|(_, s)| s)
}
