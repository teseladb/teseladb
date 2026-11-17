//! LZ4 compression of RPC frames, one LZ4 block per frame.
//!
//! [`Lz4Compressor`] compresses a whole frame as one LZ4 block, preceded by its
//! uncompressed size as a 32-bit little-endian integer, after the head space the caller
//! asks for. Fragmented frames are linearised first into per-thread reusable buffers
//! (`ReusableBuffers`), which are released periodically (every 100,000 uses) so a burst
//! of large frames does not pin memory. [`Lz4CompressorFactory`] negotiates it under the
//! name `LZ4`.
//!
//! The compression and decompression paths are on the RPC hot path and allocation-free
//! once the buffers have grown; the buffers are thread-local and never shared across shards.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::LazyLock;

use crate::engine::core::byteorder::{read_le, write_le};
use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
// The LZ4 block format implementation of this crate.
use crate::engine::lz4;
use crate::engine::rpc::rpc_types::{Bufs, Compressor, CompressorFactory, RcvBuf, SndBuf};

/// The LZ4 compressor of a connection.
#[derive(Default)]
pub struct Lz4Compressor;

/// The factory of `Lz4Compressor`.
#[derive(Default)]
pub struct Lz4CompressorFactory;

/// A static string: made once, on first use, then only read.
pub(crate) struct StaticSstring(LazyLock<Sstring>);

// SAFETY: the string is written once, by the `LazyLock`, and only read after; `Sstring` has no
// interior mutability, so reads from any thread are sound, and a static is never dropped, so its
// buffer is never freed on another thread.
unsafe impl Sync for StaticSstring {}

impl StaticSstring {
    /// A static string that `make` makes on first use.
    pub(crate) const fn new(make: fn() -> Sstring) -> Self {
        StaticSstring(LazyLock::new(make))
    }

    /// The string.
    #[inline]
    pub(crate) fn get(&self) -> &Sstring {
        &self.0
    }
}

static LZ4_NAME: StaticSstring = StaticSstring::new(|| Sstring::from("LZ4"));

impl CompressorFactory for Lz4CompressorFactory {
    fn supported(&self) -> &Sstring {
        LZ4_NAME.get()
    }

    fn negotiate(&self, feature: Sstring, _is_server: bool) -> Option<Box<dyn Compressor>> {
        if feature == *self.supported() { Some(Box::new(Lz4Compressor)) } else { None }
    }
}

/// A frame built from a contiguous buffer or from chunks of it.
pub(crate) trait ReservedOutput {
    /// A frame of one buffer.
    fn from_one(b: TemporaryBuffer<u8>) -> Self;
    /// A frame of several buffers.
    fn from_many(bufs: Vec<TemporaryBuffer<u8>>, size: usize) -> Self;
}

impl ReservedOutput for SndBuf {
    #[inline]
    fn from_one(b: TemporaryBuffer<u8>) -> Self {
        SndBuf::from_buffer(b)
    }
    #[inline]
    fn from_many(bufs: Vec<TemporaryBuffer<u8>>, size: usize) -> Self {
        SndBuf::from_buffers(bufs, size)
    }
}

impl ReservedOutput for RcvBuf {
    #[inline]
    fn from_one(b: TemporaryBuffer<u8>) -> Self {
        RcvBuf::from_buffer(b)
    }
    #[inline]
    fn from_many(bufs: Vec<TemporaryBuffer<u8>>, size: usize) -> Self {
        RcvBuf::from_buffers(bufs, size)
    }
}

// Reusable contiguous buffers needed for LZ4 compression and decompression functions.
pub(crate) struct ReusableBuffer {
    _data: Option<Box<[MaybeUninit<u8>]>>,
    _size: usize,
}

const _: () =
    assert!(SndBuf::CHUNK_SIZE == ReusableBuffer::CHUNK_SIZE, "snd_buf::chunk_size == chunk_size");

impl ReusableBuffer {
    const CHUNK_SIZE: usize = 128 * 1024;

    const fn new() -> Self {
        ReusableBuffer { _data: None, _size: 0 }
    }

    #[inline]
    fn data(&mut self) -> *mut u8 {
        match self._data.as_deref_mut() {
            Some(d) => d.as_mut_ptr().cast(),
            None => std::ptr::null_mut(),
        }
    }

    fn reserve(&mut self, n: usize) {
        if self._size < n {
            self._data = None;
            // Not value-initialised: every byte is written before it is read.
            self._data = Some(Box::new_uninit_slice(n));
            self._size = n;
        }
    }

    // Returns a pointer to a contiguous buffer containing all data stored in input.
    // The pointer remains valid until next call to this.
    pub(crate) fn prepare(&mut self, input: &Bufs, size: usize) -> *const u8 {
        match input {
            Bufs::One(single) => single.get(),
            Bufs::Many(fragments) => {
                self.reserve(size);
                let mut dst = self.data();
                for fragment in fragments {
                    // SAFETY: the reserved buffer holds `size` bytes, the sum of the fragments.
                    unsafe {
                        std::ptr::copy_nonoverlapping(fragment.get(), dst, fragment.size());
                        dst = dst.add(fragment.size());
                    }
                }
                self.data()
            }
        }
    }

    // Calls function fn passing to it a pointer to a temporary contigiuous max_size
    // buffer.
    // fn is supposed to return the actual size of the data.
    // with_reserved() returns an Output object (snd_buf or rcv_buf or compatible),
    // containing data that was written to the temporary buffer.
    // Output should be either snd_buf or rcv_buf.
    pub(crate) fn with_reserved<Output: ReservedOutput>(
        &mut self,
        max_size: usize,
        fn_: impl FnOnce(*mut u8) -> Result<usize, ExceptionPtr>,
    ) -> Result<Output, ExceptionPtr> {
        // The buffer is picked first and `fn` called once, so that it is inlined (a closure
        // called from two places is not).
        let small = max_size <= Self::CHUNK_SIZE;
        let mut dst = if small {
            TemporaryBuffer::<u8>::new(max_size)
        } else {
            self.reserve(max_size);
            TemporaryBuffer::empty_buffer()
        };
        let data = if small { dst.get_write() } else { self.data() };
        let dst_size = fn_(data)?;
        if small {
            dst.trim(dst_size);
            return Ok(Output::from_one(dst));
        }
        if dst_size <= Self::CHUNK_SIZE {
            // SAFETY: fn wrote dst_size bytes at the start of the reserved buffer.
            return Ok(Output::from_one(TemporaryBuffer::copy_from(unsafe {
                std::slice::from_raw_parts(data, dst_size)
            })));
        }
        let mut left = dst_size;
        let mut pos = data;
        let mut buffers = Vec::new();
        while left != 0 {
            let this_size = left.min(Self::CHUNK_SIZE);
            let mut b = TemporaryBuffer::<u8>::new(this_size);
            // SAFETY: `pos` walks the dst_size bytes fn wrote; `b` holds this_size bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(pos, b.get_write(), this_size);
                pos = pos.add(this_size);
            }
            buffers.push(b);
            left -= this_size;
        }
        Ok(Output::from_many(buffers, dst_size))
    }

    fn clear(&mut self) {
        self._data = None;
        self._size = 0;
    }
}

/// The reusable buffers of a thread.
pub(crate) struct ReusableBuffers {
    pub(crate) compressed_data: ReusableBuffer,
    pub(crate) decompressed_data: ReusableBuffer,
    buffer_use_count: usize,
}

thread_local! {
    static REUSABLE_BUFFERS: UnsafeCell<ReusableBuffers> = const {
        UnsafeCell::new(ReusableBuffers {
            compressed_data: ReusableBuffer::new(),
            decompressed_data: ReusableBuffer::new(),
            buffer_use_count: 0,
        })
    };
}

const DROP_BUFFERS_TRIGGER: usize = 100_000;

/// Runs `f` with the reusable buffers of this thread.
#[inline]
pub(crate) fn with_reusable_buffers<R>(f: impl FnOnce(&mut ReusableBuffers) -> R) -> R {
    // SAFETY: the buffers are thread-local and the compressors do not reenter, so this is
    // the only reference to them while `f` runs.
    REUSABLE_BUFFERS.with(|b| f(unsafe { &mut *b.get() }))
}

impl lz4::pieces::Piece for TemporaryBuffer<u8> {
    #[inline]
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

/// `decompress` of a frame of several buffers: what the reference does after copying them into
/// one reusable buffer, on the buffers as they are (`lz4::pieces`), so the frame is not copied
/// before it is decoded. The output, the errors and the buffer accounting are the same.
fn decompress_fragments(
    fragments: &[TemporaryBuffer<u8>],
    size: usize,
) -> Result<RcvBuf, ExceptionPtr> {
    let failure = |what: &'static str| {
        ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(what))
    };
    let mut head = [0u8; 4];
    if !lz4::pieces::gather(fragments, 0, &mut head) {
        return Err(failure("RPC frame LZ4 decompression failure"));
    }
    let dst_size = u32::from_le_bytes(head);
    if dst_size == 0 {
        return Err(failure(
            "RPC frame LZ4 decompression failure: decompressed size cannot be zero",
        ));
    }
    let src_size = size - size_of::<u32>();
    with_reusable_buffers(|b| {
        let dst =
            b.compressed_data.with_reserved::<RcvBuf>(dst_size as usize, |dst: *mut u8| {
                // SAFETY: `dst` holds dst_size bytes and is none of the fragments.
                let decoded = unsafe {
                    lz4::pieces::decompress_safe_pieces(
                        fragments,
                        size_of::<u32>(),
                        src_size,
                        dst,
                        dst_size as usize,
                    )
                };
                if decoded.is_err() {
                    return Err(failure("RPC frame LZ4 decompression failure"));
                }
                Ok(dst_size as usize)
            })?;
        after_buffer_use(b);
        Ok(dst)
    })
}

#[inline]
fn after_buffer_use(b: &mut ReusableBuffers) {
    let c = b.buffer_use_count;
    b.buffer_use_count += 1;
    if c == DROP_BUFFERS_TRIGGER {
        drop_buffers(b);
    }
}

/// The body of the `if` of `after_buffer_use`, once every 100,000 uses: out of the line of the
/// frames, so that the count is all they run.
#[cold]
#[inline(never)]
fn drop_buffers(b: &mut ReusableBuffers) {
    b.compressed_data.clear();
    b.decompressed_data.clear();
    b.buffer_use_count = 0;
}

impl Compressor for Lz4Compressor {
    // compress data, leaving head_space empty in returned buffer
    fn compress(&mut self, head_space: usize, data: SndBuf) -> Result<SndBuf, ExceptionPtr> {
        let head_space = head_space + 4;
        let dst_size = head_space + lz4::compress_bound(data.size as usize);
        with_reusable_buffers(|b| {
            let ReusableBuffers { compressed_data, decompressed_data, .. } = b;
            let dst = compressed_data.with_reserved::<SndBuf>(dst_size, |dst: *mut u8| {
                let src_size = data.size as usize;
                let src = decompressed_data.prepare(&data.bufs, data.size as usize);
                // SAFETY: `src` holds src_size bytes and `dst` has room for head_space plus
                // the bound.
                let size = unsafe {
                    lz4::compress_default(
                        src,
                        dst.add(head_space),
                        src_size,
                        lz4::compress_bound(src_size),
                    )
                };
                if size == 0 {
                    return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                        "RPC frame LZ4 compression failure",
                    )));
                }
                // SAFETY: the 4 bytes before the payload are inside dst.
                unsafe { write_le::<u32>(dst.add(head_space - 4), src_size as u32) };
                Ok(size + head_space)
            })?;
            after_buffer_use(b);
            Ok(dst)
        })
    }

    // decompress data
    fn decompress(&mut self, data: RcvBuf) -> Result<RcvBuf, ExceptionPtr> {
        if data.size < 4 {
            return Ok(RcvBuf::default());
        }
        if let Bufs::Many(fragments) = &data.bufs {
            return decompress_fragments(fragments, data.size as usize);
        }
        with_reusable_buffers(|b| {
            let ReusableBuffers { compressed_data, decompressed_data, .. } = b;
            let mut src_size = data.size as usize;
            let mut src = decompressed_data.prepare(&data.bufs, data.size as usize);
            // SAFETY: the frame has at least 4 bytes.
            let dst_size = unsafe { read_le::<u32>(src) };
            if dst_size == 0 {
                return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                    "RPC frame LZ4 decompression failure: decompressed size cannot be zero",
                )));
            }
            // SAFETY: the frame has at least 4 bytes.
            src = unsafe { src.add(size_of::<u32>()) };
            src_size -= size_of::<u32>();
            let dst =
                compressed_data.with_reserved::<RcvBuf>(dst_size as usize, |dst: *mut u8| {
                    // SAFETY: `src` holds src_size bytes and `dst` dst_size bytes.
                    if unsafe { lz4::decompress_safe(src, dst, src_size, dst_size as usize) }
                        .is_err()
                    {
                        return Err(ExceptionPtr::new(
                            crate::engine::core::future::RuntimeError::new(
                                "RPC frame LZ4 decompression failure",
                            ),
                        ));
                    }
                    Ok(dst_size as usize)
                })?;
            after_buffer_use(b);
            Ok(dst)
        })
    }

    fn name(&self) -> Sstring {
        Lz4CompressorFactory.supported().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(bufs: &Bufs) -> Vec<u8> {
        let one = |b: &TemporaryBuffer<u8>| {
            // SAFETY: a buffer holds `size()` bytes at `get()`.
            unsafe { std::slice::from_raw_parts(b.get(), b.size()) }.to_vec()
        };
        match bufs {
            Bufs::One(b) => one(b),
            Bufs::Many(v) => v.iter().flat_map(one).collect(),
        }
    }

    /// Frames below and above a chunk of 128 KiB, in one buffer or several, come back.
    #[test]
    fn frames_come_back_in_one_buffer_or_several() {
        let mut c = Lz4Compressor;
        for len in [1usize, 1000, 128 * 1024 + 1, 300_000] {
            let data: Vec<u8> =
                (0..len).map(|i| b"lz4 frame "[i % 10] ^ (i / 7919) as u8).collect();
            let frames = [
                SndBuf::from_buffer(TemporaryBuffer::copy_from(&data)),
                SndBuf::from_buffers(
                    data.chunks(50_000).map(TemporaryBuffer::copy_from).collect(),
                    data.len(),
                ),
            ];
            for frame in frames {
                let compressed = c.compress(8, frame).unwrap();
                let body = bytes(&compressed.bufs)[8..].to_vec();
                // In buffers of any size, the size in front cut too.
                for piece in [70_000, 1000, 3] {
                    let back = c.decompress(RcvBuf::from_buffers(
                        body.chunks(piece).map(TemporaryBuffer::copy_from).collect(),
                        body.len(),
                    ));
                    assert_eq!(bytes(&back.unwrap().bufs), data);
                }
            }
        }
        // A size of 0 and a damaged block are refused.
        let zero = RcvBuf::from_buffer(TemporaryBuffer::copy_from(&[0, 0, 0, 0, 0]));
        assert!(c.decompress(zero).is_err());
        let damaged = RcvBuf::from_buffer(TemporaryBuffer::copy_from(&[9, 0, 0, 0, 0x90]));
        assert!(c.decompress(damaged).is_err());
        let in_two = |a: &[u8], b: &[u8]| {
            RcvBuf::from_buffers(
                vec![TemporaryBuffer::copy_from(a), TemporaryBuffer::copy_from(b)],
                a.len() + b.len(),
            )
        };
        assert!(c.decompress(in_two(&[9, 0], &[0, 0, 0x90])).is_err());
        assert!(c.decompress(in_two(&[0, 0], &[0, 0, 0])).is_err());
    }
}
