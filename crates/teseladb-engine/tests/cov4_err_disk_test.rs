//! Failures of the system calls under the disk path and the reactor backends, made by the
//! seccomp filter of `common/syscall_faults.rs` (no hook in the engine), each in a process of
//! its own: `io_submit` refusing a request (`EAGAIN`, `EBADF`, `EINVAL`, `ENOTSUP`, any other
//! error) directly and in the retry loop of the page-cache mode; io_uring rings that cannot be
//! set up, mapped, advised or probed, and `io_uring_enter` interrupted; and the file calls that
//! fail (`close` of read-only and read-write files, `ftruncate` when an append-challenged file
//! trims to its logical size on flush and on close, `fallocate` without zero ranges, `lseek`),
//! plus an output stream whose write fails. Each case checks the outcome the reference engine
//! (AnotherProduct 25.05.0, `src/core/reactor_backend.cc`, `src/core/file.cc`, `src/core/fstream.cc`)
//! gives: the error each request completes with, the fallback to another backend, or the exit.
#![cfg(target_os = "linux")]
// The DMA reads and writes are unsafe engine interfaces, as are the matchers that read the
// arguments of intercepted calls; every `unsafe` block states why it is sound.
#![allow(unsafe_code)]

#[path = "common/fault_program.rs"]
mod fault_program;
#[path = "common/syscall_faults.rs"]
mod syscall_faults;

use std::sync::atomic::Ordering;

use fault_program::{FaultCase, OK, idle, nothing};
use syscall_faults::{ALWAYS, TARGET, arm, nr, peek};
use teseladb::engine::core::file::{
    File, FileOpenOptions, open_file_dma, open_file_dma_with_options,
};
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::fstream::{
    FileInputStreamOptions, FileOutputStreamOptions, make_file_input_stream,
    make_file_output_stream,
};
use teseladb::engine::core::future::{ExceptionPtr, SystemError};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;

const BLOCK: usize = 4096;
const EAGAIN: i32 = 11;
const EBADF: i32 = 9;
const EINVAL: i32 = 22;
const ENOTSUP: i32 = 95;
const ENOMEM: i32 = 12;
const EIO: i32 = 5;
const EPERM: i32 = 1;
const EINTR: i32 = 4;
const EOPNOTSUPP: i32 = 95;

// The opcodes of the linux-aio requests of the disk path (`IOCB_CMD_*`).
const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_PREADV: u16 = 7;
const IOCB_CMD_PWRITEV: u16 = 8;
// `mmap` offsets of an io_uring's queues and the advice of `io_uring_ring_dontfork`.
const IORING_OFF_SQ_RING: u64 = 0;
const IORING_OFF_SQES: u64 = 0x1000_0000;
const MAP_SHARED_POPULATE: u64 = 0x01 | 0x8000;
const MADV_DONTFORK: u64 = 10;
const IORING_REGISTER_PROBE: u64 = 8;
const IORING_ENTER_GETEVENTS: u64 = 1;

/// An `io_submit` whose first control block reads or writes (the disk path; the linux-aio
/// backend's polls and its preemption timer submit other opcodes).
fn submits_disk_io(args: &[u64; 6]) -> bool {
    // SAFETY: `io_submit(ctx, nr, iocbpp)`: the caller waits in the kernel while its array of
    // `nr` control block pointers, and the blocks, stay as they are.
    unsafe {
        if args[1] == 0 {
            return false;
        }
        let Some(first) = peek::<u64>(args[2]) else { return false };
        // `aio_lio_opcode` is at offset 16 of `struct iocb`.
        let Some(op) = peek::<u16>(first + 16) else { return false };
        matches!(op, IOCB_CMD_PREAD | IOCB_CMD_PWRITE | IOCB_CMD_PREADV | IOCB_CMD_PWRITEV)
    }
}

/// A call whose first argument is the descriptor in [`TARGET`].
fn on_target_fd(args: &[u64; 6]) -> bool {
    args[0] as i32 as i64 == TARGET.load(Ordering::Acquire) as i64
}

fn maps_sq_ring(args: &[u64; 6]) -> bool {
    args[5] == IORING_OFF_SQ_RING && args[3] == MAP_SHARED_POPULATE && args[4] as i32 >= 0
}

fn maps_sqes(args: &[u64; 6]) -> bool {
    args[5] == IORING_OFF_SQES && args[3] == MAP_SHARED_POPULATE
}

fn dontfork(args: &[u64; 6]) -> bool {
    args[2] == MADV_DONTFORK
}

fn registers_probe(args: &[u64; 6]) -> bool {
    args[1] == IORING_REGISTER_PROBE
}

fn waits_for_completions(args: &[u64; 6]) -> bool {
    args[2] >= 1 && args[3] & IORING_ENTER_GETEVENTS != 0
}

fn scratch(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("cov4_err_disk_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the case's directory");
    dir.join(tag).display().to_string()
}

/// The descriptor this process has open on `path`.
fn fd_of(path: &str) -> i32 {
    let want = std::fs::canonicalize(path).expect("the file");
    for e in std::fs::read_dir("/proc/self/fd").expect("the descriptor table").flatten() {
        if std::fs::read_link(e.path()).ok().as_ref() == Some(&want)
            && let Ok(fd) = e.file_name().to_string_lossy().parse::<i32>()
        {
            return fd;
        }
    }
    panic!("no descriptor on {path}");
}

fn block(byte: u8) -> TemporaryBuffer<u8> {
    let mut b = TemporaryBuffer::<u8>::aligned(BLOCK, BLOCK);
    // SAFETY: the buffer holds BLOCK writable bytes.
    unsafe { std::ptr::write_bytes(b.get_write(), byte, BLOCK) };
    b
}

fn write_block(f: &mut File, pos: u64, byte: u8) -> Result<usize, ExceptionPtr> {
    let b = block(byte);
    // SAFETY: the buffer lives until the write resolves.
    unsafe { f.dma_write::<u8>(pos, b.get(), BLOCK, std::ptr::null_mut()) }.get()
}

fn read_block(f: &mut File, pos: u64) -> Result<TemporaryBuffer<u8>, ExceptionPtr> {
    // SAFETY: no I/O intent.
    unsafe { f.dma_read_bulk::<u8>(pos, BLOCK, std::ptr::null_mut()) }.get()
}

fn code_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<SystemError>().map(|s| s.code())
}

fn rw_file(tag: &str) -> Result<(File, String), ExceptionPtr> {
    let name = scratch(tag);
    let f = open_file_dma(&name, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE).get()?;
    Ok((f, name))
}

/// `io_submit` refuses the first disk requests with `EAGAIN`: the reactor submits them again
/// (`handle_aio_error` consumes none) and they complete.
fn aio_eagain_is_submitted_again() -> Result<(), ExceptionPtr> {
    let (mut f, _) = rw_file("eagain")?;
    let rule = arm(nr::IO_SUBMIT, Some(submits_disk_io), EAGAIN, 0, 3);
    assert_eq!(write_block(&mut f, 0, b'a')?, BLOCK);
    assert_eq!(rule.disarm(), 3);
    assert!(read_block(&mut f, 0)?.as_slice().iter().all(|&c| c == b'a'));
    f.close().get()
}

/// Refused with `code`, the first request completes with `expect` and the next ones run.
fn aio_refusal_fails_one_request(code: i32, expect: i32) -> Result<(), ExceptionPtr> {
    let (mut f, _) = rw_file("refused")?;
    let rule = arm(nr::IO_SUBMIT, Some(submits_disk_io), code, 0, 1);
    let e = write_block(&mut f, 0, b'b').expect_err("the refused write");
    assert_eq!(code_of(&e), Some(expect), "{e}");
    assert_eq!(rule.disarm(), 1);
    assert_eq!(write_block(&mut f, 0, b'c')?, BLOCK);
    assert!(read_block(&mut f, 0)?.as_slice().iter().all(|&c| c == b'c'));
    f.close().get()
}

fn aio_ebadf_fails_the_request() -> Result<(), ExceptionPtr> {
    aio_refusal_fails_one_request(EBADF, EBADF)
}

fn aio_einval_is_not_supported() -> Result<(), ExceptionPtr> {
    aio_refusal_fails_one_request(EINVAL, ENOTSUP)
}

fn aio_enotsup_is_not_supported() -> Result<(), ExceptionPtr> {
    aio_refusal_fails_one_request(ENOTSUP, ENOTSUP)
}

/// Any other `io_submit` error escapes the reactor's main loop, which logs it and exits with
/// status 1 (`reactor::run`: `_exit(1)` after "exception running reactor main loop").
fn aio_other_error_stops_the_reactor() -> Result<(), ExceptionPtr> {
    let (mut f, _) = rw_file("other")?;
    arm(nr::IO_SUBMIT, Some(submits_disk_io), ENOMEM, 0, ALWAYS);
    let _ = write_block(&mut f, 0, b'd');
    unreachable!("the reactor exits first");
}

/// In the page-cache mode every request goes through the retry loop, which submits from the
/// syscall thread: there `EAGAIN` keeps it queued and `EBADF` fails it.
fn retry_loop_handles_refusals() -> Result<(), ExceptionPtr> {
    let (mut f, _) = rw_file("retry")?;
    let rule = arm(nr::IO_SUBMIT, Some(submits_disk_io), EAGAIN, 0, 2);
    assert_eq!(write_block(&mut f, 0, b'e')?, BLOCK);
    assert_eq!(rule.disarm(), 2);
    let rule = arm(nr::IO_SUBMIT, Some(submits_disk_io), EBADF, 0, 1);
    let e = write_block(&mut f, 0, b'f').expect_err("the refused write");
    assert_eq!(code_of(&e), Some(EBADF), "{e}");
    assert_eq!(rule.disarm(), 1);
    assert!(read_block(&mut f, 0)?.as_slice().iter().all(|&c| c == b'e'));
    f.close().get()
}

/// Some I/O on whatever backend started.
fn disk_io_works() -> Result<(), ExceptionPtr> {
    let (mut f, name) = rw_file("io")?;
    assert_eq!(write_block(&mut f, 0, b'g')?, BLOCK);
    f.flush().get()?;
    assert!(read_block(&mut f, 0)?.as_slice().iter().all(|&c| c == b'g'));
    f.close().get()?;
    std::fs::remove_file(name).map_err(ExceptionPtr::new)
}

fn refuse_uring_setup() {
    arm(nr::IO_URING_SETUP, None, EPERM, 0, ALWAYS);
}

fn refuse_sq_ring_map() {
    arm(nr::MMAP, Some(maps_sq_ring), ENOMEM, 0, ALWAYS);
}

fn refuse_sqes_map() {
    arm(nr::MMAP, Some(maps_sqes), ENOMEM, 0, ALWAYS);
}

fn refuse_probe() {
    arm(nr::IO_URING_REGISTER, Some(registers_probe), EINVAL, 0, ALWAYS);
}

/// `io_uring_ring_dontfork` ignores its errors (as the reference, which discards the result):
/// the rings work. The first and the second `madvise` of each ring fail in turn.
fn refuse_dontfork_first() {
    arm(nr::MADVISE, Some(dontfork), ENOMEM, 0, 1);
}

fn refuse_dontfork_second() {
    arm(nr::MADVISE, Some(dontfork), ENOMEM, 1, 1);
}

/// `io_uring_enter` waiting for completions is interrupted a few times: the reactor returns
/// to its loop (`EINTR`) and goes on.
fn interrupt_uring_waits() {
    arm(nr::IO_URING_ENTER, Some(waits_for_completions), EINTR, 0, 5);
}

/// `close` of a read-write file (in the syscall thread) and of a read-only one (in the
/// reactor) fails: `close()` reports it and resolves.
fn close_errors_are_reported() -> Result<(), ExceptionPtr> {
    let (mut f, name) = rw_file("close")?;
    assert_eq!(write_block(&mut f, 0, b'h')?, BLOCK);
    TARGET.store(fd_of(&name) as u64, Ordering::Release);
    let rule = arm(nr::CLOSE, Some(on_target_fd), EIO, 0, 1);
    f.close().get()?;
    assert_eq!(rule.disarm(), 1);
    // The refused close left that descriptor open: the read-only file is another one.
    let ro_name = scratch("close_ro");
    std::fs::write(&ro_name, vec![b'r'; BLOCK]).map_err(ExceptionPtr::new)?;
    let mut ro = open_file_dma(&ro_name, OpenFlags::RO).get()?;
    TARGET.store(fd_of(&ro_name) as u64, Ordering::Release);
    let rule = arm(nr::CLOSE, Some(on_target_fd), EIO, 0, 1);
    ro.close().get()?;
    assert_eq!(rule.disarm(), 1);
    Ok(())
}

/// An append-challenged file with a sloppy size grows ahead of its logical size; flushing
/// it trims the file first, and when that `ftruncate` fails the flush fails with its error.
/// Closing it trims too: a failed trim is reported and the close still resolves.
fn failed_trims_fail_flush_and_are_reported_on_close() -> Result<(), ExceptionPtr> {
    let name = scratch("sloppy");
    let options = FileOpenOptions { sloppy_size: true, ..Default::default() };
    let mut f = open_file_dma_with_options(
        &name,
        OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        options,
    )
    .get()?;
    for i in 0..4 {
        assert_eq!(write_block(&mut f, (i * BLOCK) as u64, b'i')?, BLOCK);
    }
    TARGET.store(fd_of(&name) as u64, Ordering::Release);
    let rule = arm(nr::FTRUNCATE, Some(on_target_fd), EIO, 0, 1);
    let flushed = f.flush().get();
    let hits = rule.disarm();
    if hits == 1 {
        let e = flushed.expect_err("the flush whose trim fails");
        assert_eq!(code_of(&e), Some(EIO), "{e}");
    } else {
        // The file system gave no room for speculation: nothing to trim.
        flushed?;
    }
    assert_eq!(write_block(&mut f, (4 * BLOCK) as u64, b'j')?, BLOCK);
    let rule = arm(nr::FTRUNCATE, Some(on_target_fd), EIO, 0, 1);
    f.close().get()?;
    rule.disarm();
    Ok(())
}

/// `fallocate` without `FALLOC_FL_ZERO_RANGE` support: the allocation succeeds and later ones
/// on the same device skip the call; another error fails the allocation.
fn allocate_without_zero_ranges() -> Result<(), ExceptionPtr> {
    let (mut f, name) = rw_file("alloc")?;
    TARGET.store(fd_of(&name) as u64, Ordering::Release);
    let rule = arm(nr::FALLOCATE, Some(on_target_fd), EIO, 0, 1);
    let e = f.allocate(0, BLOCK as u64).get().expect_err("the failed allocation");
    assert_eq!(code_of(&e), Some(EIO), "{e}");
    assert_eq!(rule.disarm(), 1);
    let rule = arm(nr::FALLOCATE, Some(on_target_fd), EOPNOTSUPP, 0, ALWAYS);
    f.allocate(0, BLOCK as u64).get()?;
    f.allocate(BLOCK as u64, BLOCK as u64).get()?;
    assert_eq!(rule.disarm(), 1, "the second allocation skips the call");
    f.close().get()?;
    // `size()` of a plain (read-only) file seeks to the end; a failed seek fails it.
    let mut ro = open_file_dma(&name, OpenFlags::RO).get()?;
    TARGET.store(fd_of(&name) as u64, Ordering::Release);
    let rule = arm(nr::LSEEK, Some(on_target_fd), EIO, 0, 1);
    let e = ro.size().get().expect_err("the failed seek");
    assert_eq!(code_of(&e), Some(EIO), "{e}");
    assert_eq!(rule.disarm(), 1);
    ro.close().get()
}

/// A write of an output stream fails: the stream fails its flush with the error and its close
/// with the same error (`file_data_sink_impl`: the failed write-behind marks the sink failed).
fn output_stream_write_fails() -> Result<(), ExceptionPtr> {
    // With 2 and 1 writes behind (the later buffers see the sink failed and are not written)
    // and with none (each write is waited for).
    for (write_behind, blocks) in [(2u32, 3usize), (1, 16), (0, 3)] {
        let (f, _) = rw_file("stream")?;
        let opts = FileOutputStreamOptions {
            buffer_size: BLOCK as u32,
            write_behind,
            ..Default::default()
        };
        let mut out = make_file_output_stream(f, opts).get()?;
        let rule = arm(nr::IO_SUBMIT, Some(submits_disk_io), EBADF, 0, 1);
        let data = vec![b'k'; blocks * BLOCK];
        let wrote = out.write(&data).get();
        let flushed = wrote.and_then(|()| out.flush().get());
        let closed = out.close().get();
        assert_eq!(rule.disarm(), 1);
        let e = flushed.err().or(closed.err()).expect("the stream reports the failed write");
        assert_eq!(code_of(&e), Some(EBADF), "{e}");
    }
    // A read of an input stream that fails fails the stream's read with its error.
    let (mut f, name) = rw_file("instream")?;
    for i in 0..4 {
        write_block(&mut f, (i * BLOCK) as u64, b'q')?;
    }
    f.close().get()?;
    let f = open_file_dma(&name, OpenFlags::RO).get()?;
    let opts = FileInputStreamOptions { buffer_size: BLOCK, read_ahead: 2, ..Default::default() };
    let mut input = make_file_input_stream(f, opts);
    assert!(input.read().get()?.as_slice().iter().all(|&c| c == b'q'));
    let rule = arm(nr::IO_SUBMIT, Some(submits_disk_io), EBADF, 0, ALWAYS);
    let mut failed = None;
    for _ in 0..4 {
        match input.read().get() {
            Ok(b) if b.empty() => break,
            Ok(_) => {}
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    rule.disarm();
    let e = failed.expect("a read the kernel refused");
    assert_eq!(code_of(&e), Some(EBADF), "{e}");
    input.close().get()?;
    Ok(())
}

/// More reads in flight than the io_uring submission ring has entries (200) and than the
/// linux-aio context has control blocks: the backend flushes a full ring and runs completions
/// to free entries, or keeps the requests queued until blocks are free; all complete.
fn more_reads_than_ring_entries() -> Result<(), ExceptionPtr> {
    const READS: usize = 2048;
    let (mut f, _) = rw_file("many")?;
    for i in 0..8 {
        assert_eq!(write_block(&mut f, (i * BLOCK) as u64, b'0' + i as u8)?, BLOCK);
    }
    let reads: Vec<_> = (0..READS)
        .map(|i| {
            let pos = ((i % 8) * BLOCK) as u64;
            // SAFETY: no I/O intent.
            unsafe { f.dma_read_bulk::<u8>(pos, BLOCK, std::ptr::null_mut()) }
        })
        .collect();
    for (i, r) in reads.into_iter().enumerate() {
        let b = r.get()?;
        assert!(b.as_slice().iter().all(|&c| c == b'0' + (i % 8) as u8));
    }
    f.close().get()
}

/// The epoll backend's task-quota timer thread asks for `SCHED_FIFO`; refused, shard 0 warns
/// and the timer works without it.
fn refuse_fifo() {
    arm(nr::SCHED_SETSCHEDULER, None, EPERM, 0, ALWAYS);
}

/// An `io_submit` of an `IOCB_CMD_POLL` (the linux-aio backend's polling context, and the
/// probe of `detect_aio_poll`).
fn submits_poll(args: &[u64; 6]) -> bool {
    const IOCB_CMD_POLL: u16 = 5;
    // SAFETY: as in `submits_disk_io`.
    unsafe {
        if args[1] == 0 {
            return false;
        }
        let Some(first) = peek::<u64>(args[2]) else { return false };
        peek::<u16>(first + 16) == Some(IOCB_CMD_POLL)
    }
}

/// The probe's poll is refused: linux-aio is not offered and the engine runs on the backend
/// named (epoll), as `reactor_backend_selector::available` leaves linux-aio out when
/// `detect_aio_poll` fails.
fn refuse_poll_probe() {
    arm(nr::IO_SUBMIT, Some(submits_poll), EINVAL, 0, 1);
}

/// Once running, the polling context's next two submissions are refused with `EAGAIN`:
/// `aio_general_context::flush` retries them (within its one second) and the reactor goes on.
fn poll_submissions_are_retried() -> Result<(), ExceptionPtr> {
    let rule = arm(nr::IO_SUBMIT, Some(submits_poll), EAGAIN, 0, 2);
    for _ in 0..5 {
        teseladb::engine::core::sleep::sleep_steady(std::time::Duration::from_millis(10)).get()?;
        disk_io_works()?;
    }
    assert!(rule.disarm() <= 2);
    Ok(())
}

/// `io_getevents` (forced to be the system call) is interrupted: the reap counts no event and
/// the next one completes the requests.
fn interrupt_getevents() {
    arm(nr::IO_GETEVENTS, None, EINTR, 0, 3);
}

/// `epoll_pwait` is interrupted: the epoll backend returns to its loop.
fn interrupt_epoll_waits() {
    arm(nr::EPOLL_PWAIT, None, EINTR, 0, 3);
}

/// The detection's rings are made (the reactor options probe the backends three times, as
/// the reference's `reactor_options` constructor does: the candidates, the default and the
/// help text), the backend's own is refused: the start fails with the error of
/// `io_uring_setup` ("trying to create io_uring", `try_create_uring(.., true)`).
fn refuse_backend_ring() {
    arm(nr::IO_URING_SETUP, None, ENOMEM, 3, ALWAYS);
}

// `_IOR(0x12, 112, size_t)` and `_IOR(0x12, 114, size_t)`.
const BLKBSZGET: u64 = 0x8008_1270;
const BLKGETSIZE64: u64 = 0x8008_1272;

fn asks_block_size(args: &[u64; 6]) -> bool {
    args[1] == BLKBSZGET
}

fn asks_device_size(args: &[u64; 6]) -> bool {
    args[1] == BLKGETSIZE64
}

/// A block device whose block size cannot be read cannot be opened: the open fails with the
/// system error "ioctl(BLKBSZGET) failed" (`make_file_impl`); one whose size cannot be read
/// fails `size()` with that error. Skipped where no block device can be opened.
fn block_device_ioctl_errors() -> Result<(), ExceptionPtr> {
    let device = std::fs::read_dir("/dev").map_err(ExceptionPtr::new)?.flatten().find_map(|e| {
        use std::os::unix::fs::FileTypeExt;
        let p = e.path();
        let is_block = e.file_type().is_ok_and(|t| t.is_block_device());
        (is_block && std::fs::File::open(&p).is_ok()).then(|| p.display().to_string())
    });
    let Some(device) = device else { return Ok(()) };
    let rule = arm(nr::IOCTL, Some(asks_block_size), EIO, 0, 1);
    let opened = open_file_dma(&device, OpenFlags::RO).get();
    if rule.disarm() == 0 {
        // Not opened as a block device here (no direct I/O on it).
        return opened.map(|_| ());
    }
    let e = opened.err().expect("the open without a block size");
    let se = e.downcast_ref::<SystemError>().map(|s| (s.code(), s.to_string()));
    assert_eq!(se, Some((EIO, "ioctl(BLKBSZGET) failed: Input/output error".to_string())), "{e}");
    let mut f = open_file_dma(&device, OpenFlags::RO).get()?;
    let rule = arm(nr::IOCTL, Some(asks_device_size), EIO, 0, 1);
    let e = f.size().get().expect_err("the size without BLKGETSIZE64");
    assert_eq!(code_of(&e), Some(EIO), "{e}");
    assert_eq!(rule.disarm(), 1);
    f.close().get()
}

/// The non-blocking wrappers of `file_desc` give "no progress" (`None`) for `EAGAIN` and the
/// system error otherwise (`posix.hh`: `send`, `sendto`, `sendmsg`, `write`, `writev`); a
/// thread whose stack cannot be mapped, or that the kernel cannot create, is not started.
fn posix_wrappers_on_refused_calls() -> Result<(), ExceptionPtr> {
    use std::os::fd::IntoRawFd;
    use teseladb::engine::core::posix::{FileDesc, PosixThread};
    use teseladb::engine::core::reactor_backend::{Iovec, Msghdr};

    let (a, b) = std::os::unix::net::UnixDatagram::pair().map_err(ExceptionPtr::new)?;
    let fd = FileDesc::from_fd(a.into_raw_fd());
    let _peer = b;
    TARGET.store(fd.get() as u64, Ordering::Release);
    let msg = b"m";
    let iov = [Iovec { iov_base: msg.as_ptr() as *mut _, iov_len: 1 }];
    let mh = Msghdr { msg_iov: iov.as_ptr() as *mut Iovec, msg_iovlen: 1, ..Msghdr::default() };
    let to = fd.get_address()?;
    for (errno, expect_none) in [(EAGAIN, true), (EIO, false)] {
        let rules = [
            arm(nr::SENDTO, Some(on_target_fd), errno, 0, 2),
            arm(nr::SENDMSG, Some(on_target_fd), errno, 0, 1),
            arm(nr::WRITE, Some(on_target_fd), errno, 0, 1),
            arm(nr::WRITEV, Some(on_target_fd), errno, 0, 1),
        ];
        // SAFETY: the buffers, the vector and the header live across each call.
        let results = unsafe {
            [
                fd.send(msg.as_ptr().cast(), 1, 0),
                fd.sendto(&to, msg.as_ptr().cast(), 1, 0),
                fd.sendmsg(&mh, 0),
                fd.write(msg.as_ptr().cast(), 1),
                fd.writev(&iov),
            ]
        };
        for r in results {
            match r {
                Ok(n) => assert!(expect_none && n.is_none(), "{n:?} for errno {errno}"),
                Err(e) => assert!(!expect_none && code_of(&e) == Some(errno), "{e}"),
            }
        }
        for r in rules {
            assert!(r.disarm() >= 1);
        }
    }
    // Every call works again.
    // SAFETY: as above.
    assert_eq!(unsafe { fd.write(msg.as_ptr().cast(), 1) }?, Some(1));

    let stack = arm(nr::MMAP, Some(maps_a_thread_stack), ENOMEM, 0, 1);
    let e = PosixThread::new(Box::new(|| {})).err().expect("the thread without a stack");
    assert_eq!(code_of(&e), Some(ENOMEM), "{e}");
    assert_eq!(stack.disarm(), 1);
    let clone = arm(nr::CLONE3, None, EAGAIN, 0, 1);
    let made = PosixThread::new(Box::new(|| {}));
    if clone.disarm() == 1 {
        let e = made.err().expect("the thread the kernel refused");
        assert_eq!(code_of(&e), Some(EAGAIN), "{e}");
    } else {
        made?.join();
    }
    Ok(())
}

/// The anonymous mapping of a thread's default stack: 2 MiB and the 4 MiB guard area.
fn maps_a_thread_stack(args: &[u64; 6]) -> bool {
    args[1] == (6 << 20) && args[4] as i32 == -1
}

const AIO: &str = "--reactor-backend=linux-aio";
const EPOLL: &str = "--reactor-backend=epoll";
const URING: &str = "--reactor-backend=io_uring";

fn case(
    name: &'static str,
    args: &'static [&'static str],
    syscalls: &'static [i64],
    setup: fn(),
    body: fn() -> Result<(), ExceptionPtr>,
    exit_codes: &'static [i32],
) -> FaultCase {
    FaultCase { name, args, syscalls, setup, body, exit_codes }
}

fn cases() -> Vec<FaultCase> {
    const SUBMIT: &[i64] = &[nr::IO_SUBMIT];
    const FILE_CALLS: &[i64] = &[nr::CLOSE, nr::FTRUNCATE, nr::FALLOCATE, nr::LSEEK];
    vec![
        case("aio_eagain", &[AIO], SUBMIT, nothing, aio_eagain_is_submitted_again, OK),
        case("epoll_eagain", &[EPOLL], SUBMIT, nothing, aio_eagain_is_submitted_again, OK),
        case("aio_ebadf", &[AIO], SUBMIT, nothing, aio_ebadf_fails_the_request, OK),
        case("aio_einval", &[AIO], SUBMIT, nothing, aio_einval_is_not_supported, OK),
        case("aio_enotsup", &[AIO], SUBMIT, nothing, aio_enotsup_is_not_supported, OK),
        case(
            "aio_other_error",
            &[AIO, "--smp=1"],
            SUBMIT,
            nothing,
            aio_other_error_stops_the_reactor,
            &[1],
        ),
        case(
            "aio_retry_loop",
            &[AIO, "--kernel-page-cache=1"],
            SUBMIT,
            nothing,
            retry_loop_handles_refusals,
            OK,
        ),
        case(
            "uring_setup_refused_fallback",
            &[AIO],
            &[nr::IO_URING_SETUP],
            refuse_uring_setup,
            disk_io_works,
            OK,
        ),
        case(
            "uring_setup_refused_explicit",
            &[URING],
            &[nr::IO_URING_SETUP],
            refuse_uring_setup,
            idle,
            &[1],
        ),
        case("uring_sq_map_refused", &[AIO], &[nr::MMAP], refuse_sq_ring_map, disk_io_works, OK),
        case("uring_sqes_map_refused", &[AIO], &[nr::MMAP], refuse_sqes_map, disk_io_works, OK),
        case(
            "uring_probe_refused",
            &[AIO],
            &[nr::IO_URING_REGISTER],
            refuse_probe,
            disk_io_works,
            OK,
        ),
        case(
            "uring_dontfork_first",
            &[URING],
            &[nr::MADVISE],
            refuse_dontfork_first,
            disk_io_works,
            OK,
        ),
        case(
            "uring_dontfork_second",
            &[URING],
            &[nr::MADVISE],
            refuse_dontfork_second,
            disk_io_works,
            OK,
        ),
        case(
            "uring_wait_interrupted",
            &[URING],
            &[nr::IO_URING_ENTER],
            interrupt_uring_waits,
            disk_io_works,
            OK,
        ),
        case("close_errors", &[], FILE_CALLS, nothing, close_errors_are_reported, OK),
        case(
            "trim_errors",
            &[],
            FILE_CALLS,
            nothing,
            failed_trims_fail_flush_and_are_reported_on_close,
            OK,
        ),
        case("allocate_errors", &[], FILE_CALLS, nothing, allocate_without_zero_ranges, OK),
        case("stream_write_fails", &[AIO], SUBMIT, nothing, output_stream_write_fails, OK),
        case("uring_many_reads", &[URING], &[], nothing, more_reads_than_ring_entries, OK),
        case("aio_many_reads", &[AIO], &[], nothing, more_reads_than_ring_entries, OK),
        case(
            "epoll_fifo_refused",
            &[EPOLL],
            &[nr::SCHED_SETSCHEDULER],
            refuse_fifo,
            disk_io_works,
            OK,
        ),
        case("aio_poll_probe_refused", &[EPOLL], SUBMIT, refuse_poll_probe, disk_io_works, OK),
        case("aio_poll_submit_eagain", &[AIO], SUBMIT, nothing, poll_submissions_are_retried, OK),
        case(
            "aio_getevents_interrupted",
            &[AIO, "--force-aio-syscalls=1"],
            &[nr::IO_GETEVENTS],
            interrupt_getevents,
            disk_io_works,
            OK,
        ),
        case(
            "epoll_wait_interrupted",
            &[EPOLL],
            &[nr::EPOLL_PWAIT],
            interrupt_epoll_waits,
            disk_io_works,
            OK,
        ),
        case(
            "uring_backend_ring_refused",
            &[URING],
            &[nr::IO_URING_SETUP],
            refuse_backend_ring,
            idle,
            &[1],
        ),
        case(
            "posix_wrappers",
            &[],
            &[nr::SENDTO, nr::SENDMSG, nr::WRITE, nr::WRITEV, nr::MMAP, nr::CLONE3],
            nothing,
            posix_wrappers_on_refused_calls,
            OK,
        ),
        case("block_device_ioctls", &[], &[nr::IOCTL], nothing, block_device_ioctl_errors, OK),
    ]
}

pub(crate) fn main() {
    fault_program::main(cases());
}
