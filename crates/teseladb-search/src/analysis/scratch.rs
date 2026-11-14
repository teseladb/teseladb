//! Buffers that analysis reuses from term to term (FR-028): a stemmer, a normalizer or the word
//! segmentation works on a [`Pooled`] vector, which comes from a pool of its thread and goes back
//! to it when dropped, so after the first terms of a text no term allocates.

use std::cell::RefCell;
use std::fmt;
use std::ops::{Deref, DerefMut, RangeBounds};

use super::unicode::WordBreak;

/// The vectors a thread keeps of each kind; a term rarely holds more than a few at once.
const POOLED: usize = 32;

/// The pool of a thread for vectors of one element type.
type Pool<T> = RefCell<Vec<Vec<T>>>;

/// An element type with a pool of vectors in each thread.
pub trait Poolable: Sized + 'static {
    /// Runs `f` on the pool of this thread; `None` once the thread has dropped it.
    fn with_pool<R>(f: impl FnOnce(&Pool<Self>) -> R) -> Option<R>;
}

macro_rules! pools {
    ($($name:ident: $t:ty;)*) => {
        thread_local! {
            $(static $name: Pool<$t> = const { RefCell::new(Vec::new()) };)*
        }
        $(
            impl Poolable for $t {
                fn with_pool<R>(f: impl FnOnce(&Pool<$t>) -> R) -> Option<R> {
                    $name.try_with(f).ok()
                }
            }
        )*
    };
}

pools! {
    CHARS: char;
    BREAKS: WordBreak;
    OFFSETS: usize;
    INDEXED: (usize, char);
    SPANS: (usize, usize);
}

/// A vector taken from its thread's pool.
pub struct Pooled<T: Poolable>(Vec<T>);

/// A vector of characters taken from its thread's pool.
pub type Chars = Pooled<char>;

impl<T: Poolable> Pooled<T> {
    /// An empty vector, with the capacity a former term left in it.
    pub fn new() -> Pooled<T> {
        let pooled = T::with_pool(|p| p.borrow_mut().pop()).flatten();
        Pooled(pooled.unwrap_or_default())
    }

    /// The elements `items` gives.
    pub fn collect(items: impl IntoIterator<Item = T>) -> Pooled<T> {
        let mut v = Pooled::new();
        v.0.extend(items);
        v
    }

    // The vector's methods whose arguments read the vector (`w.truncate(w.len() - 1)`): called
    // on `Pooled` itself, they borrow it the way a call on a `Vec` does.

    /// [`Vec::truncate`].
    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    /// [`Vec::remove`].
    pub fn remove(&mut self, index: usize) -> T {
        self.0.remove(index)
    }

    /// [`Vec::insert`].
    pub fn insert(&mut self, index: usize, item: T) {
        self.0.insert(index, item);
    }

    /// [`Vec::splice`], dropping what it takes out.
    pub fn splice(&mut self, range: impl RangeBounds<usize>, with: impl IntoIterator<Item = T>) {
        self.0.splice(range, with);
    }
}

impl<T: Poolable + Clone> Pooled<T> {
    /// A copy of `items`.
    pub fn copy(items: &[T]) -> Pooled<T> {
        let mut v = Pooled::new();
        v.0.extend_from_slice(items);
        v
    }

    /// [`Vec::resize`].
    pub fn resize(&mut self, len: usize, item: T) {
        self.0.resize(len, item);
    }
}

impl Chars {
    /// The characters of `text`.
    pub fn of(text: &str) -> Chars {
        Chars::collect(text.chars())
    }

    /// Appends the characters to `out`.
    pub fn write(&self, out: &mut String) {
        out.extend(self.0.iter());
    }
}

impl<T: Poolable> Default for Pooled<T> {
    fn default() -> Pooled<T> {
        Pooled::new()
    }
}

impl<T: Poolable> Drop for Pooled<T> {
    fn drop(&mut self) {
        let mut v = std::mem::take(&mut self.0);
        v.clear();
        // A thread that is ending drops its pool first; the vector is then just freed.
        T::with_pool(|p| {
            let mut p = p.borrow_mut();
            if p.len() < POOLED {
                p.push(v);
            }
        });
    }
}

impl<T: Poolable + Clone> Clone for Pooled<T> {
    fn clone(&self) -> Pooled<T> {
        Pooled::copy(&self.0)
    }
}

impl<T: Poolable + PartialEq> PartialEq for Pooled<T> {
    fn eq(&self, other: &Pooled<T>) -> bool {
        self.0 == other.0
    }
}

impl<T: Poolable + Eq> Eq for Pooled<T> {}

impl<T: Poolable + fmt::Debug> fmt::Debug for Pooled<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Poolable> Deref for Pooled<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Vec<T> {
        &self.0
    }
}

impl<T: Poolable> DerefMut for Pooled<T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.0
    }
}

/// Runs `f` with a cleared string and gives what it wrote: the `String` form of a function that
/// writes a term into a buffer.
pub fn written(f: impl FnOnce(&mut String)) -> String {
    let mut out = String::new();
    f(&mut out);
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_vector_serves_the_next_one() {
        let first = Chars::of("abcdefgh");
        let at = first.as_ptr();
        drop(first);
        let second = Chars::new();
        assert!(second.is_empty());
        assert_eq!(second.as_ptr(), at);
        assert!(second.capacity() >= 8);
    }

    #[test]
    fn copies_writes_and_edits() {
        let c = Chars::collect("xyz".chars().rev());
        let d = c.clone();
        assert_eq!(c, d);
        assert_eq!(format!("{d:?}"), "['z', 'y', 'x']");
        let mut out = String::from(">");
        Chars::copy(&d[1..]).write(&mut out);
        assert_eq!(out, ">yx");
        assert_eq!(written(|s| s.push('a')), "a");
        let mut e = Chars::default();
        e.extend("abcdef".chars());
        e.truncate(e.len() - 1);
        assert_eq!(e.remove(e.len() - 1), 'e');
        e.insert(e.len(), 'z');
        e.splice(..1, ['q', 'r']);
        e.resize(e.len() + 1, '!');
        assert_eq!(e.iter().collect::<String>(), "qrbcdz!");
    }

    #[test]
    fn the_pool_keeps_a_bounded_number() {
        let many: Vec<Chars> = (0..POOLED + 8).map(|_| Chars::of("ab")).collect();
        drop(many);
        assert_eq!(CHARS.with(|p| p.borrow().len()), POOLED);
    }
}
