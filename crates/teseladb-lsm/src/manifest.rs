//! The manifest (FR-004, data-model §3, `sst-format.md` «Manifiesto»).
//!
//! ```text
//! record := len(u32 LE) · kind(u8) · body · crc32c(u32 LE)     len counts the body
//! edit   := BeginEdit · record* · EndEdit                      padded with 0x5a to 4 KiB
//! ```
//!
//! Each edit starts on a 4 KiB boundary, so appending one is a single aligned write (FR-016). On
//! reading, a region of zeros ends the manifest, and so does an edit that does not parse when no
//! complete edit follows it: that is the tail of a write a crash cut, never acknowledged. A broken
//! edit with a complete one after it is corruption. `CURRENT` names the manifest in use and is
//! replaced by an atomic rename when the manifest rotates.

use std::collections::BTreeMap;

use teseladb_crypto::at_rest::{KeyRing, TRAILER_LEN, WRAPPED_KEY_LEN};
use teseladb_hash::crc32c;
use teseladb_keys::{Hlc, VERSION_LEN};
use teseladb_types::GroupId;

use crate::sst::{ALIGN, Bad, PAD, get_bytes, get_varint, pad, put_bytes, put_varint};

const BEGIN: u8 = 1;
const END: u8 = 2;
const ADD_FILE: u8 = 3;
const REMOVE_FILE: u8 = 4;
const RANGE_FLUSHED: u8 = 5;
const NEXT_FILE: u8 = 6;
/// The flushed index of a range goes back to 0 (005 FR-012: before its data is replaced).
const RANGE_RESET: u8 = 7;

/// Name of manifest `number`.
pub(crate) fn manifest_name(number: u64) -> String {
    format!("MANIFEST-{number:06}")
}

/// Contents of `CURRENT` for manifest `number`.
pub(crate) fn current_contents(number: u64) -> Vec<u8> {
    format!("{}\n", manifest_name(number)).into_bytes()
}

/// The manifest number named by the contents of `CURRENT`.
pub(crate) fn parse_current(b: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(b).ok()?.strip_suffix('\n')?.strip_prefix("MANIFEST-")?;
    if s.len() < 6 || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A file as the manifest describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileMeta {
    pub(crate) level: u8,
    pub(crate) number: u64,
    pub(crate) size: u64,
    pub(crate) lo: Vec<u8>,
    pub(crate) hi: Vec<u8>,
    pub(crate) min_ts: Hlc,
    pub(crate) max_ts: Hlc,
}

/// One atomic change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Edit {
    pub(crate) adds: Vec<FileMeta>,
    pub(crate) removes: Vec<(u8, u64)>,
    pub(crate) flushed: Vec<(GroupId, u64)>,
    /// Ranges whose flushed index goes back to 0, applied before `flushed`.
    pub(crate) resets: Vec<GroupId>,
    /// The file counter after the edit (every edit carries it).
    pub(crate) next_file: u64,
}

fn record(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    let start = out.len();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(body);
    let crc = crc32c(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

fn get_ts(b: &[u8], at: &mut usize) -> Result<Hlc, Bad> {
    let s = b.get(*at..*at + VERSION_LEN).ok_or(Bad)?;
    *at += VERSION_LEN;
    Ok(Hlc::from_be_bytes(s.try_into().map_err(|_| Bad)?))
}

/// First byte of a sealed edit (013 FR-015); a clear edit starts with the length of its `BeginEdit`
/// record, zero.
const SEALED: u8 = 0x5e;
/// What a sealed edit is bound to.
const SEALED_AAD: &[u8] = b"teseladb manifest edit";
/// Bytes before a sealed edit's ciphertext: marker, length, wrapped key and trailer.
const SEALED_HEAD: usize = 1 + 4 + WRAPPED_KEY_LEN + TRAILER_LEN;

impl Edit {
    /// The edit as an aligned region.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = self.records();
        pad(&mut out);
        out
    }

    /// The edit as an aligned region, sealed with a new data key of `ring` when there is one:
    ///
    /// ```text
    /// sealed := 0x5e · len(u32 LE) · wrapped key(64) · trailer(32) · ciphertext · crc32c(u32 LE)
    /// ```
    ///
    /// The ciphertext is the edit's records; the CRC covers everything before it.
    pub(crate) fn encode_with(&self, ring: Option<&KeyRing>) -> Vec<u8> {
        let Some(ring) = ring else { return self.encode() };
        // One data key per edit: each seals a single block.
        let (key, wrapped) = ring
            .new_file_key()
            .expect("a key ring seeded by the system makes every data key it is asked for");
        let (sealed, trailer) =
            key.seal(SEALED_AAD, &self.records()).expect("a new data key seals its first block");
        let mut out = Vec::with_capacity(SEALED_HEAD + sealed.len() + 4);
        out.push(SEALED);
        out.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
        out.extend_from_slice(&wrapped);
        out.extend_from_slice(&trailer);
        out.extend_from_slice(&sealed);
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        pad(&mut out);
        out
    }

    /// The edit's records, `BeginEdit` to `EndEdit`.
    fn records(&self) -> Vec<u8> {
        let mut out = Vec::new();
        record(&mut out, BEGIN, &[]);
        for f in &self.adds {
            let mut b = vec![f.level];
            put_varint(&mut b, f.number);
            put_varint(&mut b, f.size);
            put_bytes(&mut b, &f.lo);
            put_bytes(&mut b, &f.hi);
            b.extend_from_slice(&f.min_ts.to_be_bytes());
            b.extend_from_slice(&f.max_ts.to_be_bytes());
            record(&mut out, ADD_FILE, &b);
        }
        for (level, number) in &self.removes {
            let mut b = vec![*level];
            put_varint(&mut b, *number);
            record(&mut out, REMOVE_FILE, &b);
        }
        for range in &self.resets {
            let mut b = Vec::new();
            put_varint(&mut b, range.0);
            record(&mut out, RANGE_RESET, &b);
        }
        for (range, index) in &self.flushed {
            let mut b = Vec::new();
            put_varint(&mut b, range.0);
            put_varint(&mut b, *index);
            record(&mut out, RANGE_FLUSHED, &b);
        }
        let mut b = Vec::new();
        put_varint(&mut b, self.next_file);
        record(&mut out, NEXT_FILE, &b);
        record(&mut out, END, &[]);
        out
    }

    fn take(&mut self, kind: u8, body: &[u8]) -> Result<(), Bad> {
        let mut at = 0;
        match kind {
            ADD_FILE => {
                let level = *body.first().ok_or(Bad)?;
                at = 1;
                let f = FileMeta {
                    level,
                    number: get_varint(body, &mut at)?,
                    size: get_varint(body, &mut at)?,
                    lo: get_bytes(body, &mut at)?.to_vec(),
                    hi: get_bytes(body, &mut at)?.to_vec(),
                    min_ts: get_ts(body, &mut at)?,
                    max_ts: get_ts(body, &mut at)?,
                };
                self.adds.push(f);
            }
            REMOVE_FILE => {
                let level = *body.first().ok_or(Bad)?;
                at = 1;
                self.removes.push((level, get_varint(body, &mut at)?));
            }
            RANGE_FLUSHED => {
                let range = GroupId(get_varint(body, &mut at)?);
                self.flushed.push((range, get_varint(body, &mut at)?));
            }
            RANGE_RESET => self.resets.push(GroupId(get_varint(body, &mut at)?)),
            NEXT_FILE => self.next_file = get_varint(body, &mut at)?,
            _ => return Err(Bad),
        }
        if at != body.len() { Err(Bad) } else { Ok(()) }
    }
}

/// Reads one record at `*at`.
fn read_record<'a>(b: &'a [u8], at: &mut usize) -> Result<(u8, &'a [u8]), Bad> {
    let head = b.get(*at..*at + 5).ok_or(Bad)?;
    let len = u32::from_le_bytes(head[..4].try_into().map_err(|_| Bad)?) as usize;
    let kind = head[4];
    let end = (*at + 5).checked_add(len).ok_or(Bad)?;
    let body = b.get(*at + 5..end).ok_or(Bad)?;
    let crc = u32::from_le_bytes(b.get(end..end + 4).ok_or(Bad)?.try_into().map_err(|_| Bad)?);
    if crc32c(&b[*at..end]) != crc {
        return Err(Bad);
    }
    *at = end + 4;
    Ok((kind, body))
}

/// Reads the edit starting at the aligned offset `start`; returns it and where the next starts.
/// Whether `p`, the rest of an edit's page, is its padding: `PAD` as written, or zeros where a
/// crash cut the page after the edit and a later append past it left a hole.
fn padding(p: &[u8]) -> bool {
    let first_zero = p.iter().position(|x| *x == 0).unwrap_or(p.len());
    p[..first_zero].iter().all(|x| *x == PAD) && p[first_zero..].iter().all(|x| *x == 0)
}

fn read_edit(b: &[u8], start: usize) -> Result<(Edit, usize), Bad> {
    let mut at = start;
    if read_record(b, &mut at)?.0 != BEGIN {
        return Err(Bad);
    }
    let mut edit = Edit::default();
    loop {
        let (kind, body) = read_record(b, &mut at)?;
        match kind {
            BEGIN => return Err(Bad),
            END if body.is_empty() => break,
            END => return Err(Bad),
            _ => edit.take(kind, body)?,
        }
    }
    let next = at.next_multiple_of(ALIGN);
    if b.get(at..next.min(b.len())).is_some_and(|p| !padding(p)) {
        return Err(Bad);
    }
    Ok((edit, next))
}

/// Why an edit does not read.
enum Unread {
    /// It does not parse or its checksum fails: a cut tail, or corruption if a good edit follows.
    Broken,
    /// A whole sealed edit that this node cannot open: no key ring, or another master key.
    Locked,
}

/// Opens the sealed edit at the aligned offset `start` with `ring`; returns it and where the next
/// starts.
fn read_sealed(b: &[u8], start: usize, ring: Option<&KeyRing>) -> Result<(Edit, usize), Unread> {
    let head = b.get(start..start + SEALED_HEAD).ok_or(Unread::Broken)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let end = (start + SEALED_HEAD).checked_add(len).ok_or(Unread::Broken)?;
    let crc = b.get(end..end + 4).ok_or(Unread::Broken)?;
    if crc32c(&b[start..end]).to_le_bytes() != crc {
        return Err(Unread::Broken);
    }
    let ring = ring.ok_or(Unread::Locked)?;
    let key = ring.open_file_key(&head[5..5 + WRAPPED_KEY_LEN]).map_err(|_| Unread::Locked)?;
    let trailer: &[u8; TRAILER_LEN] =
        head[5 + WRAPPED_KEY_LEN..].try_into().map_err(|_| Unread::Broken)?;
    let mut plain =
        key.open(SEALED_AAD, &b[start + SEALED_HEAD..end], trailer).map_err(|_| Unread::Locked)?;
    pad(&mut plain);
    let (edit, _) = read_edit(&plain, 0).map_err(|_| Unread::Broken)?;
    let next = (end + 4).next_multiple_of(ALIGN);
    if b.get(end + 4..next.min(b.len())).is_some_and(|p| !padding(p)) {
        return Err(Unread::Broken);
    }
    Ok((edit, next))
}

/// The edit at the aligned offset `start`, clear or sealed.
fn read_any(b: &[u8], start: usize, ring: Option<&KeyRing>) -> Result<(Edit, usize), Unread> {
    if b.get(start) == Some(&SEALED) {
        read_sealed(b, start, ring)
    } else {
        read_edit(b, start).map_err(|_| Unread::Broken)
    }
}

/// What a manifest says: its edits and where the next one goes. A broken edit with a good one
/// after it gives the offset of the broken one. Sealed edits open with `ring`; a whole sealed edit
/// that does not open (no ring, another master key) gives its offset too.
pub(crate) fn read(b: &[u8], ring: Option<&KeyRing>) -> Result<(Vec<Edit>, u64), u64> {
    let mut edits = Vec::new();
    let mut at = 0;
    while at < b.len() {
        if b[at..(at + ALIGN).min(b.len())].iter().all(|x| *x == 0) {
            break;
        }
        match read_any(b, at, ring) {
            Ok((e, next)) => {
                edits.push(e);
                at = next;
            }
            // A whole edit this node cannot open is never a cut tail.
            Err(Unread::Locked) => return Err(at as u64),
            Err(Unread::Broken) => {
                let later =
                    (at + ALIGN..b.len()).step_by(ALIGN).any(|p| read_any(b, p, ring).is_ok());
                if later {
                    return Err(at as u64);
                }
                break;
            }
        }
    }
    Ok((edits, at as u64))
}

/// The state the edits build: files by number, flushed index per range, next file number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) files: BTreeMap<u64, FileMeta>,
    pub(crate) flushed: BTreeMap<GroupId, u64>,
    pub(crate) next_file: u64,
}

impl State {
    /// Applies an edit.
    pub(crate) fn apply(&mut self, e: &Edit) {
        for (_, number) in &e.removes {
            self.files.remove(number);
        }
        for f in &e.adds {
            self.files.insert(f.number, f.clone());
        }
        for range in &e.resets {
            self.flushed.remove(range);
        }
        for (range, index) in &e.flushed {
            let v = self.flushed.entry(*range).or_insert(0);
            *v = (*v).max(*index);
        }
        self.next_file = self.next_file.max(e.next_file);
    }

    /// One edit that rebuilds this state (the first edit of a new manifest).
    pub(crate) fn snapshot(&self) -> Edit {
        Edit {
            adds: self.files.values().cloned().collect(),
            removes: Vec::new(),
            flushed: self.flushed.iter().map(|(r, i)| (*r, *i)).collect(),
            resets: Vec::new(),
            next_file: self.next_file,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn file(number: u64) -> FileMeta {
        FileMeta {
            level: 1,
            number,
            size: 8192,
            lo: b"a".to_vec(),
            hi: b"m".to_vec(),
            min_ts: Hlc { physical: 1, logical: 0 },
            max_ts: Hlc { physical: 9, logical: 2 },
        }
    }

    fn edit(n: u64) -> Edit {
        Edit {
            adds: vec![file(n)],
            removes: vec![(0, n - 1)],
            flushed: vec![(GroupId(3), n * 10)],
            resets: if n.is_multiple_of(2) { vec![GroupId(4)] } else { Vec::new() },
            next_file: n + 1,
        }
    }

    #[test]
    fn edits_round_trip_aligned() {
        let mut m = Vec::new();
        for n in 2..6 {
            let e = edit(n).encode();
            assert_eq!(e.len() % ALIGN, 0);
            m.extend_from_slice(&e);
        }
        let (edits, end) = read(&m, None).unwrap();
        assert_eq!(edits, (2..6).map(edit).collect::<Vec<_>>());
        assert_eq!(end, m.len() as u64);
        let mut s = State::default();
        for e in &edits {
            s.apply(e);
        }
        // Each edit removes the file the one before added.
        assert_eq!(s.files.keys().copied().collect::<Vec<_>>(), vec![5]);
        assert_eq!(s.flushed[&GroupId(3)], 50);
        assert_eq!(s.next_file, 6);
        let mut again = State::default();
        again.apply(&s.snapshot());
        assert_eq!(again, s);
        assert_eq!(read(&[], None).unwrap(), (Vec::new(), 0));
    }

    #[test]
    fn a_cut_tail_is_ignored_and_a_broken_middle_is_corruption() {
        let mut m = Vec::new();
        for n in 2..5 {
            m.extend_from_slice(&edit(n).encode());
        }
        // Zeros after the edits (preallocated space) end the manifest.
        let mut zeros = m.clone();
        zeros.extend_from_slice(&[0; ALIGN]);
        assert_eq!(read(&zeros, None).unwrap().0.len(), 3);
        // A torn last edit: the two before it count, and the next edit goes where it was.
        let mut torn = m.clone();
        let last = torn.len() - ALIGN;
        torn[last + 30] ^= 1;
        assert_eq!(read(&torn, None).unwrap(), ((2..4).map(edit).collect(), last as u64));
        // The same damage in the middle, with a good edit after it.
        let mut middle = m.clone();
        middle[ALIGN + 30] ^= 1;
        assert_eq!(read(&middle, None), Err(ALIGN as u64));
    }

    #[test]
    fn malformed_records_are_refused() {
        let begin_only = {
            let mut o = Vec::new();
            record(&mut o, BEGIN, &[]);
            pad(&mut o);
            o
        };
        assert_eq!(
            read(&begin_only, None).unwrap().0.len(),
            0,
            "an edit without its end is a cut tail"
        );
        for (kind, body) in [
            (9u8, vec![]),
            (ADD_FILE, vec![]),
            (REMOVE_FILE, vec![1]),
            (NEXT_FILE, vec![1, 2]),
            (END, vec![1]),
        ] {
            let mut o = Vec::new();
            record(&mut o, BEGIN, &[]);
            record(&mut o, kind, &body);
            record(&mut o, END, &[]);
            pad(&mut o);
            assert!(read_edit(&o, 0).is_err(), "kind {kind}");
        }
        let mut twice = Vec::new();
        record(&mut twice, BEGIN, &[]);
        record(&mut twice, BEGIN, &[]);
        assert!(read_edit(&twice, 0).is_err());
        let mut not_begin = Vec::new();
        record(&mut not_begin, END, &[]);
        assert!(read_edit(&not_begin, 0).is_err());
        // Garbage instead of padding after the end.
        let mut o = edit(2).encode();
        let last = o.len() - 1;
        o[last] = 7;
        assert!(read_edit(&o, 0).is_err());
        // Zeros at the end of the padding are the hole a crash and a later append leave: still
        // padding; padding after zeros is not.
        o[last] = 0;
        assert!(read_edit(&o, 0).is_ok());
        o[last - 1] = 0;
        o[last] = PAD;
        assert!(read_edit(&o, 0).is_err());
        let mut huge = Vec::new();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.push(BEGIN);
        assert!(read_edit(&huge, 0).is_err());
    }

    #[test]
    fn current_names_the_manifest() {
        assert_eq!(parse_current(&current_contents(7)), Some(7));
        assert_eq!(parse_current(&current_contents(1_234_567)), Some(1_234_567));
        for bad in [
            &b"MANIFEST-000007"[..],
            b"MANIFEST-7\n",
            b"OTHER-000007\n",
            b"MANIFEST-00000x\n",
            b"\xff\n",
        ] {
            assert_eq!(parse_current(bad), None);
        }
    }

    #[test]
    fn sealed_edits_read_with_their_ring_and_a_cut_one_is_a_tail() {
        use teseladb_crypto::at_rest::CipherMode;
        let ring = KeyRing::new(&[5; 32], CipherMode::Gcm, |b: &mut [u8]| {
            b.fill(1);
            Ok(())
        });
        let mut m = Vec::new();
        for n in 2..5 {
            let region = edit(n).encode_with(Some(&ring));
            assert_eq!((region[0], region.len() % ALIGN), (SEALED, 0));
            m.extend_from_slice(&region);
        }
        let (edits, end) = read(&m, Some(&ring)).unwrap();
        assert_eq!((edits, end), ((2..5).map(edit).collect(), m.len() as u64));
        // No ring or another master key: the first sealed edit is an error, not a tail.
        assert_eq!(read(&m, None), Err(0));
        let other = KeyRing::new(&[6; 32], CipherMode::Gcm, |b: &mut [u8]| {
            b.fill(1);
            Ok(())
        });
        assert_eq!(read(&m, Some(&other)), Err(0));
        // A cut last edit is a tail; a damaged one in the middle is corruption.
        let mut torn = m.clone();
        torn[2 * ALIGN + 200] ^= 1;
        assert_eq!(read(&torn, Some(&ring)).unwrap().0.len(), 2);
        let mut middle = m.clone();
        middle[ALIGN + 3] ^= 1;
        assert_eq!(read(&middle, Some(&ring)), Err(ALIGN as u64));
        let mut short = m[..ALIGN].to_vec();
        short.truncate(SEALED_HEAD - 1);
        assert_eq!(read(&short, Some(&ring)).unwrap().0.len(), 0);
        // Bytes after the sealed edit's checksum must be padding.
        let mut unpadded = m[..ALIGN].to_vec();
        let len = u32::from_le_bytes(unpadded[1..5].try_into().unwrap()) as usize;
        unpadded[SEALED_HEAD + len + 4] = 0;
        assert_eq!(read(&unpadded, Some(&ring)).unwrap().0.len(), 0);
        // Clear and sealed edits mix (encryption turned on later).
        let mut mixed = edit(2).encode();
        mixed.extend_from_slice(&edit(3).encode_with(Some(&ring)));
        assert_eq!(read(&mixed, Some(&ring)).unwrap().0.len(), 2);
        assert_eq!(edit(2).encode_with(None), edit(2).encode());
    }
}
