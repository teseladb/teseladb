//! One [`IoOp`] on the engine's files: what each operation does, and its result.
//!
//! Every file is the engine's: with `O_DIRECT` unless the reactor runs with the kernel's page cache
//! (`--direct-io off`) or the file system does not take it (tmpfs). A direct file takes whole
//! aligned blocks only, so a write of other bytes reads the blocks around it, patches them and
//! writes them back, then cuts the file back to its length if the last block made it longer (the
//! rules let such a write run alone on its file). Reads need no alignment: the engine reads the
//! aligned span and trims it. Durability is the engine's `fdatasync` of the file, and of the
//! directory for creations, renames and removals.
//!
//! The steps are chained continuations of the engine's futures.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use teseladb::engine::core::api::{
    link_file, recursive_touch_directory, remove_file, rename_file, sync_directory,
};
use teseladb::engine::core::file::{
    EofError, File, FileOpenOptions, OpenFlags, open_file_dma_with_options,
};
use teseladb::engine::core::file_types::FilePermissions;
use teseladb::engine::core::future::{ExceptionPtr, Future, SystemError, make_ready_future};
use teseladb::engine::core::posix::Stat;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb_raftlog::{FileId, IoDone, IoErrno, IoOp};
use teseladb_sys::dma::{self, ALIGN};
use teseladb_types::Bytes;

/// The alignment of a direct access, as a file offset.
const BLOCK: u64 = ALIGN as u64;

/// An open file of the executor.
#[derive(Clone)]
pub(crate) struct OpenFile {
    /// The engine's file.
    pub(crate) file: File,
    /// Whether it is opened with `O_DIRECT`, so it only takes aligned writes.
    pub(crate) direct: bool,
    /// Its device and inode: two identifiers opened on one path are one file.
    pub(crate) inode: (u64, u64),
    /// Its length as the executor's writes and truncations left it.
    pub(crate) size: u64,
}

/// What a finished operation changes in the executor's table of files.
pub(crate) enum Effect {
    /// Nothing.
    None,
    /// A file opened or created under an identifier.
    Opened(FileId, OpenFile),
    /// A write that reached byte `end` of the file.
    Grew(FileId, u64),
    /// A file whose length is now this.
    Resized(FileId, u64),
}

/// A finished operation.
pub(crate) struct Finished {
    /// Its result.
    pub(crate) done: IoDone,
    /// What it changes in the table of files.
    pub(crate) effect: Effect,
}

/// A step's future: its value or the operation's error.
type Io<T> = Future<Result<T, IoErrno>>;

/// Whether `len` bytes at `offset` are whole aligned blocks.
pub(crate) fn is_aligned(offset: u64, len: u64) -> bool {
    offset.is_multiple_of(BLOCK) && len.is_multiple_of(BLOCK) && len > 0
}

/// The [`IoErrno`] of a system error number.
pub(crate) fn errno_of_code(code: i32) -> IoErrno {
    match code {
        28 => IoErrno::NoSpace,
        2 => IoErrno::NotFound,
        17 => IoErrno::AlreadyExists,
        39 => IoErrno::NotEmpty,
        5 => IoErrno::Io,
        c => IoErrno::Other(c),
    }
}

/// The [`IoErrno`] of a failed engine future: its system error, or `Io` for anything else.
pub(crate) fn errno_of(e: &ExceptionPtr) -> IoErrno {
    e.downcast_ref::<SystemError>().map_or(IoErrno::Io, |s| errno_of_code(s.code()))
}

/// The result of `op` failing with `errno`.
pub(crate) fn fail(op: &IoOp, errno: IoErrno) -> IoDone {
    match op {
        IoOp::ReadAt { token, .. } => IoDone::Read { token: *token, data: Err(errno) },
        op => IoDone::Err { op: op.without_data(), errno },
    }
}

/// `f` with its failure as an [`IoErrno`].
fn caught<T: 'static>(f: Future<T>) -> Io<T> {
    f.then_wrapped(|f: Future<T>| Ok::<_, ExceptionPtr>(f.get().map_err(|e| errno_of(&e))))
}

/// A step already done.
fn ready<T: 'static>(r: Result<T, IoErrno>) -> Io<T> {
    make_ready_future(r)
}

/// `next` after `f` succeeds; `f`'s error otherwise.
fn and_then<T: 'static, U: 'static>(f: Io<T>, next: impl FnOnce(T) -> Io<U> + 'static) -> Io<U> {
    f.then(move |r: Result<T, IoErrno>| match r {
        Ok(v) => next(v),
        Err(e) => ready(Err(e)),
    })
}

/// A path as the engine takes it.
fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Starts `op` on the files under `root`; `file` is the open file it names, if it names one and
/// it is open. The future never fails: a failure is the operation's result.
/// With `through`, the files it creates and opens write through (`O_DSYNC`): each write ends
/// durable, and a sync has nothing left to do.
pub(crate) fn start(
    root: Rc<PathBuf>,
    through: bool,
    op: IoOp,
    file: Option<OpenFile>,
) -> Future<Finished> {
    let extra = if through { OpenFlags::DSYNC } else { OpenFlags::from_bits(0) };
    let outcome: Io<Effect> = match (&op, file) {
        (IoOp::Create { file, path, prealloc }, _) => {
            create(root.join(path), *file, *prealloc, extra)
        }
        (IoOp::Open { file, path }, _) => open(&root.join(path), *file, OpenFlags::RW | extra),
        (IoOp::SyncDir { path }, _) => none(sync_directory(&text(&root.join(path)))),
        (IoOp::Rename { from, to }, _) => {
            none(rename_file(&text(&root.join(from)), &text(&root.join(to))))
        }
        (IoOp::Link { from, to }, _) => link(root.join(from), root.join(to)),
        (IoOp::Remove { path } | IoOp::RemoveDir { path }, _) => {
            none(remove_file(&text(&root.join(path))))
        }
        (IoOp::ReadAt { token, offset, len, .. }, Some(mut f)) => {
            let token = *token;
            return read(&mut f.file, *offset, *len as usize).then(move |data| {
                Ok::<_, ExceptionPtr>(Finished {
                    done: IoDone::Read { token, data },
                    effect: Effect::None,
                })
            });
        }
        (IoOp::WriteAt { file: id, offset, data, .. }, Some(f)) => {
            write(f, *id, *offset, data.clone())
        }
        (IoOp::Sync { .. }, Some(mut f)) => {
            let started = std::time::Instant::now();
            caught(f.file.flush()).then(move |r: Result<(), IoErrno>| {
                let took = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
                teseladb_stages::record(teseladb_stages::Stage::DiskSync, took);
                Ok::<_, ExceptionPtr>(r.map(|()| Effect::None))
            })
        }
        (IoOp::Truncate { file: id, len }, Some(mut f)) => {
            let (id, len) = (*id, *len);
            caught(f.file.truncate(len)).then(move |r: Result<(), IoErrno>| {
                Ok::<_, ExceptionPtr>(r.map(|()| Effect::Resized(id, len)))
            })
        }
        // The executor took the file out of its table when the close started; closing never fails.
        (IoOp::Close { .. }, Some(mut f)) => {
            f.file.close().then(|()| Ok::<_, ExceptionPtr>(Ok(Effect::None)))
        }
        (_, None) => ready(Err(IoErrno::BadFile)),
    };
    outcome.then(move |r: Result<Effect, IoErrno>| {
        Ok::<_, ExceptionPtr>(match r {
            Ok(effect) => Finished { done: IoDone::Ok(op.without_data()), effect },
            Err(errno) => Finished { done: fail(&op, errno), effect: Effect::None },
        })
    })
}

/// A step that changes nothing in the table of files.
fn none(f: Future<()>) -> Io<Effect> {
    and_then(caught(f), |()| ready(Ok(Effect::None)))
}

/// Reads `len` bytes at `offset`, fewer at the end of the file and none past it.
fn read(file: &mut File, offset: u64, len: usize) -> Io<Bytes> {
    if len == 0 {
        return ready(Ok(Bytes::new()));
    }
    dma::read(file, offset, len).then_wrapped(move |f: Future<TemporaryBuffer<u8>>| {
        Ok::<_, ExceptionPtr>(match f.get() {
            Ok(buf) => Ok(Bytes::copy_from_slice(&buf.as_slice()[..len.min(buf.size())])),
            Err(e) if e.downcast_ref::<EofError>().is_some() => Ok(Bytes::new()),
            Err(e) => Err(errno_of(&e)),
        })
    })
}

/// Writes all of `buf` at `offset`, going on after short writes while the rest stays aligned.
fn write_all(mut file: File, offset: u64, mut buf: TemporaryBuffer<u8>) -> Io<()> {
    let part = buf.share();
    let wrote = caught(dma::write(&mut file, offset, part));
    and_then(wrote, move |n: usize| {
        if n == buf.size() {
            return ready(Ok(()));
        }
        // A write that moves nothing, or leaves a misaligned rest of a direct write, failed.
        if n == 0 || n > buf.size() || !(n as u64).is_multiple_of(BLOCK) {
            return ready(Err(IoErrno::Io));
        }
        buf.trim_front(n);
        write_all(file, offset + n as u64, buf)
    })
}

/// Writes `data` at `offset` of `f`, opened as `id`.
fn write(mut f: OpenFile, id: FileId, offset: u64, data: Bytes) -> Io<Effect> {
    let len = data.len() as u64;
    if len == 0 {
        return ready(Ok(Effect::None));
    }
    let end = offset + len;
    let grew = move |()| ready(Ok(Effect::Grew(id, end)));
    if !f.direct {
        return and_then(write_all(f.file, offset, TemporaryBuffer::copy_from(&data)), grew);
    }
    if is_aligned(offset, len) {
        let buf = dma::aligned_buffer(data.len(), &[(0, &data)]);
        return and_then(write_all(f.file, offset, buf), grew);
    }
    // The whole blocks around the bytes: read the partial first and last ones that hold data,
    // patch them and write all of them back.
    let start = offset / BLOCK * BLOCK;
    let stop = end.next_multiple_of(BLOCK);
    let last = stop - BLOCK;
    let read_head = offset > start && start < f.size;
    let read_tail = end < stop && last < f.size && (last > start || !read_head);
    let head = if read_head { read(&mut f.file, start, ALIGN) } else { ready(Ok(Bytes::new())) };
    and_then(head, move |head: Bytes| {
        let tail = if read_tail { read(&mut f.file, last, ALIGN) } else { ready(Ok(Bytes::new())) };
        and_then(tail, move |tail: Bytes| {
            let span = (stop - start) as usize;
            let tail_at = (last - start) as usize;
            let parts =
                [(0, &head[..]), (tail_at, &tail[..]), ((offset - start) as usize, &data[..])];
            let buf = dma::aligned_buffer(span, &parts);
            let size = f.size.max(end);
            let mut file = f.file.clone();
            and_then(write_all(f.file, start, buf), move |()| {
                let cut = if stop > size { caught(file.truncate(size)) } else { ready(Ok(())) };
                and_then(cut, move |()| ready(Ok(Effect::Resized(id, size))))
            })
        })
    })
}

/// Opens `full` with `flags` as `id`.
fn open(full: &Path, id: FileId, flags: OpenFlags) -> Io<Effect> {
    // Not the engine's append-challenged files: each of those keeps its own idea of the length
    // and cuts the file to it when closed, which loses what another identifier of the same file
    // wrote after it (the executor tracks the length per file itself).
    let options = FileOpenOptions { append_is_unlikely: true, ..FileOpenOptions::default() };
    let opened = open_file_dma_with_options(&text(full), flags, options);
    and_then(caught(opened), move |mut file: File| {
        let mut probe = file.clone();
        and_then(caught(dma::is_direct(&mut probe)), move |direct: bool| {
            and_then(caught(file.stat()), move |st: Stat| {
                and_then(caught(file.size()), move |size: u64| {
                    let inode = (st.st_dev, st.st_ino);
                    ready(Ok(Effect::Opened(id, OpenFile { file, direct, inode, size })))
                })
            })
        })
    })
}

/// Makes the directories above `full`.
fn parents(full: &Path) -> Io<()> {
    match full.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            caught(recursive_touch_directory(&text(dir), FilePermissions::DEFAULT_DIR_PERMISSIONS))
        }
        _ => ready(Ok(())),
    }
}

/// Creates `full` as `id`, with `prealloc` bytes reserved that read as zeros. A file that cannot
/// get its space does not stay. `extra` are flags it is opened with besides.
fn create(full: PathBuf, id: FileId, prealloc: u64, extra: OpenFlags) -> Io<Effect> {
    let flags = OpenFlags::RW | OpenFlags::CREATE | OpenFlags::EXCLUSIVE | extra;
    let first = open(&full, id, flags);
    let again = full.clone();
    let made = first.then(move |r: Result<Effect, IoErrno>| match r {
        Err(IoErrno::NotFound) => and_then(parents(&again), move |()| open(&again, id, flags)),
        other => ready(other),
    });
    and_then(made, move |made: Effect| {
        let Effect::Opened(id, mut f) = made else { unreachable!("an open gives its file") };
        if prealloc == 0 {
            return ready(Ok(Effect::Opened(id, f)));
        }
        let mut file = f.file.clone();
        let reserved = and_then(caught(f.file.allocate(0, prealloc)), move |()| {
            caught(file.truncate(prealloc))
        });
        reserved.then(move |r: Result<(), IoErrno>| match r {
            Ok(()) => {
                f.size = prealloc;
                ready(Ok(Effect::Opened(id, f)))
            }
            Err(e) => f
                .file
                .close()
                .then(move |()| caught(remove_file(&text(&full))).then(move |_| ready(Err(e)))),
        })
    })
}

/// Gives the file at `from` the second name `to`.
fn link(from: PathBuf, to: PathBuf) -> Io<Effect> {
    and_then(parents(&to), move |()| none(link_file(&text(&from), &text(&to))))
}
