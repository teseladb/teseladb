//! [`Tmpdir`], a temporary directory removed when the guard is dropped.

#![allow(dead_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::path::PathBuf;

use teseladb::engine::core::file_types::FilePermissions;
use teseladb::engine::util::tmp_file::{TmpDir as EngineTmpDir, default_tmpdir};

/// Temp dir helper for RAII usage when doing tests
/// in engine threads. Will not work in "normal" mode.
/// Just use `tmp_dir::do_with` for that.
pub struct Tmpdir {
    tmp: EngineTmpDir,
}

impl Tmpdir {
    /// A new directory `testXXXX` (with a random suffix) under the default temporary directory.
    pub fn new() -> Self {
        Self::with_name(default_tmpdir().join("testXXXX"))
    }

    pub fn with_name(name: PathBuf) -> Self {
        let mut tmp = EngineTmpDir::new();
        // SAFETY: the temporary directory stays in place and unused until get() returns, right here.
        unsafe { tmp.create(name, FilePermissions::DEFAULT_DIR_PERMISSIONS) }
            .get()
            .expect("tmp_dir::create");
        Tmpdir { tmp }
    }

    pub fn path(&self) -> PathBuf {
        self.tmp.get_path().to_path_buf()
    }
}

impl Default for Tmpdir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Tmpdir {
    fn drop(&mut self) {
        self.tmp.remove().get().expect("tmp_dir::remove");
    }
}
