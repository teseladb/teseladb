//! Tests of scheduling groups and their I/O classes: names, shares, I/O bandwidth limits,
//! file I/O issued from a group, and the names of the I/O request operations.
#![cfg(target_os = "linux")]

use std::ffi::c_void;

use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::internal::io_request::{IoRequest, Operation};
use teseladb::engine::core::reactor::{create_scheduling_group, destroy_scheduling_group};
use teseladb::engine::core::scheduling::current_scheduling_group;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb::engine::util::internal::iovec_utils::Iovec;
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

thread_case!(groups_have_names_shares_and_bandwidth, {
    let sg = create_scheduling_group(Sstring::from("cov-group"), Sstring::from("cov"), 100.0)
        .get()
        .unwrap();
    assert_eq!(sg.name().as_str(), "cov-group");
    // Short names are padded to four columns for the log lines.
    assert_eq!(sg.short_name().as_str(), " cov");
    assert_eq!(sg.get_shares(), 100.0);
    sg.set_shares(250.0);
    assert_eq!(sg.get_shares(), 250.0);
    sg.update_io_bandwidth(64 << 20).get().unwrap();
    // Zero lifts the limit.
    sg.update_io_bandwidth(0).get().unwrap();

    // File I/O issued from the group goes through its I/O class.
    let path = std::env::temp_dir().join(format!("cov_sched_io_{}", std::process::id()));
    let name = path.display().to_string();
    let n2 = name.clone();
    // The thread started inside the group runs in it.
    let read_back = with_scheduling_group(sg, move || {
        thread_async(move || {
            assert_eq!(current_scheduling_group().name().as_str(), "cov-group");
            let mut f = open_file_dma(&n2, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE)
                .get()?;
            let align = f.disk_write_dma_alignment() as usize;
            let mut buf = TemporaryBuffer::<u8>::aligned(align, 2 * align);
            // SAFETY: the buffer owns `2 * align` writable bytes.
            unsafe { std::ptr::write_bytes(buf.get_write(), b'q', 2 * align) };
            let null = std::ptr::null_mut();
            // SAFETY: the buffer is aligned and stays alive until the write completes.
            let written = unsafe { f.dma_write(0, buf.get(), align, null) }.get()?;
            assert_eq!(written, align);
            // A gathered write of the second block.
            let iov = vec![Iovec { iov_base: buf.get_write().cast::<c_void>(), iov_len: align }];
            // SAFETY: as above; the vector's buffer outlives the write.
            assert_eq!(unsafe { f.dma_write_iov(align as u64, iov, null) }.get()?, align);
            f.flush().get()?;
            // SAFETY: no intent.
            let bulk = unsafe { f.dma_read_bulk::<u8>(0, 2 * align, null) }.get()?;
            assert_eq!(bulk.size(), 2 * align);
            // SAFETY: no intent.
            let one = unsafe { f.dma_read_buf::<u8>(align as u64, align, null) }.get()?;
            assert_eq!(one.size(), align);
            // SAFETY: no intent.
            let exact = unsafe { f.dma_read_exactly::<u8>(0, align, null) }.get()?;
            assert_eq!(exact.size(), align);
            // Reading exactly past the end fails.
            // SAFETY: no intent.
            assert!(
                unsafe { f.dma_read_exactly::<u8>(4 * align as u64, align, null) }.get().is_err()
            );
            let data = bulk;
            f.close().get()?;
            Ok::<Vec<u8>, ExceptionPtr>(data.as_slice().to_vec())
        })
    })
    .get()
    .unwrap();
    assert!(read_back.iter().all(|&b| b == b'q'));
    std::fs::remove_file(&name).unwrap();

    destroy_scheduling_group(sg).get().unwrap();
});

#[test]
fn io_requests_name_their_operation() {
    let mut byte = 0u8;
    let p = (&mut byte as *mut u8).cast::<c_void>();
    let mut iov = Vec::new();
    let msg = std::ptr::null_mut();
    let mut len = 0u32;
    for (req, op, name) in [
        (IoRequest::make_read(0, 0, p, 1, false), Operation::Read, "read"),
        (IoRequest::make_readv(0, 0, &mut iov, false), Operation::Readv, "vectored read"),
        (IoRequest::make_write(0, 0, p, 1, true), Operation::Write, "write"),
        (IoRequest::make_writev(0, 0, &mut iov, true), Operation::Writev, "vectored write"),
        (IoRequest::make_recv(0, p, 1, 0), Operation::Recv, "recv"),
        (IoRequest::make_recvmsg(0, msg, 0), Operation::Recvmsg, "recvmsg"),
        (IoRequest::make_send(0, p, 1, 0), Operation::Send, "send"),
        (IoRequest::make_sendmsg(0, msg, 0), Operation::Sendmsg, "sendmsg"),
        (IoRequest::make_fdatasync(0), Operation::Fdatasync, "fdatasync"),
        (IoRequest::make_accept(0, p, &mut len, 0), Operation::Accept, "accept"),
        (IoRequest::make_connect(0, p, 0), Operation::Connect, "connect"),
        (IoRequest::make_poll_add(0, 1), Operation::PollAdd, "poll add"),
        (IoRequest::make_poll_remove(0, p), Operation::PollRemove, "poll remove"),
        (IoRequest::make_cancel(0, p), Operation::Cancel, "cancel"),
    ] {
        assert_eq!(req.opcode(), op);
        assert_eq!(req.opname().as_str(), name);
    }
}
