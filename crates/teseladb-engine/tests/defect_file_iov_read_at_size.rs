#![cfg(target_os = "linux")]
//! A gathered read of an append-challenged file that crosses the file's logical size is
//! cut there: the buffer that crosses the size keeps the bytes before the size, so the read
//! returns every byte of the file from its position.
//!
//! The file lives under the build's temporary directory, which must be on a file system
//! the engine treats as append-challenged (not tmpfs, NFS or FUSE).
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/fs_kind.rs"]
mod fs_kind;

use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine_thread_test_case;

const BLOCK: usize = 4096;

/// A block-aligned buffer filled with `byte`.
fn block(byte: u8, len: usize) -> TemporaryBuffer<u8> {
    let mut b = TemporaryBuffer::<u8>::aligned(BLOCK, len);
    // SAFETY: the buffer holds `len` writable bytes.
    unsafe { std::ptr::write_bytes(b.get_write(), byte, len) };
    b
}

engine_thread_test_case!(a_gathered_read_across_the_size_returns_the_whole_file, {
    let dir = env!("CARGO_TARGET_TMPDIR");
    assert!(fs_kind::is_append_challenged(dir), "{dir} must not be on tmpfs, NFS or FUSE");
    let name = format!("{dir}/defect_iov_read_{}", std::process::id());
    let flags = OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let mut f = open_file_dma(&name, flags).get().unwrap();

    // Three blocks: 'a', 'b', 'c'.
    let mut data = block(b'a', 3 * BLOCK);
    // SAFETY: the buffer holds 3 * BLOCK writable bytes.
    unsafe {
        std::ptr::write_bytes(data.get_write().add(BLOCK), b'b', BLOCK);
        std::ptr::write_bytes(data.get_write().add(2 * BLOCK), b'c', BLOCK);
    }
    // SAFETY: the buffer lives until the write resolves.
    let n =
        unsafe { f.dma_write::<u8>(0, data.get(), 3 * BLOCK, std::ptr::null_mut()) }.get().unwrap();
    assert_eq!(n, 3 * BLOCK);

    // One block, then three: the second buffer crosses the size after two blocks.
    let mut first = block(0, BLOCK);
    let mut second = block(0, 3 * BLOCK);
    let iov = vec![
        Iovec { iov_base: first.get_write().cast(), iov_len: BLOCK },
        Iovec { iov_base: second.get_write().cast(), iov_len: 3 * BLOCK },
    ];
    // SAFETY: both buffers live until the read resolves.
    let n = unsafe { f.dma_read_iov(0, iov, std::ptr::null_mut()) }.get().unwrap();
    assert_eq!(n, 3 * BLOCK, "the read stops short of the logical size");
    assert!(first.as_slice().iter().all(|&c| c == b'a'));
    assert!(second.as_slice()[..BLOCK].iter().all(|&c| c == b'b'));
    assert!(second.as_slice()[BLOCK..2 * BLOCK].iter().all(|&c| c == b'c'));

    f.close().get().unwrap();
    std::fs::remove_file(&name).unwrap();
});
