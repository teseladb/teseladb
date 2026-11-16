//! Close adapters for tests. A `deferred_close` guard keeps a reference to `obj` while the test
//! goes on using `obj`. `DeferredClose` holds a `&mut`, which would be invalidated by those
//! other uses, so the tests that reach a file or a stream through a pointer while the guard
//! lives hold that pointer instead and close through the library's `Closeable`.

#![allow(dead_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::future::Future;
use teseladb::engine::util::closeable::Closeable;

/// A closer that holds a raw pointer to the object: `close()` closes `*obj`.
pub struct CloseRef<T: Closeable>(*mut T);

impl<T: Closeable> CloseRef<T> {
    /// # Safety
    /// `obj` stays alive and in place for as long as this adapter, or a guard built on it,
    /// may close it, and no other reference to it is live while it is closed.
    pub unsafe fn new(obj: *mut T) -> Self {
        CloseRef(obj)
    }
}

impl<T: Closeable> Closeable for CloseRef<T> {
    fn close(&mut self) -> Future<()> {
        // SAFETY: the contract of `CloseRef::new`.
        unsafe { (*self.0).close() }
    }
}
