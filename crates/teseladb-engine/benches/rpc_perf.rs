//! Microbenchmarks of the RPC compressors: compressing and decompressing small and large,
//! random and zeroed buffers with each compressor.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/perf_tests.rs"]
mod perf_tests;

#[path = "../tests/common/minstd.rs"]
mod minstd;

use minstd::UniformIntDistribution;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::rpc::lz4_compressor::Lz4Compressor;
use teseladb::engine::rpc::lz4_fragmented_compressor::Lz4FragmentedCompressor;
use teseladb::engine::rpc::rpc_types::{Bufs, Compressor, RcvBuf, SndBuf};
use teseladb::engine::testing::perf_tests::perf_tests::do_not_optimize;
use teseladb::engine::testing::random::testing::local_random;

const SMALL_BUFFER_SIZE: usize = 128;
const LARGE_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// The fixture of the RPC compression benchmarks: a compressor and the small and large
/// buffers, random and all-zero, it compresses and decompresses.
pub struct Compression<C: Compressor + Default> {
    compressor: C,
    small_buffer_random: TemporaryBuffer<u8>,
    small_buffer_zeroes: TemporaryBuffer<u8>,
    large_buffer_random: Vec<TemporaryBuffer<u8>>,
    large_buffer_zeroes: Vec<TemporaryBuffer<u8>>,
    small_compressed_buffer_random: Vec<TemporaryBuffer<u8>>,
    small_compressed_buffer_zeroes: Vec<TemporaryBuffer<u8>>,
    large_compressed_buffer_random: Vec<TemporaryBuffer<u8>>,
    large_compressed_buffer_zeroes: Vec<TemporaryBuffer<u8>>,
}

// SAFETY: a bench fixture lives and runs on one shard.
unsafe impl<C: Compressor + Default> Send for Compression<C> {}

impl<C: Compressor + Default> Compression<C> {
    fn get_rcv_buf(input: &mut [TemporaryBuffer<u8>]) -> RcvBuf {
        if input.len() == 1 {
            return RcvBuf::from_buffer(input[0].share());
        }
        let mut bufs = Vec::new();
        let mut total_size = 0usize;
        for buf in input.iter_mut() {
            bufs.push(buf.share());
            total_size += buf.size();
        }
        RcvBuf::from_buffers(bufs, total_size)
    }

    fn get_snd_buf_vec(input: &mut [TemporaryBuffer<u8>]) -> SndBuf {
        let mut bufs = Vec::new();
        let mut total_size = 0usize;
        for buf in input.iter_mut() {
            bufs.push(buf.share());
            total_size += buf.size();
        }
        SndBuf::from_buffers(bufs, total_size)
    }

    fn get_snd_buf(input: &mut TemporaryBuffer<u8>) -> SndBuf {
        SndBuf::from_buffer(input.share())
    }

    /// The buffers of a compressed `snd_buf`, one or many.
    fn into_vec(rcv: SndBuf) -> Vec<TemporaryBuffer<u8>> {
        match rcv.bufs {
            Bufs::One(buffer) => vec![buffer],
            Bufs::Many(v) => v,
        }
    }

    fn compressor(&mut self) -> &mut C {
        &mut self.compressor
    }

    fn small_buffer_random(&mut self) -> SndBuf {
        Self::get_snd_buf(&mut self.small_buffer_random)
    }
    fn small_buffer_zeroes(&mut self) -> SndBuf {
        Self::get_snd_buf(&mut self.small_buffer_zeroes)
    }
    fn large_buffer_random(&mut self) -> SndBuf {
        Self::get_snd_buf_vec(&mut self.large_buffer_random)
    }
    fn large_buffer_zeroes(&mut self) -> SndBuf {
        Self::get_snd_buf_vec(&mut self.large_buffer_zeroes)
    }
    fn small_compressed_buffer_random(&mut self) -> RcvBuf {
        Self::get_rcv_buf(&mut self.small_compressed_buffer_random)
    }
    fn small_compressed_buffer_zeroes(&mut self) -> RcvBuf {
        Self::get_rcv_buf(&mut self.small_compressed_buffer_zeroes)
    }
    fn large_compressed_buffer_random(&mut self) -> RcvBuf {
        Self::get_rcv_buf(&mut self.large_compressed_buffer_random)
    }
    fn large_compressed_buffer_zeroes(&mut self) -> RcvBuf {
        Self::get_rcv_buf(&mut self.large_compressed_buffer_zeroes)
    }
}

impl<C: Compressor + Default> Default for Compression<C> {
    fn default() -> Self {
        let mut small_buffer_random = TemporaryBuffer::<u8>::new(SMALL_BUFFER_SIZE);
        // temporary_buffer<char>(n) leaves its bytes uninitialised; zero them so the
        // "zeroes" buffer is what its name says.
        let mut small_buffer_zeroes = TemporaryBuffer::<u8>::new(SMALL_BUFFER_SIZE);
        // SAFETY: the buffer has SMALL_BUFFER_SIZE writable bytes.
        unsafe { std::ptr::write_bytes(small_buffer_zeroes.get_write(), 0, SMALL_BUFFER_SIZE) };

        let dist = UniformIntDistribution::new(0, i8::MAX as u64);
        let gen_ = || dist.sample_fn(&mut || u64::from(local_random())) as u8;
        for i in 0..SMALL_BUFFER_SIZE {
            // SAFETY: as above, for the random buffer.
            unsafe { *small_buffer_random.get_write().add(i) = gen_() };
        }
        let mut large_buffer_random = Vec::new();
        let mut large_buffer_zeroes = Vec::new();
        for _ in 0..LARGE_BUFFER_SIZE / SndBuf::CHUNK_SIZE {
            let mut r = TemporaryBuffer::<u8>::new(SndBuf::CHUNK_SIZE);
            for j in 0..SndBuf::CHUNK_SIZE {
                // SAFETY: the chunk has CHUNK_SIZE writable bytes.
                unsafe { *r.get_write().add(j) = gen_() };
            }
            large_buffer_random.push(r);
            let mut z = TemporaryBuffer::<u8>::new(SndBuf::CHUNK_SIZE);
            // SAFETY: as above.
            unsafe { std::ptr::write_bytes(z.get_write(), 0, SndBuf::CHUNK_SIZE) };
            large_buffer_zeroes.push(z);
        }

        let mut compressor = C::default();

        let rcv = compressor
            .compress(0, SndBuf::from_buffer(small_buffer_random.share()))
            .expect("compress");
        let small_compressed_buffer_random = Self::into_vec(rcv);

        let rcv = compressor
            .compress(0, SndBuf::from_buffer(small_buffer_zeroes.share()))
            .expect("compress");
        let small_compressed_buffer_zeroes = Self::into_vec(rcv);

        let bufs: Vec<TemporaryBuffer<u8>> =
            large_buffer_random.iter().map(|b| b.clone_buffer()).collect();
        let rcv = compressor
            .compress(0, SndBuf::from_buffers(bufs, LARGE_BUFFER_SIZE))
            .expect("compress");
        let large_compressed_buffer_random = Self::into_vec(rcv);

        let bufs: Vec<TemporaryBuffer<u8>> =
            large_buffer_zeroes.iter().map(|b| b.clone_buffer()).collect();
        let rcv = compressor
            .compress(0, SndBuf::from_buffers(bufs, LARGE_BUFFER_SIZE))
            .expect("compress");
        let large_compressed_buffer_zeroes = Self::into_vec(rcv);

        Compression {
            compressor,
            small_buffer_random,
            small_buffer_zeroes,
            large_buffer_random,
            large_buffer_zeroes,
            small_compressed_buffer_random,
            small_compressed_buffer_zeroes,
            large_compressed_buffer_random,
            large_compressed_buffer_zeroes,
        }
    }
}

/// The eight cases of a compressor, in a fixed order.
macro_rules! compression_tests {
    ($group:ident) => {
        teseladb::perf_test_f!($group, small_random_buffer_compress, |s| -> () {
            let b = s.small_buffer_random();
            do_not_optimize(&s.compressor().compress(0, b));
        });
        teseladb::perf_test_f!($group, small_zeroed_buffer_compress, |s| -> () {
            let b = s.small_buffer_zeroes();
            do_not_optimize(&s.compressor().compress(0, b));
        });
        teseladb::perf_test_f!($group, large_random_buffer_compress, |s| -> () {
            let b = s.large_buffer_random();
            do_not_optimize(&s.compressor().compress(0, b));
        });
        teseladb::perf_test_f!($group, large_zeroed_buffer_compress, |s| -> () {
            let b = s.large_buffer_zeroes();
            do_not_optimize(&s.compressor().compress(0, b));
        });
        teseladb::perf_test_f!($group, small_random_buffer_decompress, |s| -> () {
            let b = s.small_compressed_buffer_random();
            do_not_optimize(&s.compressor().decompress(b));
        });
        teseladb::perf_test_f!($group, small_zeroed_buffer_decompress, |s| -> () {
            let b = s.small_compressed_buffer_zeroes();
            do_not_optimize(&s.compressor().decompress(b));
        });
        teseladb::perf_test_f!($group, large_random_buffer_decompress, |s| -> () {
            let b = s.large_compressed_buffer_random();
            do_not_optimize(&s.compressor().decompress(b));
        });
        teseladb::perf_test_f!($group, large_zeroed_buffer_decompress, |s| -> () {
            let b = s.large_compressed_buffer_zeroes();
            do_not_optimize(&s.compressor().decompress(b));
        });

        pub fn register_all() {
            small_random_buffer_compress::register();
            small_zeroed_buffer_compress::register();
            large_random_buffer_compress::register();
            large_zeroed_buffer_compress::register();
            small_random_buffer_decompress::register();
            small_zeroed_buffer_decompress::register();
            large_random_buffer_decompress::register();
            large_zeroed_buffer_decompress::register();
        }
    };
}

/// `using lz4 = compression<teseladb::engine::rpc::lz4_compressor>;`
#[allow(non_camel_case_types)]
mod group_lz4 {
    use super::*;
    pub type lz4 = Compression<Lz4Compressor>;
    compression_tests!(lz4);
}

/// `using lz4_fragmented = compression<teseladb::engine::rpc::lz4_fragmented_compressor>;`
#[allow(non_camel_case_types)]
mod group_lz4_fragmented {
    use super::*;
    pub type lz4_fragmented = Compression<Lz4FragmentedCompressor>;
    compression_tests!(lz4_fragmented);
}

pub(crate) fn main() {
    group_lz4::register_all();
    group_lz4_fragmented::register_all();
    perf_tests::main();
}
