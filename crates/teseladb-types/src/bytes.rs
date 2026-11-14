//! Immutable reference-counted buffer: sharing and slicing never copy.

use core::fmt;
use core::ops::{Bound, Deref, RangeBounds};
use std::sync::Arc;

/// Immutable view over shared bytes: an `Arc<[u8]>`, or memory another owner keeps
/// ([`Bytes::from_owner`]). Cloning or slicing with [`Bytes::slice`] is O(1).
#[derive(Clone)]
pub struct Bytes {
    buf: Buf,
    start: usize,
    end: usize,
}

/// The bytes a view shares.
#[derive(Clone)]
enum Buf {
    /// On the heap.
    Heap(Arc<[u8]>),
    /// Kept by their owner, dropped with the last view.
    Owner(Arc<dyn AsRef<[u8]> + Send + Sync>),
}

impl Buf {
    #[inline]
    fn bytes(&self) -> &[u8] {
        match self {
            Buf::Heap(b) => b,
            Buf::Owner(o) => (**o).as_ref(),
        }
    }

    #[cfg(test)]
    fn same(&self, other: &Buf) -> bool {
        match (self, other) {
            (Buf::Heap(a), Buf::Heap(b)) => Arc::ptr_eq(a, b),
            (Buf::Owner(a), Buf::Owner(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Bytes {
    /// Empty buffer.
    #[must_use]
    pub fn new() -> Bytes {
        Bytes::from_vec(Vec::new())
    }

    /// Takes ownership of a `Vec` (a single copy when turning it into an `Arc<[u8]>`).
    #[must_use]
    pub fn from_vec(v: Vec<u8>) -> Bytes {
        let end = v.len();
        Bytes { buf: Buf::Heap(Arc::from(v)), start: 0, end }
    }

    /// The bytes `owner` keeps, without a copy: memory the caller placed itself (a block on
    /// pages of its own, a mapped file). The owner is dropped with the last view; its bytes must
    /// not change while a view exists, which `AsRef` alone cannot change.
    #[must_use]
    pub fn from_owner(owner: impl AsRef<[u8]> + Send + Sync + 'static) -> Bytes {
        let end = owner.as_ref().len();
        Bytes { buf: Buf::Owner(Arc::new(owner)), start: 0, end }
    }

    /// Copies a slice.
    #[must_use]
    pub fn copy_from_slice(s: &[u8]) -> Bytes {
        Bytes::from_vec(s.to_vec())
    }

    /// Length of the view.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Tells whether the view is empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Sub-view relative to this one.
    ///
    /// # Panics
    ///
    /// When the range goes outside the view or ends before it starts, like indexing a slice.
    #[must_use]
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
        Bytes { buf: self.buf.clone(), start: self.start + lo, end: self.start + hi }
    }

    /// Copies the contents into a new `Vec`.
    #[must_use]
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
        &self.buf.bytes()[self.start..self.end]
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
        assert!(b.buf.same(&t.buf));
    }

    /// An owner that counts its drops.
    struct Counted(Vec<u8>, Arc<std::sync::atomic::AtomicUsize>);

    impl AsRef<[u8]> for Counted {
        fn as_ref(&self) -> &[u8] {
            &self.0
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn bytes_of_an_owner_are_shared_without_a_copy_and_dropped_with_the_last_view() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner = Counted((0u8..10).collect(), Arc::clone(&drops));
        let at = owner.0.as_ptr();
        let b = Bytes::from_owner(owner);
        assert_eq!(b.as_ptr(), at, "the view reads the owner's own bytes");
        let s = b.slice(3..6);
        assert_eq!(&*s, &[3, 4, 5]);
        assert!(b.buf.same(&s.buf));
        assert!(!b.buf.same(&Bytes::from("x").buf));
        assert_eq!(b, Bytes::from_vec((0u8..10).collect()));
        drop(b);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(s);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    #[should_panic(expected = "outside a view")]
    fn slice_out_of_range_panics() {
        let _ = Bytes::from("abc").slice(1..5);
    }

    #[test]
    #[should_panic(expected = "outside a view")]
    fn slice_reversed_range_panics() {
        let (lo, hi) = (2, 1);
        let _ = Bytes::from("abc").slice(lo..hi);
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
        assert_eq!(a, *b"ping".as_slice());
        assert_eq!(a.as_ref(), b"ping");
        assert_eq!(format!("{a:?}"), "Bytes[4](70696e67)");
        let long = Bytes::from_vec(vec![0xab; 40]);
        assert!(format!("{long:?}").ends_with("…)"));
        let mut set = std::collections::HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&c));
    }
}
