//! File system qualification for asynchronous direct I/O.
//!
//! The reactor submits disk I/O with linux-aio and expects `io_submit(2)` never to block. Some file
//! systems block in it when a write extends the file, which stalls the whole shard.
//! [`filesystem_has_good_aio_support`] measures that on a scratch file in a given directory.

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::linux_aio::internal::linux_abi::{AioContextT, IoEvent, Iocb};
use crate::engine::core::linux_aio::internal::{
    io_destroy, io_getevents, io_setup, io_submit, make_write_iocb,
};
use crate::engine::core::posix::{FileDesc, kernel_error, throw_system_error_on};
use crate::engine::core::reactor::internal_misc::Rusage;
use crate::engine::core::reactor_backend::{EINTR, engine_assert, errno};

use crate::engine::core::internal::linux_abi::{O_CREAT, O_DIRECT, O_EXCL, O_RDWR};
const RUSAGE_THREAD: i32 = 1;

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    fn unlink(path: *const std::ffi::c_char) -> i32;
    fn aligned_alloc(alignment: usize, size: usize) -> *mut std::ffi::c_void;
    fn free(p: *mut std::ffi::c_void);
}

/// Returns the number of voluntary context switches of the calling thread so far.
fn nvcsw() -> i64 {
    let mut usage = Rusage::default();
    // SAFETY: `usage` is a valid rusage.
    unsafe { getrusage(RUSAGE_THREAD, &mut usage) };
    usage.ru_nvcsw
}

/// Runs `func` and adds to `counter` the number of voluntary context switches that happened
/// during the call.
#[inline]
fn with_ctxsw_counting<R, F: FnOnce() -> R>(counter: &mut i64, func: F) -> R {
    *counter -= nvcsw();
    let r = func();
    *counter += nvcsw();
    r
}

/// Returns whether appending direct-I/O writes under `directory` complete without blocking the
/// submitting thread.
///
/// Creates a temporary file in `directory` (removed at once, so it disappears when closed), and
/// appends 1000 blocks of 4 KiB with `O_DIRECT` through linux-aio, one at a time, counting the
/// voluntary context switches each `io_submit` causes. The file system is good if there are fewer
/// than 0.1 per write. With `verbose`, the rate and the verdict are printed to standard output.
///
/// # Errors
///
/// Fails if the AIO context cannot be set up, the file cannot be created with `O_DIRECT` (for
/// example because the file system does not support it), or a write fails.
///
/// A failed internal assertion aborts the process if the kernel reports a short write or an unexpected number of submitted or completed
/// requests.
pub fn filesystem_has_good_aio_support(
    directory: &str,
    verbose: bool,
) -> Result<bool, ExceptionPtr> {
    let mut ioctx: AioContextT = 0;
    let r = io_setup(1, &mut ioctx);
    throw_system_error_on(r == -1, "io_setup")?;
    let _cleanup = crate::engine::util::defer::defer(move || {
        io_destroy(ioctx);
    });
    let fname = format!("{}/fsqual.tmp", directory);
    let fd = FileDesc::open(&fname, O_CREAT | O_EXCL | O_RDWR | O_DIRECT, 0o600)?;
    let cname = std::ffi::CString::new(fname).unwrap_or_default();
    // SAFETY: `cname` is a valid NUL-terminated path that lives for the call.
    unsafe { unlink(cname.as_ptr()) };
    let nr = 1000usize;
    fd.truncate(nr * 4096)?;
    let bufsize = 4096usize;
    let mut ctxsw: i64 = 0;
    // SAFETY: the alignment is a power of two and the size a multiple of it, as aligned_alloc
    // requires; the block is freed by `_del` below.
    let buf = unsafe { aligned_alloc(4096, 4096) };
    let _del = crate::engine::util::defer::defer(move || {
        // SAFETY: `buf` came from aligned_alloc and is freed only here, once.
        unsafe { free(buf) };
    });
    for i in 0..nr {
        let mut cmd: Iocb = make_write_iocb(fd.get(), (bufsize * i) as u64, buf, bufsize);
        let mut cmds: [*mut Iocb; 1] = [&mut cmd];
        with_ctxsw_counting(&mut ctxsw, || -> Result<(), ExceptionPtr> {
            // SAFETY: `cmds` holds one prepared iocb that lives until it completes below.
            let r = unsafe { io_submit(ioctx, 1, cmds.as_mut_ptr()) };
            throw_system_error_on(r == -1, "io_submit")?;
            engine_assert!(r == 1);
            Ok(())
        })?;
        let mut ioev = IoEvent::default();
        let mut n: i32;
        loop {
            // SAFETY: `ioctx` was set up above and is destroyed only by `_cleanup`, after the
            // loop; `ioev` is the one slot asked for; no timeout.
            n = unsafe { io_getevents(ioctx, 1, 1, &mut ioev, std::ptr::null(), false) };
            throw_system_error_on((n == -1) && (errno() != EINTR), "io_getevents")?;
            if n != -1 {
                break;
            }
        }
        engine_assert!(n == 1);
        if ioev.res < 0 {
            return Err(kernel_error(ioev.res as isize));
        }
        engine_assert!(ioev.res == bufsize as i64);
    }
    let rate = ctxsw as f32 / nr as f32;
    let ok = rate < 0.1;
    if verbose {
        let verdict = if ok { "GOOD" } else { "BAD" };
        println!("context switch per appending io: {} ({})", rate, verdict);
    }
    Ok(ok)
}
