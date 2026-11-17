#![cfg(target_os = "linux")]
//! `File::allocate` reserves space with `fallocate(FALLOC_FL_ZERO_RANGE)` and succeeds
//! without doing anything on a file system that does not support it. That a file system
//! (tmpfs) does not support it must not turn the reservation off for files on other file
//! systems: a reservation larger than the file system can hold still fails there.
//!
//! Needs `/dev/shm` on tmpfs and the build's temporary directory on a file system that
//! supports zero-range allocation; both are checked first.
// This file calls libc directly; every `unsafe` block states why it is sound in a
// `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/fs_kind.rs"]
mod fs_kind;

use std::ffi::c_int;
use std::os::fd::AsRawFd;

use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine_thread_test_case;

unsafe extern "C" {
    fn fallocate(fd: c_int, mode: c_int, offset: i64, len: i64) -> c_int;
}

const FALLOC_FL_KEEP_SIZE: c_int = 0x01;
const FALLOC_FL_ZERO_RANGE: c_int = 0x10;
const EOPNOTSUPP: i32 = 95;
/// A reservation no file system of a test machine can hold: one pebibyte.
const TOO_LARGE: u64 = 1 << 50;

/// The errno of a zero-range allocation of one block on a new file in `dir`, or 0.
fn zero_range_errno(dir: &str) -> i32 {
    let name = format!("{dir}/defect_zero_range_probe_{}", std::process::id());
    let f = std::fs::File::create(&name).unwrap();
    // SAFETY: plain system call on a descriptor `f` owns.
    let r =
        unsafe { fallocate(f.as_raw_fd(), FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE, 0, 4096) };
    let errno = if r == 0 { 0 } else { std::io::Error::last_os_error().raw_os_error().unwrap() };
    drop(f);
    std::fs::remove_file(&name).unwrap();
    errno
}

engine_thread_test_case!(an_unsupporting_file_system_does_not_disable_reservations_elsewhere, {
    let shm = "/dev/shm";
    let target = env!("CARGO_TARGET_TMPDIR");
    assert_eq!(fs_kind::fs_magic(shm), fs_kind::TMPFS_MAGIC, "{shm} must be tmpfs");
    assert!(fs_kind::is_append_challenged(target), "{target} must not be tmpfs, NFS or FUSE");
    assert_eq!(zero_range_errno(shm), EOPNOTSUPP, "tmpfs must not support zero-range");
    assert_eq!(zero_range_errno(target), 0, "{target} must support zero-range");

    let flags = OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    // On tmpfs the reservation is not supported: it succeeds and does nothing.
    let on_shm = format!("{shm}/defect_zero_range_{}", std::process::id());
    let mut f = open_file_dma(&on_shm, flags).get().unwrap();
    f.allocate(0, 4096).get().unwrap();
    f.close().get().unwrap();
    std::fs::remove_file(&on_shm).unwrap();

    // On the other file system it still reserves: too much space is refused.
    let on_target = format!("{target}/defect_zero_range_{}", std::process::id());
    let mut f = open_file_dma(&on_target, flags).get().unwrap();
    f.allocate(0, 4096).get().unwrap();
    let r = f.allocate(0, TOO_LARGE).get();
    f.close().get().unwrap();
    std::fs::remove_file(&on_target).unwrap();
    assert!(r.is_err(), "a one-pebibyte reservation succeeded");
});
