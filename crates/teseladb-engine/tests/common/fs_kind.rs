//! The file-system type of a path, from `statfs(2)`, for tests whose behaviour depends on
//! the file system the engine detects.

use std::ffi::{CString, c_char, c_int};

unsafe extern "C" {
    fn statfs(path: *const c_char, buf: *mut StatfsBuf) -> c_int;
}

/// Room for `struct statfs` on 64-bit Linux (120 bytes), whose first field is the
/// file-system magic number.
#[repr(C)]
pub struct StatfsBuf {
    words: [i64; 16],
}

/// tmpfs.
pub const TMPFS_MAGIC: i64 = 0x0102_1994;
/// NFS.
pub const NFS_MAGIC: i64 = 0x6969;
/// FUSE.
pub const FUSE_MAGIC: i64 = 0x6573_5546;

/// The file-system magic number of `path`.
///
/// # Panics
/// If `statfs` fails.
pub fn fs_magic(path: &str) -> i64 {
    let c = CString::new(path).expect("a path without NUL");
    let mut buf = StatfsBuf { words: [0; 16] };
    // SAFETY: `c` is a NUL-terminated path and `buf` has room for the whole structure.
    let r = unsafe { statfs(c.as_ptr(), &mut buf) };
    assert_eq!(r, 0, "statfs({path}): {}", std::io::Error::last_os_error());
    buf.words[0]
}

/// Whether the engine opens regular files on `path` as append-challenged files: every
/// file system but tmpfs, NFS and FUSE.
pub fn is_append_challenged(path: &str) -> bool {
    ![TMPFS_MAGIC, NFS_MAGIC, FUSE_MAGIC].contains(&fs_magic(path))
}
