//! Operations of the buffered byte streams declared in [`crate::engine::core::iostream`].
//!
//! This module holds the longer methods of [`InputStreamState`] and [`OutputStreamState`]
//! (reads, consumption loops, buffered and zero-copy writes, flushing and closing), the
//! default data source skip, the packet fallback for data sinks, and [`copy`].
//!
//! Every asynchronous operation takes a `StateRef` hold on its stream's state: the state
//! counts the operation as pending and cannot be freed until the hold is gone, which is
//! what lets continuations reach the state through a raw pointer. All of it runs on the
//! shard that owns the stream.

use std::ptr;

use crate::engine::core::coroutine::make_coroutine;
use crate::engine::core::do_with::do_with;
use crate::engine::core::future::{ExceptionPtr, Future, make_exception_future, make_ready_future};
use crate::engine::core::iostream::{
    ConsumptionResult, ConsumptionVariant, DataSinkImpl, DataSourceImpl, InputStream,
    InputStreamConsumer, InputStreamState, OutputStream, OutputStreamState,
};
use crate::engine::core::loop_::{StopIteration, repeat, repeat_until_value};
use crate::engine::core::reactor_backend::engine_assert;
use crate::engine::core::scattered_message::ScatteredMessage;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::{ByteType, TemporaryBuffer};
use crate::engine::net::packet::Packet;

/// The default `DataSourceImpl::skip`: reads and discards buffers until `n` bytes are
/// skipped, and resolves with the rest of the last buffer read (empty at end of stream).
pub(crate) fn data_source_impl_skip<S: DataSourceImpl + ?Sized>(
    this: &mut S,
    n: u64,
) -> Future<TemporaryBuffer<u8>> {
    let this = this as *mut S;
    do_with(n, move |n: &mut u64| {
        let n = n as *mut u64;
        repeat_until_value(move || {
            // SAFETY: a data source outlives the futures of its own skip().
            unsafe { (*this).get() }.then(
                move |mut buffer: TemporaryBuffer<u8>| -> Future<Option<TemporaryBuffer<u8>>> {
                    // SAFETY: do_with keeps `n` alive until the loop ends.
                    let n = unsafe { &mut *n };
                    if buffer.empty() {
                        // The source ended before `n` bytes: skip() consumes exactly `n`.
                        return make_exception_future(premature_end_of_stream());
                    }
                    if buffer.size() as u64 >= *n {
                        // SAFETY: `*n <= size` (checked above).
                        unsafe { buffer.trim_front_unchecked(*n as usize) };
                        return make_ready_future(Some(buffer));
                    }
                    *n -= buffer.size() as u64;
                    make_ready_future(None)
                },
            )
        })
    })
}

/// The error of a skip that reaches the end of the stream first.
fn premature_end_of_stream() -> ExceptionPtr {
    ExceptionPtr::new(crate::engine::core::future::RuntimeError::new("premature end of stream"))
}

/// Writes a packet to a sink one buffer at a time, with `put_buffer`.
///
/// Sinks that only implement the buffer form use it to implement the packet form. It is
/// not the trait's default packet method because the two defaults would recurse into each
/// other.
pub fn fallback_put<S: DataSinkImpl + ?Sized>(this: &mut S, mut data: Packet) -> Future<()> {
    let this = this as *mut S;
    let buffers = data.release();
    make_coroutine(async move {
        for buf in buffers {
            // SAFETY: a data sink outlives the futures of its own put().
            unsafe { (*this).put_buffer(buf) }.await?;
        }
        Ok(())
    })
}

impl<CharType: ByteType> OutputStreamState<CharType> {
    /// Writes the bytes of the NUL-terminated string at `buf`, without the NUL.
    ///
    /// # Safety
    /// `buf` points to a NUL-terminated string.
    #[inline]
    pub unsafe fn write_cstr(&mut self, buf: *const CharType) -> Future<()> {
        // SAFETY: guaranteed by the caller, as strlen() requires.
        let n =
            unsafe { std::ffi::CStr::from_ptr(buf as *const std::ffi::c_char) }.to_bytes().len();
        // SAFETY: `buf` has `n` readable bytes.
        unsafe { self.write_raw(buf, n) }
    }

    /// Writes the bytes of `s`, copying them into the stream's buffer.
    #[inline]
    pub fn write_sstring(&mut self, s: &Sstring) -> Future<()> {
        let b = s.as_bytes();
        // SAFETY: the bytes of `s` are readable during the call.
        unsafe { self.write_raw(b.as_ptr() as *const CharType, b.len()) }
    }

    /// Writes the bytes of `s`, copying them into the stream's buffer.
    #[inline]
    pub fn write_str(&mut self, s: &str) -> Future<()> {
        // SAFETY: the bytes of `s` are readable during the call.
        unsafe { self.write_raw(s.as_ptr() as *const CharType, s.len()) }
    }
}

impl OutputStreamState<u8> {
    /// Writes the packet assembled by `msg`, without copying (a zero-copy write).
    ///
    /// # Panics
    /// Asserts that the stream holds no buffered (copied) bytes: buffered and zero-copy
    /// writes cannot be mixed.
    #[inline]
    pub fn write_scattered(&mut self, msg: ScatteredMessage) -> Future<()> {
        self.write_packet(msg.release())
    }
}

impl<CharType: ByteType> OutputStreamState<CharType> {
    /// Hands `p` to the sink, after the flush in progress if there is one.
    pub(crate) fn zero_copy_put(&mut self, p: Packet) -> Future<()> {
        // If a flush is scheduled, disable it, so it does not write in parallel.
        self._flush = false;
        if self._flushing {
            let this = self.hold();
            // A flush is in progress: wait for it to end before continuing.
            match self._in_batch.as_mut() {
                Some(pr) => pr.get_future().then(move |()| {
                    // SAFETY: `this` keeps the state live (see `StateRef`).
                    unsafe { (*this.ptr())._fd.put(p) }
                }),
                None => unreachable!("a flushing stream has its batch promise"),
            }
        } else {
            self._fd.put(p)
        }
    }

    /// Writes `p` in chunks of the stream's buffer size; a last chunk shorter than that is
    /// kept as pending zero-copy data.
    pub(crate) fn zero_copy_split_and_put(&mut self, p: Packet) -> Future<()> {
        let this = self.hold();
        let mut p = p;
        repeat(move || -> Future<StopIteration> {
            // SAFETY: `this` keeps the state live (see `StateRef`); the reference ends with
            // this call.
            let me = unsafe { &mut *this.ptr() };
            if (p.len() as usize) < me._size {
                if p.len() != 0 {
                    me._zc_bufs = std::mem::replace(&mut p, Packet::make_null_packet());
                } else {
                    me._zc_bufs = Packet::make_null_packet();
                }
                return make_ready_future(StopIteration::YES);
            }
            let chunk = p.share_range(0, me._size);
            p.trim_front(me._size);
            me.zero_copy_put(chunk).then(|()| make_ready_future(StopIteration::NO))
        })
    }
}

impl OutputStreamState<u8> {
    /// Appends `p` to the pending zero-copy data and sends it once it reaches the buffer
    /// size (split into buffer-sized chunks if the stream trims to size).
    ///
    /// # Panics
    /// Asserts that the stream holds no buffered (copied) bytes: buffered and zero-copy
    /// writes cannot be mixed.
    pub fn write_packet(&mut self, p: Packet) -> Future<()> {
        if p.len() != 0 {
            engine_assert!(
                self._end == 0,
                "Mixing buffered writes and zero-copy writes not supported yet"
            );

            if self._zc_bufs.is_valid() {
                self._zc_bufs.append(p);
            } else {
                self._zc_bufs = p;
            }

            if self._zc_bufs.len() as usize >= self._size {
                let zc = std::mem::replace(&mut self._zc_bufs, Packet::make_null_packet());
                if self._trim_to_size {
                    return self.zero_copy_split_and_put(zc);
                } else {
                    return self.zero_copy_put(zc);
                }
            }
        }
        make_ready_future(())
    }

    /// Writes `p` without copying, as a one-buffer packet.
    ///
    /// # Panics
    /// Asserts that the stream holds no buffered (copied) bytes: buffered and zero-copy
    /// writes cannot be mixed.
    pub fn write_buffer(&mut self, p: TemporaryBuffer<u8>) -> Future<()> {
        if p.empty() {
            return make_ready_future(());
        }
        engine_assert!(
            self._end == 0,
            "Mixing buffered writes and zero-copy writes not supported yet"
        );
        self.write_packet(Packet::from_buffer(p))
    }
}

impl<CharType: ByteType> InputStreamState<CharType> {
    /// The slow path of [`InputStreamState::read_exactly`]: copies into a new buffer of `n`
    /// bytes until it is full or the stream ends, and returns the bytes read.
    fn read_exactly_part(&mut self, n: usize) -> Future<TemporaryBuffer<CharType>> {
        let held = self.hold();
        make_coroutine(async move {
            // `held` is owned by the coroutine until it finishes.
            let this = held.ptr();
            let mut out = TemporaryBuffer::<CharType>::new(n);
            let mut completed: usize = 0;
            while completed < n {
                // SAFETY: the coroutine's `StateRef` keeps the state live; the reference is
                // formed again after each suspension and is not
                // held across one, so the stream's own accesses meanwhile overlap none.
                let me = unsafe { &mut *this };
                let avail = me.available();
                if avail != 0 {
                    let now = (n - completed).min(avail);
                    // SAFETY: `_buf` has `avail >= now` bytes and `out` has `n - completed >= now` free.
                    unsafe {
                        ptr::copy_nonoverlapping(me._buf.get(), out.get_write().add(completed), now)
                    };
                    // SAFETY: `now <= avail`, the buffer's size.
                    unsafe { me._buf.trim_front_unchecked(now) };
                    completed += now;
                    if completed == n {
                        break;
                    }
                }

                // `_buf` is now empty.
                let f = me._fd.get();
                let buf = f.await?;
                // SAFETY: as above, after the suspension.
                let me = unsafe { &mut *this };
                if buf.size() == 0 {
                    me._eof = true;
                    // SAFETY: `completed < n`, the size of `out`.
                    unsafe { out.trim_unchecked(completed) };
                    break;
                }
                me._buf = buf.retype();
            }
            Ok(out)
        })
    }

    /// Reads `n` bytes from the stream, or fewer if the end of stream is reached.
    ///
    /// The future waits until `n` bytes are available and returns them. If the end of
    /// stream is reached first, fewer than `n` bytes are returned, so despite the name the
    /// caller must not assume the buffer always holds exactly `n` bytes.
    ///
    /// # Errors
    /// The future fails if an I/O error occurs during the read. Reaching the end of stream
    /// early is not an I/O error.
    pub fn read_exactly(&mut self, n: usize) -> Future<TemporaryBuffer<CharType>> {
        if self._buf.size() == n {
            // Easy case: steal the buffer and return it to the caller.
            make_ready_future(std::mem::take(&mut self._buf))
        } else if self._buf.size() > n {
            // The buffer is large enough: share its head with the caller.
            // SAFETY: `n < size` (checked above).
            let front = unsafe { self._buf.share_range_unchecked(0, n) };
            // SAFETY: as above.
            unsafe { self._buf.trim_front_unchecked(n) };
            make_ready_future(front)
        } else if self._buf.size() == 0 {
            // The buffer is empty: at the end of the stream there is nothing more to read (the
            // source must not be asked again); otherwise fetch one and retry.
            if self._eof {
                return make_ready_future(TemporaryBuffer::default());
            }
            let this = self.hold();
            self._fd.get().then(move |buf: TemporaryBuffer<u8>| {
                // SAFETY: `this` keeps the state live (see `StateRef`).
                let me = unsafe { &mut *this.ptr() };
                if buf.size() == 0 {
                    me._eof = true;
                    return make_ready_future(buf.retype());
                }
                me._buf = buf.retype();
                me.read_exactly(n)
            })
        } else {
            // The buffer is too small: start the copy/read loop.
            self.read_exactly_part(n)
        }
    }

    /// Feeds the stream's buffers to `consumer` until it stops or the stream ends.
    ///
    /// The consumer may consume part of a buffer and hand back the rest, which the stream
    /// keeps for the next read, or skip a number of bytes. The future fails with the first
    /// read error or consumer failure.
    pub fn consume<Consumer: InputStreamConsumer<CharType> + 'static>(
        &mut self,
        consumer: Consumer,
    ) -> Future<()> {
        let held = self.hold();
        let mut consumer = consumer;
        repeat(move || -> Future<StopIteration> {
            // `held` lives in the loop's action, which outlives every continuation below.
            let this = held.ptr();
            // SAFETY: `held` keeps the state live (see `StateRef`).
            let me = unsafe { &mut *this };
            if me._buf.empty() && !me._eof {
                return me._fd.get().then(move |buf: TemporaryBuffer<u8>| {
                    // SAFETY: as above.
                    let me = unsafe { &mut *this };
                    me._buf = buf.retype();
                    me._eof = me._buf.empty();
                    make_ready_future(StopIteration::NO)
                });
            }
            consumer.call(std::mem::take(&mut me._buf)).then(
                move |mut result: ConsumptionResult<CharType>| -> Future<StopIteration> {
                    // SAFETY: as above.
                    let me = unsafe { &mut *this };
                    match result.get() {
                        ConsumptionVariant::Continue(_) => {
                            // The consumer consumed the entire buffer and is ready for more, so
                            // the loop continues, unless the stream is at its end.
                            make_ready_future(StopIteration::new(me._eof))
                        }
                        ConsumptionVariant::Stop(stop) => {
                            // The consumer is done.
                            me._buf = std::mem::take(stop.get_buffer());
                            make_ready_future(StopIteration::YES)
                        }
                        ConsumptionVariant::Skip(skip) => {
                            me._fd.skip(skip.get_value()).then(move |buf: TemporaryBuffer<u8>| {
                                if !buf.empty() {
                                    // SAFETY: as above.
                                    unsafe { (*this)._buf = buf.retype() };
                                }
                                make_ready_future(StopIteration::NO)
                            })
                        }
                    }
                },
            )
        })
    }

    /// [`InputStreamState::consume`] with a consumer the caller keeps, passed by reference.
    ///
    /// # Safety
    /// The consumer outlives the returned future and is not used elsewhere meanwhile.
    pub unsafe fn consume_ref<Consumer: InputStreamConsumer<CharType> + 'static>(
        &mut self,
        consumer: &mut Consumer,
    ) -> Future<()> {
        struct Ref<C>(*mut C);
        impl<CharType: ByteType, C: InputStreamConsumer<CharType>> InputStreamConsumer<CharType>
            for Ref<C>
        {
            #[inline]
            fn call(
                &mut self,
                buf: TemporaryBuffer<CharType>,
            ) -> Future<ConsumptionResult<CharType>> {
                // SAFETY: the caller keeps the consumer alive until consume() resolves.
                unsafe { (*self.0).call(buf) }
            }
        }
        self.consume(Ref(consumer as *mut Consumer))
    }

    /// Returns up to `n` bytes from the stream, or an empty buffer at the end of stream.
    pub fn read_up_to(&mut self, n: usize) -> Future<TemporaryBuffer<CharType>> {
        if self._buf.empty() {
            if self._eof {
                make_ready_future(TemporaryBuffer::default())
            } else {
                let this = self.hold();
                self._fd.get().then(move |buf: TemporaryBuffer<u8>| {
                    // SAFETY: `this` keeps the state live (see `StateRef`).
                    let me = unsafe { &mut *this.ptr() };
                    me._eof = buf.empty();
                    me._buf = buf.retype();
                    me.read_up_to(n)
                })
            }
        } else if self._buf.size() <= n {
            // Easy case: steal the buffer and return it to the caller.
            make_ready_future(std::mem::take(&mut self._buf))
        } else {
            // The buffer is larger than `n`: share its head with the caller.
            // SAFETY: `n < size` (checked above).
            let front = unsafe { self._buf.share_range_unchecked(0, n) };
            // SAFETY: as above.
            unsafe { self._buf.trim_front_unchecked(n) };
            make_ready_future(front)
        }
    }

    /// Returns some data from the stream, or an empty buffer at the end of stream.
    pub fn read(&mut self) -> Future<TemporaryBuffer<CharType>> {
        if self._eof {
            return make_ready_future(TemporaryBuffer::default());
        }
        if self._buf.empty() {
            let this = self.hold();
            self._fd.get().then(move |buf: TemporaryBuffer<u8>| {
                // SAFETY: `this` keeps the state live (see `StateRef`).
                unsafe { (*this.ptr())._eof = buf.empty() };
                make_ready_future(buf.retype())
            })
        } else {
            make_ready_future(std::mem::take(&mut self._buf))
        }
    }

    /// Discards the next `n` bytes of the stream (fewer if it ends first).
    pub fn skip(&mut self, mut n: u64) -> Future<()> {
        let skip_buf = n.min(self._buf.size() as u64);
        // SAFETY: `skip_buf <= size`.
        unsafe { self._buf.trim_front_unchecked(skip_buf as usize) };
        n -= skip_buf;
        if n == 0 {
            return make_ready_future(());
        }
        // The source already ended: the source must not be asked again.
        if self._eof {
            return make_exception_future(premature_end_of_stream());
        }
        let this = self.hold();
        self._fd.skip(n).then(move |buffer: TemporaryBuffer<u8>| {
            // SAFETY: `this` keeps the state live (see `StateRef`).
            unsafe { (*this.ptr())._buf = buffer.retype() };
        })
    }
}

impl<CharType: ByteType> OutputStreamState<CharType> {
    /// Writes `buf` in chunks of the stream's buffer size; a last chunk shorter than that
    /// stays buffered.
    pub(crate) fn split_and_put(&mut self, buf: TemporaryBuffer<CharType>) -> Future<()> {
        engine_assert!(self._end == 0);

        let this = self.hold();
        let mut buf = buf;
        repeat(move || -> Future<StopIteration> {
            // SAFETY: `this` keeps the state live (see `StateRef`); the reference ends with
            // this call.
            let me = unsafe { &mut *this.ptr() };
            if buf.size() < me._size {
                if !me._buf.is_nonempty() {
                    me._buf = me._fd.allocate_buffer(me._size).retype();
                }
                // SAFETY: `_buf` holds `_size > buf.size()` bytes.
                unsafe { ptr::copy_nonoverlapping(buf.get(), me._buf.get_write(), buf.size()) };
                me._end = buf.size();
                return make_ready_future(StopIteration::YES);
            }
            // SAFETY: the loop splits only a buffer at least `_size` long (the shorter one was
            // handled above).
            let chunk = unsafe { buf.share_range_unchecked(0, me._size) };
            // SAFETY: as above.
            unsafe { buf.trim_front_unchecked(me._size) };
            me.put(chunk).then(|()| make_ready_future(StopIteration::NO))
        })
    }

    /// Writes a copy of `buf`: the bytes are copied into the stream's buffer (or sent in
    /// buffer-sized pieces) before the call returns, so `buf` may be reused at once.
    #[inline]
    pub fn write(&mut self, buf: &[CharType]) -> Future<()> {
        // SAFETY: the slice is readable for its length during the call.
        unsafe { self.write_raw(buf.as_ptr(), buf.len()) }
    }

    /// [`OutputStreamState::write`] on a raw pointer and length.
    ///
    /// # Safety
    /// `buf` is valid for reads of `n` elements during the call: the bytes are copied into
    /// the stream's own buffers before it returns.
    #[inline]
    pub unsafe fn write_raw(&mut self, buf: *const CharType, n: usize) -> Future<()> {
        if !self._buf.is_nonempty() || n > self._size - self._end {
            // SAFETY: forwarded from the caller.
            return unsafe { self.slow_write(buf, n) };
        }
        // SAFETY: `_buf` has `_size - _end >= n` free bytes; the caller passes `n` readable ones.
        unsafe { ptr::copy_nonoverlapping(buf, self._buf.get_write().add(self._end), n) };
        self._end += n;
        make_ready_future(())
    }

    /// The out-of-line path of [`OutputStreamState::write_raw`], taken when the bytes do not
    /// fit in the current buffer.
    ///
    /// # Safety
    /// As [`OutputStreamState::write_raw`].
    #[inline(never)]
    #[cold]
    unsafe fn slow_write(&mut self, buf: *const CharType, n: usize) -> Future<()> {
        engine_assert!(
            !self._zc_bufs.is_valid(),
            "Mixing buffered writes and zero-copy writes not supported yet"
        );
        let bulk_threshold = if self._end != 0 { 2 * self._size - self._end } else { self._size };
        if n >= bulk_threshold {
            if self._end != 0 {
                let now = self._size - self._end;
                // SAFETY: `_buf` has `now` free bytes after `_end`; `buf` has `n >= now` bytes.
                unsafe { ptr::copy_nonoverlapping(buf, self._buf.get_write().add(self._end), now) };
                self._end = self._size;
                let mut tmp = self._fd.allocate_buffer(n - now);
                // SAFETY: `tmp` holds `n - now` bytes.
                unsafe {
                    ptr::copy_nonoverlapping(buf.add(now) as *const u8, tmp.get_write(), n - now)
                };
                // SAFETY: `_end <= _size`, the buffer's size.
                unsafe { self._buf.trim_unchecked(self._end) };
                self._end = 0;
                let this = self.hold();
                let b = std::mem::take(&mut self._buf);
                return self.put(b).then(move |()| {
                    // SAFETY: `this` keeps the state live (see `StateRef`).
                    let me = unsafe { &mut *this.ptr() };
                    if me._trim_to_size {
                        me.split_and_put(tmp.retype())
                    } else {
                        me.put(tmp.retype())
                    }
                });
            } else {
                let mut tmp = self._fd.allocate_buffer(n);
                // SAFETY: `tmp` holds `n` bytes.
                unsafe { ptr::copy_nonoverlapping(buf as *const u8, tmp.get_write(), n) };
                if self._trim_to_size {
                    return self.split_and_put(tmp.retype());
                } else {
                    return self.put(tmp.retype());
                }
            }
        }

        if !self._buf.is_nonempty() {
            self._buf = self._fd.allocate_buffer(self._size).retype();
        }

        let now = n.min(self._size - self._end);
        // SAFETY: `_buf` has `_size - _end >= now` free bytes.
        unsafe { ptr::copy_nonoverlapping(buf, self._buf.get_write().add(self._end), now) };
        self._end += now;
        if now == n {
            make_ready_future(())
        } else {
            let mut next = self._fd.allocate_buffer(self._size);
            // SAFETY: `next` holds `_size > n - now` bytes.
            unsafe {
                ptr::copy_nonoverlapping(buf.add(now) as *const u8, next.get_write(), n - now)
            };
            self._end = n - now;
            let next = std::mem::replace(&mut self._buf, next.retype());
            self.put(next)
        }
    }
}

/// Stream helpers shared with the reactor: the flush poller hook and the copy consumer.
pub mod internal {
    use crate::engine::core::future::{Future, make_ready_future};
    use crate::engine::core::iostream::{
        ConsumptionResult, InputStreamConsumer, OutputStreamState, StateRef, continue_consuming,
        stop_consuming,
    };
    use crate::engine::core::temporary_buffer::{ByteType, TemporaryBuffer};

    // Registers a stream for a deferred flush; implemented by the reactor.
    pub use crate::engine::core::reactor::internal_misc::add_to_flush_poller;

    /// An input stream consumer that writes everything it receives to an output stream.
    ///
    /// The output stream is held as a `StateRef`: the consumer counts as a pending
    /// operation of the output stream for as long as it exists.
    pub struct StreamCopyConsumer<CharType: ByteType> {
        _os: StateRef<OutputStreamState<CharType>>,
    }

    impl<CharType: ByteType> StreamCopyConsumer<CharType> {
        /// A consumer writing to `os`.
        ///
        /// The consumer counts as a pending operation of `os` (see `StateRef`), so the
        /// stream cannot be destroyed under it; the stream's operations run in sequence,
        /// so it is not used elsewhere while the consumer writes to it.
        #[inline]
        pub fn new(os: &OutputStreamState<CharType>) -> Self {
            StreamCopyConsumer { _os: os.hold() }
        }
    }

    impl<CharType: ByteType> InputStreamConsumer<CharType> for StreamCopyConsumer<CharType> {
        /// Writes `data`; an empty buffer (end of stream) stops the consumption.
        fn call(&mut self, data: TemporaryBuffer<CharType>) -> Future<ConsumptionResult<CharType>> {
            if data.empty() {
                return make_ready_future(stop_consuming(data));
            }
            // SAFETY: `_os` keeps the output stream's state live (see `StateRef`); write()
            // copies the bytes before it returns, so `data`
            // may go at the end of this call.
            unsafe { (*self._os.ptr()).write(data.as_slice()) }
                .then(|()| make_ready_future(continue_consuming()))
        }
    }
}

/// Copies all the content of the input stream to the output stream.
///
/// The future fails with the first read or write error. Both streams count the copy as pending, so neither is destroyed under it.
pub fn copy<CharType: ByteType>(
    in_: &mut InputStream<CharType>,
    out: &mut OutputStream<CharType>,
) -> Future<()> {
    let consumer = internal::StreamCopyConsumer::new(out);
    in_.consume(consumer)
}

impl<CharType: ByteType> OutputStreamState<CharType> {
    /// Sends the buffered bytes, or the pending zero-copy data, and then flushes the sink.
    pub(crate) fn do_flush(&mut self) -> Future<()> {
        if self._end != 0 {
            // SAFETY: `_end <= _size`, the buffer's size.
            unsafe { self._buf.trim_unchecked(self._end) };
            self._end = 0;
            let b = std::mem::take(&mut self._buf).retype();
            let this = self.hold();
            self._fd.put_buffer(b).then(move |()| {
                // SAFETY: `this` keeps the state live (see `StateRef`).
                unsafe { (*this.ptr())._fd.flush() }
            })
        } else if self._zc_bufs.is_valid() {
            let zc = std::mem::replace(&mut self._zc_bufs, Packet::make_null_packet());
            let this = self.hold();
            self._fd.put(zc).then(move |()| {
                // SAFETY: as above.
                unsafe { (*this.ptr())._fd.flush() }
            })
        } else {
            self._fd.flush()
        }
    }

    /// Flushes the stream.
    ///
    /// Without batch flushing the data is sent now. With batch flushing the stream is
    /// registered with the reactor's flush poller and the future resolves at once,
    /// reporting the error of an earlier batched flush if there was one.
    pub fn flush(&mut self) -> Future<()> {
        if !self._batch_flushes {
            return self.do_flush();
        } else if !self._ex.is_null() {
            // A flush is a good time to deliver outstanding errors.
            return make_exception_future(std::mem::take(&mut self._ex));
        } else {
            self._flush = true;
            if self._in_batch.is_none() {
                // SAFETY: `OutputStreamState` is `repr(C)` and generic only over one-byte types, so
                // every instantiation has the layout of `OutputStreamState<u8>`; the list keeps the
                // state's root pointer, and the state stays in its node while it is in the list
                // (close() waits for the batch).
                unsafe {
                    internal::add_to_flush_poller(self.this_ptr().cast::<OutputStreamState<u8>>())
                };
                self._in_batch = Some(crate::engine::core::future::Promise::new());
            }
        }
        make_ready_future(())
    }

    /// Hands `buf` to the sink, after the flush in progress if there is one.
    pub(crate) fn put(&mut self, buf: TemporaryBuffer<CharType>) -> Future<()> {
        // If a flush is scheduled, disable it, so it does not write in parallel.
        self._flush = false;
        if self._flushing {
            let this = self.hold();
            // A flush is in progress: wait for it to end before continuing.
            match self._in_batch.as_mut() {
                Some(pr) => pr.get_future().then(move |()| {
                    // SAFETY: `this` keeps the state live (see `StateRef`).
                    unsafe { (*this.ptr())._fd.put_buffer(buf.retype()) }
                }),
                None => unreachable!("a flushing stream has its batch promise"),
            }
        } else {
            self._fd.put_buffer(buf.retype())
        }
    }

    /// Runs a batched flush; called by the reactor's flush poller.
    pub(crate) fn poll_flush(&mut self) {
        if !self._flush {
            // The flush was cancelled: nothing to do.
            self._flushing = false;
            if let Some(mut pr) = self._in_batch.take() {
                pr.set_value(());
            }
            return;
        }

        self._flush = false;
        self._flushing = true; // make whoever wants to write into the fd to wait for flush to complete

        let this = self.hold();
        // FIXME: the future is discarded.
        let _ = self.do_flush().then_wrapped(move |f: Future<()>| {
            // SAFETY: `this` keeps the state live (see `StateRef`); a stream in a batch also
            // stays alive until close() waits for the batch.
            let me = unsafe { &mut *this.ptr() };
            if let Err(e) = f.get() {
                me._ex = e;
                me._fd.on_batch_flush_error();
            }
            // If flush() was called while flushing, flush once more.
            me.poll_flush();
        });
    }

    /// Flushes the stream, then closes it and the underlying data sink to further writes.
    ///
    /// The future must be waited on before the stream is dropped. It fails with the error
    /// of the final flush, or of an earlier batched flush.
    pub fn close(&mut self) -> Future<()> {
        let (this, this2, this3) = (self.hold(), self.hold(), self.hold());
        self.flush()
            .finally(move || {
                // SAFETY: `this` keeps the state live (see `StateRef`).
                let me = unsafe { &mut *this.ptr() };
                match me._in_batch.as_mut() {
                    Some(pr) => pr.get_future(),
                    None => make_ready_future(()),
                }
            })
            .then(move |()| -> Result<(), ExceptionPtr> {
                // SAFETY: as above, with `this2`.
                let me = unsafe { &mut *this2.ptr() };
                // Report the final error as the close error.
                if !me._ex.is_null() {
                    return Err(me._ex.clone());
                }
                Ok(())
            })
            .finally(move || {
                // SAFETY: as above, with `this3`.
                unsafe { (*this3.ptr())._fd.close() }
            })
    }
}
