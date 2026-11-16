//! Fixed-size pipes between two fibers, and an overview of the fiber utilities.
//!
//! Engine continuations are normally short, but often chained to one
//! another, so that one continuation does a bit of work and then schedules
//! another continuation for later. Such chains can be long, and often even
//! involve loops (see for example `repeat`). We call such chains
//! "fibers" of execution.
//!
//! These fibers are not threads - each is just a string of continuations -
//! but they share some common requirements with traditional threads.
//! For example, we want to avoid one fiber getting starved while a second
//! fiber continuously runs its continuations one after another.
//! As another example, fibers may want to communicate - e.g., one fiber
//! produces data that a second fiber consumes, and we wish to ensure that
//! both fibers get a chance to run, and that if one stops prematurely,
//! the other doesn't hang forever.
//!
//! Consult the following table to see which APIs are useful for fiber tasks:
//!
//! Task                                           | APIs
//! -----------------------------------------------|-------------------
//! Repeat a blocking task indefinitely            | `keep_doing`
//! Repeat a blocking task, then exit              | `repeat`, `do_until`
//! Provide mutual exclusion between two tasks     | `Semaphore`, `SharedMutex`
//! Pass a stream of data between two fibers       | [`Pipe`]
//! Safely shut down a resource                    | `Gate`
//! Hold on to an object while a fiber is running  | `do_with`
//!
//! A [`Pipe`] is made of a [`PipeReader`] and a [`PipeWriter`] that share a bounded queue on
//! one shard. Each end can be moved to a different fiber; dropping one end makes the other
//! see end of file or [`BrokenPipeException`] instead of blocking forever.

use std::fmt;

use crate::engine::core::future::{
    ExceptionPtr, Future, make_exception_future, make_exception_ptr, make_ready_future,
};
use crate::engine::core::queue::Queue;

/// Error returned when writing to a pipe whose read side was closed.
#[derive(Debug, Clone, Copy, Default)]
pub struct BrokenPipeException;

impl fmt::Display for BrokenPipeException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Broken pipe")
    }
}

impl std::error::Error for BrokenPipeException {}

/// Error returned by [`PipeReader::unread`] when an item is already waiting to be re-read.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnreadOverflowException;

impl fmt::Display for UnreadOverflowException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("pipe_reader::unread() overflow")
    }
}

impl std::error::Error for UnreadOverflowException {}

/// The buffer shared by the two ends of a pipe.
pub mod internal {
    use super::*;

    /// A bounded queue of items plus the open state of each end; `None` in the queue marks end
    /// of file.
    pub struct PipeBuffer<T: 'static> {
        buf: Queue<Option<T>>,
        read_open: bool,
        write_open: bool,
    }

    impl<T: 'static> PipeBuffer<T> {
        /// Creates an empty buffer holding at most `size` items, both ends open.
        pub fn new(size: usize) -> Self {
            Self { buf: Queue::new(size), read_open: true, write_open: true }
        }
        /// Pops the next item, waiting while the buffer is empty; `None` is end of file.
        #[inline]
        ///
        /// # Safety
        /// As `Queue::pop_eventually`: the buffer must outlive the returned future.
        pub unsafe fn read(&mut self) -> Future<Option<T>> {
            // SAFETY: forwarded from the caller.
            unsafe { self.buf.pop_eventually() }
        }
        /// Pushes an item, waiting while the buffer is full.
        #[inline]
        ///
        /// # Safety
        /// As `Queue::push_eventually`: the buffer must outlive the returned future.
        pub unsafe fn write(&mut self, data: T) -> Future<()> {
            // SAFETY: forwarded from the caller.
            unsafe { self.buf.push_eventually(Some(data)) }
        }
        /// Returns whether reads can still produce items: the write side is open or items remain.
        #[inline]
        pub fn readable(&self) -> bool {
            self.write_open || !self.buf.empty()
        }
        /// Returns whether writes are still accepted: the read side is open.
        #[inline]
        pub fn writeable(&self) -> bool {
            self.read_open
        }
        /// Closes the read side, failing a blocked writer with [`BrokenPipeException`];
        /// returns whether the write side is closed too (the buffer can then be freed).
        pub fn close_read(&mut self) -> bool {
            // If a writer blocking (on a full queue), need to stop it.
            if self.buf.full() {
                self.buf.abort(make_exception_ptr(BrokenPipeException));
            }
            self.read_open = false;
            !self.write_open
        }
        /// Closes the write side, waking a blocked reader with end of file; returns whether
        /// the read side is closed too (the buffer can then be freed).
        pub fn close_write(&mut self) -> bool {
            // If the queue is empty, write the EOF (disengaged optional) to the
            // queue to wake a blocked reader. If the queue is not empty, there is
            // no need to write the EOF to the queue - the reader will return an
            // EOF when it sees that the write side is closed.
            if self.buf.empty() {
                let _ = self.buf.push(None);
            }
            self.write_open = false;
            !self.read_open
        }
    }
}
/// The read side of a [`Pipe`], which allows only reading from the pipe.
///
/// A reader cannot be created separately, only as part of a reader/writer pair through
/// [`Pipe::new`].
pub struct PipeReader<T: 'static> {
    bufp: *mut internal::PipeBuffer<T>,
    unread: Option<T>,
}

impl<T: 'static> PipeReader<T> {
    /// Reads the next item from the pipe.
    ///
    /// Returns a future which is fulfilled when the pipe's buffer becomes non-empty, or the
    /// write side is closed. The value is `None` to mark end of file (the write side was
    /// closed, and everything it sent has been read).
    ///
    /// # Safety
    /// The returned future must be resolved before both ends of the pipe are dropped: the
    /// continuation that completes it runs on the pipe's buffer, which the last end frees.
    pub unsafe fn read(&mut self) -> Future<Option<T>> {
        if let Some(ret) = self.unread.take() {
            return make_ready_future(Some(ret));
        }
        // SAFETY: the buffer lives until both ends are gone.
        let b = unsafe { &mut *self.bufp };
        if b.readable() {
            // SAFETY: the buffer outlives the future by the caller's contract.
            unsafe { b.read() }
        } else {
            make_ready_future(None)
        }
    }

    /// Returns an item to the front of the pipe.
    ///
    /// Pushes the given item to the front of the pipe, so it will be returned by the next
    /// [`PipeReader::read`] call. The typical use case is to unread the last item returned by
    /// `read`. More generally, any item may be unread, but only one at a time.
    ///
    /// # Errors
    /// Returns [`UnreadOverflowException`] if an unread item is already pending, that is,
    /// two calls without an intervening `read`.
    pub fn unread(&mut self, item: T) -> Result<(), ExceptionPtr> {
        if self.unread.is_some() {
            return Err(make_exception_ptr(UnreadOverflowException));
        }
        self.unread = Some(item);
        Ok(())
    }
}

impl<T: 'static> Drop for PipeReader<T> {
    fn drop(&mut self) {
        // SAFETY: the buffer lives until both ends are gone; the last one frees it.
        unsafe {
            if !self.bufp.is_null() && (*self.bufp).close_read() {
                drop(Box::from_raw(self.bufp));
            }
        }
    }
}

/// The write side of a [`Pipe`], which allows only writing to the pipe.
///
/// A writer cannot be created separately, only as part of a reader/writer pair through
/// [`Pipe::new`].
pub struct PipeWriter<T: 'static> {
    bufp: *mut internal::PipeBuffer<T>,
}

impl<T: 'static> PipeWriter<T> {
    /// Writes an item to the pipe.
    ///
    /// Returns a future which is fulfilled when the data was written to the buffer (when it
    /// becomes non-full). If the data could not be written because the read side was closed,
    /// the future fails with [`BrokenPipeException`].
    ///
    /// # Safety
    /// As [`PipeReader::read`]: the returned future must be resolved before both ends of
    /// the pipe are dropped.
    pub unsafe fn write(&mut self, data: T) -> Future<()> {
        // SAFETY: the buffer lives until both ends are gone.
        let b = unsafe { &mut *self.bufp };
        if b.writeable() {
            // SAFETY: the buffer outlives the future by the caller's contract.
            unsafe { b.write(data) }
        } else {
            make_exception_future(make_exception_ptr(BrokenPipeException))
        }
    }
}

impl<T: 'static> Drop for PipeWriter<T> {
    fn drop(&mut self) {
        // SAFETY: as in PipeReader::drop.
        unsafe {
            if !self.bufp.is_null() && (*self.bufp).close_write() {
                drop(Box::from_raw(self.bufp));
            }
        }
    }
}

/// A fixed-size pipe for communicating between two fibers.
///
/// A pipe is a mechanism to transfer data between two fibers, one
/// producing data, and the other consuming it. The fixed-size buffer also
/// ensures a balanced execution of the two fibers, because the producer
/// fiber blocks when it writes to a full pipe, until the consumer fiber gets
/// to run and read from the pipe.
///
/// A pipe resembles a Unix pipe, in that it has a read side, a write side,
/// and a fixed-sized buffer between them, and supports either end to be closed
/// independently (and EOF or broken pipe when using the other side).
/// A pipe object holds the reader and write sides of the pipe as two
/// separate objects. These objects can be moved into two different fibers.
/// Importantly, if one of the pipe ends is destroyed (i.e., the continuations
/// capturing it end), the other end of the pipe will stop blocking, so the
/// other fiber will not hang.
///
/// The pipe's read and write interfaces are future-based blocking. I.e., the
/// write() and read() methods return a future which is fulfilled when the
/// operation is complete. The pipe is single-reader single-writer, meaning
/// that until the future returned by read() is fulfilled, read() must not be
/// called again (and same for write).
///
/// Note: The pipe reader and writer are movable, but *not* copyable. It is
/// often convenient to wrap each end in a shared pointer, so it can be
/// shared or easily captured into multiple continuations.
pub struct Pipe<T: 'static> {
    /// The read side.
    pub reader: PipeReader<T>,
    /// The write side.
    pub writer: PipeWriter<T>,
}

impl<T: 'static> Pipe<T> {
    /// Creates a pipe whose buffer holds at most `size` items. One heap allocation, shared by
    /// the two ends.
    pub fn new(size: usize) -> Self {
        let bufp = Box::into_raw(Box::new(internal::PipeBuffer::new(size)));
        Self { reader: PipeReader { bufp, unread: None }, writer: PipeWriter { bufp } }
    }
}
