//! Tests of files beyond plain reads and writes: the defaults of a file implementation that
//! supports neither ioctl, fcntl nor duplication, uninitialized files, handles that carry a
//! file to another shard, append-challenged files with a sloppy size (speculative growth,
//! reads past the logical size, allocation, hole punching and the final trim), directory
//! entry types, block devices, the strict-DMA and fsync-bypass settings, and the error types.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/mock_file.rs"]
mod mock_file;

use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;

use mock_file::MockReadOnlyFile;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::file::{
    CancelledError, EofError, File, FileHandle, FileImpl, FileOpenOptions, open_directory,
    open_file_dma, open_file_dma_with_options,
};
use teseladb::engine::core::file_types::{DirectoryEntryType, OpenFlags};
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::shared_ptr::{make_shared, static_pointer_cast};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine_thread_test_case;

const BLOCK: usize = 4096;

/// A fresh directory under the system's temporary directory, removed when dropped.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cov_file_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A block-aligned buffer filled with `byte`.
fn block(byte: u8, len: usize) -> TemporaryBuffer<u8> {
    let mut b = TemporaryBuffer::<u8>::aligned(BLOCK, len);
    // SAFETY: the buffer holds `len` writable bytes.
    unsafe { std::ptr::write_bytes(b.get_write(), byte, len) };
    b
}

fn r() -> &'static mut teseladb::engine::core::reactor::Reactor {
    // SAFETY: called on a reactor thread; the reference is used for one call at a time.
    unsafe { &mut *engine() }
}

engine_thread_test_case!(files_without_ioctl_fcntl_or_dup_say_so, {
    let mock = make_shared(MockReadOnlyFile::new(8192));
    // SAFETY: the cast keeps the pointer into the same object.
    let mut f = File::from_impl(unsafe { static_pointer_cast(&mock, |x| x as *mut dyn FileImpl) });
    assert!(f.is_initialized());
    assert!(<dyn FileImpl>::get_file_impl(&mut f).is_some());
    let null = std::ptr::null_mut();
    // SAFETY: the defaults never touch the argument.
    let e = unsafe { f.ioctl(0, null) }.get().unwrap_err();
    assert!(e.to_string().contains("does not support ioctl"), "{e}");
    // SAFETY: as above.
    let e = unsafe { f.ioctl_short(0, null) }.get().unwrap_err();
    assert!(e.to_string().contains("does not support ioctl_short"), "{e}");
    // SAFETY: as above.
    let e = unsafe { f.fcntl(0, 0) }.get().unwrap_err();
    assert!(e.to_string().contains("does not support fcntl"), "{e}");
    // SAFETY: as above.
    let e = unsafe { f.fcntl_short(0, 0) }.get().unwrap_err();
    assert!(e.to_string().contains("does not support fcntl_short"), "{e}");
    // The lifetime hints go through fcntl and fail with it.
    assert!(f.set_inode_lifetime_hint(1).get().is_err());
    assert!(f.get_inode_lifetime_hint().get().is_err());
    let e = f.dup().err().expect("the mock cannot be duplicated");
    assert!(e.to_string().contains("cannot be duplicated"), "{e}");
    assert_eq!(f.size().get().unwrap(), 8192);
    f.close().get().unwrap();

    let mut empty = File::new();
    assert!(!empty.is_initialized());
    assert!(<dyn FileImpl>::get_file_impl(&mut empty).is_none());
    assert!(!File::default().is_initialized());
    // Any operation on an uninitialized file panics.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| empty.memory_dma_alignment()));
    assert!(r.is_err());

    assert_eq!(EofError.to_string(), "end of file");
    assert_eq!(CancelledError.what(), "cancelled");
    assert_eq!(CancelledError.to_string(), "cancelled");
});

engine_thread_test_case!(file_handles_reopen_the_file_on_any_shard, {
    let d = TmpDir::new("handles");
    let name = d.path("data");
    std::fs::write(&name, vec![b'h'; BLOCK]).unwrap();
    let mut f = open_file_dma(&name, OpenFlags::RO).get().unwrap();
    let handle: FileHandle = f.dup().unwrap();
    let copy = handle.clone();
    let mut again = handle.to_file();
    // SAFETY: no I/O intent.
    let buf = unsafe { again.dma_read_bulk::<u8>(0, 16, std::ptr::null_mut()) }.get().unwrap();
    assert!(buf.as_slice()[..16].iter().all(|&b| b == b'h'));
    again.close().get().unwrap();
    let mut from_handle = File::from_handle(handle);
    assert_eq!(from_handle.size().get().unwrap(), BLOCK as u64);
    from_handle.close().get().unwrap();
    // A handle moves to another shard and becomes a file there.
    let target = Smp::count() - 1;
    let size = Smp::submit_to(target, move || {
        let mut f = copy.into_file();
        f.size().then(move |s: u64| f.close().then(move |()| Ok::<u64, ExceptionPtr>(s)))
    })
    .get()
    .unwrap();
    assert_eq!(size, BLOCK as u64);
    // The file opened first closes last.
    f.close().get().unwrap();
});

engine_thread_test_case!(append_challenged_files_grow_speculatively_and_trim_on_close, {
    let d = TmpDir::new("sloppy");
    let name = d.path("log");
    let options = FileOpenOptions {
        sloppy_size: true,
        sloppy_size_hint: 64 * BLOCK as u64,
        ..FileOpenOptions::default()
    };
    let flags = OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let mut f = open_file_dma_with_options(&name, flags, options).get().unwrap();
    let one = block(b'1', BLOCK);
    // SAFETY: the buffer lives until the write resolves.
    let n = unsafe { f.dma_write::<u8>(0, one.get(), BLOCK, std::ptr::null_mut()) }.get().unwrap();
    assert_eq!(n, BLOCK);
    // A second appending write doubles the speculative size.
    let two = block(b'2', BLOCK);
    // SAFETY: as above.
    let n = unsafe { f.dma_write::<u8>(BLOCK as u64, two.get(), BLOCK, std::ptr::null_mut()) }
        .get()
        .unwrap();
    assert_eq!(n, BLOCK);
    // Reads at or past the logical size return nothing without I/O.
    let mut out = block(0, BLOCK);
    // SAFETY: the buffer lives until the read resolves.
    let n =
        unsafe { f.dma_read::<u8>(2 * BLOCK as u64, out.get_write(), BLOCK, std::ptr::null_mut()) }
            .get()
            .unwrap();
    assert_eq!(n, 0);
    let mut iov_buf = block(0, BLOCK);
    let iov = vec![Iovec { iov_base: iov_buf.get_write().cast(), iov_len: BLOCK }];
    // SAFETY: as above.
    let n = unsafe { f.dma_read_iov(4 * BLOCK as u64, iov, std::ptr::null_mut()) }.get().unwrap();
    assert_eq!(n, 0);
    // A gathered read that crosses the logical size is cut there (see the report: the cut
    // keeps the part past the size, which a file grown speculatively fills with zeros).
    let mut a = block(0, BLOCK);
    let mut b = block(0, 2 * BLOCK);
    let iov = vec![
        Iovec { iov_base: a.get_write().cast(), iov_len: BLOCK },
        Iovec { iov_base: b.get_write().cast(), iov_len: 2 * BLOCK },
    ];
    // SAFETY: as above.
    let n = unsafe { f.dma_read_iov(BLOCK as u64, iov, std::ptr::null_mut()) }.get().unwrap();
    assert!(n >= BLOCK, "at least the second written block");
    assert!(a.as_slice().iter().all(|&c| c == b'2'));
    // Space can be reserved and holes punched.
    f.allocate(0, 2 * BLOCK as u64).get().unwrap();
    // Punching holes needs file-system support; without it the error is the system's.
    let punched = match f.discard(0, BLOCK as u64).get() {
        Ok(()) => true,
        Err(e) => {
            assert!(!e.to_string().is_empty());
            false
        }
    };
    f.flush().get().unwrap();
    f.close().get().unwrap();
    // Closing trims the speculative growth back to the data written.
    let meta = std::fs::metadata(&name).unwrap();
    assert_eq!(meta.len(), 2 * BLOCK as u64);
    let bytes = std::fs::read(&name).unwrap();
    if punched {
        assert!(bytes[..BLOCK].iter().all(|&c| c == 0), "the hole reads as zeros");
    } else {
        assert!(bytes[..BLOCK].iter().all(|&c| c == b'1'));
    }
    assert!(bytes[BLOCK..].iter().all(|&c| c == b'2'));
});

engine_thread_test_case!(directory_listings_report_entry_types, {
    let d = TmpDir::new("types");
    std::fs::write(d.path("file"), b"x").unwrap();
    std::os::unix::fs::symlink(d.path("file"), d.path("link")).unwrap();
    let fifo = std::ffi::CString::new(d.path("fifo")).unwrap();
    // SAFETY: a valid C string path.
    assert_eq!(unsafe { mkfifo(fifo.as_ptr(), 0o600) }, 0);
    std::fs::create_dir(d.path("dir")).unwrap();
    let mut seen = std::collections::BTreeMap::new();
    for dir in [d.0.display().to_string(), "/dev".to_string()] {
        let mut f = open_directory(&dir).get().unwrap();
        let entries = make_coroutine(async move {
            let mut found = Vec::new();
            {
                let mut lister = f.experimental_list_directory();
                while let Some(de) = lister.next().await? {
                    found.push((de.name.to_string(), de.type_));
                }
            }
            f.close().await?;
            Ok::<_, ExceptionPtr>(found)
        })
        .get()
        .unwrap();
        for (name, type_) in entries {
            seen.insert(format!("{dir}/{name}"), type_);
        }
    }
    let at = |p: &str| seen.get(p).copied().flatten();
    assert_eq!(at(&d.path("file")), Some(DirectoryEntryType::Regular));
    assert_eq!(at(&d.path("link")), Some(DirectoryEntryType::Link));
    assert_eq!(at(&d.path("fifo")), Some(DirectoryEntryType::Fifo));
    assert_eq!(at(&d.path("dir")), Some(DirectoryEntryType::Directory));
    assert_eq!(at("/dev/null"), Some(DirectoryEntryType::CharDevice));
    // Every block device of /dev is listed as one.
    for entry in std::fs::read_dir("/dev").unwrap().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_block_device()) {
            let p = format!("/dev/{}", entry.file_name().to_string_lossy());
            assert_eq!(at(&p), Some(DirectoryEntryType::BlockDevice), "{p}");
        }
    }
});

unsafe extern "C" {
    fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
}

engine_thread_test_case!(block_devices_open_read_only, {
    // The first block device this process may open, if any (a container may have none).
    let device = std::fs::read_dir("/dev").unwrap().flatten().find_map(|e| {
        let p = e.path();
        let is_block = e.file_type().is_ok_and(|t| t.is_block_device());
        (is_block && std::fs::File::open(&p).is_ok()).then(|| p.display().to_string())
    });
    let Some(device) = device else { return };
    let Ok(mut f) = open_file_dma(&device, OpenFlags::RO).get() else { return };
    // The size comes from the device, as seeking to its end tells.
    let expected = {
        use std::io::Seek;
        std::fs::File::open(&device).and_then(|mut d| d.seek(std::io::SeekFrom::End(0)))
    };
    if let (Ok(size), Ok(expected)) = (f.size().get(), expected) {
        assert_eq!(size, expected);
    }
    // Truncating and allocating are no-ops on a device.
    f.truncate(0).get().unwrap();
    f.allocate(0, BLOCK as u64).get().unwrap();
    assert!(f.memory_dma_alignment() > 0);
    f.close().get().unwrap();
});

engine_thread_test_case!(strict_dma_and_fsync_bypass_settings, {
    // tmpfs has no direct I/O: with strict DMA the open still succeeds there.
    let shm = std::path::Path::new("/dev/shm");
    if shm.is_dir() {
        let name = format!("/dev/shm/cov_file_strict_{}", std::process::id());
        r().set_strict_dma(true);
        let opened = open_file_dma(&name, OpenFlags::RW | OpenFlags::CREATE).get();
        r().set_strict_dma(false);
        let mut f = opened.unwrap();
        f.close().get().unwrap();
        let _ = std::fs::remove_file(&name);
    }
    // With the fsync bypass a flush returns at once and data-sync opens drop the flag.
    let d = TmpDir::new("bypass");
    r().set_bypass_fsync(true);
    let opened =
        open_file_dma(&d.path("f"), OpenFlags::RW | OpenFlags::CREATE | OpenFlags::DSYNC).get();
    let result = opened.and_then(|mut f| {
        let data = block(b'b', BLOCK);
        // SAFETY: the buffer lives until the write resolves.
        unsafe { f.dma_write::<u8>(0, data.get(), BLOCK, std::ptr::null_mut()) }.get()?;
        f.flush().get()?;
        f.close().get()
    });
    r().set_bypass_fsync(false);
    result.unwrap();
    assert_eq!(std::fs::read(d.path("f")).unwrap(), vec![b'b'; BLOCK]);
});
