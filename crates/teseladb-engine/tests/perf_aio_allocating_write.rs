//! A write that lands where an append-challenged file has no blocks yet goes from the
//! linux-aio (and epoll) backend to the syscall thread at once, instead of first being
//! submitted from the reactor with `RWF_NOWAIT`, which on ext4, XFS and btrfs only fails
//! with `EAGAIN` (the block allocation would block) and is then retried there anyway.
//! Writes over blocks the file already has keep the `RWF_NOWAIT` submission from the reactor.
//!
//! The `io_submit` calls are watched by the seccomp filter of `common/syscall_faults.rs`, with
//! no hook in the engine: a rule that fails nothing counts the submissions of disk writes that
//! carry `RWF_NOWAIT` (only the reactor makes those; the syscall thread clears the flag).
//! Each case runs in a process of its own. The file lives in `CARGO_TARGET_TMPDIR` (or
//! `AIO_TEST_DIR`), which must be on a file system where the engine uses `RWF_NOWAIT`
//! (ext4, XFS or btrfs); elsewhere the cases say so and pass without checking.
#![cfg(target_os = "linux")]
// The DMA writes and the matcher that reads the control blocks of intercepted calls are
// unsafe; every `unsafe` block states why it is sound.
#![allow(unsafe_code)]

#[path = "common/fault_program.rs"]
mod fault_program;
// Only its `fs_magic` is used here.
#[allow(dead_code)]
#[path = "common/fs_kind.rs"]
mod fs_kind;
#[path = "common/syscall_faults.rs"]
mod syscall_faults;

use fault_program::{FaultCase, OK, nothing};
use syscall_faults::{arm, nr, peek};
use teseladb::engine::core::file::{File, open_file_dma};
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;

const BLOCK: usize = 4096;
const WRITES: u64 = 8;

const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_PWRITEV: u16 = 8;
const RWF_NOWAIT: i32 = 0x8;

const EXT4_MAGIC: i64 = 0xEF53;
const XFS_MAGIC: i64 = 0x5846_5342;
const BTRFS_MAGIC: i64 = 0x9123_683E;

/// An `io_submit` with a disk write that carries `RWF_NOWAIT`.
fn submits_nowait_write(args: &[u64; 6]) -> bool {
    // SAFETY: `io_submit(ctx, nr, iocbpp)`: the caller waits in the kernel while its array of
    // `nr` control block pointers, and the blocks, stay as they are.
    unsafe {
        (0..args[1]).any(|i| {
            let Some(iocb) = peek::<u64>(args[2] + i * 8) else { return false };
            // `aio_rw_flags` is at offset 12 of `struct iocb`, `aio_lio_opcode` at 16.
            let op = peek::<u16>(iocb + 16);
            let flags = peek::<i32>(iocb + 12).unwrap_or(0);
            matches!(op, Some(IOCB_CMD_PWRITE | IOCB_CMD_PWRITEV)) && flags & RWF_NOWAIT != 0
        })
    }
}

fn test_dir() -> String {
    std::env::var("AIO_TEST_DIR")
        .ok()
        .or_else(|| option_env!("CARGO_TARGET_TMPDIR").map(str::to_string))
        .unwrap_or_else(|| std::env::temp_dir().display().to_string())
}

/// A new file in the test directory, or `None` when its file system does not take
/// `RWF_NOWAIT` writes from the engine.
fn new_file(tag: &str) -> Result<Option<File>, ExceptionPtr> {
    let dir = format!("{}/perf_aio_allocating_write_{}", test_dir(), std::process::id());
    std::fs::create_dir_all(&dir).expect("the case's directory");
    let magic = fs_kind::fs_magic(&dir);
    if ![EXT4_MAGIC, XFS_MAGIC, BTRFS_MAGIC].contains(&magic) {
        println!("{dir} is on file system {magic:#x}, without RWF_NOWAIT: nothing to check");
        return Ok(None);
    }
    let path = format!("{dir}/{tag}");
    let f = open_file_dma(&path, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE).get()?;
    Ok(Some(f))
}

fn write_block(f: &mut File, pos: u64, byte: u8) -> Result<usize, ExceptionPtr> {
    let mut b = TemporaryBuffer::<u8>::aligned(BLOCK, BLOCK);
    // SAFETY: the buffer holds BLOCK writable bytes.
    unsafe { std::ptr::write_bytes(b.get_write(), byte, BLOCK) };
    // SAFETY: the buffer lives until the write resolves.
    unsafe { f.dma_write::<u8>(pos, b.get(), BLOCK, std::ptr::null_mut()) }.get()
}

fn read_block(f: &mut File, pos: u64) -> Result<TemporaryBuffer<u8>, ExceptionPtr> {
    // SAFETY: no I/O intent.
    unsafe { f.dma_read_bulk::<u8>(pos, BLOCK, std::ptr::null_mut()) }.get()
}

/// Counts, with a rule that fails nothing, the `RWF_NOWAIT` write submissions `body` makes.
fn nowait_writes(body: impl FnOnce() -> Result<(), ExceptionPtr>) -> Result<u32, ExceptionPtr> {
    let rule = arm(nr::IO_SUBMIT, Some(submits_nowait_write), 0, u32::MAX, 0);
    body()?;
    let seen = rule.seen();
    rule.disarm();
    Ok(seen)
}

/// Appending writes go to the syscall thread without a `RWF_NOWAIT` attempt, and land.
fn appending_writes_skip_nowait() -> Result<(), ExceptionPtr> {
    let Some(mut f) = new_file("append")? else { return Ok(()) };
    let seen = nowait_writes(|| {
        for i in 0..WRITES {
            assert_eq!(write_block(&mut f, i * BLOCK as u64, b'a' + i as u8)?, BLOCK);
        }
        Ok(())
    })?;
    assert_eq!(seen, 0, "appending writes submitted with RWF_NOWAIT from the reactor");
    for i in 0..WRITES {
        let b = read_block(&mut f, i * BLOCK as u64)?;
        assert!(b.as_slice().iter().all(|&c| c == b'a' + i as u8), "block {i}");
    }
    assert_eq!(f.size().get()?, WRITES * BLOCK as u64);
    f.close().get()
}

/// A write over blocks the file has keeps the `RWF_NOWAIT` submission; after a truncation
/// drops blocks, a write past the new size skips it again.
fn overwrites_keep_nowait() -> Result<(), ExceptionPtr> {
    let Some(mut f) = new_file("overwrite")? else { return Ok(()) };
    for i in 0..2 {
        write_block(&mut f, i * BLOCK as u64, b'x')?;
    }
    let seen = nowait_writes(|| write_block(&mut f, 0, b'y').map(|_| ()))?;
    assert_eq!(seen, 1, "an overwrite tried from the reactor with RWF_NOWAIT");
    f.truncate(BLOCK as u64).get()?;
    let seen = nowait_writes(|| write_block(&mut f, BLOCK as u64, b'z').map(|_| ()))?;
    assert_eq!(seen, 0, "a write past a truncation submitted with RWF_NOWAIT");
    assert!(read_block(&mut f, 0)?.as_slice().iter().all(|&c| c == b'y'));
    assert!(read_block(&mut f, BLOCK as u64)?.as_slice().iter().all(|&c| c == b'z'));
    f.close().get()
}

const AIO: &str = "--reactor-backend=linux-aio";
const EPOLL: &str = "--reactor-backend=epoll";

fn cases() -> Vec<FaultCase> {
    const SUBMIT: &[i64] = &[nr::IO_SUBMIT];
    let case = |name, args, body| FaultCase {
        name,
        args,
        syscalls: SUBMIT,
        setup: nothing,
        body,
        exit_codes: OK,
    };
    vec![
        case("aio_appending", &[AIO], appending_writes_skip_nowait as fn() -> _),
        case("epoll_appending", &[EPOLL], appending_writes_skip_nowait),
        case("aio_overwrite", &[AIO], overwrites_keep_nowait),
        case("epoll_overwrite", &[EPOLL], overwrites_keep_nowait),
    ]
}

pub(crate) fn main() {
    fault_program::main(cases());
}
