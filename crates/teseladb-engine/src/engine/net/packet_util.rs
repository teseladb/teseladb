//! Reassembly of out-of-order byte ranges into contiguous packets.
//!
//! [`PacketMerger`] keeps the segments received so far keyed by their starting offset
//! (a TCP sequence number or an IP fragment offset), merging each new packet with the
//! segments it overlaps or touches so that the map always holds disjoint, non-adjacent
//! ranges. It is used by TCP for out-of-order data and by IPv4 for fragment reassembly; it
//! belongs to one shard and is not shared.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::ops::Bound::{Excluded, Unbounded};
use std::thread::LocalKey;

use crate::engine::net::packet::Packet;

/// The offset type a [`PacketMerger`] keys its segments by: ordered, with `offset + len` and
/// the distance between two offsets (always non-negative where the merger uses it).
pub trait MergerOffset: Copy + Ord + 'static {
    /// Returns `self + len`, wrapping on overflow.
    fn add_len(self, len: u32) -> Self;
    /// Returns `self - earlier` as a byte count, wrapping on underflow.
    fn diff(self, earlier: Self) -> usize;
}

impl MergerOffset for u32 {
    fn add_len(self, len: u32) -> Self {
        self.wrapping_add(len)
    }
    fn diff(self, earlier: Self) -> usize {
        self.wrapping_sub(earlier) as usize
    }
}

impl MergerOffset for u64 {
    fn add_len(self, len: u32) -> Self {
        self.wrapping_add(len as u64)
    }
    fn diff(self, earlier: Self) -> usize {
        self.wrapping_sub(earlier) as usize
    }
}

/// A marker type that selects the per-thread linearization counter of a [`PacketMerger`];
/// each tag is used with one offset type, so there is one counter per kind of merger.
pub trait MergerTag: 'static {
    /// The tag's per-thread counter of packet linearizations.
    fn linearization_count() -> &'static LocalKey<Cell<u64>>;
}

/// Segments of a byte stream received out of order, keyed by their starting offset.
///
/// Invariant after every [`merge`](Self::merge): the segments in `map` are disjoint and not
/// adjacent, and each is a linearized (single-fragment) packet unless it grew by appending.
pub struct PacketMerger<Offset: MergerOffset, Tag: MergerTag> {
    /// The segments, keyed by the offset of their first byte.
    pub map: BTreeMap<Offset, Packet>,
    _tag: PhantomData<Tag>,
}

impl<Offset: MergerOffset, Tag: MergerTag> Default for PacketMerger<Offset, Tag> {
    fn default() -> Self {
        PacketMerger { map: BTreeMap::new(), _tag: PhantomData }
    }
}

impl<Offset: MergerOffset, Tag: MergerTag> PacketMerger<Offset, Tag> {
    #[inline]
    fn bump_linearizations() {
        Tag::linearization_count().with(|c| c.set(c.get() + 1));
    }

    /// Number of packets this thread has linearized while merging, for all mergers with the
    /// same tag (a statistic exported as a metric).
    #[inline]
    pub fn linearizations() -> u64 {
        Tag::linearization_count().with(Cell::get)
    }

    fn next_key(&self, k: Offset) -> Option<Offset> {
        self.map.range((Excluded(k), Unbounded)).next().map(|(k, _)| *k)
    }

    fn first_key(&self) -> Option<Offset> {
        self.map.keys().next().copied()
    }

    /// Adds the bytes of `p`, which start at `offset`, merging them with the stored segments
    /// they overlap or touch. Bytes already present are dropped; the stored data wins over the
    /// new one where they overlap.
    ///
    /// O(n) in the number of stored segments; linearizes the packets it stores.
    ///
    /// # Panics
    /// Aborts the process if the segment map is found inconsistent (an internal bug).
    pub fn merge(&mut self, offset: Offset, mut p: Packet) {
        let mut insert = true;
        let beg = offset;
        let end = beg.add_len(p.len());
        // First, try to merge the packet with an existing segment
        let mut it = self.first_key();
        while let Some(key) = it {
            let seg_beg = key;
            let seg_len = self.map[&key].len();
            let seg_end = seg_beg.add_len(seg_len);
            // There are 6 cases:
            if seg_beg <= beg && end <= seg_end {
                // 1) seg_beg beg end seg_end
                // We already have data in this packet
                return;
            } else if beg <= seg_beg && seg_end <= end {
                // 2) beg seg_beg seg_end end
                // The new segment contains more data than this old segment
                // Delete the old one, insert the new one
                self.map.remove(&key);
                insert = true;
                break;
            } else if beg < seg_beg && seg_beg <= end && end <= seg_end {
                // 3) beg seg_beg end seg_end
                // Merge two segments, trim front of old segment
                let trim = end.diff(seg_beg);
                let mut seg_pkt = self.map.remove(&key).expect("segment");
                seg_pkt.trim_front(trim);
                p.append(seg_pkt);
                // Delete the old one, insert the new one
                insert = true;
                break;
            } else if seg_beg <= beg && beg <= seg_end && seg_end < end {
                // 4) seg_beg beg seg_end end
                // Merge two segments, trim front of new segment
                let trim = seg_end.diff(beg);
                p.trim_front(trim);
                // Append new data to the old segment, keep the old segment
                let seg_pkt = self.map.get_mut(&key).expect("segment");
                seg_pkt.append(std::mem::replace(&mut p, Packet::make_null_packet()));
                seg_pkt.linearize_all();
                Self::bump_linearizations();
                insert = false;
                break;
            } else {
                // 5) beg end < seg_beg seg_end
                //   or
                // 6) seg_beg seg_end < beg end
                // Can not merge with this segment, keep looking
                it = self.next_key(key);
                insert = true;
            }
        }

        if insert {
            p.linearize_all();
            Self::bump_linearizations();
            self.map.entry(beg).or_insert(p);
        }

        // Second, merge adjacent segments after this packet has been merged,
        // because this packet might fill a hole and make two adjacent
        // segments mergeable
        let mut it = self.first_key();
        while let Some(key) = it {
            // The first segment
            let seg_beg = key;
            let seg_end = seg_beg.add_len(self.map[&key].len());

            // The second segment
            let Some(it_next) = self.next_key(key) else {
                break;
            };
            let beg = it_next;
            let end = beg.add_len(self.map[&it_next].len());

            // Merge the second segment into the first segment if possible
            if seg_beg <= beg && beg <= seg_end && seg_end < end {
                // Merge two segments, trim front of second segment
                let trim = seg_end.diff(beg);
                // Delete the second segment
                let mut p = self.map.remove(&it_next).expect("segment");
                p.trim_front(trim);
                // Append new data to the first segment, keep the first segment
                self.map.get_mut(&key).expect("segment").append(p);

                // Keep merging this first segment with its new next packet
                // So we do not update the iterator: it
                continue;
            } else if end <= seg_end {
                // The first segment has all the data in the second segment
                // Delete the second segment
                self.map.remove(&it_next);
                continue;
            } else if seg_end < beg {
                // Can not merge first segment with second segment
                it = Some(it_next);
                continue;
            } else {
                // If we reach here, we have a bug with merge.
                std::process::abort();
            }
        }
    }
}
