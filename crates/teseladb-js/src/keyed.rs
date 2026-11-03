//! The entries of a `Map` or a `Set` (A016-3-23): in insertion order, with an index of where
//! each key is by SameValueZero, so that `get`, `set`, `has`, `add` and `delete` do not go
//! through every entry.
//!
//! A deleted entry leaves an empty slot behind, so the other entries keep their place and an
//! iteration in progress goes on from where it was (ECMA-262 §24.1.3.3 and §24.1.5.1: entries
//! added while iterating are visited, deleted ones are not). An iteration reads through a
//! cursor the table knows about. When the empty slots are more than half of them, the table
//! drops them and moves each live cursor to the same entry in the shorter list.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::bigint::BigInt;
use crate::value::{Name, Val};

/// A key as SameValueZero compares it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    /// `undefined`; an array hole is read as `undefined` before it can be a key.
    Undef,
    Null,
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
        Val::Undef | Val::Hole => Key::Undef,
        Val::Null => Key::Null,
        Val::Bool(b) => Key::Bool(*b),
        Val::Num(n) if n.is_nan() => Key::Num(f64::NAN.to_bits()),
        Val::Num(n) => Key::Num((n + 0.0).to_bits()),
        Val::BigInt(b) => Key::BigInt((**b).clone()),
        Val::Str(s) => Key::Str(s.clone()),
        Val::Sym(s) => Key::Sym(s.1),
        Val::Obj(o) => Key::Obj(Rc::as_ptr(&o.0) as usize),
    }
}

/// An entry of a table: a `Set`'s value, or a `Map`'s key and value.
pub trait Entry: Clone {
    fn key(&self) -> &Val;
}

impl Entry for Val {
    fn key(&self) -> &Val {
        self
    }
}

impl<V: Clone> Entry for (Val, V) {
    fn key(&self) -> &Val {
        &self.0
    }
}

/// Where an iteration of a table is: the slot it reads next.
pub type Cursor = Rc<Cell<usize>>;

/// The entries of a `Map` (`Table<(Val, Val)>`) or a `Set` (`Table<Val>`).
#[derive(Debug)]
pub struct Table<T> {
    /// The entries in insertion order; `None` where one was deleted.
    slots: Vec<Option<T>>,
    /// How many slots hold an entry.
    live: usize,
    /// The slot of each key.
    at: HashMap<Key, usize>,
    /// The cursors of the iterations that may still read the table.
    cursors: Vec<Weak<Cell<usize>>>,
    /// How many cursors there may be before the dropped ones are swept.
    sweep_at: usize,
}

/// Cursors a table keeps before it first sweeps the dropped ones.
const FIRST_SWEEP: usize = 16;

impl<T> Default for Table<T> {
    fn default() -> Self {
        Table {
            slots: Vec::new(),
            live: 0,
            at: HashMap::new(),
            cursors: Vec::new(),
            sweep_at: FIRST_SWEEP,
        }
    }
}

/// A copy has the same entries and no iterations.
impl<T: Clone> Clone for Table<T> {
    fn clone(&self) -> Self {
        Table {
            slots: self.slots.clone(),
            live: self.live,
            at: self.at.clone(),
            cursors: Vec::new(),
            sweep_at: FIRST_SWEEP,
        }
    }
}

impl<T: Entry> Table<T> {
    /// How many entries it has.
    pub fn len(&self) -> usize {
        self.live
    }

    /// The entry of the key `k`.
    pub fn get(&self, k: &Val) -> Option<&T> {
        self.at.get(&key_of(k)).and_then(|&i| self.slots[i].as_ref())
    }

    /// The entry of the key `k`, to change it.
    pub fn get_mut(&mut self, k: &Val) -> Option<&mut T> {
        self.at.get(&key_of(k)).and_then(|&i| self.slots[i].as_mut())
    }

    /// Whether the key `k` has an entry.
    pub fn contains(&self, k: &Val) -> bool {
        self.at.contains_key(&key_of(k))
    }

    /// Adds `e`, whose key has no entry, at the end.
    pub fn push(&mut self, e: T) {
        self.at.insert(key_of(e.key()), self.slots.len());
        self.slots.push(Some(e));
        self.live += 1;
    }

    /// Deletes the entry of the key `k`; whether there was one.
    pub fn remove(&mut self, k: &Val) -> bool {
        let Some(i) = self.at.remove(&key_of(k)) else { return false };
        self.slots[i] = None;
        self.live -= 1;
        if (self.slots.len() - self.live) * 2 > self.slots.len() {
            self.compact();
        }
        true
    }

    /// Deletes every entry; an iteration in progress goes on with the entries added after.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.at.clear();
        self.live = 0;
        self.cursors.retain(|c| c.upgrade().inspect(|c| c.set(0)).is_some());
    }

    /// The entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().flatten()
    }

    /// A cursor at the first entry, which follows the entries when the table drops its empty
    /// slots.
    pub fn cursor(&mut self) -> Cursor {
        if self.cursors.len() >= self.sweep_at {
            self.cursors.retain(|c| c.strong_count() > 0);
            self.sweep_at = (self.cursors.len() * 2).max(FIRST_SWEEP);
        }
        let c = Rc::new(Cell::new(0));
        self.cursors.push(Rc::downgrade(&c));
        c
    }

    /// The entry at or after `cursor`, which then points past it.
    pub fn next(&self, cursor: &Cell<usize>) -> Option<T> {
        let from = cursor.get();
        let found =
            self.slots.iter().enumerate().skip(from).find_map(|(i, e)| Some((i, e.as_ref()?)));
        cursor.set(found.as_ref().map_or(self.slots.len(), |(i, _)| i + 1));
        found.map(|(_, e)| e.clone())
    }

    /// Drops the empty slots: each live cursor goes to the slot of the entry it was at, and the
    /// index to the new slots.
    fn compact(&mut self) {
        // `before[i]`: how many entries are in the slots before `i`.
        let mut before = Vec::with_capacity(self.slots.len() + 1);
        let mut n = 0;
        for s in &self.slots {
            before.push(n);
            n += usize::from(s.is_some());
        }
        before.push(n);
        self.cursors.retain(|c| c.upgrade().inspect(|c| c.set(before[c.get()])).is_some());
        self.slots.retain(Option::is_some);
        for (i, e) in self.slots.iter().flatten().enumerate() {
            self.at.insert(key_of(e.key()), i);
        }
    }
}
