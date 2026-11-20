//! A scan in key order that hands over whole data blocks as their column blocks (010 FR-033): a
//! block of documents of one version whose versions the read sees all, and that no other source
//! can change, is the same as its column block's documents in key order.

use teseladb_keys::Hlc;
use teseladb_keys::columns::ColumnsView;
use teseladb_types::Bytes;

use crate::cursor::Cursor;
use crate::entry::{self, RangeTombstone};
use crate::read::Step;
use crate::sst::reader::Table;

/// The documents of one data block, as its column block. A version of the block that only
/// serves the history (014) is no document and the batch leaves it out, where a scan of the rows
/// gives it for its reader to pass over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Columns {
    /// The logical key of the block's last document: a scan that stops after the batch goes on
    /// after it.
    pub last: Vec<u8>,
    /// Documents of the block.
    pub docs: usize,
    payload: Bytes,
}

impl Columns {
    /// The batch's columns.
    pub fn view(&self) -> ColumnsView<'_> {
        ColumnsView::parse(&self.payload).expect("a column block checked when it was read")
    }
}

/// A step of a scan that asks for columns: a step as any scan's, or a batch of documents.
#[derive(Debug)]
pub enum ColumnStep {
    /// A step as [`crate::Scan::next`] gives.
    Step(Step),
    /// The next documents of the scan, in key order.
    Columns(Columns),
}

/// The batch of data block `block` of `table` whose column block is `payload`.
pub(crate) fn batch(table: &Table, block: usize, payload: Bytes, docs: usize) -> Columns {
    Columns { last: entry::logical(table.last_key(block)).to_vec(), docs, payload }
}

/// The number of documents of column block `payload`, when it gives every field of `want`.
pub(crate) fn fits(payload: &Bytes, want: &[String]) -> Option<usize> {
    let view = ColumnsView::parse(payload).ok()?;
    view.has_all(want).then(|| view.docs())
}

/// What a scan at `read` of keys below `end`, with the range tombstones it sees, knows when it
/// decides whether to take a block's columns.
pub(crate) struct Ask<'a> {
    pub(crate) read: Hlc,
    pub(crate) end: &'a [u8],
    pub(crate) tombstones: &'a [RangeTombstone],
}

/// Whether a scan takes data block `i` of `table` as its column block, the cursor seeking
/// `target` into it or, without one, coming to its first entry: the block has a column block,
/// the read sees each of its versions, it starts where the cursor goes on and ends below
/// `ask.end`, no range tombstone touches it and no source of `others` may hold one of its keys.
pub(crate) fn takes<'c>(
    table: &Table,
    i: usize,
    target: Option<&[u8]>,
    ask: &Ask<'_>,
    mut others: impl Iterator<Item = &'c Cursor>,
) -> bool {
    let choice = (|| {
        let Some(e) = table.column_entry(i) else { return ColumnChoice::NoColumns };
        // The batch's last key comes from the index: a short one is not the block's last
        // document.
        if e.max_ts > ask.read || !table.exact_last(i) {
            return ColumnChoice::NotSeenWhole;
        }
        let (lo, hi) = table.block_span(i);
        let from_start = match (target, i) {
            (None, _) => true,
            (Some(t), 0) => entry::compare(t, &entry::first_version(lo)).is_le(),
            // The block's keys come after `lo`, the last of the block before it.
            (Some(t), _) => entry::compare(t, &entry::after_all_versions(lo)).is_le(),
        };
        if !from_start || hi >= ask.end {
            return ColumnChoice::Edge;
        }
        if ask.tombstones.iter().any(|t| t.start.as_slice() <= hi && t.end.as_slice() > lo) {
            return ColumnChoice::Tombstone;
        }
        match others.find(|c| c.may_give(lo, hi, table)) {
            Some(Cursor::Mem(_)) => ColumnChoice::Memtable,
            Some(_) => ColumnChoice::OtherFile,
            None => ColumnChoice::Taken,
        }
    })();
    CHOICES[choice as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    choice == ColumnChoice::Taken
}

/// What a scan that asks for columns decided at a data block ([`takes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnChoice {
    /// Its column block was taken.
    Taken,
    /// The block has no column block.
    NoColumns,
    /// A version of the block is above the read's mark, or its last key in the index is cut.
    NotSeenWhole,
    /// The scan starts inside the block or ends in it.
    Edge,
    /// A range tombstone touches the block.
    Tombstone,
    /// A memtable may give a key of the block.
    Memtable,
    /// Another file may give a key of the block.
    OtherFile,
}

impl ColumnChoice {
    /// Every choice, in the order of [`column_choices`].
    pub const ALL: [ColumnChoice; 7] = [
        ColumnChoice::Taken,
        ColumnChoice::NoColumns,
        ColumnChoice::NotSeenWhole,
        ColumnChoice::Edge,
        ColumnChoice::Tombstone,
        ColumnChoice::Memtable,
        ColumnChoice::OtherFile,
    ];

    /// Its label in the metrics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ColumnChoice::Taken => "taken",
            ColumnChoice::NoColumns => "no_columns",
            ColumnChoice::NotSeenWhole => "not_seen_whole",
            ColumnChoice::Edge => "edge",
            ColumnChoice::Tombstone => "tombstone",
            ColumnChoice::Memtable => "memtable",
            ColumnChoice::OtherFile => "other_file",
        }
    }
}

static CHOICES: [std::sync::atomic::AtomicU64; 7] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// How many data blocks the scans of this process that ask for columns decided each way of
/// [`ColumnChoice::ALL`] (010 FR-033).
#[must_use]
pub fn column_choices() -> [u64; 7] {
    std::array::from_fn(|i| CHOICES[i].load(std::sync::atomic::Ordering::Relaxed))
}
