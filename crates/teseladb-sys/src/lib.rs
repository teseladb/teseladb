//! Minimal FFI with the operating system for what `std` does not offer.
//!
//! Every `extern "C"` function is wrapped in a safe function that documents with `// SAFETY:` why
//! the call is sound. Linux only (D-177): on every other target the crate is empty.
#![cfg(target_os = "linux")]
#![cfg_attr(coverage, feature(coverage_attribute))]
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub mod aligned;
pub mod handoff;
pub mod host;
pub mod latest;
mod linux;
/// io_uring (012 FR-001).
pub use linux::io_uring as uring;
/// Network namespaces and `veth` pairs, for the tests of AF_XDP (012 FR-033).
pub use linux::netns;
/// AF_XDP and its XDP program (012 FR-033).
pub use linux::xdp;
pub mod poll;
pub mod process;
pub mod ring;

use linux as os;

/// Sends what `file` has written so far towards the device without waiting for a flush of the
/// drive (`sync_file_range` with `SYNC_FILE_RANGE_WRITE`). A large file written this way as it
/// grows leaves little for the sync that ends it, so that sync no longer holds the drive's flush,
/// and every sync of the other logs behind it, for the time of the whole file (H-049 of the
/// native tests).
///
/// # Errors
///
/// The error of the system call.
pub fn write_behind(file: &std::fs::File) -> io::Result<()> {
    os::write_behind(file)
}

/// `flock(2)` operations.
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn getentropy(buf: *mut u8, len: usize) -> i32;
    fn geteuid() -> u32;
}

/// The process's effective user id (`geteuid(2)`): 0 is `root`.
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no argument, touches no memory of the caller and cannot fail.
    unsafe { geteuid() }
}

/// Largest request `getentropy(3)` accepts at once.
const GETENTROPY_MAX: usize = 256;

/// Fills `buf` from the operating system's random generator (`getentropy(3)`):
/// the seed of the cryptographic generators of `teseladb-crypto` (013 FR-001).
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    for chunk in buf.chunks_mut(GETENTROPY_MAX) {
        // SAFETY: `chunk` is valid for writes of `chunk.len()` bytes, at most 256 as the call
        // requires, and the function keeps no pointer after returning.
        check_rc(unsafe { getentropy(chunk.as_mut_ptr(), chunk.len()) })?;
    }
    Ok(())
}

/// `Ok` for a zero return code, otherwise the error in `errno`.
fn check_rc(rc: i32) -> io::Result<()> {
    if rc != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Exclusive lock on a file for as long as the value lives (the data directory `LOCK`).
#[derive(Debug)]
pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Tries to take an exclusive lock without waiting. Returns `WouldBlock` if another process holds
    /// it.
    pub fn try_lock(file: File) -> io::Result<FileLock> {
        // SAFETY: `file` is a valid descriptor for the duration of the call; `flock` keeps no
        // pointers.
        check_rc(unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) })?;
        Ok(FileLock { file })
    }

    /// Locked file.
    pub fn file(&self) -> &File {
        &self.file
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still open (`File` closes it after this). A failure to unlock
        // cannot be remedied here: closing the descriptor releases the lock anyway.
        unsafe {
            flock(self.file.as_raw_fd(), LOCK_UN);
        }
    }
}

/// Reserves `len` bytes from the start of the file so that later writes do not fail for lack of
/// space (`ENOSPC` shows up here, before anything is written). The logical size becomes at least
/// `len` and the new part reads as zeros.
pub fn preallocate(file: &File, len: u64) -> io::Result<()> {
    os::preallocate(file, len)
}

/// Tells whether the error is "no space left on device".
pub fn is_no_space(e: &io::Error) -> bool {
    e.raw_os_error() == Some(os::ENOSPC)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("teseladb-sys-{}-{name}", std::process::id()));
        p
    }

    #[test]
    fn exclusive_lock_is_exclusive_and_released_on_drop() {
        let p = temp_path("lock");
        let open = || File::options().create(true).truncate(false).write(true).open(&p).unwrap();
        let l = FileLock::try_lock(open()).unwrap();
        assert!(l.file().metadata().is_ok());
        let err = FileLock::try_lock(open()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(l);
        let l2 = FileLock::try_lock(open()).unwrap();
        drop(l2);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn preallocate_extends_with_zeros() {
        let p = temp_path("prealloc");
        let mut f =
            File::options().create(true).truncate(true).read(true).write(true).open(&p).unwrap();
        preallocate(&f, 1 << 20).unwrap();
        assert!(f.metadata().unwrap().len() >= 1 << 20);
        f.seek(SeekFrom::Start((1 << 20) - 16)).unwrap();
        let mut buf = [1u8; 16];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0; 16]);
        // Reserving less than what is already there does not shrink the file.
        preallocate(&f, 4096).unwrap();
        assert!(f.metadata().unwrap().len() >= 1 << 20);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn preallocate_fails_on_read_only_descriptor() {
        let p = temp_path("ro-prealloc");
        std::fs::write(&p, b"").unwrap();
        let f = File::open(&p).unwrap();
        assert!(preallocate(&f, 1 << 16).is_err());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn classifies_no_space() {
        assert!(is_no_space(&io::Error::from_raw_os_error(os::ENOSPC)));
        assert!(!is_no_space(&io::Error::from_raw_os_error(1)));
        assert!(!is_no_space(&io::Error::other("x")));
    }

    #[test]
    fn lock_error_on_bad_descriptor_type() {
        // A file opened read-only can be locked too: flock does not depend on the mode.
        let p = temp_path("ro");
        std::fs::write(&p, b"x").unwrap();
        let l = FileLock::try_lock(File::open(&p).unwrap()).unwrap();
        drop(l);
        std::fs::remove_file(&p).unwrap();
    }
}
