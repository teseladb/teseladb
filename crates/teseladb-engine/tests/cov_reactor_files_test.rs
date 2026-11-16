//! Tests of the reactor's file-system calls: hard links, entry types with and without following
//! symbolic links, ownership and permissions, statistics, file-system information, pipes,
//! groups, and the direct-I/O check.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use teseladb::engine::core::api::FollowSymlink;
use teseladb::engine::core::file_types::{AccessFlags, DirectoryEntryType, FilePermissions};
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{self, check_direct_io_support, engine};
use teseladb::engine::core::thread::async_ as thread_async;
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
        let p = std::env::temp_dir().join(format!("cov_{tag}_{}", std::process::id()));
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

fn r() -> &'static mut reactor::Reactor {
    // SAFETY: called on a reactor thread; the reference is used for one call at a time.
    unsafe { &mut *engine() }
}

thread_case!(hard_links_share_the_inode, {
    let d = TmpDir::new("link");
    let a = d.path("a");
    std::fs::write(&a, b"data").unwrap();
    r().link_file(&a, &d.path("b")).get().unwrap();
    let st = r().file_stat(&d.path("b"), FollowSymlink::YES).get().unwrap();
    assert_eq!(st.number_of_links, 2);
    assert_eq!(st.size, 4);
    assert_eq!(st.inode_number, std::fs::metadata(&a).unwrap().ino());
    // Linking onto an existing name fails.
    assert!(r().link_file(&a, &d.path("b")).get().is_err());
    assert!(r().link_file(&d.path("missing"), &d.path("c")).get().is_err());
});

thread_case!(entry_types_follow_links_on_request, {
    let d = TmpDir::new("types");
    let f = d.path("file");
    std::fs::write(&f, b"x").unwrap();
    let dir = d.path("dir");
    std::fs::create_dir(&dir).unwrap();
    let link = d.path("link");
    std::os::unix::fs::symlink(&f, &link).unwrap();
    let fifo = d.path("fifo");
    let c = std::ffi::CString::new(fifo.clone()).unwrap();
    // SAFETY: a valid C string path.
    assert_eq!(unsafe { libc_mkfifo(c.as_ptr(), 0o600) }, 0);

    let ty = |p: &str, follow| r().file_type(p, follow).get().unwrap();
    assert_eq!(ty(&f, FollowSymlink::YES), Some(DirectoryEntryType::Regular));
    assert_eq!(ty(&dir, FollowSymlink::NO), Some(DirectoryEntryType::Directory));
    assert_eq!(ty(&link, FollowSymlink::YES), Some(DirectoryEntryType::Regular));
    assert_eq!(ty(&link, FollowSymlink::NO), Some(DirectoryEntryType::Link));
    assert_eq!(ty(&fifo, FollowSymlink::NO), Some(DirectoryEntryType::Fifo));
    assert_eq!(ty("/dev/null", FollowSymlink::YES), Some(DirectoryEntryType::CharDevice));
    assert_eq!(ty(&d.path("missing"), FollowSymlink::YES), None);
    // The free function asks the local reactor.
    assert_eq!(
        reactor::file_type(&dir, FollowSymlink::YES).get().unwrap(),
        Some(DirectoryEntryType::Directory)
    );
    // A path through a file does not exist either; other errors are reported.
    assert_eq!(ty(&format!("{f}/below"), FollowSymlink::YES), None);
    let too_long = format!("/{}", "x".repeat(5000));
    assert!(r().file_type(&too_long, FollowSymlink::YES).get().is_err());
});

unsafe extern "C" {
    #[link_name = "mkfifo"]
    fn libc_mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
    fn getuid() -> u32;
    fn getgid() -> u32;
    fn dup(fd: i32) -> i32;
}

thread_case!(ownership_permissions_and_access, {
    let d = TmpDir::new("perm");
    let f = d.path("f");
    std::fs::write(&f, b"abc").unwrap();
    // SAFETY: plain libc calls without arguments.
    let (uid, gid) = unsafe { (getuid(), getgid()) };
    r().chown(&f, uid, gid).get().unwrap();
    let st = r().file_stat(&f, FollowSymlink::NO).get().unwrap();
    assert_eq!((st.uid, st.gid), (u64::from(uid), u64::from(gid)));
    assert!(r().chown(&d.path("missing"), uid, gid).get().is_err());

    r().chmod(&f, FilePermissions::USER_READ | FilePermissions::USER_WRITE).get().unwrap();
    assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(r().chmod(&d.path("missing"), FilePermissions::USER_READ).get().is_err());

    assert!(r().file_accessible(&f, AccessFlags::EXISTS).get().unwrap());
    assert!(r().file_accessible(&f, AccessFlags::READ).get().unwrap());
    assert!(!r().file_accessible(&d.path("missing"), AccessFlags::EXISTS).get().unwrap());
    assert_eq!(r().file_size(&f).get().unwrap(), 3);
    assert!(r().file_size(&d.path("missing")).get().is_err());
});

thread_case!(file_system_information, {
    let d = TmpDir::new("fs");
    let p = d.path("");
    let space = r().file_system_space(&p).get().unwrap();
    assert!(space.capacity > 0);
    assert!(space.free <= space.capacity);
    assert!(space.available <= space.free);
    let vfs = r().statvfs(&p).get().unwrap();
    assert!(vfs.f_bsize > 0);
    assert!(vfs.f_blocks > 0);
    let _fs = r().file_system_at(&p).get().unwrap();
    assert!(r().file_system_at(&d.path("missing")).get().is_err());
    assert!(r().statvfs(&d.path("missing")).get().is_err());
    // The temporary directory either supports direct I/O or reports why not.
    match check_direct_io_support(&p).get() {
        Ok(()) => {}
        Err(e) => assert!(!e.to_string().is_empty()),
    }
    assert!(check_direct_io_support(&d.path("missing/deeper")).get().is_err());
});

thread_case!(pipes_carry_bytes, {
    let (rd, wr) = r().make_pipe().get().unwrap();
    // SAFETY: the duplicated descriptors are owned by the files from here on.
    let mut w = unsafe { std::fs::File::from_raw_fd(dup(wr.get())) };
    // SAFETY: as above.
    let mut rf = unsafe { std::fs::File::from_raw_fd(dup(rd.get())) };
    // The pipe is non-blocking: read only after the writer is closed.
    w.write_all(b"through the pipe").unwrap();
    drop(w);
    drop(wr);
    let mut out = String::new();
    rf.read_to_string(&mut out).unwrap();
    assert_eq!(out, "through the pipe");
    let (a, b) = reactor::experimental::make_pipe().get().unwrap();
    assert_ne!(a.get(), b.get());
});

thread_case!(groups_are_looked_up_by_name, {
    let root = r().getgrnam("root").get().unwrap().expect("the root group exists");
    assert_eq!(root.group_id, 0);
    assert_eq!(root.group_name.as_str(), "root");
    assert!(r().getgrnam("no-such-group-cov").get().unwrap().is_none());
});
