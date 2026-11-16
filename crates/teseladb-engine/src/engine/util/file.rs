//! File system operations and file/stream helpers.
//!
//! The top-level functions (directory creation, removal, renaming, links, stat, access,
//! file system information) are the asynchronous file system API: each returns a future
//! that resolves with the result of the operation. The core API module re-exports them. [`util`] adds helpers that open a file as an input stream and
//! read it whole, plus [`recursive_remove_directory`].

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::engine::core::api::FollowSymlink;
use crate::engine::core::coroutine::make_coroutine;
use crate::engine::core::do_with::do_with;
use crate::engine::core::file::{
    DirectoryEntry, DirectoryEntryType, File, FileOpenOptions, FilePermissions, StatData,
};
use crate::engine::core::file::{
    GroupDetails, SpaceInfo, open_directory, open_file_dma_with_options,
};
use crate::engine::core::file_types::FsType;
use crate::engine::core::file_types::{AccessFlags, OpenFlags};
use crate::engine::core::fstream::{FileInputStreamOptions, make_file_input_stream};
use crate::engine::core::future::{
    ExceptionPtr, Future, current_exception_as_future, futurize_invoke, make_ready_future,
};
use crate::engine::core::iostream::InputStream;
use crate::engine::core::loop_::do_until;
use crate::engine::core::reactor::engine;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::coroutine::as_future::as_future;
use crate::engine::util::short_streams::util::{read_entire_stream, read_entire_stream_contiguous};

/// Creates a new directory.
pub fn make_directory(name: &str, permissions: FilePermissions) -> Future<()> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).make_directory(name, permissions) }
}

/// Ensures a directory exists, creating it if needed.
pub fn touch_directory(name: &str, permissions: FilePermissions) -> Future<()> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).touch_directory(name, permissions) }
}

/// Synchronizes a directory's metadata to disk.
pub fn sync_directory(name: &str) -> Future<()> {
    open_directory(name).then(|f: File| {
        do_with(f, |f: &mut File| {
            let fp: *mut File = f;
            f.flush().then(move |()| {
                // SAFETY: do_with keeps the file alive until the chain resolves.
                unsafe { (*fp).close() }
            })
        })
    })
}

fn do_recursive_touch_directory(
    base_view: &str,
    name: &str,
    permissions: FilePermissions,
) -> Future<()> {
    let mut base = Sstring::from(base_view);
    const SEPARATOR: char = '/';
    if name.is_empty() {
        return make_ready_future(());
    }

    let pos = std::cmp::min(name.find(SEPARATOR).unwrap_or(usize::MAX), name.len() - 1);
    base += &name[..pos + 1];
    let mut name = &name[pos + 1..];
    if name.len() == 1 && name.as_bytes()[0] == SEPARATOR as u8 {
        name = "";
    }
    // use the optional permissions only for last component,
    // other directories in the patch will always be created using the FilePermissions::DEFAULT_DIR_PERMISSIONS
    let f = if name.is_empty() {
        touch_directory(base.as_str(), permissions)
    } else {
        touch_directory(base.as_str(), FilePermissions::DEFAULT_DIR_PERMISSIONS)
    };
    let name = Sstring::from(name);
    let base2 = base.clone();
    f.then(move |()| do_recursive_touch_directory(base.as_str(), name.as_str(), permissions)).then(
        move |()| {
            // We will now flush the directory that holds the entry we potentially
            // created. Technically speaking, we only need to touch when we did
            // create. But flushing the unchanged ones should be cheap enough - and
            // it simplifies the code considerably.
            if base2.empty() {
                return make_ready_future(());
            }
            sync_directory(base2.as_str())
        },
    )
}

/// Recursively ensures a directory and its parents exist.
pub fn recursive_touch_directory(name: &str, permissions: FilePermissions) -> Future<()> {
    // If the name is empty,  it will be of the type a/b/c, which should be interpreted as
    // a relative path. This means we have to flush our current directory
    let mut base = "";
    let first = name.as_bytes().first().copied().unwrap_or(0);
    if first != b'/' || first == b'.' {
        base = "./";
    }
    // futurize_invoke only turns a throw into a failed future; the Rust function
    // already returns its failures in the future.
    do_recursive_touch_directory(base, name, permissions)
}

/// Removes (unlinks) a file or an empty directory.
pub fn remove_file(pathname: &str) -> Future<()> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).remove_file(pathname) }
}

/// Renames (moves) a file.
pub fn rename_file(old_pathname: &str, new_pathname: &str) -> Future<()> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).rename_file(old_pathname, new_pathname) }
}

/// Returns the file system type of the file system holding `name`.
pub fn file_system_at(name: &str) -> Future<FsType> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_system_at(name) }
}

/// Returns the available space of the file system holding `name`.
pub fn fs_avail(name: &str) -> Future<u64> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).statvfs(name) }.then(|st: crate::engine::core::posix::Statvfs| {
        make_ready_future::<u64>(st.f_bavail * st.f_frsize)
    })
}

/// Returns the free space of the file system holding `name`.
pub fn fs_free(name: &str) -> Future<u64> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).statvfs(name) }.then(|st: crate::engine::core::posix::Statvfs| {
        make_ready_future::<u64>(st.f_bfree * st.f_frsize)
    })
}

/// Returns the space information of the file system holding `name`.
pub fn file_system_space(name: &str) -> Future<SpaceInfo> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_system_space(name) }
}

/// Returns stat information about `name`.
pub fn file_stat(name: &str, follow: FollowSymlink) -> Future<StatData> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_stat(name, follow) }
}

/// Looks up a group by name.
pub fn getgrnam(name: &str) -> Future<Option<GroupDetails>> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).getgrnam(name) }
}

/// Changes the owner and group of a file.
pub fn chown(filepath: &str, owner: u32, group: u32) -> Future<()> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).chown(filepath, owner, group) }
}

/// Returns the size of a file.
pub fn file_size(name: &str) -> Future<u64> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_size(name) }
}

/// Checks access to a file.
pub fn file_accessible(name: &str, flags: AccessFlags) -> Future<bool> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_accessible(name, flags) }
}

/// Checks whether a file exists.
pub fn file_exists(name: &str) -> Future<bool> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_exists(name) }
}

/// Creates a hard link.
pub fn link_file(oldpath: &str, newpath: &str) -> Future<()> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).link_file(oldpath, newpath) }
}

/// Changes the permissions of a file.
pub fn chmod(name: &str, permissions: FilePermissions) -> Future<()> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).chmod(name, permissions) }
}

struct WorkEntry {
    path: PathBuf,
    listed: bool,
}

impl WorkEntry {
    fn new(path: PathBuf, listed: bool) -> Self {
        WorkEntry { path, listed }
    }
}

fn do_recursive_remove_directory(path: PathBuf) -> Future<()> {
    do_with(VecDeque::<WorkEntry>::new(), move |work_queue: &mut VecDeque<WorkEntry>| {
        work_queue.push_back(WorkEntry::new(path, false));
        let wq: *mut VecDeque<WorkEntry> = work_queue;
        do_until(
            // SAFETY: do_with keeps the queue alive while the loop runs.
            move || unsafe { (*wq).is_empty() },
            move || {
                // SAFETY: as above; the loop is single-threaded.
                let work_queue = unsafe { &mut *wq };
                let ent = work_queue.pop_back().expect("checked non-empty by do_until");
                if ent.listed {
                    return remove_file(&ent.path.to_string_lossy());
                }
                work_queue.push_back(WorkEntry::new(ent.path.clone(), true));
                do_with(ent.path, move |path: &mut PathBuf| {
                    let pp: *const PathBuf = path;
                    open_directory(&path.to_string_lossy()).then(move |dir: File| {
                        do_with(dir, move |dir: &mut File| {
                            let dp: *mut File = dir;
                            dir.list_directory(move |de: DirectoryEntry| {
                                // SAFETY: the enclosing do_with frames keep path and queue alive.
                                let (path, work_queue) = unsafe { (&*pp, &mut *wq) };
                                let sub_path = path.join(de.name.as_str());
                                if de.type_ == Some(DirectoryEntryType::Directory) {
                                    work_queue.push_back(WorkEntry::new(sub_path, false));
                                } else {
                                    work_queue.push_back(WorkEntry::new(sub_path, true));
                                }
                                make_ready_future(())
                            })
                            .done()
                            // SAFETY: do_with keeps the directory alive.
                            .then(move |()| unsafe { (*dp).close() })
                        })
                    })
                })
            },
        )
    })
}

/// Recursively removes a directory and all of its contents.
///
/// `path` is the directory to remove. Unlike `rm -rf`, it has to be a directory and may
/// not refer to a regular file.
///
/// The function flushes the parent directory of the removed path and so guaranteeing that
/// the remove is stable on disk.
///
/// The function bails out on first error. In that case, some files and/or sub-directories
/// (and their contents) may be left behind at the level in which the error was detected.
///
pub fn recursive_remove_directory(path: PathBuf) -> Future<()> {
    let parent = match path.join("..").to_str() {
        Some(p) => Sstring::from(p),
        None => {
            return current_exception_as_future(ExceptionPtr::new(
                crate::engine::core::future::InvalidArgument::new("path is not valid UTF-8"),
            ));
        }
    };
    open_directory(parent.as_str()).then(move |parent: File| {
        do_with(parent, move |parent: &mut File| {
            let pp: *mut File = parent;
            do_recursive_remove_directory(path).then(move |()| {
                // SAFETY: do_with keeps the parent directory alive.
                unsafe { (*pp).flush() }.then(move |()| unsafe { (*pp).close() })
            })
        })
    })
}

/// File and stream utilities: helpers to perform operations on files through I/O streams.
pub mod util {
    use super::*;

    /// Opens `path`, runs `func` on an input stream over it, then closes the stream and
    /// the file, returning `func`'s result.
    pub fn with_file_input_stream<Func, R>(
        path: &Path,
        func: Func,
        file_opts: FileOpenOptions,
        input_stream_opts: FileInputStreamOptions,
    ) -> Future<R>
    where
        Func: FnOnce(&mut InputStream<u8>) -> Future<R> + 'static,
        R: 'static,
    {
        let path = path.to_string_lossy().into_owned();
        make_coroutine(async move {
            let mut f = open_file_dma_with_options(&path, OpenFlags::RO, file_opts).await?;
            // `File` is a shared handle: the stream takes a clone of it.
            let r: Result<InputStream<u8>, ExceptionPtr> =
                Ok(make_file_input_stream(f.clone(), input_stream_opts));
            let mut in_ = match r {
                Ok(s) => s,
                Err(ex) => {
                    f.close().await?;
                    return Err(ex);
                }
            };
            let res = as_future(futurize_invoke(|| func(&mut in_))).await;
            in_.close().await?;
            f.close().await?;
            res.await
        })
    }

    /// Reads the file at `path` until end of file and returns its bytes in chunks.
    ///
    /// Use only on short files, to avoid running out of memory.
    ///
    /// # Errors
    ///
    /// The future fails if the file cannot be opened or read.
    pub fn read_entire_file(path: PathBuf) -> Future<Vec<TemporaryBuffer<u8>>> {
        with_file_input_stream(
            &path,
            // SAFETY: with_file_input_stream keeps the stream alive and in place until the
            // future resolves, and uses it for nothing else meanwhile.
            |in_: &mut InputStream<u8>| unsafe { read_entire_stream(in_) },
            FileOpenOptions::default(),
            FileInputStreamOptions::default(),
        )
    }

    /// Reads the file at `path` until end of file and returns its bytes as one string.
    ///
    /// Use only on short files, to avoid running out of memory.
    ///
    /// # Errors
    ///
    /// The future fails if the file cannot be opened or read.
    pub fn read_entire_file_contiguous(path: PathBuf) -> Future<Sstring> {
        with_file_input_stream(
            &path,
            // SAFETY: with_file_input_stream keeps the stream alive and in place until the
            // future resolves, and uses it for nothing else meanwhile.
            |in_: &mut InputStream<u8>| unsafe { read_entire_stream_contiguous(in_) },
            FileOpenOptions::default(),
            FileInputStreamOptions::default(),
        )
    }
}
