//! What a zone map knows of one field in one data block (004 FR-039), shared by the LSM that
//! writes and reads zone maps and the executor that decides from them whether a block may hold a
//! document its filter accepts.
//!
//! The bounds are encodings of [`crate::encode_value`] in ascending order, without collation: their
//! bytes compare as the reference server compares the values across types, so a block whose
//! interval leaves a literal out holds no document whose value equals it or is on the far side of
//! it, and a block that keeps its values one by one holds no document whose value equals a
//! literal that is not among them.

/// What a block's zone map says of a top-level field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bounds<'a> {
    /// Nothing is known: the file keeps no zone for the field, the block was not summarised, or
    /// one of its versions holds an array or a value too long to keep there.
    Unknown,
    /// No version of the block holds the field.
    Absent,
    /// Every value of the field in the block's versions, encoded, lies in `[min, max]`; the
    /// versions without the field are not counted.
    Within {
        /// The lowest encoded value.
        min: &'a [u8],
        /// The highest encoded value.
        max: &'a [u8],
    },
    /// Every value of the field in the block's versions, encoded, is one of these, which are
    /// distinct and in ascending order; the versions without the field are not counted. A block
    /// whose versions hold few values of the field keeps them so.
    Among(&'a [Box<[u8]>]),
}

impl<'a> Bounds<'a> {
    /// The lowest and the highest encoded value of the field in the block, when it knows them.
    #[must_use]
    pub fn span(self) -> Option<(&'a [u8], &'a [u8])> {
        match self {
            Bounds::Unknown | Bounds::Absent => None,
            Bounds::Within { min, max } => Some((min, max)),
            Bounds::Among(values) => Some((&**values.first()?, &**values.last()?)),
        }
    }

    /// Whether a version of the block may hold the field with encoded value `x`: `false` only
    /// when the block knows that none does.
    #[must_use]
    pub fn may_hold(self, x: &[u8]) -> bool {
        match self {
            Bounds::Unknown => true,
            Bounds::Absent => false,
            Bounds::Within { min, max } => min <= x && x <= max,
            Bounds::Among(values) => values.binary_search_by(|v| (**v).cmp(x)).is_ok(),
        }
    }
}

/// The zone map of one data block, field by field.
pub trait Zones {
    /// What the block knows of top-level field `name`.
    fn field(&self, name: &str) -> Bounds<'_>;
}
