//! LZ4 compression of RPC frames in independent-size chunks with a streaming dictionary.
//!
//! [`Lz4FragmentedCompressor`] (negotiated as `LZ4_FRAGMENTED`) compresses a frame in
//! chunks of 32 KiB of input with the LZ4 streaming API, so each chunk uses the previous
//! ones as its dictionary and a frame never has to be linearised. The wire format is
//! described next to the chunk constants below. Decompression uses a ring buffer of the
//! size LZ4 requires for chunks of this size. Like [`Lz4Compressor`](crate::engine::rpc::lz4_compressor::Lz4Compressor),
//! it is on the RPC hot path and keeps its streams per thread.

use std::cell::UnsafeCell;

use crate::engine::core::byteorder::{read_le, write_le};
use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
// The LZ4 block and streaming implementation of this crate.
use crate::engine::lz4::{Lz4Stream, Lz4StreamDecode, compress_bound as lz4_compressbound};
use crate::engine::rpc::lz4_compressor::StaticSstring;
use crate::engine::rpc::rpc_types::{Bufs, Compressor, CompressorFactory, RcvBuf, SndBuf};
use crate::engine_assert;

// The ring buffer size the LZ4 streaming decoder needs for a given maximum block size.
use crate::engine::lz4::decoder_ring_buffer_size as lz4_decoder_ring_buffer_size;

/// The LZ4 compressor of a connection, in 32 kB chunks.
#[derive(Default)]
pub struct Lz4FragmentedCompressor;

/// The factory of `Lz4FragmentedCompressor`.
#[derive(Default)]
pub struct Lz4FragmentedCompressorFactory;

static NAME: StaticSstring = StaticSstring::new(|| Sstring::from("LZ4_FRAGMENTED"));

impl CompressorFactory for Lz4FragmentedCompressorFactory {
    fn supported(&self) -> &Sstring {
        NAME.get()
    }

    fn negotiate(&self, feature: Sstring, _is_server: bool) -> Option<Box<dyn Compressor>> {
        if feature == *self.supported() { Some(Box::new(Lz4FragmentedCompressor)) } else { None }
    }
}

// Compressed message format:
// The message consists of one or more data chunks each preceeded by a 4 byte header.
// The value of the header detrmines the size of the chunk:
// - most significant bit is cleared: intermediate chunk, 31 least significant bits
//   contain the compressed size of the chunk (i.e. how it appears on wire), the
//   decompressed size is 32 kB.
// - most significant bit is set: last chunk, 31 least significant bits contain the
//   decompressed size of the chunk, the compressed size is the remaining part of
//   the message.
// Compression and decompression is done using LZ4 streaming interface. Each chunk
// depends on the one that precedes it.
// All metadata is little-endian.

const LAST_CHUNK_FLAG: u32 = 1u32 << 31;
const CHUNK_HEADER_SIZE: usize = size_of::<u32>();
const CHUNK_SIZE: usize = 32 * 1024;

// The streams are boxed: a `Box` frees the stream it holds.
type CompressionStream = Box<Lz4Stream>;

type DecompressionStream = Box<Lz4StreamDecode>;

const _: () = assert!(CHUNK_SIZE <= SndBuf::CHUNK_SIZE, "chunk_size <= snd_buf::chunk_size");

const CHUNK_COMPRESS_BOUND: usize = lz4_compressbound(CHUNK_SIZE);
const CHUNK_MAXIMUM_COMPRESSED_SIZE: usize = CHUNK_COMPRESS_BOUND + CHUNK_HEADER_SIZE;
const _: () = assert!(
    CHUNK_MAXIMUM_COMPRESSED_SIZE < SndBuf::CHUNK_SIZE,
    "chunk_maximum_compressed_size is too large"
);

// Input chunks do not have to be multiplies of chunk_size.
// We handle such cases by reassembling a chunk in this temporary buffer.
// Note that this is similar to the ring buffer compression case in docs,
// we need to ensure that a suitable amount of (maybe) previous data is
// stable in this or input buffer, thus make the temp buffer
// LZ4_DECODER_RING_BUFFER_SIZE(chunk_size) large, and treat it as a ring.
const LIN_BUF_SIZE: usize = lz4_decoder_ring_buffer_size(CHUNK_SIZE);

struct CompressState {
    stream: CompressionStream,
    temporary_chunk_data: Box<[u8; LIN_BUF_SIZE]>,
}

thread_local! {
    static COMPRESS_STATE: UnsafeCell<CompressState> = UnsafeCell::new(CompressState {
        stream: Box::new(Lz4Stream::new()),
        // SAFETY: zeroed bytes are a valid `[u8; N]`; made on the heap, not copied from the stack.
        temporary_chunk_data: unsafe { Box::<[u8; LIN_BUF_SIZE]>::new_zeroed().assume_init() },
    });
    static DECOMPRESS_STATE: UnsafeCell<(DecompressionStream, TemporaryBuffer<u8>)> = UnsafeCell::new((
        Box::new(Lz4StreamDecode::new()),
        // Let's be a bit paranoid and not assume that the remote has the same
        // LZ4_COMPRESSBOUND as we do and allow any compressed chunk size.
        TemporaryBuffer::new(lz4_compressbound(CHUNK_SIZE)),
    ));
}

/// The fragments of a frame, walked through a raw pointer.
#[inline(always)]
fn fragments(bufs: &Bufs) -> *const TemporaryBuffer<u8> {
    match bufs {
        Bufs::One(b) => b,
        Bufs::Many(v) => v.as_ptr(),
    }
}

impl Lz4FragmentedCompressor {
    fn compress_with(st: &mut CompressState, head_space: usize, data: &SndBuf) -> SndBuf {
        let stream = &mut *st.stream;
        stream.reset_stream();

        let size_left = data.size as usize;
        let mut src = fragments(&data.bufs);

        // SAFETY: `src` walks the fragments of `data`, whose sizes add up to
        // data.size, and every write stays inside the destination buffers sized above it.
        unsafe {
            let single_chunk_size = lz4_compressbound(size_left) + head_space + CHUNK_HEADER_SIZE;
            if single_chunk_size <= CHUNK_SIZE
                && size_left <= CHUNK_SIZE
                && (*src).size() == size_left
            {
                // faster path for small messages
                let mut dst = TemporaryBuffer::<u8>::new(single_chunk_size);
                let header = dst.get_write().add(head_space);
                let compressed_data = header.add(CHUNK_HEADER_SIZE);
                let compressed_size = stream.compress_fast_continue(
                    (*src).get(),
                    compressed_data,
                    size_left,
                    lz4_compressbound(size_left),
                    0,
                );
                write_le::<u32>(header, LAST_CHUNK_FLAG | size_left as u32);
                dst.trim(head_space + CHUNK_HEADER_SIZE + compressed_size);
                return SndBuf::from_buffer(dst);
            }

            let mut dst_buffers: Vec<TemporaryBuffer<u8>> = Vec::new();
            let mut dst_offset = head_space;
            dst_buffers.push(TemporaryBuffer::new(head_space.max(SndBuf::CHUNK_SIZE)));

            // Intermediate chunks
            let mut total_compressed_size: usize = 0;
            let mut src_left = data.size as usize;
            let mut src_current_offset: usize = 0;

            // Advance offset in the current source fragment, move to the next fragment if needed.
            let advance_src = |src: &mut *const TemporaryBuffer<u8>,
                               src_current_offset: &mut usize,
                               src_left: &mut usize,
                               n: usize| {
                *src_current_offset += n;
                if *src_current_offset >= (**src).size() {
                    *src = src.add(1);
                    *src_current_offset = 0;
                }
                // The last chunk advances by chunk_size even when less is left (maybe_linearize
                // below): src_left is not read again after the last chunk, so the subtraction
                // wraps instead of overflowing.
                *src_left = src_left.wrapping_sub(n);
            };

            let temporary_chunk_data = st.temporary_chunk_data.as_mut_ptr();
            let mut lin_off: usize = 0;
            let maybe_linearize = |src: &mut *const TemporaryBuffer<u8>,
                                   src_current_offset: &mut usize,
                                   src_left: &mut usize,
                                   lin_off: &mut usize,
                                   size: usize|
             -> *const u8 {
                let mut src_ptr = (**src).get().add(*src_current_offset);
                if (**src).size() - *src_current_offset < size {
                    let mut left = size;
                    engine_assert!(LIN_BUF_SIZE > size);
                    if LIN_BUF_SIZE - *lin_off < size {
                        *lin_off = 0;
                    }
                    let mut tmp =
                        temporary_chunk_data.add(std::mem::replace(lin_off, *lin_off + size));
                    src_ptr = tmp;
                    while left != 0 {
                        let this_size = ((**src).size() - *src_current_offset).min(left);
                        std::ptr::copy_nonoverlapping(
                            (**src).get().add(*src_current_offset),
                            tmp,
                            this_size,
                        );
                        tmp = tmp.add(this_size);
                        left -= this_size;
                        advance_src(src, src_current_offset, src_left, this_size);
                    }
                } else {
                    advance_src(src, src_current_offset, src_left, CHUNK_SIZE);
                    *lin_off = 0;
                }
                src_ptr
            };

            while src_left > CHUNK_SIZE {
                // Check if we can fit another chunk in the current destination fragment.
                // If not allocate a new one.
                if dst_offset + CHUNK_MAXIMUM_COMPRESSED_SIZE
                    > dst_buffers.last().map_or(0, |b| b.size())
                {
                    if let Some(b) = dst_buffers.last_mut() {
                        b.trim(dst_offset);
                    }
                    dst_buffers.push(TemporaryBuffer::new(SndBuf::CHUNK_SIZE));
                    dst_offset = 0;
                }

                // Check if there is at least a contiguous chunk_size of data in the current
                // source fragment. If not, linearise that into temporary_chunk_data.
                let src_ptr = maybe_linearize(
                    &mut src,
                    &mut src_current_offset,
                    &mut src_left,
                    &mut lin_off,
                    CHUNK_SIZE,
                );
                let last = dst_buffers.len() - 1;
                let header = dst_buffers[last].get_write().add(dst_offset);
                let dst = header.add(CHUNK_HEADER_SIZE);
                let compressed_size = stream.compress_fast_continue(
                    src_ptr,
                    dst,
                    CHUNK_SIZE,
                    CHUNK_COMPRESS_BOUND,
                    0,
                );
                total_compressed_size += compressed_size + CHUNK_HEADER_SIZE;
                dst_offset += compressed_size + CHUNK_HEADER_SIZE;
                write_le::<u32>(header, compressed_size as u32);
            }

            // Last chunk
            let last_chunk_compress_bound = lz4_compressbound(src_left);
            let last_chunk_maximum_compressed_size = last_chunk_compress_bound + CHUNK_HEADER_SIZE;
            // Check if we can fit the last chunk in the current destination fragment. Allocate a new one if not.
            if dst_offset + last_chunk_maximum_compressed_size
                > dst_buffers.last().map_or(0, |b| b.size())
            {
                if let Some(b) = dst_buffers.last_mut() {
                    b.trim(dst_offset);
                }
                dst_buffers.push(TemporaryBuffer::new(SndBuf::CHUNK_SIZE));
                dst_offset = 0;
            }
            let last = dst_buffers.len() - 1;
            let header = dst_buffers[last].get_write().add(dst_offset);
            let dst = header.add(CHUNK_HEADER_SIZE);
            // Check if all remaining source data is contiguous. If not linearise it into temporary_chunk_data.
            let rem = src_left;
            let src_ptr = maybe_linearize(
                &mut src,
                &mut src_current_offset,
                &mut src_left,
                &mut lin_off,
                rem,
            );
            let compressed_size =
                stream.compress_fast_continue(src_ptr, dst, rem, last_chunk_compress_bound, 0);
            dst_offset += compressed_size + CHUNK_HEADER_SIZE;
            write_le::<u32>(header, LAST_CHUNK_FLAG | rem as u32);
            total_compressed_size += compressed_size + CHUNK_HEADER_SIZE + head_space;

            dst_buffers[last].trim(dst_offset);
            if dst_buffers.len() == 1 {
                return SndBuf::from_buffer(dst_buffers.swap_remove(0));
            }
            SndBuf::from_buffers(dst_buffers, total_compressed_size)
        }
    }

    fn decompress_with(
        stream: &mut Lz4StreamDecode,
        chunk_buffer: &mut TemporaryBuffer<u8>,
        data: &RcvBuf,
    ) -> Result<RcvBuf, ExceptionPtr> {
        if !stream.set_stream_decode(std::ptr::null(), 0) {
            return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                "RPC frame LZ4_FRAGMENTED decompression failed to reset state",
            )));
        }
        let single = matches!(data.bufs, Bufs::One(_));
        let mut src = fragments(&data.bufs);
        let mut src_left = data.size as usize;
        let mut src_offset: usize = 0;

        // SAFETY: `src` walks the fragments of `data`, whose sizes add up to
        // data.size; the destination writes stay inside the buffers get_dest() hands out.
        unsafe {
            // Prepare source data. Returns pointer to n contiguous bytes of source data.
            // Avoids copy if possible, otherwise uses dst as a temporary storage.
            // (None when fewer than n bytes are left: a header of a damaged frame that says more
            // than the frame has, so nothing is read past the end of the frame.)
            let copy_src = |src: &mut *const TemporaryBuffer<u8>,
                            src_offset: &mut usize,
                            src_left: &mut usize,
                            mut dst: *mut u8,
                            mut n: usize|
             -> Option<*const u8> {
                if n > *src_left {
                    return None;
                }
                // Fast path, no need to copy anything.
                if (**src).size() - *src_offset >= n {
                    let ptr = (**src).get().add(*src_offset);
                    *src_left -= n;
                    *src_offset += n;
                    return Some(ptr);
                }
                // Need to linearise source chunk into dst.
                let ptr = dst;
                *src_left -= n;
                while n != 0 {
                    if *src_offset == (**src).size() {
                        *src = src.add(1);
                        *src_offset = 0;
                    }
                    let this_size = n.min((**src).size() - *src_offset);
                    std::ptr::copy_nonoverlapping((**src).get().add(*src_offset), dst, this_size);
                    n -= this_size;
                    dst = dst.add(this_size);
                    *src_offset += this_size;
                }
                Some(ptr)
            };

            // Read, possibly fragmented, header.
            let read_header = |src: &mut *const TemporaryBuffer<u8>,
                               src_offset: &mut usize,
                               src_left: &mut usize|
             -> Option<u32> {
                let mut header_value = [0u8; 4];
                let hp = header_value.as_mut_ptr();
                let ptr = copy_src(src, src_offset, src_left, hp, CHUNK_HEADER_SIZE)?;
                if !std::ptr::eq(ptr, hp) {
                    std::ptr::copy_nonoverlapping(ptr, hp, size_of::<u32>());
                }
                Some(read_le::<u32>(header_value.as_ptr()))
            };
            let cut_short = |at: usize| {
                ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(format!(
                    "RPC frame LZ4_FRAGMENTED decompression failure (long, at {at} bytes)"
                )))
            };

            if single {
                let mut header = read_le::<u32>((*src).get());
                if header & LAST_CHUNK_FLAG != 0 {
                    // faster path for small messages: single chunk in a single buffer
                    header &= !LAST_CHUNK_FLAG;
                    src_offset += CHUNK_HEADER_SIZE;
                    src_left -= CHUNK_HEADER_SIZE;
                    let mut dst = TemporaryBuffer::<u8>::new(header as usize);
                    if stream
                        .decompress_safe_continue(
                            (*src).get().add(src_offset),
                            dst.get_write(),
                            src_left,
                            header as usize,
                        )
                        .is_err()
                    {
                        return Err(ExceptionPtr::new(
                            crate::engine::core::future::RuntimeError::new(
                                "RPC frame LZ4_FRAGMENTED decompression failure (short)",
                            ),
                        ));
                    }
                    return Ok(RcvBuf::from_buffer(dst));
                }
                // not eligible for fast path: multiple chunks in a single buffer
            }
            // else: not eligible for fast path: multiple buffers

            let mut dst_buffers: Vec<TemporaryBuffer<u8>> = Vec::new();
            let mut total_size: usize = 0;

            // Decompressing requires either dest to be fully split or
            // "preserved" in 64KB or larger, depending on how it was
            // compressed. If not, decompression will fail, typically
            // on text-like constructs. Making our dest buffers 64K
            // ensures we retain a suitable dictionary region for all
            // passes.
            const BUF_SIZE: usize = 64 * 1024;
            let mut dst_offset: usize = 0;
            let get_dest = |dst_buffers: &mut Vec<TemporaryBuffer<u8>>,
                            dst_offset: &mut usize,
                            size: usize|
             -> *mut u8 {
                if dst_buffers.is_empty() {
                    dst_buffers.push(TemporaryBuffer::new(BUF_SIZE));
                }
                let last = dst_buffers.len() - 1;
                if dst_buffers[last].size() - *dst_offset < size {
                    dst_buffers[last].trim(*dst_offset);
                    dst_buffers.push(TemporaryBuffer::new(BUF_SIZE));
                    *dst_offset = 0;
                }
                let last = dst_buffers.len() - 1;
                dst_buffers[last].get_write().add(std::mem::replace(dst_offset, *dst_offset + size))
            };

            // Intermediate chunks
            let mut header_value = read_header(&mut src, &mut src_offset, &mut src_left)
                .ok_or_else(|| cut_short(0))?;
            while header_value & LAST_CHUNK_FLAG == 0 {
                total_size += CHUNK_SIZE;
                if chunk_buffer.size() < header_value as usize {
                    *chunk_buffer = TemporaryBuffer::new(header_value as usize);
                }
                let src_ptr = copy_src(
                    &mut src,
                    &mut src_offset,
                    &mut src_left,
                    chunk_buffer.get_write(),
                    header_value as usize,
                )
                .ok_or_else(|| cut_short(total_size - CHUNK_SIZE))?;
                let dst = get_dest(&mut dst_buffers, &mut dst_offset, CHUNK_SIZE);
                if stream
                    .decompress_safe_continue(
                        src_ptr,
                        /*dst_buffers.back().get_write()*/ dst,
                        header_value as usize,
                        CHUNK_SIZE,
                    )
                    .is_err()
                {
                    return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                        format!(
                            "RPC frame LZ4_FRAGMENTED decompression failure (long, at {} bytes)",
                            total_size - CHUNK_SIZE
                        ),
                    )));
                }
                header_value = read_header(&mut src, &mut src_offset, &mut src_left)
                    .ok_or_else(|| cut_short(total_size))?;
            }

            // Last chunk
            header_value &= !LAST_CHUNK_FLAG;
            total_size += header_value as usize;
            let dst = get_dest(&mut dst_buffers, &mut dst_offset, header_value as usize);
            if chunk_buffer.size() < src_left {
                *chunk_buffer = TemporaryBuffer::new(src_left);
            }
            let last_chunk_compressed_size = src_left;
            let n = src_left;
            let src_ptr =
                copy_src(&mut src, &mut src_offset, &mut src_left, chunk_buffer.get_write(), n)
                    .ok_or_else(|| cut_short(total_size - header_value as usize))?;
            if stream
                .decompress_safe_continue(
                    src_ptr,
                    /*dst_buffers.back().get_write()*/ dst,
                    last_chunk_compressed_size,
                    header_value as usize,
                )
                .is_err()
            {
                return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                    format!(
                        "RPC frame LZ4_FRAGMENTED decompression failure (long, last frame, at {} bytes)",
                        total_size - header_value as usize
                    ),
                )));
            }
            let last = dst_buffers.len() - 1;
            dst_buffers[last].trim(dst_offset);
            if dst_buffers.len() == 1 {
                return Ok(RcvBuf::from_buffer(dst_buffers.swap_remove(0)));
            }
            Ok(RcvBuf::from_buffers(dst_buffers, total_size))
        }
    }
}

impl Compressor for Lz4FragmentedCompressor {
    fn compress(&mut self, head_space: usize, data: SndBuf) -> Result<SndBuf, ExceptionPtr> {
        // SAFETY: the state is thread-local and compress() does not reenter.
        Ok(COMPRESS_STATE
            .with(|s| Self::compress_with(unsafe { &mut *s.get() }, head_space, &data)))
    }

    fn decompress(&mut self, data: RcvBuf) -> Result<RcvBuf, ExceptionPtr> {
        if data.size < 4 {
            return Ok(RcvBuf::default());
        }
        DECOMPRESS_STATE.with(|s| {
            // SAFETY: the state is thread-local and decompress() does not reenter.
            let (stream, chunk_buffer) = unsafe { &mut *s.get() };
            Self::decompress_with(stream, chunk_buffer, &data)
        })
    }

    fn name(&self) -> Sstring {
        Lz4FragmentedCompressorFactory.supported().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Documents and noise, so that the chunks find matches in the ones before.
    fn payload(len: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9u32;
        let mut v = Vec::with_capacity(len);
        let mut i = 0u32;
        while v.len() < len {
            v.extend_from_slice(
                format!("{{\"_id\":{i},\"name\":\"user{}\",\"v\":", i % 97).as_bytes(),
            );
            for _ in 0..1 + i % 13 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(x as u8);
            }
            v.extend_from_slice(b"}\n");
            i += 1;
        }
        v.truncate(len);
        v
    }

    /// `data` in buffers of the lengths of `pieces`, taken in turn (the last takes the rest).
    fn split(data: &[u8], pieces: &[usize]) -> Vec<TemporaryBuffer<u8>> {
        let mut out = Vec::new();
        let mut at = 0;
        for &n in pieces.iter().cycle() {
            if at == data.len() {
                return out;
            }
            let n = n.min(data.len() - at);
            out.push(TemporaryBuffer::copy_from(&data[at..at + n]));
            at += n;
        }
        unreachable!()
    }

    /// The bytes of the buffers of a frame.
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

    fn snd(data: &[u8], pieces: &[usize]) -> SndBuf {
        let mut bufs = split(data, pieces);
        if bufs.len() == 1 {
            return SndBuf::from_buffer(bufs.swap_remove(0));
        }
        SndBuf::from_buffers(bufs, data.len())
    }

    fn rcv(data: &[u8], pieces: &[usize]) -> RcvBuf {
        let mut bufs = split(data, pieces);
        if bufs.len() == 1 {
            return RcvBuf::from_buffer(bufs.swap_remove(0));
        }
        RcvBuf::from_buffers(bufs, data.len())
    }

    /// Frames of several chunks of 32 KiB and a partial last one, sent in one buffer or in
    /// fragments that cut the chunks anywhere, come back with each head space and in any split
    /// of the compressed frame.
    #[test]
    fn frames_of_several_chunks_and_a_partial_last_one_come_back() {
        let mut c = Lz4FragmentedCompressor;
        for len in [100, CHUNK_SIZE, CHUNK_SIZE + 1, 3 * CHUNK_SIZE + 1000, 200_000, 400_000] {
            let data = payload(len);
            for pieces in [&[usize::MAX][..], &[40_000], &[CHUNK_SIZE], &[7, 70_000, 1, 5000]] {
                for head_space in [0, 16] {
                    let frame = c.compress(head_space, snd(&data, pieces)).unwrap();
                    let compressed = bytes(&frame.bufs);
                    assert_eq!(frame.size as usize, compressed.len(), "{len} {pieces:?}");
                    let body = &compressed[head_space..];
                    for split_at in [&[usize::MAX][..], &[3], &[50_000, 2]] {
                        let back = c.decompress(rcv(body, split_at)).unwrap();
                        assert_eq!(back.size as usize, data.len(), "{len} {pieces:?} {split_at:?}");
                        assert_eq!(bytes(&back.bufs), data, "{len} {pieces:?} {split_at:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_damaged_frame_is_refused() {
        let mut c = Lz4FragmentedCompressor;
        let data = payload(3 * CHUNK_SIZE + 1000);
        let body = bytes(&c.compress(0, snd(&data, &[usize::MAX])).unwrap().bufs);
        for at in (0..body.len()).step_by(997) {
            let mut bad = body.clone();
            bad[at] ^= 0x40;
            if let Ok(back) = c.decompress(rcv(&bad, &[usize::MAX])) {
                assert_eq!(back.size as usize, data.len());
            }
        }
        // The header of the first chunk says it is longer than the frame.
        let mut bad = body.clone();
        bad[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
        assert!(c.decompress(rcv(&bad, &[usize::MAX])).is_err());
    }
}
