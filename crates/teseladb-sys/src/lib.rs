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

/// Safe forms of the engine's DMA file operations (023 FR-010).
pub mod dma;
pub mod host;
pub mod latest;
mod linux;
/// Network namespaces and `veth` pairs, for the tests of AF_XDP (012 FR-033).
pub use linux::netns;
/// AF_XDP and its XDP program (012 FR-033).
pub use linux::xdp;
pub mod process;

use linux as os;

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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

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
    fn lock_error_on_bad_descriptor_type() {
        // A file opened read-only can be locked too: flock does not depend on the mode.
        let p = temp_path("ro");
        std::fs::write(&p, b"x").unwrap();
        let l = FileLock::try_lock(File::open(&p).unwrap()).unwrap();
        drop(l);
        std::fs::remove_file(&p).unwrap();
    }
}
