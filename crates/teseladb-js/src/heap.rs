//! The heap of an invocation (016 FR-007, A016-3-22): what its live values take, counted twice.
//!
//! In units of the heap of a reference router 9.0.4, whose limit is the 100 MB `getJSHeapLimitMB()`
//! gives: each kind of value costs what probes of growing size measured there (03-11), so that
//! the same programs fit. Passing that limit throws the string `"out of memory"`, which `try`
//! catches, as there. And in bytes of this process, against a limit of its own that protects
//! the node: passing it ends the call without a catch.
//!
//! Objects give back what they took when they go ([`crate::value::ObjCell`]). Strings have no
//! hook when they go: those made by the interpreter are kept in a list of weak references that
//! is swept when the heap fills or the list grows, which gives back what the dead ones took.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// The units of the heap in a byte of the reference router: costs are sixteenths of a byte.
pub const UNITS_PER_BYTE: usize = 16;

// The costs, measured on a reference router 9.0.4 on 03-11 with `getJSHeapLimitMB()` at 100 (MiB),
// where a value that goes past the limit is `"out of memory"`: 2 000 000 nested one-element arrays
// fit and 2 400 000 do not, as with empty objects in an array; 1 800 000 objects of four properties
// fit; 1 000 000 of eight fit and 1 400 000 do not; 600 000 of sixteen fit and 800 000 do not;
// 68 000 000 numbers pushed into an array fit and `new Array(8e7).fill(1)` does not; 10 000 000
// entries of a `Map` fit; 5 000 000 short strings in an array fit and 8 500 000 do not; a string
// of 2^30 - 2 characters fits.

/// An object with room for its first [`FREE_PROPERTIES`] properties: 48 bytes.
pub const OBJECT_UNITS: usize = 48 * UNITS_PER_BYTE;
/// The properties an object holds in [`OBJECT_UNITS`].
pub const FREE_PROPERTIES: usize = 4;
/// A property past the first [`FREE_PROPERTIES`]: 8 bytes.
pub const PROPERTY_UNITS: usize = 8 * UNITS_PER_BYTE;
/// An element of an array, a `Set` or a `Map`: 1.4375 bytes.
pub const ELEMENT_UNITS: usize = 23;
/// A string, before its characters: 13 bytes.
pub const STRING_UNITS: usize = 13 * UNITS_PER_BYTE;
/// A byte of a string's text or of an `ArrayBuffer`: a sixteenth of a byte for text, which lets a
/// string of 2^30 - 2 characters fit as it does there; three for a buffer, an element's cost
/// spread over the eight bytes of a number.
pub const TEXT_BYTE_UNITS: usize = 1;
/// See [`TEXT_BYTE_UNITS`].
pub const BUFFER_BYTE_UNITS: usize = 3;

// What the same values take of this process (`size_of` on 03-11, with the reference count).

/// An object: its reference-counted cell.
pub const OBJECT_BYTES: usize = 160;
/// An element: a value.
pub const ELEMENT_BYTES: usize = 24;
/// A property: its key and its descriptor.
pub const PROPERTY_BYTES: usize = 64;
/// A string, before its text: the reference counts and the allocation's rounding.
pub const STRING_BYTES: usize = 32;

/// What a value takes, in both counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    /// Units of the reference router heap ([`UNITS_PER_BYTE`] to a byte).
    pub units: usize,
    /// Bytes of this process.
    pub bytes: usize,
}

impl Cost {
    pub const ZERO: Cost = Cost { units: 0, bytes: 0 };

    /// An object without properties or elements.
    pub const fn object() -> Cost {
        Cost { units: OBJECT_UNITS, bytes: OBJECT_BYTES }
    }

    /// `n` elements.
    pub const fn elements(n: usize) -> Cost {
        Cost { units: n.saturating_mul(ELEMENT_UNITS), bytes: n.saturating_mul(ELEMENT_BYTES) }
    }

    /// `n` properties more on an object that has `had`: the first [`FREE_PROPERTIES`] take no
    /// units of their own.
    pub const fn properties_after(had: usize, n: usize) -> Cost {
        let paid = had.saturating_add(n).saturating_sub(FREE_PROPERTIES)
            - had.saturating_sub(FREE_PROPERTIES);
        Cost { units: paid.saturating_mul(PROPERTY_UNITS), bytes: n.saturating_mul(PROPERTY_BYTES) }
    }

    /// A string of `len` bytes.
    pub const fn string(len: usize) -> Cost {
        Cost {
            units: STRING_UNITS + len.saturating_mul(TEXT_BYTE_UNITS),
            bytes: STRING_BYTES + len,
        }
    }

    /// `len` bytes of text that is not a string of its own (what `print` keeps).
    pub const fn text(len: usize) -> Cost {
        Cost { units: len.saturating_mul(TEXT_BYTE_UNITS), bytes: len }
    }

    /// `len` bytes of an `ArrayBuffer`.
    pub const fn buffer(len: usize) -> Cost {
        Cost { units: len.saturating_mul(BUFFER_BYTE_UNITS), bytes: len }
    }

    pub const fn plus(self, other: Cost) -> Cost {
        Cost {
            units: self.units.saturating_add(other.units),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    pub const fn minus(self, other: Cost) -> Cost {
        Cost {
            units: self.units.saturating_sub(other.units),
            bytes: self.bytes.saturating_sub(other.bytes),
        }
    }

    fn within(self, limit: Cost) -> Result<(), Exhausted> {
        if self.bytes > limit.bytes {
            Err(Exhausted::Memory)
        } else if self.units > limit.units {
            Err(Exhausted::Heap)
        } else {
            Ok(())
        }
    }
}

/// Which limit a charge passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exhausted {
    /// The reference router heap: `"out of memory"`, which `try` catches.
    Heap,
    /// The bytes of this process: the call ends.
    Memory,
}

/// The strings the interpreter made, which give back what they took once swept dead.
#[derive(Debug, Default)]
struct Strings {
    live: Vec<(Weak<str>, Cost)>,
    /// What the list holds, dead strings included.
    held: Cost,
    /// The length and the bytes past which the list is swept: twice what it held after the last
    /// sweep, so that dead strings never hold more than the live ones and a mebibyte.
    sweep_len: usize,
    sweep_bytes: usize,
}

/// Entries kept between sweeps however few strings live.
const SWEEP_MIN_LEN: usize = 4096;
/// Bytes of dead strings kept between sweeps however few strings live.
const SWEEP_MIN_BYTES: usize = 1 << 20;

/// What the live values of an invocation take, against its limits.
#[derive(Debug)]
pub struct Budget {
    pub used: Cell<Cost>,
    pub limit: Cost,
    strings: RefCell<Strings>,
    /// Whether a call changed an object of the shared globals ([`crate::value::ObjCell::shared`]):
    /// the globals are then built again before the next call.
    pub touched: Cell<bool>,
}

impl Budget {
    /// A budget of `heap` bytes of the reference router heap and `memory` bytes of this process.
    pub fn new(heap: usize, memory: usize) -> Budget {
        Budget {
            used: Cell::new(Cost::ZERO),
            limit: Cost { units: heap.saturating_mul(UNITS_PER_BYTE), bytes: memory },
            strings: RefCell::new(Strings::default()),
            touched: Cell::new(false),
        }
    }

    /// Whether `c` more fits, after sweeping the dead strings if it does not.
    pub fn room(&self, c: Cost) -> Result<(), Exhausted> {
        if self.used.get().plus(c).within(self.limit).is_ok() {
            return Ok(());
        }
        self.sweep();
        self.used.get().plus(c).within(self.limit)
    }

    /// Charges `c`, or charges nothing and says which limit it passes.
    pub fn charge(&self, c: Cost) -> Result<(), Exhausted> {
        self.room(c)?;
        self.used.set(self.used.get().plus(c));
        Ok(())
    }

    pub fn release(&self, c: Cost) {
        self.used.set(self.used.get().minus(c));
    }

    /// Charges the string `s`, `len` bytes long, until it is swept dead.
    pub fn charge_string(&self, s: &Rc<str>, len: usize) -> Result<(), Exhausted> {
        let c = Cost::string(len);
        self.charge(c)?;
        let full = {
            let mut st = self.strings.borrow_mut();
            st.live.push((Rc::downgrade(s), c));
            st.held = st.held.plus(c);
            st.live.len() > st.sweep_len.max(SWEEP_MIN_LEN)
                || st.held.bytes > st.sweep_bytes.max(SWEEP_MIN_BYTES)
        };
        if full {
            self.sweep();
        }
        Ok(())
    }

    /// Gives back what the dead strings took.
    pub fn sweep(&self) {
        let mut st = self.strings.borrow_mut();
        let mut freed = Cost::ZERO;
        st.live.retain(|(w, c)| {
            let alive = w.strong_count() > 0;
            if !alive {
                freed = freed.plus(*c);
            }
            alive
        });
        st.held = st.held.minus(freed);
        st.sweep_len = st.live.len() * 2;
        st.sweep_bytes = st.held.bytes * 2;
        drop(st);
        self.release(freed);
    }

    /// The strings the list holds: a call that starts from shared globals forgets those of the
    /// calls before it, whose charges it does not carry.
    pub fn strings_len(&self) -> usize {
        self.strings.borrow().live.len()
    }

    /// Forgets the strings past the first `len` (see [`Budget::strings_len`]).
    pub fn forget_strings(&self, len: usize) {
        let mut st = self.strings.borrow_mut();
        if st.live.len() > len {
            let gone: Cost = st.live.drain(len..).fold(Cost::ZERO, |a, (_, c)| a.plus(c));
            st.held = st.held.minus(gone);
        }
    }
}
