//! The index of the keys of a `Map` or a `Set` (A016-3-23): where each key is among the entries,
//! by SameValueZero, so that `get`, `set`, `has` and `add` do not go through every entry.
//!
//! The entries stay in their vector, in insertion order. The index remembers the vector it was
//! built for (its buffer and length): any change it did not see (a delete, `clear`, new entries
//! from a set method) changes one of them, and the index is built again.

use std::collections::HashMap;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::value::{Name, Val};

/// A key as SameValueZero compares it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Undef,
    Null,
    Hole,
    Bool(bool),
    /// The bits of the number, with −0 as +0 and every NaN as one.
    Num(u64),
    BigInt(BigInt),
    Str(Name),
    /// A symbol by its identity.
    Sym(u64),
    /// An object by its address, which it keeps while it is a key.
    Obj(usize),
}

fn key_of(v: &Val) -> Key {
    match v {
        Val::Undef => Key::Undef,
        Val::Null => Key::Null,
        Val::Hole => Key::Hole,
        Val::Bool(b) => Key::Bool(*b),
        Val::Num(n) if n.is_nan() => Key::Num(f64::NAN.to_bits()),
        Val::Num(n) => Key::Num((n + 0.0).to_bits()),
        Val::BigInt(b) => Key::BigInt((**b).clone()),
        Val::Str(s) => Key::Str(s.clone()),
        Val::Sym(s) => Key::Sym(s.1),
        Val::Obj(o) => Key::Obj(Rc::as_ptr(&o.0) as usize),
    }
}

/// Where the keys of a vector of entries are.
#[derive(Debug, Default)]
pub struct KeyIndex {
    at: HashMap<Key, usize>,
    buffer: usize,
    len: usize,
}

impl KeyIndex {
    /// Builds the index again when `keys` (`len` of them, in a buffer at `buffer`) are not the
    /// ones it was built for.
    fn refresh<'a>(&mut self, buffer: usize, len: usize, keys: impl Iterator<Item = &'a Val>) {
        if self.buffer == buffer && self.len == len {
            return;
        }
        self.at.clear();
        for (i, k) in keys.enumerate() {
            self.at.entry(key_of(k)).or_insert(i);
        }
        self.buffer = buffer;
        self.len = len;
    }

    /// Where `k` is among the keys of a `Map`'s entries.
    pub fn find_entry(&mut self, entries: &[(Val, Val)], k: &Val) -> Option<usize> {
        self.refresh(entries.as_ptr() as usize, entries.len(), entries.iter().map(|(x, _)| x));
        self.at.get(&key_of(k)).copied()
    }

    /// Where `k` is among a `Set`'s values.
    pub fn find_value(&mut self, values: &[Val], k: &Val) -> Option<usize> {
        self.refresh(values.as_ptr() as usize, values.len(), values.iter());
        self.at.get(&key_of(k)).copied()
    }

    /// Records that `k` went at the end of entries now at `buffer`, `len` long.
    pub fn pushed(&mut self, k: &Val, buffer: usize, len: usize) {
        self.at.insert(key_of(k), len - 1);
        self.buffer = buffer;
        self.len = len;
    }
}
