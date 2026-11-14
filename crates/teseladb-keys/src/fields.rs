//! The dictionary of field names of a collection (003 FR-022): each name gets a 16-bit number,
//! the next one free, and keeps it. No number is given to another name or reused, so a number
//! read from a table written with a dictionary names the same field for as long as that
//! dictionary exists. Each dictionary has an identifier of its own, which the tables written
//! with it repeat: a reader compares numbers only when its dictionary is the table's.
//!
//! [`FieldNumbers`] is both the catalog's dictionary and the part of it a write or a filter
//! carries ([`FieldNumbers::subset`]), with the same identifier.

/// The tag of a field that had no number when its document was written.
pub const UNNUMBERED: u16 = u16::MAX;
/// The most names a dictionary numbers.
pub const MAX_FIELD_NAMES: usize = 1024;
/// The longest name a dictionary numbers, in bytes.
pub const MAX_FIELD_NAME_LEN: usize = 64;

/// Field names and their numbers, under the identifier of their dictionary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldNumbers {
    dictionary: u32,
    /// By name, for the lookups.
    by_name: Vec<(Box<[u8]>, u16)>,
    /// The next number free: how many names the whole dictionary has given numbers to.
    next: u16,
}

/// Whether a dictionary numbers `name`: 1 to [`MAX_FIELD_NAME_LEN`] bytes.
#[must_use]
pub fn numberable(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_FIELD_NAME_LEN
}

impl FieldNumbers {
    /// An empty dictionary with identifier `dictionary`.
    #[must_use]
    pub fn new(dictionary: u32) -> FieldNumbers {
        FieldNumbers { dictionary, by_name: Vec::new(), next: 0 }
    }

    /// The names of `names` with their numbers, in that order, from 0: a dictionary as the
    /// catalog writes it.
    #[must_use]
    pub fn from_names<'a>(
        dictionary: u32,
        names: impl IntoIterator<Item = &'a [u8]>,
    ) -> FieldNumbers {
        let mut d = FieldNumbers::new(dictionary);
        for n in names {
            d.add(n);
        }
        d
    }

    /// The identifier of the dictionary.
    #[must_use]
    pub fn dictionary(&self) -> u32 {
        self.dictionary
    }

    /// How many names it has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether it has no names.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Whether the dictionary gives no more numbers.
    #[must_use]
    pub fn is_full(&self) -> bool {
        usize::from(self.next) >= MAX_FIELD_NAMES
    }

    /// The number of `name`.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<u16> {
        self.by_name.binary_search_by(|(n, _)| (**n).cmp(name)).ok().map(|i| self.by_name[i].1)
    }

    /// Gives `name` the next number free, unless it has one, the dictionary is full or the name
    /// is not [`numberable`]: its number, if it has one now.
    pub fn add(&mut self, name: &[u8]) -> Option<u16> {
        match self.by_name.binary_search_by(|(n, _)| (**n).cmp(name)) {
            Ok(i) => Some(self.by_name[i].1),
            Err(_) if self.is_full() || !numberable(name) => None,
            Err(i) => {
                let n = self.next;
                self.by_name.insert(i, (name.into(), n));
                self.next += 1;
                Some(n)
            }
        }
    }

    /// Adds `name` with number `number`, as a part of a dictionary that travels with a write or
    /// a filter has it. A name already there, or a number that cannot be one, is left out.
    pub fn insert(&mut self, name: &[u8], number: u16) {
        if number == UNNUMBERED || usize::from(number) >= MAX_FIELD_NAMES {
            return;
        }
        if let Err(i) = self.by_name.binary_search_by(|(n, _)| (**n).cmp(name)) {
            self.by_name.insert(i, (name.into(), number));
            self.next = self.next.max(number + 1);
        }
    }

    /// The names of `names` that have a number, with it, under the same identifier.
    #[must_use]
    pub fn subset<'a>(&self, names: impl IntoIterator<Item = &'a [u8]>) -> FieldNumbers {
        let mut out = FieldNumbers::new(self.dictionary);
        for name in names {
            if let Some(n) = self.get(name) {
                out.insert(name, n);
            }
        }
        out
    }

    /// The names in the order of their numbers, with them.
    #[must_use]
    pub fn by_number(&self) -> Vec<(&[u8], u16)> {
        let mut v: Vec<(&[u8], u16)> = self.by_name.iter().map(|(n, k)| (&**n, *k)).collect();
        v.sort_unstable_by_key(|&(_, k)| k);
        v
    }

    /// Its bytes as a write or a filter carries it: the identifier (u32), then each name as its
    /// number (u16), its length (u8) and its bytes, integers in little-endian.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(4 + self.by_name.iter().map(|(n, _)| 3 + n.len()).sum::<usize>());
        out.extend_from_slice(&self.dictionary.to_le_bytes());
        for (name, n) in &self.by_name {
            out.extend_from_slice(&n.to_le_bytes());
            out.push(name.len() as u8);
            out.extend_from_slice(name);
        }
        out
    }

    /// Reads what [`FieldNumbers::to_bytes`] wrote; `None` when the bytes do not hold.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<FieldNumbers> {
        let mut out = FieldNumbers::new(u32::from_le_bytes(b.get(..4)?.try_into().ok()?));
        let mut at = 4;
        while at < b.len() {
            let n = u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?);
            let len = usize::from(*b.get(at + 2)?);
            let name = b.get(at + 3..at + 3 + len)?;
            if !numberable(name) {
                return None;
            }
            out.insert(name, n);
            at += 3 + len;
        }
        Some(out)
    }
}
