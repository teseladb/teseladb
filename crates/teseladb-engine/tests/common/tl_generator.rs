//! A synchronous, move-only generator type for the coroutine tests.
//!
//! Rust has no stable synchronous coroutines, so the coroutine frame is the state of a
//! closure: each call resumes the body until its next yielded value (`Ok(Some(v))`), its end
//! (`Ok(None)`) or an exception that escaped it (`Err`). The generator is a move-only
//! input range: an [`Iterator`].

#![allow(dead_code)]

use teseladb::engine::core::future::ExceptionPtr;

/// Major version of the generator type.
pub const TL_GENERATOR_VERSION_MAJOR: u32 = 0;
/// Minor version of the generator type.
pub const TL_GENERATOR_VERSION_MINOR: u32 = 3;
/// Patch version of the generator type.
pub const TL_GENERATOR_VERSION_PATCH: u32 = 0;

/// The resumable body of a generator (the coroutine handle).
type Handle<T> = Box<dyn FnMut() -> Result<Option<T>, ExceptionPtr>>;

/// A generator of `T` values driven by a resumable body.
pub struct Generator<T> {
    handle: Option<Handle<T>>,
}

impl<T> Default for Generator<T> {
    /// An empty generator, with no body.
    fn default() -> Self {
        Generator { handle: None }
    }
}

impl<T> Generator<T> {
    /// The generator of `body`. The body starts suspended: it first runs when the generator
    /// is iterated.
    pub fn new<F: FnMut() -> Result<Option<T>, ExceptionPtr> + 'static>(body: F) -> Self {
        Generator { handle: Some(Box::new(body)) }
    }

    /// Exchanges the bodies of two generators.
    pub fn swap(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.handle, &mut other.handle);
    }

    /// The iterator, after resuming the body up to its first value.
    pub fn begin(mut self) -> GeneratorIterator<T> {
        let mut it = GeneratorIterator { handle: self.handle.take(), value: None };
        it.advance();
        it
    }
}

/// The iterator of a [`Generator`]: not cloneable, because it owns the body.
pub struct GeneratorIterator<T> {
    handle: Option<Handle<T>>,
    value: Option<T>,
}

impl<T> GeneratorIterator<T> {
    /// Resumes the body; an exception that escaped it is rethrown as a panic carrying it.
    fn advance(&mut self) {
        let Some(h) = self.handle.as_mut() else {
            self.value = None;
            return;
        };
        match h() {
            Ok(Some(v)) => self.value = Some(v),
            Ok(None) => {
                // The body is done.
                self.value = None;
                self.handle = None;
            }
            Err(e) => {
                self.value = None;
                self.handle = None;
                std::panic::panic_any(e);
            }
        }
    }

    /// Whether the body has finished.
    pub fn at_end(&self) -> bool {
        self.value.is_none()
    }
}

impl<T> Iterator for GeneratorIterator<T> {
    type Item = T;

    /// The current value, then advances.
    fn next(&mut self) -> Option<T> {
        let v = self.value.take()?;
        self.advance();
        Some(v)
    }
}

impl<T> IntoIterator for Generator<T> {
    type Item = T;
    type IntoIter = GeneratorIterator<T>;

    /// A generator is iterated from [`Generator::begin`] to the end of its body.
    fn into_iter(self) -> GeneratorIterator<T> {
        self.begin()
    }
}
