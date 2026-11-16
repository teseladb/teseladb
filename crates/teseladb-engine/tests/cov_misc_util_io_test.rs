#![cfg(target_os = "linux")]
//! Tests of the file-system and process helpers that need a reactor: file-system type and
//! space, ownership, sizes, hard links, whole-file reads, recursive directory creation with
//! relative paths and trailing slashes, an invalid path for recursive removal, and child
//! processes fed a large input, read from their error stream and killed by a signal.

// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use teseladb::engine::core::file_types::FilePermissions;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::util::file::util::read_entire_file;
use teseladb::engine::util::file::{
    chown, file_size, file_system_at, fs_avail, fs_free, link_file, recursive_remove_directory,
    recursive_touch_directory,
};
use teseladb::engine::util::process::experimental::{
    SpawnParameters, WaitStatus, spawn_process_with,
};
use teseladb::engine::util::short_streams::util::read_entire_stream_contiguous;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

/// A fresh directory under the system's temporary directory, removed when dropped.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cov_misc_{tag}_{}", std::process::id()));
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

fn ss(s: &str) -> Sstring {
    Sstring::from(s)
}

thread_case!(file_system_queries_links_and_whole_file_reads, {
    let d = TmpDir::new("io");
    let f = d.path("data.txt");
    std::fs::write(&f, "0123456789").unwrap();

    file_system_at(&d.path("")).get().unwrap();
    let avail = fs_avail(&f).get().unwrap();
    let free = fs_free(&f).get().unwrap();
    assert!(free >= avail);
    assert_eq!(file_size(&f).get().unwrap(), 10);

    // Changing the owner to the current owner needs no privilege.
    let meta = std::fs::metadata(&f).unwrap();
    chown(&f, meta.uid(), meta.gid()).get().unwrap();
    assert!(chown(&d.path("missing"), meta.uid(), meta.gid()).get().is_err());

    let link = d.path("link.txt");
    link_file(&f, &link).get().unwrap();
    assert_eq!(std::fs::metadata(&link).unwrap().ino(), meta.ino());
    assert!(link_file(&f, &link).get().is_err(), "the link exists");

    let bufs = read_entire_file(PathBuf::from(&f)).get().unwrap();
    let text: Vec<u8> = bufs.iter().flat_map(|b| b.as_slice().to_vec()).collect();
    assert_eq!(text, b"0123456789");
    assert!(read_entire_file(PathBuf::from(d.path("missing"))).get().is_err());
});

thread_case!(directories_are_created_recursively_from_any_path, {
    let d = TmpDir::new("dirs");
    // A trailing double slash ends the path.
    let nested = format!("{}/a/b//", d.0.display());
    recursive_touch_directory(&nested, FilePermissions::DEFAULT_DIR_PERMISSIONS).get().unwrap();
    assert!(d.0.join("a").join("b").is_dir());

    // A relative path is resolved from the working directory; an empty one does nothing.
    let rel = format!("cov_misc_rel_{}", std::process::id());
    let r =
        recursive_touch_directory(&format!("{rel}/x"), FilePermissions::DEFAULT_DIR_PERMISSIONS)
            .get();
    let created = Path::new(&rel).join("x").is_dir();
    let _ = std::fs::remove_dir_all(&rel);
    r.unwrap();
    assert!(created);
    recursive_touch_directory("", FilePermissions::DEFAULT_DIR_PERMISSIONS).get().unwrap();

    // A path that is not UTF-8 cannot be removed.
    let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/cov_misc_\xff"));
    assert!(recursive_remove_directory(bad).get().is_err());
});

thread_case!(processes_take_input_report_errors_and_die_by_signal, {
    let params = SpawnParameters {
        argv: vec![ss("sh"), ss("-c"), ss("echo oops >&2; cat > /dev/null; exit 3")],
        env: vec![],
    };
    let mut p = spawn_process_with(Path::new("/bin/sh"), params).get().unwrap();
    let mut cin = p.cin();
    // More than a pipe holds, so the writes are partial.
    let data = vec![b'x'; 256 * 1024];
    cin.write(&data).get().unwrap();
    cin.flush().get().unwrap();
    cin.close().get().unwrap();
    let mut cerr = p.cerr();
    // SAFETY: `cerr` lives in this frame until the read, waited for here, resolves.
    let err = unsafe { read_entire_stream_contiguous(&mut cerr) }.get().unwrap();
    assert_eq!(err, "oops\n");
    cerr.close().get().unwrap();
    match p.wait().get().unwrap() {
        WaitStatus::Exited(e) => assert_eq!(e.exit_code, 3),
        WaitStatus::Signaled(s) => panic!("killed by {}", s.terminating_signal),
    }

    let params = SpawnParameters { argv: vec![ss("sleep"), ss("30")], env: vec![] };
    let mut p = spawn_process_with(Path::new("/bin/sleep"), params).get().unwrap();
    p.kill().unwrap();
    match p.wait().get().unwrap() {
        WaitStatus::Signaled(s) => assert_eq!(s.terminating_signal, 9),
        WaitStatus::Exited(e) => panic!("exited with {}", e.exit_code),
    }
});
