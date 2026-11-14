//! The first byte of a binary of subtype 6: what kind of encrypted value or payload follows
//! (the reference encryption library's `mc_fle_blob_subtype_t`).

/// The kinds of subtype 6 binaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobSubtype {
    /// A placeholder the reference query analysis daemon leaves for the driver (CSFLE).
    Fle1Placeholder = 0,
    /// A CSFLE value encrypted deterministically.
    Fle1Deterministic = 1,
    /// A CSFLE value encrypted at random.
    Fle1Random = 2,
    /// A placeholder of Queryable Encryption for the driver.
    Fle2Placeholder = 3,
    /// Version 1 insert payload (removed in 7.0).
    Fle2InsertUpdateV1 = 4,
    /// Version 1 equality find payload (removed in 7.0).
    Fle2FindEqualityV1 = 5,
    /// Version 1 unindexed value (removed in 7.0).
    Fle2UnindexedV1 = 6,
    /// Version 1 equality indexed value (removed in 7.0).
    Fle2EqualityIndexedV1 = 7,
    /// Version 1 range indexed value (removed in 7.0).
    Fle2RangeIndexedV1 = 9,
    /// Version 1 range find payload (removed in 7.0).
    Fle2FindRangeV1 = 10,
    /// What an insert or an update sends for an indexed field.
    Fle2InsertUpdate = 11,
    /// What an equality query sends.
    Fle2FindEquality = 12,
    /// What a range query sends.
    Fle2FindRange = 13,
    /// An equality indexed value as stored.
    Fle2EqualityIndexed = 14,
    /// A range indexed value as stored.
    Fle2RangeIndexed = 15,
    /// An unindexed value, stored as sent.
    Fle2Unindexed = 16,
    /// A text indexed value as stored.
    Fle2TextIndexed = 17,
    /// What a prefix, suffix or substring query sends.
    Fle2FindText = 18,
}

impl BlobSubtype {
    /// The subtype of a first byte.
    pub fn from_byte(b: u8) -> Option<BlobSubtype> {
        use BlobSubtype::*;
        Some(match b {
            0 => Fle1Placeholder,
            1 => Fle1Deterministic,
            2 => Fle1Random,
            3 => Fle2Placeholder,
            4 => Fle2InsertUpdateV1,
            5 => Fle2FindEqualityV1,
            6 => Fle2UnindexedV1,
            7 => Fle2EqualityIndexedV1,
            9 => Fle2RangeIndexedV1,
            10 => Fle2FindRangeV1,
            11 => Fle2InsertUpdate,
            12 => Fle2FindEquality,
            13 => Fle2FindRange,
            14 => Fle2EqualityIndexed,
            15 => Fle2RangeIndexed,
            16 => Fle2Unindexed,
            17 => Fle2TextIndexed,
            18 => Fle2FindText,
            _ => return None,
        })
    }

    /// The subtype of an encrypted binary's bytes.
    pub fn of(bytes: &[u8]) -> Option<BlobSubtype> {
        bytes.first().copied().and_then(BlobSubtype::from_byte)
    }

    /// Whether it is a value of Queryable Encryption as the collection keeps it.
    pub fn is_stored_fle2(self) -> bool {
        matches!(
            self,
            BlobSubtype::Fle2EqualityIndexed
                | BlobSubtype::Fle2RangeIndexed
                | BlobSubtype::Fle2Unindexed
                | BlobSubtype::Fle2TextIndexed
        )
    }

    /// Whether it is a payload of a query.
    pub fn is_find_payload(self) -> bool {
        matches!(
            self,
            BlobSubtype::Fle2FindEquality | BlobSubtype::Fle2FindRange | BlobSubtype::Fle2FindText
        )
    }
}
