//! Tests of file I/O: open flags and errors, DMA reads and writes (aligned, vectored, past
//! the end), iovec splitting at `IOV_MAX`, concurrent appends, truncation and allocation,
//! file system queries and ioctls, layered files, `with_file` and `with_file_close_on_failure`,
//! the posix and append-challenged file implementations, and I/O with intents.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/closers.rs"]
mod closers;
#[path = "common/minstd.rs"]
mod minstd;

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

use closers::CloseRef;
use minstd::UniformIntDistribution;
use teseladb::engine::core::align::align_down;
use teseladb::engine::core::aligned_buffer::allocate_aligned_buffer;
use teseladb::engine::core::api::{
    FollowSymlink, chmod, file_accessible, file_exists, file_stat, file_system_space,
    make_directory, recursive_touch_directory, remove_file, touch_directory,
};
use teseladb::engine::core::condition_variable::ConditionVariable;
use teseladb::engine::core::file::{
    CancelledError, DirectoryEntry, File, FileHandleImpl, FileImpl, FileImplBase, FileOpenOptions,
    OpenFlags, StatData, open_file_dma, open_file_dma_with_options, with_file,
    with_file_close_on_failure,
};
use teseladb::engine::core::file_types::{AccessFlags, FilePermissions};
use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, SystemError, make_ready_future,
};
use teseladb::engine::core::io_intent::IoIntent;
use teseladb::engine::core::layered_file::LayeredFileImpl;
use teseladb::engine::core::loop_::max_concurrent_for_each;
use teseladb::engine::core::posix::Stat;
use teseladb::engine::core::reactor::internal_iovecs::sanitize_iovecs;
use teseladb::engine::core::reactor::internal_misc::report_reactor_stalls;
use teseladb::engine::core::reactor::{aio_nowait_supported, engine};
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::semaphore::Semaphore;
use teseladb::engine::core::shared_ptr::{make_shared, static_pointer_cast};
use teseladb::engine::core::stall_sampler::internal::StallReport;
use teseladb::engine::core::stream::{NextFn, Subscription};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::Thread;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::testing::random::testing::LOCAL_RANDOM_ENGINE;
use teseladb::engine::util::alloc_failure_injector::memory::with_allocation_failures;
use teseladb::engine::util::closeable::DeferredClose;
use teseladb::engine::util::internal::magic::internal::FsMagic as fs_magic;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;
use teseladb::engine::util::tmp_file::TmpDir;
use teseladb::engine_test_case;

/// `IOV_MAX` of <limits.h> on Linux.
const IOV_MAX: usize = 1024;

// <fcntl.h> and <linux/fs.h>
const F_SETLEASE: i32 = 1024;
const F_GETLEASE: i32 = 1025;
const F_WRLCK: i32 = 1;
const FIGETBSZ: u64 = 2;
const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const RWF_WRITE_LIFE_NOT_SET: u64 = 0;
const RWH_WRITE_LIFE_NONE: u64 = 1;
const RWH_WRITE_LIFE_SHORT: u64 = 2;
const RWH_WRITE_LIFE_MEDIUM: u64 = 3;
const RWH_WRITE_LIFE_LONG: u64 = 4;
const RWH_WRITE_LIFE_EXTREME: u64 = 5;

/// The C library's `statfs` record (x86_64 and aarch64 layout); only `f_type` is read.
#[repr(C)]
struct Statfs {
    f_type: i64,
    _rest: [i64; 15],
}

unsafe extern "C" {
    fn umask(mask: u32) -> u32;
    fn open(path: *const std::ffi::c_char, flags: i32, ...) -> i32;
    fn fstatfs(fd: i32, buf: *mut Statfs) -> i32;
    fn abort() -> !;
}

fn testfile(t: &TmpDir, name: &str) -> String {
    t.get_path().join(name).to_string_lossy().into_owned()
}

/// The permission bits of a file mode.
fn perms(mode: u64) -> u64 {
    mode & FilePermissions::ALL_PERMISSIONS.bits() as u64
}

fn stat_of(name: &str) -> Result<StatData, ExceptionPtr> {
    file_stat(name, FollowSymlink::YES).get()
}

engine_test_case!(open_flags_test, {
    let flags = OpenFlags::RW | OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
    assert!(
        flags.bits()
            == (OpenFlags::RW.bits() | OpenFlags::CREATE.bits() | OpenFlags::EXCLUSIVE.bits())
    );

    let mask = OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
    assert!((flags & mask) == mask);
    make_ready_future(())
});

engine_test_case!(access_flags_test, {
    let flags = AccessFlags::READ | AccessFlags::WRITE | AccessFlags::EXECUTE;
    assert!(
        flags.bits()
            == (AccessFlags::READ.bits() | AccessFlags::WRITE.bits() | AccessFlags::EXECUTE.bits())
    );
    make_ready_future(())
});

engine_test_case!(file_exists_test, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        f.close().get()?;
        let mut exists = file_exists(&filename).get()?;
        assert!(exists);
        remove_file(&filename).get()?;
        exists = file_exists(&filename).get()?;
        assert!(!exists);
        Ok(())
    })
});

engine_test_case!(handle_bad_alloc_test, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        f.close().get()?;
        let exists = Rc::new(Cell::new(false));
        let e = exists.clone();
        with_allocation_failures(NoncopyableFunction::<fn()>::new(move || {
            // An allocation failure inside surfaces as an allocation error that
            // with_allocation_failures() catches and retries.
            if let Ok(v) = file_exists(&filename).get() {
                e.set(v);
            }
        }));
        assert!(exists.get());
        Ok(())
    })
});

engine_test_case!(file_access_test, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        f.close().get()?;
        let is_accessible =
            file_accessible(&filename, AccessFlags::READ | AccessFlags::WRITE).get()?;
        assert!(is_accessible);
        Ok(())
    })
});

/// A file and the semaphores that pace the concurrent writes and reads of `test1`.
struct FileTest {
    f: File,
    sem: Semaphore,
    par: Semaphore,
}

impl FileTest {
    fn new(f: File) -> Self {
        FileTest { f, sem: Semaphore::new(0), par: Semaphore::new(1000) }
    }
}

engine_test_case!(test1, {
    // Note: this tests generates a file "testfile.tmp" with size 4096 * max (= 40 MB).
    TmpDir::do_with_default(|t: &mut TmpDir| {
        const MAX: usize = 10000;
        let filename = testfile(t, "testfile.tmp");
        open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).then(move |f: File| {
            let ft: *mut FileTest = Box::into_raw(Box::new(FileTest::new(f)));
            for i in 0..MAX {
                // Don't wait for future, use semaphore to signal when done instead.
                // SAFETY: `ft` is deleted only after every continuation signalled `sem`.
                let _ = unsafe { (*ft).par.wait(1) }.then(move |()| -> Result<(), ExceptionPtr> {
                    let wbuf = allocate_aligned_buffer(4096, 4096)?;
                    // SAFETY: the buffer holds 4096 bytes.
                    unsafe { std::ptr::write_bytes(wbuf.get(), i as u8, 4096) };
                    let wb = wbuf.get();
                    // SAFETY: as above; the buffer moves into the continuation and lives
                    // until the write resolved.
                    let _ = unsafe {
                        (*ft).f.dma_write(i as u64 * 4096, wb, 4096, std::ptr::null_mut())
                    }
                    .then(move |ret: usize| -> Result<(), ExceptionPtr> {
                        assert!(ret == 4096);
                        let rbuf = allocate_aligned_buffer(4096, 4096)?;
                        let rb = rbuf.get();
                        // SAFETY: as above.
                        let _ = unsafe {
                            (*ft).f.dma_read(i as u64 * 4096, rb, 4096, std::ptr::null_mut())
                        }
                        .then(move |ret: usize| {
                            assert!(ret == 4096);
                            // SAFETY: both buffers hold 4096 bytes.
                            unsafe {
                                assert!(
                                    std::slice::from_raw_parts(rbuf.get(), 4096)
                                        == std::slice::from_raw_parts(wbuf.get(), 4096)
                                );
                                (*ft).sem.signal(1);
                                (*ft).par.signal(1);
                            }
                        });
                        Ok(())
                    });
                    Ok(())
                });
            }
            // SAFETY: as above.
            unsafe { (*ft).sem.wait(MAX) }
                .then(move |()| unsafe { (*ft).f.flush() })
                .then(move |()| unsafe { (*ft).f.close() })
                .then(move |()| {
                    // SAFETY: every continuation is done with `ft`.
                    drop(unsafe { Box::from_raw(ft) });
                })
        })
    })
});

engine_test_case!(parallel_write_fsync, {
    report_reactor_stalls(NoncopyableFunction::<fn() -> Future<()>>::new(|| {
        TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
            // Plan: open a file and write to it like crazy. In parallel fsync() it all the time.
            let fname = testfile(t, "testfile.tmp");
            let sz: u64 = 32 * 1024 * 1024;
            let buffer_size: usize = 32768;
            let write_concurrency: usize = 16;
            let fsync_every: usize = 1024 * 1024;
            let max_write_ahead_of_fsync: u64 = 4 * 1024 * 1024; // ensures writes don't complete too quickly
            let written: Rc<Cell<u64>> = Rc::new(Cell::new(0));
            let fsynced_at: Rc<Cell<u64>> = Rc::new(Cell::new(0));

            let mut f =
                open_file_dma(&fname, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE)
                    .get()?;
            let fp: *mut File = &mut f;
            // SAFETY: `f` outlives the deferred close, declared after it.
            let mut fp_ref = unsafe { CloseRef::new(fp) };
            let mut close_f = DeferredClose::new(&mut fp_ref);

            // Avoid filesystem problems with size-extending operations
            // SAFETY: as above.
            unsafe { (*fp).truncate(sz) }.get()?;

            let mut fsync_semaphore = Semaphore::new(0);
            let fsp: *mut Semaphore = &mut fsync_semaphore;
            let may_write_condvar = std::pin::pin!(ConditionVariable::new());
            let cvp: *const ConditionVariable = &*may_write_condvar;
            let (w1, fa1) = (written.clone(), fsynced_at.clone());
            let mut fsync_thread = Thread::new(move || -> Result<(), ExceptionPtr> {
                let mut fsynced: u64 = 0;
                while fsynced < sz {
                    // SAFETY: the semaphore, the condition variable and the file outlive
                    // this thread, joined below.
                    unsafe { (*fsp).wait(fsync_every) }.get()?;
                    fa1.set(w1.get());
                    // Signal the condition variable now so that writes proceed
                    // in parallel with the fsync
                    // SAFETY: the condition variable outlives this thread, joined below.
                    unsafe { (*cvp).broadcast() };
                    // SAFETY: the file outlives this thread, joined below.
                    unsafe { (*fp).flush() }.get()?;
                    fsynced += fsync_every as u64;
                }
                Ok(())
            })?;

            let mut write_semaphore = Semaphore::new(write_concurrency);
            let wsp: *mut Semaphore = &mut write_semaphore;
            while written.get() < sz {
                write_semaphore.wait(1).get()?;
                let (w2, fa2) = (written.clone(), fsynced_at.clone());
                // SAFETY: the condition variable is pinned in this frame and outlives the
                // wait, resolved right here.
                unsafe {
                    may_write_condvar
                        .as_ref()
                        .wait_pred(move || w2.get() <= fa2.get() + max_write_ahead_of_fsync)
                }
                .get()?;
                // SAFETY: as above.
                let mut buf = TemporaryBuffer::<u8>::aligned(
                    unsafe { (*fp).memory_dma_alignment() } as usize,
                    buffer_size,
                );
                // SAFETY: the buffer owns `size()` writable bytes.
                unsafe { std::ptr::write_bytes(buf.get_write(), 0, buf.size()) };
                // Write asynchronously, signal when done.
                let (p, n) = (buf.get(), buf.size());
                // SAFETY: as above; the buffer moves into the continuation.
                let _ = unsafe { (*fp).dma_write(written.get(), p, n, std::ptr::null_mut()) }.then(
                    move |_w: usize| {
                        // SAFETY: both semaphores outlive the writes, waited for below.
                        unsafe {
                            (*fsp).signal(buf.size());
                            (*wsp).signal(1);
                        }
                    },
                );
                written.set(written.get() + buffer_size as u64);
            }
            write_semaphore.wait(write_concurrency).get()?;

            fsync_thread.join().get()?;
            close_f.close_now();
            remove_file(&fname).get()?;
            Ok(())
        })
    }))
    .then(|sr: StallReport| {
        println!("parallel_write_fsync: {}", sr);
    })
});

engine_test_case!(test_iov_max, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        const BUFFER_SIZE: usize = 4096;
        const BUFFER_COUNT: usize = IOV_MAX * 2 + 1;
        let mut written_buffers: Vec<TemporaryBuffer<u8>> = Vec::new();
        let mut iovecs: Vec<Iovec> = Vec::new();
        for i in 0..BUFFER_COUNT {
            written_buffers.push(TemporaryBuffer::<u8>::aligned(BUFFER_SIZE, BUFFER_SIZE));
            let b = written_buffers.last_mut().expect("just pushed");
            // SAFETY: the buffer owns BUFFER_SIZE writable bytes.
            unsafe { std::ptr::write_bytes(b.get_write(), i as u8, BUFFER_SIZE) };
            iovecs.push(Iovec { iov_base: b.get_write() as *mut c_void, iov_len: BUFFER_SIZE });
        }

        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);
        let mut left = BUFFER_SIZE * BUFFER_COUNT;
        let mut position: usize = 0;
        while left != 0 {
            // SAFETY: as above.
            let written = unsafe {
                (*fp).dma_write_iov(position as u64, iovecs.clone(), std::ptr::null_mut())
            }
            .get()?;
            iovecs.drain(0..written / BUFFER_SIZE);
            assert!(written % BUFFER_SIZE == 0);
            position += written;
            left -= written;
        }

        assert!(iovecs.is_empty());

        let mut read_buffers: Vec<TemporaryBuffer<u8>> = Vec::new();
        for _ in 0..BUFFER_COUNT {
            read_buffers.push(TemporaryBuffer::<u8>::aligned(BUFFER_SIZE, BUFFER_SIZE));
            let b = read_buffers.last_mut().expect("just pushed");
            // SAFETY: as above.
            unsafe { std::ptr::write_bytes(b.get_write(), 0, BUFFER_SIZE) };
            iovecs.push(Iovec { iov_base: b.get_write() as *mut c_void, iov_len: BUFFER_SIZE });
        }

        left = BUFFER_SIZE * BUFFER_COUNT;
        position = 0;
        while left != 0 {
            // SAFETY: as above.
            let read = unsafe {
                (*fp).dma_read_iov(position as u64, iovecs.clone(), std::ptr::null_mut())
            }
            .get()?;
            iovecs.drain(0..read / BUFFER_SIZE);
            assert!(read % BUFFER_SIZE == 0);
            position += read;
            left -= read;
        }

        for i in 0..BUFFER_COUNT {
            assert!(written_buffers[i].as_slice() == read_buffers[i].as_slice());
        }
        Ok(())
    })
});

engine_test_case!(test_sanitize_iovecs, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut buf = TemporaryBuffer::<u8>::aligned(4096, 4096);
        let base = buf.get_write() as *mut c_void;
        let size = buf.size();

        let iovec_equal = |a: &Iovec, b: &Iovec| a.iov_base == b.iov_base && a.iov_len == b.iov_len;
        let all_equal = |a: &[Iovec], b: &[Iovec]| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| iovec_equal(x, y))
        };

        {
            // Single fragment, sanitize is noop
            let expected_iovecs: Vec<Iovec> = vec![Iovec { iov_base: base, iov_len: size }];
            let mut actual_iovecs = expected_iovecs.clone();
            let actual_length = sanitize_iovecs(&mut actual_iovecs, 4096);
            assert_eq!(actual_length, 4096);
            assert_eq!(actual_iovecs.len(), 1);
            assert!(iovec_equal(
                expected_iovecs.last().expect("one"),
                actual_iovecs.last().expect("one")
            ));
        }

        {
            // one 1024 buffer and IOV_MAX+6 buffers of 512; 4096 byte disk alignment, sanitize needs to drop buffers
            let mut expected_iovecs: Vec<Iovec> = Vec::new();
            for i in 0..IOV_MAX + 7 {
                expected_iovecs
                    .push(Iovec { iov_base: base, iov_len: if i == 0 { 1024 } else { 512 } });
            }
            let mut actual_iovecs = expected_iovecs.clone();
            let actual_length = sanitize_iovecs(&mut actual_iovecs, 4096);
            assert_eq!(actual_length, 512 * IOV_MAX);
            assert_eq!(actual_iovecs.len(), IOV_MAX - 1);

            expected_iovecs.truncate(IOV_MAX - 1);
            assert!(all_equal(&expected_iovecs, &actual_iovecs));
        }

        {
            // IOV_MAX-1 buffers of 512, one 1024 buffer, and 6 512 buffers; 4096 byte disk alignment, sanitize needs to drop and trim buffers
            let mut expected_iovecs: Vec<Iovec> = Vec::new();
            for i in 0..IOV_MAX + 7 {
                expected_iovecs.push(Iovec {
                    iov_base: base,
                    iov_len: if i == IOV_MAX - 1 { 1024 } else { 512 },
                });
            }
            let mut actual_iovecs = expected_iovecs.clone();
            let actual_length = sanitize_iovecs(&mut actual_iovecs, 4096);
            assert_eq!(actual_length, 512 * IOV_MAX);
            assert_eq!(actual_iovecs.len(), IOV_MAX);

            expected_iovecs.truncate(IOV_MAX);
            if let Some(last) = expected_iovecs.last_mut() {
                last.iov_len = 512;
            }
            assert!(all_equal(&expected_iovecs, &actual_iovecs));
        }

        {
            // IOV_MAX+8 buffers of 512; 4096 byte disk alignment, sanitize needs to drop buffers
            let mut expected_iovecs: Vec<Iovec> = Vec::new();
            for _ in 0..IOV_MAX + 8 {
                expected_iovecs.push(Iovec { iov_base: base, iov_len: 512 });
            }
            let mut actual_iovecs = expected_iovecs.clone();
            let actual_length = sanitize_iovecs(&mut actual_iovecs, 4096);
            assert_eq!(actual_length, 512 * IOV_MAX);
            assert_eq!(actual_iovecs.len(), IOV_MAX);

            expected_iovecs.truncate(IOV_MAX);
            assert!(all_equal(&expected_iovecs, &actual_iovecs));
        }
        Ok(())
    })
});

engine_test_case!(test_chmod, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");
        if file_exists(&filename).get()? {
            remove_file(&filename).get()?;
        }

        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        // test default_file_permissions
        let mut f = open_file_dma(&filename, oflags).get()?;
        f.close().get()?;
        let mut sd = stat_of(&filename)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_FILE_PERMISSIONS.bits() as u64);

        // test chmod with new_permissions
        let new_permissions =
            FilePermissions::USER_READ | FilePermissions::GROUP_READ | FilePermissions::OTHERS_READ;
        assert!(new_permissions != FilePermissions::DEFAULT_FILE_PERMISSIONS);
        assert!(file_exists(&filename).get()?);
        chmod(&filename, new_permissions).get()?;
        sd = stat_of(&filename)?;
        assert_eq!(perms(sd.mode), new_permissions.bits() as u64);
        remove_file(&filename).get()?;

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_open_file_dma_permissions, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");
        if file_exists(&filename).get()? {
            remove_file(&filename).get()?;
        }

        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        // test default_file_permissions
        let mut f = open_file_dma(&filename, oflags).get()?;
        f.close().get()?;
        let mut sd = stat_of(&filename)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_FILE_PERMISSIONS.bits() as u64);
        remove_file(&filename).get()?;

        // test options.create_permissions
        let options = FileOpenOptions {
            create_permissions: FilePermissions::USER_READ
                | FilePermissions::GROUP_READ
                | FilePermissions::OTHERS_READ,
            ..FileOpenOptions::default()
        };
        assert!(options.create_permissions != FilePermissions::DEFAULT_FILE_PERMISSIONS);
        let create_permissions = options.create_permissions;
        f = open_file_dma_with_options(&filename, oflags, options).get()?;
        f.close().get()?;
        sd = stat_of(&filename)?;
        assert_eq!(perms(sd.mode), create_permissions.bits() as u64);
        remove_file(&filename).get()?;

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_make_directory_permissions, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let dirname = testfile(t, "testdir.tmp");
        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        // test default_dir_permissions with make_directory
        make_directory(&dirname, FilePermissions::DEFAULT_DIR_PERMISSIONS).get()?;
        let mut sd = stat_of(&dirname)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_DIR_PERMISSIONS.bits() as u64);
        remove_file(&dirname).get()?;

        // test make_directory
        let create_permissions =
            FilePermissions::USER_READ | FilePermissions::GROUP_READ | FilePermissions::OTHERS_READ;
        assert!(create_permissions != FilePermissions::DEFAULT_DIR_PERMISSIONS);
        make_directory(&dirname, create_permissions).get()?;
        sd = stat_of(&dirname)?;
        assert_eq!(perms(sd.mode), create_permissions.bits() as u64);
        remove_file(&dirname).get()?;

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_touch_directory_permissions, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let dirname = testfile(t, "testdir.tmp");
        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        // test default_dir_permissions with touch_directory
        touch_directory(&dirname, FilePermissions::DEFAULT_DIR_PERMISSIONS).get()?;
        let mut sd = stat_of(&dirname)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_DIR_PERMISSIONS.bits() as u64);
        remove_file(&dirname).get()?;

        // test touch_directory, dir creation
        let create_permissions =
            FilePermissions::USER_READ | FilePermissions::GROUP_READ | FilePermissions::OTHERS_READ;
        assert!(create_permissions != FilePermissions::DEFAULT_DIR_PERMISSIONS);
        assert!(!file_exists(&dirname).get()?);
        touch_directory(&dirname, create_permissions).get()?;
        sd = stat_of(&dirname)?;
        assert_eq!(perms(sd.mode), create_permissions.bits() as u64);

        // test touch_directory of existing dir, dir mode need not change
        assert!(file_exists(&dirname).get()?);
        touch_directory(&dirname, FilePermissions::DEFAULT_DIR_PERMISSIONS).get()?;
        sd = stat_of(&dirname)?;
        assert_eq!(perms(sd.mode), create_permissions.bits() as u64);
        remove_file(&dirname).get()?;

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_recursive_touch_directory_permissions, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let base_dirname = testfile(t, "testbasedir.tmp");
        let dirpath = format!("{}/{}", base_dirname, "testsubdir.tmp");
        if file_exists(&dirpath).get()? {
            remove_file(&dirpath).get()?;
        }
        if file_exists(&base_dirname).get()? {
            remove_file(&base_dirname).get()?;
        }

        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        // test default_dir_permissions with recursive_touch_directory
        recursive_touch_directory(&dirpath, FilePermissions::DEFAULT_DIR_PERMISSIONS).get()?;
        let mut sd = stat_of(&base_dirname)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_DIR_PERMISSIONS.bits() as u64);
        sd = stat_of(&dirpath)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_DIR_PERMISSIONS.bits() as u64);
        remove_file(&dirpath).get()?;

        // test recursive_touch_directory, dir creation
        let create_permissions =
            FilePermissions::USER_READ | FilePermissions::GROUP_READ | FilePermissions::OTHERS_READ;
        assert!(create_permissions != FilePermissions::DEFAULT_DIR_PERMISSIONS);
        assert!(file_exists(&base_dirname).get()?);
        assert!(!file_exists(&dirpath).get()?);
        recursive_touch_directory(&dirpath, create_permissions).get()?;
        sd = stat_of(&base_dirname)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_DIR_PERMISSIONS.bits() as u64);
        sd = stat_of(&dirpath)?;
        assert_eq!(perms(sd.mode), create_permissions.bits() as u64);

        // test recursive_touch_directory of existing dir, dir mode need not change
        assert!(file_exists(&dirpath).get()?);
        recursive_touch_directory(&dirpath, FilePermissions::DEFAULT_DIR_PERMISSIONS).get()?;
        sd = stat_of(&base_dirname)?;
        assert_eq!(perms(sd.mode), FilePermissions::DEFAULT_DIR_PERMISSIONS.bits() as u64);
        sd = stat_of(&dirpath)?;
        assert_eq!(perms(sd.mode), create_permissions.bits() as u64);
        remove_file(&dirpath).get()?;
        remove_file(&base_dirname).get()?;

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_file_stat_method, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");

        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        let mut f = open_file_dma(&filename, oflags).get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);
        // SAFETY: as above.
        let st = unsafe { (*fp).stat() }.get()?;
        assert_eq!(
            u64::from(st.st_mode) & FilePermissions::ALL_PERMISSIONS.bits() as u64,
            FilePermissions::DEFAULT_FILE_PERMISSIONS.bits() as u64
        );

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_file_write_lifetime_method, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");

        let mut f1 = open_file_dma(&filename, oflags).get()?;
        let f1p: *mut File = &mut f1;
        // SAFETY: `f1` outlives the deferred close, declared after it.
        let mut f1p_ref = unsafe { CloseRef::new(f1p) };
        let _close_f1 = DeferredClose::new(&mut f1p_ref);
        let mut f2 = open_file_dma(&filename, oflags).get()?;
        let f2p: *mut File = &mut f2;
        // SAFETY: as above.
        let mut f2p_ref = unsafe { CloseRef::new(f2p) };
        let _close_f2 = DeferredClose::new(&mut f2p_ref);

        // Write life time hint values
        let hint_set: Vec<u64> = vec![
            RWF_WRITE_LIFE_NOT_SET,
            RWH_WRITE_LIFE_NONE,
            RWH_WRITE_LIFE_SHORT,
            RWH_WRITE_LIFE_MEDIUM,
            RWH_WRITE_LIFE_LONG,
            RWH_WRITE_LIFE_EXTREME,
        ];

        for &hint in &hint_set {
            // Set and verify the lifetime hint of the inode
            // SAFETY: as above.
            unsafe { (*f1p).set_inode_lifetime_hint(hint) }.get()?;
            // SAFETY: `f1p` points at the open file, alive until its deferred close at the end of the test.
            let o_hint1 = unsafe { (*f1p).get_inode_lifetime_hint() }.get()?;
            assert_eq!(hint, o_hint1);
        }

        // Perform invalid ops
        let hint: u64 = RWH_WRITE_LIFE_EXTREME + 1;
        // SAFETY: as above.
        match unsafe { (*f1p).set_inode_lifetime_hint(hint) }.get() {
            Ok(()) => panic!("expected a SystemError"),
            Err(e) => assert!(e.is::<SystemError>()),
        }
        Ok(())
    })
});

engine_test_case!(test_file_fcntl, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");

        let mut f = open_file_dma(&filename, oflags).get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);
        // SAFETY: as above.
        let f = unsafe { &mut *fp };

        // Set and verify a lease value
        let lease = F_WRLCK;
        // SAFETY: F_SETLEASE and F_GETLEASE take an integer, not a
        // pointer, so any value is memory-safe (the kernel rejects the invalid ones).
        assert!(unsafe { f.fcntl(F_SETLEASE, lease as usize) }.get()? == 0);
        // SAFETY: F_GETLEASE takes no pointer argument, so the call is memory-safe.
        let mut o_lease = unsafe { f.fcntl(F_GETLEASE, 0) }.get()?;
        assert_eq!(lease, o_lease);

        // Use _short version and test the same
        // SAFETY: F_GETLEASE takes no pointer argument, so the call is memory-safe.
        o_lease = unsafe { f.fcntl_short(F_GETLEASE, 0) }.get()?;
        assert_eq!(lease, o_lease);

        // Perform invalid ops
        assert!(
            // SAFETY: F_SETLEASE takes an integer, not a pointer, so any value is memory-safe; the kernel rejects this one.
            matches!(unsafe { f.fcntl(F_SETLEASE, !0usize) }.get(), Err(ref e) if e.is::<SystemError>())
        );
        assert!(
            // SAFETY: F_SETLEASE takes an integer, not a pointer, so any value is memory-safe; the kernel rejects this one.
            matches!(unsafe { f.fcntl_short(F_SETLEASE, !0usize) }.get(), Err(ref e) if e.is::<SystemError>())
        );
        Ok(())
    })
});

engine_test_case!(test_file_ioctl, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");
        let mut block_size: u64 = 0;

        let mut f = open_file_dma(&filename, oflags).get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);
        // SAFETY: as above.
        let f = unsafe { &mut *fp };
        let bsp = &mut block_size as *mut u64 as *mut c_void;

        // Issueing an FS ioctl which is applicable on regular files
        // and can be executed as normal user
        let attempt = (|| -> Result<(), ExceptionPtr> {
            // SAFETY: FIGETBSZ writes one int through `argp`, which points
            // to `block_size`, live and unborrowed until each get() returns; the null `argp`
            // makes the kernel fail with EFAULT without writing.
            assert!(unsafe { f.ioctl(FIGETBSZ, bsp) }.get()? == 0);
            assert!(block_size != 0);

            // Use _short version and test the same
            // SAFETY: FIGETBSZ writes one int through `bsp`, which points to `block_size`, live and unborrowed until get() returns.
            assert!(unsafe { f.ioctl_short(FIGETBSZ, bsp) }.get()? == 0);
            assert!(block_size != 0);
            Ok(())
        })();
        match attempt {
            Ok(()) => {}
            Err(e) if e.is::<SystemError>() => {
                // anon_bdev filesystems do not support FIGETBSZ, and return EINVAL
                assert_eq!(e.downcast_ref::<SystemError>().map(|s| s.code()), Some(EINVAL));
            }
            Err(e) => return Err(e),
        }

        // Perform invalid ops
        assert!(
            // SAFETY: as above (a null `argp`: the kernel fails without writing).
            matches!(unsafe { f.ioctl(FIGETBSZ, std::ptr::null_mut()) }.get(), Err(ref e) if e.is::<SystemError>())
        );
        assert!(
            // SAFETY: as above.
            matches!(unsafe { f.ioctl_short(FIGETBSZ, std::ptr::null_mut()) }.get(), Err(ref e) if e.is::<SystemError>())
        );
        Ok(())
    })
});

/// A layered file implementation that forwards every operation to the file below it.
struct TestLayeredFile {
    layered: LayeredFileImpl,
}

impl TestLayeredFile {
    fn new(f: File) -> Self {
        TestLayeredFile { layered: LayeredFileImpl::new(f) }
    }
}

impl FileImpl for TestLayeredFile {
    fn file_impl_base(&self) -> &FileImplBase {
        &self.layered.base
    }
    unsafe fn write_dma(
        &self,
        _pos: u64,
        _buffer: *const u8,
        _len: usize,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: abort has no preconditions.
        unsafe { abort() }
    }
    unsafe fn write_dma_iov(
        &self,
        _pos: u64,
        _iov: Vec<Iovec>,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    unsafe fn read_dma(
        &self,
        _pos: u64,
        _buffer: *mut u8,
        _len: usize,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    unsafe fn read_dma_iov(
        &self,
        _pos: u64,
        _iov: Vec<Iovec>,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn flush(&self) -> Future<()> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn stat(&self) -> Future<Stat> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn truncate(&self, _length: u64) -> Future<()> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn discard(&self, _offset: u64, _length: u64) -> Future<()> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn allocate(&self, _position: u64, _length: u64) -> Future<()> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn size(&self) -> Future<u64> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn close(&self) -> Future<()> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn dup(&self) -> Result<Box<dyn FileHandleImpl>, ExceptionPtr> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    fn list_directory(&self, _next: NextFn<DirectoryEntry>) -> Subscription<DirectoryEntry> {
        // SAFETY: as above.
        unsafe { abort() }
    }
    unsafe fn dma_read_bulk(
        &self,
        _offset: u64,
        _range_size: usize,
        _intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: as above.
        unsafe { abort() }
    }
}

engine_test_case!(test_underlying_file, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, oflags).get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);
        // SAFETY: as above.
        let f = unsafe { &mut *fp };
        let layered = make_shared(TestLayeredFile::new(f.clone()));
        // SAFETY: the cast keeps the pointer into the same object (an upcast to the
        // `FileImpl` trait object).
        let lf = File::from_impl(unsafe {
            static_pointer_cast(&layered, |x: *mut TestLayeredFile| x as *mut dyn FileImpl)
        });
        assert_eq!(f.memory_dma_alignment(), lf.memory_dma_alignment());
        assert_eq!(f.disk_read_dma_alignment(), lf.disk_read_dma_alignment());
        assert_eq!(f.disk_write_dma_alignment(), lf.disk_write_dma_alignment());
        // The layered file is never closed (its close() aborts).
        std::mem::forget(lf);
        Ok(())
    })
});

engine_test_case!(test_file_stat_method_with_file, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        let filename = testfile(t, "testfile.tmp");
        let mut ref_ = File::default();
        let refp: *mut File = &mut ref_;

        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        let st = with_file(open_file_dma(&filename, oflags), move |f: &mut File| {
            // make a copy of f to verify f is auto-closed when `with_file` returns.
            // SAFETY: `ref_` outlives the future, waited for right here.
            unsafe { *refp = f.clone() };
            f.stat()
        })
        .get()?;
        assert_eq!(
            u64::from(st.st_mode) & FilePermissions::ALL_PERMISSIONS.bits() as u64,
            FilePermissions::DEFAULT_FILE_PERMISSIONS.bits() as u64
        );

        // verify that the file was auto-closed
        assert!(matches!(ref_.stat().get(), Err(ref e) if e.is::<SystemError>()));

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_open_error_with_file, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let open_file = |do_open: bool| -> Result<Future<File>, ExceptionPtr> {
            let oflags = OpenFlags::RO;
            if do_open {
                Ok(open_file_dma(&filename, oflags))
            } else {
                Err(ExceptionPtr::from(RuntimeError::new("expected exception")))
            }
        };
        let got_exception = Rc::new(Cell::new(false));

        // No error is expected.
        let ge = got_exception.clone();
        let r = with_file(open_file(true)?, |_f: &mut File| -> Future<()> {
            panic!("must not be reached");
        })
        .handle_exception_type(move |e: &SystemError| {
            ge.set(true);
            assert!(e.code() == ENOENT);
        })
        .get();
        assert!(r.is_ok());
        assert!(got_exception.get());

        got_exception.set(false);
        // A runtime error is expected: open_file(false) fails before
        // with_file() is called.
        match open_file(false) {
            Ok(fut) => {
                let ge = got_exception.clone();
                let _ = with_file(fut, |_f: &mut File| -> Future<()> {
                    panic!("must not be reached");
                })
                .handle_exception_type(move |_e: &RuntimeError| {
                    ge.set(true);
                })
                .get();
                panic!("expected a RuntimeError");
            }
            Err(e) => assert!(e.is::<RuntimeError>()),
        }
        assert!(!got_exception.get());
        Ok(())
    })
});

engine_test_case!(test_with_file_close_on_failure, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        let filename = testfile(t, "testfile.tmp");

        // SAFETY: umask has no preconditions.
        let orig_umask = unsafe { umask(0) };

        // error-free case
        let mut ref_ =
            with_file_close_on_failure(open_file_dma(&filename, oflags), |f: &mut File| {
                make_ready_future(f.clone())
            })
            .get()?;
        let st = ref_.stat().get()?;
        ref_.close().get()?;
        assert_eq!(
            u64::from(st.st_mode) & FilePermissions::ALL_PERMISSIONS.bits() as u64,
            FilePermissions::DEFAULT_FILE_PERMISSIONS.bits() as u64
        );

        // close-on-error case
        let refp: *mut File = &mut ref_;
        let r = with_file_close_on_failure(
            open_file_dma(&filename, oflags),
            move |f: &mut File| -> Result<(), ExceptionPtr> {
                // SAFETY: `ref_` outlives the future, waited for right here.
                unsafe { *refp = f.clone() };
                Err(ExceptionPtr::from(RuntimeError::new("expected exception")))
            },
        )
        .get();
        assert!(matches!(r, Err(ref e) if e.is::<RuntimeError>()));

        // verify that file was auto-closed on error
        assert!(matches!(ref_.stat().get(), Err(ref e) if e.is::<SystemError>()));

        // SAFETY: as above.
        unsafe { umask(orig_umask) };
        Ok(())
    })
});

engine_test_case!(test_nowait_flag_correctness, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let filename = testfile(t, "testfile.tmp");
        let is_tmpfs = |filename: &str| -> bool {
            let mut buf = std::mem::MaybeUninit::<Statfs>::zeroed();
            let c = std::ffi::CString::new(filename).expect("no NUL in the path");
            // SAFETY: `c` is a NUL-terminated path.
            let fd = unsafe { open(c.as_ptr(), OpenFlags::RO.bits()) };
            assert!(fd != -1);
            // SAFETY: `buf` is a writable statfs.
            let r = unsafe { fstatfs(fd, buf.as_mut_ptr()) };
            if r == -1 {
                return false;
            }
            // SAFETY: fstatfs filled `buf`.
            unsafe { buf.assume_init() }.f_type as u64 == fs_magic::TMPFS
        };

        if !aio_nowait_supported() {
            eprintln!(
                "warning: Skipping this test because RWF_NOWAIT is not supported by the system"
            );
            return Ok(());
        }

        let mut f = open_file_dma(&filename, oflags).get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);

        if is_tmpfs(&filename) {
            eprintln!(
                "warning: Skipping this test because TMPFS was detected, and RWF_NOWAIT is only supported by disk-based FSes"
            );
            return Ok(());
        }

        for i in 0..10u64 {
            let wbuf = allocate_aligned_buffer(4096, 4096)?;
            // SAFETY: the buffer holds 4096 bytes.
            unsafe { std::ptr::write_bytes(wbuf.get(), i as u8, 4096) };
            let wb = wbuf.get();
            // SAFETY: as above; `f` is alive.
            unsafe { (*fp).dma_write(i * 4096, wb, 4096, std::ptr::null_mut()) }.get()?;
            // SAFETY: `f` is alive and open until the end of the test.
            unsafe { (*fp).flush() }.get()?;
        }
        Ok(())
    })
});

engine_test_case!(test_destruct_just_constructed_append_challenged_file, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let _f = open_file_dma(&filename, oflags).get()?;
        Ok(())
    })
});

engine_test_case!(test_destruct_just_constructed_append_challenged_file_with_sloppy_size, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let oflags = OpenFlags::RW | OpenFlags::CREATE;
        let opt = FileOpenOptions { sloppy_size: true, ..FileOpenOptions::default() };
        let _f = open_file_dma_with_options(&filename, oflags, opt).get()?;
        Ok(())
    })
});

engine_test_case!(test_destruct_append_challenged_file_after_write, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let buf = allocate_aligned_buffer(4096, 4096)?;
        // SAFETY: the buffer holds 4096 bytes.
        unsafe { std::ptr::write_bytes(buf.get(), 0, 4096) };

        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        unsafe { f.dma_write(0, buf.get(), 4096, std::ptr::null_mut()) }.get()?;
        Ok(())
    })
});

engine_test_case!(test_destruct_append_challenged_file_after_read, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let buf = allocate_aligned_buffer(4096, 4096)?;
        // SAFETY: the buffer holds 4096 bytes.
        unsafe { std::ptr::write_bytes(buf.get(), 0, 4096) };

        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        unsafe { f.dma_write(0, buf.get(), 4096, std::ptr::null_mut()) }.get()?;
        f.flush().get()?;
        f.close().get()?;

        f = open_file_dma(&filename, OpenFlags::RW).get()?;
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        unsafe { f.dma_read(0, buf.get(), 4096, std::ptr::null_mut()) }.get()?;
        Ok(())
    })
});

engine_test_case!(test_dma_iovec, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        const ALIGNMENT: usize = 4096;
        let wbuf = allocate_aligned_buffer(ALIGNMENT, ALIGNMENT)?;
        let size: usize = 1234;
        // SAFETY: the buffer holds ALIGNMENT bytes.
        unsafe {
            std::ptr::write_bytes(wbuf.get(), 0, ALIGNMENT);
            std::ptr::write_bytes(wbuf.get(), 42, size);
        }
        let mut iovecs: Vec<Iovec> = Vec::new();

        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        iovecs.push(Iovec { iov_base: wbuf.get() as *mut c_void, iov_len: ALIGNMENT });
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        let mut count =
            unsafe { f.dma_write_iov(0, iovecs.clone(), std::ptr::null_mut()) }.get()?;
        assert_eq!(count, ALIGNMENT);
        f.truncate(size as u64).get()?;
        f.close().get()?;

        let rbuf = allocate_aligned_buffer(ALIGNMENT, ALIGNMENT)?;

        // this tests the posix_file_impl
        f = open_file_dma(&filename, OpenFlags::RO).get()?;
        // SAFETY: the buffer holds ALIGNMENT bytes.
        unsafe { std::ptr::write_bytes(rbuf.get(), 0, ALIGNMENT) };
        iovecs.clear();
        iovecs.push(Iovec { iov_base: rbuf.get() as *mut c_void, iov_len: ALIGNMENT });
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        count = unsafe { f.dma_read_iov(0, iovecs.clone(), std::ptr::null_mut()) }.get()?;
        assert_eq!(count, size);
        // SAFETY: both buffers hold ALIGNMENT bytes.
        assert!(unsafe {
            std::slice::from_raw_parts(wbuf.get(), ALIGNMENT)
                == std::slice::from_raw_parts(rbuf.get(), ALIGNMENT)
        });

        // this tests the append_challenged_posix_file_impl
        f = open_file_dma(&filename, OpenFlags::RW).get()?;
        // SAFETY: as above.
        unsafe { std::ptr::write_bytes(rbuf.get(), 0, ALIGNMENT) };
        iovecs.clear();
        iovecs.push(Iovec { iov_base: rbuf.get() as *mut c_void, iov_len: ALIGNMENT });
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        count = unsafe { f.dma_read_iov(0, iovecs.clone(), std::ptr::null_mut()) }.get()?;
        assert_eq!(count, size);
        // SAFETY: as above.
        assert!(unsafe {
            std::slice::from_raw_parts(wbuf.get(), ALIGNMENT)
                == std::slice::from_raw_parts(rbuf.get(), ALIGNMENT)
        });
        Ok(())
    })
});

engine_test_case!(test_intent, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;
        let buf = allocate_aligned_buffer(1024, 1024)?;
        // SAFETY: the buffer holds 1024 bytes.
        unsafe { std::ptr::write_bytes(buf.get(), b'a', 1024) };
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        unsafe { f.dma_write(0, buf.get(), 1024, std::ptr::null_mut()) }.get()?;
        // SAFETY: as above.
        unsafe { std::ptr::write_bytes(buf.get(), b'b', 1024) };
        let mut intent = std::pin::pin!(IoIntent::new());
        // SAFETY: `buf` stays alive and untouched until both writes have resolved below, and
        // the intent is pinned in this frame and outlives the request attached to it.
        let f1 = unsafe { f.dma_write(0, buf.get(), 512, std::ptr::null_mut()) };
        // SAFETY: as above.
        let f2 = unsafe { f.dma_write(512, buf.get(), 512, intent.as_mut().as_ptr()) };
        intent.as_mut().cancel();
        let mut cancelled = false;
        f1.get()?;
        match f2.get() {
            Ok(_) => {}
            Err(ex) if ex.is::<CancelledError>() => cancelled = true,
            Err(ex) => return Err(ex),
        }
        let rbuf = allocate_aligned_buffer(1024, 1024)?;
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        unsafe { f.dma_read(0, rbuf.get(), 1024, std::ptr::null_mut()) }.get()?;
        // SAFETY: the buffer holds 1024 bytes.
        let r = unsafe { std::slice::from_raw_parts(rbuf.get(), 1024) };
        assert!(r[0] == b'b');
        if cancelled {
            assert!(r[512] == b'a');
        } else {
            // The file::dma_write doesn't preemt, but if it
            // suddenly will, the 2nd write will pass before
            // the intent would be cancelled
            eprintln!("warning: Write won the race with cancellation");
            assert!(r[512] == b'b');
        }
        Ok(())
    })
});

engine_test_case!(parallel_overwrite, {
    // Avoid /tmp for tmp_dir, since it can be tmpfs
    TmpDir::do_with(
        std::path::PathBuf::from("XXXXXXXX.tmp"),
        |t: &mut TmpDir| {
            let fname = testfile(t, "testfile.tmp");
            thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
                // Check that overwrites at disk_overwrite_dma_alignment() do not cause stalls. First,
                // create a file.
                let sz: u64 = 1024 * 1024;
                let buffer_size: usize = 128 * 1024;
                let mut f =
                    open_file_dma(&fname, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE)
                        .get()?;
                // Avoid filesystem problems with size-extending operations
                f.truncate(sz).get()?;
                let buf = allocate_aligned_buffer(buffer_size, f.memory_dma_alignment() as usize)?;
                let mut offset: u64 = 0;
                while offset < sz {
                    // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
                    unsafe { f.dma_write(offset, buf.get(), buffer_size, std::ptr::null_mut()) }
                        .get()?;
                    offset += buffer_size as u64;
                }
                // A copy of the local random engine.
                let mut random_engine = LOCAL_RANDOM_ENGINE.with(|e| e.get());
                let dist = UniformIntDistribution::new(0, sz - 1);
                let alignment = f.disk_overwrite_dma_alignment();
                let offsets: Vec<u64> = (0..5000)
                    .map(|_| {
                        align_down(
                            dist.sample_fn(&mut || u64::from(random_engine.next())),
                            alignment,
                        )
                    })
                    .collect();
                let fp: *mut File = &mut f;
                let bufp = buf.get();
                let stall_report = report_reactor_stalls(
                    NoncopyableFunction::<fn() -> Future<()>>::new(move || {
                        max_concurrent_for_each(offsets.clone(), 10, move |offset: u64| {
                            // SAFETY: the file and the buffer outlive the stall report, waited for below.
                            unsafe {
                                (*fp).dma_write(
                                    offset,
                                    bufp,
                                    (*fp).disk_overwrite_dma_alignment() as usize,
                                    std::ptr::null_mut(),
                                )
                            }
                            .discard_result()
                        })
                    }),
                )
                .get()?;
                println!(
                    "parallel_overwrite: {} (overwrite dma alignment {})",
                    stall_report,
                    f.disk_overwrite_dma_alignment()
                );
                f.close().get()?;
                remove_file(&fname).get()?;
                Ok(())
            })
        },
        FilePermissions::DEFAULT_DIR_PERMISSIONS,
    )
});

engine_test_case!(test_oversized_io_works, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t, "testfile.tmp");
        let mut f = open_file_dma(&filename, OpenFlags::RW | OpenFlags::CREATE).get()?;

        let max_write = f.disk_write_max_length();
        let max_read = f.disk_read_max_length();
        let buf_size = std::cmp::max(max_write, max_read) + 4096;

        let buf = allocate_aligned_buffer(buf_size, 4096)?;
        // SAFETY: the buffer holds `buf_size` bytes.
        unsafe { std::ptr::write_bytes(buf.get(), b'a', buf_size) };

        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        unsafe { f.dma_write(0, buf.get(), buf_size, std::ptr::null_mut()) }.get()?;
        f.flush().get()?;
        f.close().get()?;

        // SAFETY: as above.
        unsafe { std::ptr::write_bytes(buf.get(), b'b', buf_size) };
        f = open_file_dma(&filename, OpenFlags::RW).get()?;
        // SAFETY: the buffers hold the bytes the call reads or writes and stay alive and untouched until get() returns, right here; the intent is null.
        unsafe { f.dma_read(0, buf.get(), buf_size, std::ptr::null_mut()) }.get()?;

        // SAFETY: as above.
        let s = unsafe { std::slice::from_raw_parts(buf.get(), buf_size) };
        assert!(s.iter().filter(|&&x| x == b'a').count() == buf_size);
        Ok(())
    })
});

engine_test_case!(test_file_system_space, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let name = t.get_path().to_string_lossy().into_owned();
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only.
        let st = unsafe { (*engine()).statvfs(&name) }.get()?;
        let si = file_system_space(&name).get()?;

        assert_eq!(st.f_blocks * st.f_frsize, si.capacity);
        assert!(si.free <= si.capacity);
        assert!(si.available <= si.capacity);
        assert!(si.available <= si.free);
        Ok(())
    })
});
