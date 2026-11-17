//! File paths the other file tests leave out: a regular file opened with
//! `append_is_unlikely` (no append queue: plain and gathered writes, truncation, hole
//! punching, allocation), the same file on tmpfs and on the filesystem of `$HOME` (the
//! coverage runs put it on ext4, where fsync is not exclusive and a flush goes straight to the
//! kernel), data-sync files whose flush is a no-op, ioctl in the syscall thread and on the
//! reactor, a second close() of a shared implementation (warned, ignored), duplicated
//! descriptors dropped without close(), the doubling of a sloppy file's speculative size, and
//! close() of an uninitialized file.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::path::PathBuf;

use teseladb::engine::core::file::{
    File, FileHandle, FileOpenOptions, open_file_dma, open_file_dma_with_options,
};
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine_thread_test_case;

const BLOCK: usize = 4096;
const FIONREAD: u64 = 0x541B;

/// A fresh directory under `base`, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn under(base: PathBuf, tag: &str) -> Self {
        let p = base.join(format!("cov4_rest_file_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn block(byte: u8, len: usize) -> TemporaryBuffer<u8> {
    let mut b = TemporaryBuffer::<u8>::aligned(BLOCK, len);
    // SAFETY: the buffer holds `len` writable bytes.
    unsafe { std::ptr::write_bytes(b.get_write(), byte, len) };
    b
}

fn rw_create() -> OpenFlags {
    OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE
}

/// Writes one block of 'a' at 0 and two gathered blocks of 'b' and 'c' after it, truncates the
/// file to two blocks and punches the first one (when the filesystem can), then checks the
/// bytes on disk.
fn plain_file_ops(name: &str, options: FileOpenOptions) {
    let mut f = open_file_dma_with_options(name, rw_create(), options).get().unwrap();
    let a = block(b'a', BLOCK);
    // SAFETY: the buffer lives until the write resolves.
    let n = unsafe { f.dma_write::<u8>(0, a.get(), BLOCK, std::ptr::null_mut()) }.get().unwrap();
    assert_eq!(n, BLOCK);
    let b = block(b'b', BLOCK);
    let c = block(b'c', BLOCK);
    let iov = vec![
        Iovec { iov_base: b.get() as *mut _, iov_len: BLOCK },
        Iovec { iov_base: c.get() as *mut _, iov_len: BLOCK },
    ];
    // SAFETY: the buffers live until the write resolves.
    let n = unsafe { f.dma_write_iov(BLOCK as u64, iov, std::ptr::null_mut()) }.get().unwrap();
    assert_eq!(n, 2 * BLOCK);
    assert_eq!(f.size().get().unwrap(), 3 * BLOCK as u64);
    f.truncate(2 * BLOCK as u64).get().unwrap();
    assert_eq!(f.size().get().unwrap(), 2 * BLOCK as u64);
    // Allocation past the data keeps the size; twice, as a filesystem without zero-range
    // support is remembered after the first call.
    f.allocate(2 * BLOCK as u64, BLOCK as u64).get().unwrap();
    f.allocate(2 * BLOCK as u64, BLOCK as u64).get().unwrap();
    assert_eq!(f.size().get().unwrap(), 2 * BLOCK as u64);
    let punched = f.discard(0, BLOCK as u64).get().is_ok();
    f.flush().get().unwrap();
    f.close().get().unwrap();
    let bytes = std::fs::read(name).unwrap();
    assert_eq!(bytes.len(), 2 * BLOCK);
    let first = if punched { 0 } else { b'a' };
    assert!(bytes[..BLOCK].iter().all(|&x| x == first));
    assert!(bytes[BLOCK..].iter().all(|&x| x == b'b'));
}

engine_thread_test_case!(files_without_append_queue, {
    let unlikely = FileOpenOptions { append_is_unlikely: true, ..FileOpenOptions::default() };
    let d = Dir::under(std::env::temp_dir(), "unlikely");
    plain_file_ops(&d.path("f"), unlikely);
    if std::path::Path::new("/dev/shm").is_dir() {
        let d = Dir::under(PathBuf::from("/dev/shm"), "shm");
        plain_file_ops(&d.path("f"), FileOpenOptions::default());
    }
    // On the filesystem of $HOME (ext4 in the coverage runs) the default file is
    // append-challenged, with a non-exclusive fsync.
    if let Some(home) = std::env::var_os("HOME") {
        let d = Dir::under(PathBuf::from(home), "home");
        plain_file_ops(&d.path("f"), FileOpenOptions::default());
    }
});

engine_thread_test_case!(dsync_flush_and_ioctls, {
    let d = Dir::under(std::env::temp_dir(), "dsync");
    let name = d.path("f");
    let mut f = open_file_dma(&name, rw_create() | OpenFlags::DSYNC).get().unwrap();
    let a = block(b'd', BLOCK);
    // SAFETY: the buffer lives until the write resolves.
    unsafe { f.dma_write::<u8>(0, a.get(), BLOCK, std::ptr::null_mut()) }.get().unwrap();
    // Every write is synced already: flush has nothing to do.
    f.flush().get().unwrap();
    f.close().get().unwrap();

    let mut f = open_file_dma(&name, OpenFlags::RO).get().unwrap();
    let mut avail: i32 = -1;
    // SAFETY: FIONREAD writes an int, which lives until the future resolves.
    let r = unsafe { f.ioctl(FIONREAD, (&mut avail as *mut i32).cast()) }.get().unwrap();
    assert_eq!((r, avail), (0, BLOCK as i32));
    avail = -1;
    // SAFETY: as above.
    let r = unsafe { f.ioctl_short(FIONREAD, (&mut avail as *mut i32).cast()) }.get().unwrap();
    assert_eq!((r, avail), (0, BLOCK as i32));
    // A second close() of the same implementation, through a copy of the file, is reported
    // and ignored.
    let mut copy = f.clone();
    f.close().get().unwrap();
    copy.close().get().unwrap();
});

engine_thread_test_case!(duplicated_descriptors_without_close, {
    let d = Dir::under(std::env::temp_dir(), "dups");
    let name = d.path("f");
    std::fs::write(&name, vec![b'x'; BLOCK]).unwrap();
    // Files from a handle, dropped without close(): the last one closes the descriptor.
    let mut f = open_file_dma(&name, OpenFlags::RO).get().unwrap();
    let h: FileHandle = f.dup().unwrap();
    let g = h.to_file();
    assert_eq!(g.size().get().unwrap(), BLOCK as u64);
    drop(g);
    drop(h);
    assert_eq!(f.size().get().unwrap(), BLOCK as u64);
    drop(f);
    // A handle outliving the closed files closes the descriptor when it goes.
    let mut f = open_file_dma(&name, OpenFlags::RO).get().unwrap();
    let h: FileHandle = f.dup().unwrap();
    let h2 = h.clone();
    f.close().get().unwrap();
    let mut g = h.into_file();
    assert_eq!(g.size().get().unwrap(), BLOCK as u64);
    g.close().get().unwrap();
    drop(h2);
});

engine_thread_test_case!(sloppy_size_doubles_the_committed_size, {
    let d = Dir::under(std::env::temp_dir(), "sloppy2");
    let name = d.path("log");
    let options = FileOpenOptions {
        sloppy_size: true,
        sloppy_size_hint: 16 * BLOCK as u64,
        ..FileOpenOptions::default()
    };
    let mut f = open_file_dma_with_options(&name, rw_create(), options).get().unwrap();
    let one = block(b'1', BLOCK);
    // The first appending write grows the file to the hint.
    // SAFETY: the buffer lives until the write resolves.
    unsafe { f.dma_write::<u8>(0, one.get(), BLOCK, std::ptr::null_mut()) }.get().unwrap();
    assert_eq!(std::fs::metadata(&name).unwrap().len(), 16 * BLOCK as u64);
    // A write just past it doubles the committed size instead of growing by one block.
    let two = block(b'2', BLOCK);
    // SAFETY: as above.
    unsafe { f.dma_write::<u8>(16 * BLOCK as u64, two.get(), BLOCK, std::ptr::null_mut()) }
        .get()
        .unwrap();
    assert_eq!(std::fs::metadata(&name).unwrap().len(), 32 * BLOCK as u64);
    assert_eq!(f.size().get().unwrap(), 17 * BLOCK as u64);
    f.close().get().unwrap();
    assert_eq!(std::fs::metadata(&name).unwrap().len(), 17 * BLOCK as u64);
});

engine_thread_test_case!(close_of_uninitialized_file_panics, {
    let mut f = File::new();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = f.close();
    }));
    let e = r.expect_err("close() of an uninitialized file");
    let msg = e.downcast_ref::<&str>().copied().unwrap_or_default();
    assert!(msg.contains("uninitialized"), "{msg}");
});
