//! Immutable reference-counted buffer: sharing and slicing never copy.

use core::fmt;
use core::ops::{Bound, Deref, RangeBounds};
use std::sync::Arc;

/// Immutable view over an `Arc<[u8]>`. Cloning or slicing with [`Bytes::slice`] is O(1).
#[derive(Clone)]
pub struct Bytes {
    buf: Arc<[u8]>,
    start: usize,
    end: usize,
}

impl Bytes {
    /// Empty buffer.
    pub fn new() -> Bytes {
        Bytes::from_vec(Vec::new())
    }

    /// Takes ownership of a `Vec` (a single copy when turning it into an `Arc<[u8]>`).
    pub fn from_vec(v: Vec<u8>) -> Bytes {
        let end = v.len();
        Bytes { buf: Arc::from(v), start: 0, end }
    }

    /// Copies a slice.
    pub fn copy_from_slice(s: &[u8]) -> Bytes {
        Bytes::from_vec(s.to_vec())
    }

    /// Length of the view.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Tells whether the view is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Sub-view relative to this one. Panics if the range goes outside the view, like indexing a
    /// slice.
    pub fn slice(&self, range: impl RangeBounds<usize>) -> Bytes {
        let len = self.len();
        let lo = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };
        let hi = match range.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => len,
        };
        assert!(lo <= hi && hi <= len, "range {lo}..{hi} outside a view of {len} bytes");
        Bytes { buf: Arc::clone(&self.buf), start: self.start + lo, end: self.start + hi }
    }

    /// Copies the contents into a new `Vec`.
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_ref().to_vec()
    }
}

impl Default for Bytes {
    fn default() -> Self {
        Bytes::new()
    }
}

impl Deref for Bytes {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }
}

impl AsRef<[u8]> for Bytes {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes::from_vec(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(s: &[u8]) -> Self {
        Bytes::copy_from_slice(s)
    }
}

impl From<&str> for Bytes {
    fn from(s: &str) -> Self {
        Bytes::copy_from_slice(s.as_bytes())
    }
}

impl From<String> for Bytes {
    fn from(s: String) -> Self {
        Bytes::from_vec(s.into_bytes())
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl Eq for Bytes {}

impl PartialEq<[u8]> for Bytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_ref() == other
    }
}

impl core::hash::Hash for Bytes {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SHOW: usize = 32;
        write!(f, "Bytes[{}](", self.len())?;
        for b in self.iter().take(SHOW) {
            write!(f, "{b:02x}")?;
        }
        if self.len() > SHOW {
            f.write_str("…")?;
        }
        f.write_str(")")
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn slicing_shares_and_is_relative() {
        let b = Bytes::from_vec((0u8..10).collect());
        let s = b.slice(2..8);
        assert_eq!(&*s, &[2, 3, 4, 5, 6, 7]);
        let t = s.slice(1..=2);
        assert_eq!(&*t, &[3, 4]);
        assert_eq!(&*s.slice(..), &*s);
        assert_eq!(&*s.slice((Bound::Excluded(0), Bound::Unbounded)), &[3, 4, 5, 6, 7]);
        assert!(s.slice(3..3).is_empty());
        assert_eq!(t.to_vec(), vec![3, 4]);
        assert!(Arc::ptr_eq(&b.buf, &t.buf));
    }

    #[test]
    #[should_panic(expected = "outside a view")]
    fn slice_out_of_range_panics() {
        Bytes::from("abc").slice(1..5);
    }

    #[test]
    #[should_panic(expected = "outside a view")]
    fn slice_reversed_range_panics() {
        let (lo, hi) = (2, 1);
        Bytes::from("abc").slice(lo..hi);
    }

    #[test]
    fn conversions_equality_and_debug() {
        assert!(Bytes::new().is_empty());
        assert_eq!(Bytes::default().len(), 0);
        let a = Bytes::from(&b"ping"[..]);
        let b = Bytes::from("ping");
        let c: Bytes = vec![b'p', b'i', b'n', b'g'].into();
        assert_eq!(Bytes::from(String::from("ping")), c);
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a == *b"ping".as_slice());
        assert_eq!(a.as_ref(), b"ping");
        assert_eq!(format!("{a:?}"), "Bytes[4](70696e67)");
        let long = Bytes::from_vec(vec![0xab; 40]);
        assert!(format!("{long:?}").ends_with("…)"));
        let mut set = std::collections::HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&c));
    }
}
