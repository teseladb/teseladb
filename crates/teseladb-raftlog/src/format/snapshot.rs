//! Group snapshots (data-model §7).
//!
//! With encryption at rest the body is sealed with a data key of its own, wrapped with
//! the master key in the reserved area of the header:
//!
//! ```text
//! sealed := header (wrapped key) · trailer(32) · ciphertext of the body · crc32c(u32 LE)
//! ```

use teseladb_crypto::at_rest::{KeyRing, TRAILER_LEN};
use teseladb_hash::crc32c;
use teseladb_types::{Bytes, Decoder, Encoder, GroupId};

use super::{BLOCK, FileHeader, FileKind, FormatError, le_u32};

/// Snapshot of a Raft group (opaque contents for the engine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupSnapshot {
    /// Group.
    pub group: GroupId,
    /// Index it reflects.
    pub index: u64,
    /// Term of that index.
    pub term: u64,
    /// Group configuration.
    pub conf: Bytes,
    /// State of the state machine.
    pub data: Bytes,
}

/// What a sealed snapshot is bound to: `<domain> group snapshot`, with the domain of the key ring.
fn sealed_aad(ring: &KeyRing) -> Vec<u8> {
    [ring.domain().as_bytes(), b" group snapshot"].concat()
}

impl GroupSnapshot {
    /// Encodes the complete file (common header included), sealed with a new data key of `ring`
    /// when there is one.
    ///
    /// # Panics
    ///
    /// When the key ring's seed source fails: the data cannot be written unencrypted.
    #[must_use]
    pub fn encode_with(&self, header: &FileHeader, ring: Option<&KeyRing>) -> Vec<u8> {
        let Some(ring) = ring else {
            return self.encode(header);
        };
        // The ring reseeds itself from its seed source, and a new key seals its first block.
        let (key, w) = ring
            .new_file_key()
            .expect("a key ring seeded by the system makes every data key it is asked for");
        let (ct, trailer) = key
            .seal(&sealed_aad(ring), &self.body())
            .expect("a new data key seals its first block");
        let header =
            FileHeader { kind: FileKind::Snapshot, reserved: w.to_vec(), ..header.clone() };
        let mut out = header.encode().to_vec();
        let mut sealed = trailer.to_vec();
        sealed.extend_from_slice(&ct);
        let crc = crc32c(&sealed);
        out.extend_from_slice(&sealed);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decodes a complete file, opening a sealed one with `ring`; a sealed file without a ring,
    /// or whose key the ring does not open, is [`FormatError::Invalid`].
    ///
    /// # Errors
    ///
    /// As [`Self::decode`], and [`FormatError::Invalid`] for a sealed file without a ring or whose key the ring
    /// does not open.
    pub fn decode_with(
        file: &[u8],
        ring: Option<&KeyRing>,
    ) -> Result<(FileHeader, GroupSnapshot), FormatError> {
        let header = FileHeader::decode(file)?;
        let Some(w) = header.wrapped_key() else {
            return GroupSnapshot::decode(file);
        };
        let body = GroupSnapshot::checked_body(file, &header)?;
        let ring = ring.ok_or(FormatError::Invalid("encrypted snapshot"))?;
        let key = ring.open_file_key(&w).map_err(|_| FormatError::Invalid("snapshot key"))?;
        let (trailer, ct) = body.split_at(TRAILER_LEN.min(body.len()));
        let trailer: &[u8; TRAILER_LEN] = trailer.try_into().map_err(|_| FormatError::Truncated)?;
        let plain = key
            .open(&sealed_aad(ring), ct, trailer)
            .map_err(|_| FormatError::Invalid("snapshot"))?;
        Ok((header, GroupSnapshot::parse(&plain)?))
    }

    /// Whether the file is whole: its header and checksum, without opening a sealed body.
    ///
    /// # Errors
    ///
    /// [`FormatError::Truncated`], [`FormatError::BadMagic`], [`FormatError::BadCrc`], [`FormatError::Version`] or
    /// [`FormatError::Invalid`] for bytes that are not a valid snapshot file.
    pub fn verify(file: &[u8]) -> Result<(), FormatError> {
        let header = FileHeader::decode(file)?;
        match header.wrapped_key() {
            None => GroupSnapshot::decode(file).map(|_| ()),
            Some(_) => GroupSnapshot::checked_body(file, &header).map(|_| ()),
        }
    }

    /// The body of a snapshot file after its checksum is checked.
    fn checked_body<'a>(file: &'a [u8], header: &FileHeader) -> Result<&'a [u8], FormatError> {
        if header.kind != FileKind::Snapshot {
            return Err(FormatError::BadMagic);
        }
        if file.len() < BLOCK + 4 {
            return Err(FormatError::Truncated);
        }
        let body = &file[BLOCK..file.len() - 4];
        if crc32c(body) != le_u32(&file[file.len() - 4..]) {
            return Err(FormatError::BadCrc);
        }
        Ok(body)
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "a configuration is a few identifiers long"
    )]
    fn body(&self) -> Vec<u8> {
        let mut e = Encoder::with_capacity(36 + self.conf.len() + self.data.len());
        e.u64(self.group.0)
            .u64(self.index)
            .u64(self.term)
            .u32(self.conf.len() as u32)
            .raw(&self.conf)
            .u64(self.data.len() as u64)
            .raw(&self.data);
        e.finish()
    }

    /// Encodes the complete file (common header included), in clear.
    #[must_use]
    pub fn encode(&self, header: &FileHeader) -> Vec<u8> {
        let header = FileHeader { kind: FileKind::Snapshot, ..header.clone() };
        let mut out = header.encode().to_vec();
        let body = self.body();
        let crc = crc32c(&body);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decodes a complete file in clear.
    ///
    /// # Errors
    ///
    /// [`FormatError::Truncated`], [`FormatError::BadMagic`], [`FormatError::BadCrc`], [`FormatError::Version`] or
    /// [`FormatError::Invalid`] for bytes that are not a valid unsealed snapshot file.
    pub fn decode(file: &[u8]) -> Result<(FileHeader, GroupSnapshot), FormatError> {
        let header = FileHeader::decode(file)?;
        if header.wrapped_key().is_some() {
            return Err(FormatError::Invalid("encrypted snapshot"));
        }
        let body = GroupSnapshot::checked_body(file, &header)?;
        Ok((header, GroupSnapshot::parse(body)?))
    }

    fn parse(body: &[u8]) -> Result<GroupSnapshot, FormatError> {
        let mut d = Decoder::new(body);
        let group = GroupId(d.u64()?);
        let index = d.u64()?;
        let term = d.u64()?;
        let n = d.u32()? as usize;
        let conf = Bytes::copy_from_slice(d.raw(n)?);
        let n = usize::try_from(d.u64()?).map_err(|_| FormatError::Truncated)?;
        let data = Bytes::copy_from_slice(d.raw(n)?);
        d.finish().map_err(|_| FormatError::Invalid("trailing bytes"))?;
        Ok(GroupSnapshot { group, index, term, conf, data })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn snap_header() -> FileHeader {
        FileHeader::new(FileKind::Snapshot, 1, 2, 3, 4)
    }

    #[test]
    fn group_snapshot_round_trip_and_corruption() {
        let s = GroupSnapshot {
            group: GroupId(4),
            index: 100,
            term: 3,
            conf: Bytes::from("voters"),
            data: Bytes::from_vec(vec![5; 1000]),
        };
        let f = s.encode(&snap_header());
        let (h, got) = GroupSnapshot::decode(&f).expect("decode succeeds");
        assert_eq!(h, snap_header());
        assert_eq!(got, s);
        for i in (0..f.len()).step_by(97) {
            let mut c = f.clone();
            c[i] ^= 2;
            assert!(GroupSnapshot::decode(&c).is_err(), "byte {i}");
        }
        assert_eq!(GroupSnapshot::decode(&f[..BLOCK]), Err(FormatError::Truncated));
        let mut wrong_kind = f.clone();
        wrong_kind[..BLOCK]
            .copy_from_slice(&FileHeader { kind: FileKind::Wal, ..snap_header() }.encode());
        assert_eq!(GroupSnapshot::decode(&wrong_kind), Err(FormatError::BadMagic));
    }

    #[test]
    fn group_snapshot_trailing_bytes() {
        let mut f = snap_header().encode().to_vec();
        let mut e = Encoder::new();
        e.u64(1).u64(2).u64(3).u32(0).u64(0).u8(9);
        let body = e.finish();
        f.extend_from_slice(&body);
        f.extend_from_slice(&crc32c(&body).to_le_bytes());
        assert_eq!(GroupSnapshot::decode(&f), Err(FormatError::Invalid("trailing bytes")));
    }
}
