//! Pollable descriptor operations in every backend (the original's pollable_fd and the
//! backends' `read`, `read_some`, `recvmsg` and `recv_some`): two descriptors of one socket
//! (a `dup`) both wait for readability; one byte arrives and wakes both; the first reads it and
//! the second finds nothing (`EAGAIN`), polls again and completes when more data arrives; reads
//! that fill their buffer exactly leave the descriptor speculated readable; the byte, raw
//! write and readiness entry points of a pollable descriptor.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;

use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::internal::buffer_allocator::BufferAllocator;
use teseladb::engine::core::internal::pollable_fd::PollableFd;
use teseladb::engine::core::posix::FileDesc;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::util::internal::iovec_utils::Iovec;
use teseladb::engine_test_case;

/// Two connected non-blocking stream sockets, and a second descriptor of the reading end.
fn pair_and_dup() -> (PollableFd, PollableFd, PollableFd) {
    let (a, b) = UnixStream::pair().expect("socketpair");
    a.set_nonblocking(true).unwrap();
    b.set_nonblocking(true).unwrap();
    let b = FileDesc::from_fd(b.into_raw_fd());
    let b2 = b.dup().unwrap();
    (
        PollableFd::from_file_desc(FileDesc::from_fd(a.into_raw_fd())),
        PollableFd::from_file_desc(b),
        PollableFd::from_file_desc(b2),
    )
}

fn send(fd: &mut PollableFd, data: &[u8]) -> Result<(), ExceptionPtr> {
    // SAFETY: `data` outlives the write, resolved here.
    unsafe { fd.write_all_chars(data.as_ptr(), data.len()) }.get()
}

/// Buffers of `n` bytes.
struct Fixed(usize);

impl BufferAllocator for Fixed {
    fn allocate_buffer(&mut self) -> TemporaryBuffer<u8> {
        TemporaryBuffer::new(self.0)
    }
}

/// Starts `first` and `second` reads on the two descriptors of the reading end, sends one
/// byte (taken by one of them), then a second one, and checks that both complete with one
/// byte each.
fn race<T: 'static>(
    a: &mut PollableFd,
    first: Future<T>,
    second: Future<T>,
    bytes: impl Fn(T) -> usize,
) -> Result<(), ExceptionPtr> {
    yield_().get()?;
    assert!(!first.available() && !second.available());
    send(a, b"1")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !first.available() && !second.available() && std::time::Instant::now() < deadline {
        sleep(std::time::Duration::from_millis(2)).get()?;
    }
    // Give the other read time to be woken and to poll again.
    sleep(std::time::Duration::from_millis(20)).get()?;
    let done = usize::from(first.available()) + usize::from(second.available());
    if done != 1 {
        // Unblock both before failing, so that no read is left pending on freed buffers.
        send(a, b"23")?;
        let _ = (first.get(), second.get());
        panic!("{done} reads completed after one byte; one byte satisfies one read");
    }
    send(a, b"2")?;
    assert_eq!(bytes(first.get()?) + bytes(second.get()?), 2);
    Ok(())
}

engine_test_case!(a_read_woken_without_data_polls_again, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b, mut c) = pair_and_dup();
        let mut x = [0u8; 4];
        let mut y = [0u8; 4];
        // SAFETY: the buffers outlive the reads, resolved in race().
        let (f1, f2) = unsafe {
            (b.read_some_bytes(x.as_mut_ptr(), x.len()), c.read_some_chars(y.as_mut_ptr(), y.len()))
        };
        race(&mut a, f1, f2, |n| n)
    })
});

engine_test_case!(a_buffer_read_woken_without_data_polls_again, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b, mut c) = pair_and_dup();
        let mut alloc1 = Fixed(8);
        let mut alloc2 = Fixed(8);
        let p1: *mut dyn BufferAllocator = &mut alloc1;
        let p2: *mut dyn BufferAllocator = &mut alloc2;
        // SAFETY: the allocators outlive the reads, resolved in race().
        let (f1, f2) = unsafe { (b.read_some(p1), c.read_some(p2)) };
        race(&mut a, f1, f2, |buf| buf.size())
    })
});

engine_test_case!(a_receive_woken_without_data_polls_again, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b, mut c) = pair_and_dup();
        let mut alloc1 = Fixed(8);
        let mut alloc2 = Fixed(8);
        let p1: *mut dyn BufferAllocator = &mut alloc1;
        let p2: *mut dyn BufferAllocator = &mut alloc2;
        // SAFETY: the allocators outlive the receives, resolved in race().
        let (f1, f2) = unsafe { (b.recv_some(p1), c.recv_some(p2)) };
        race(&mut a, f1, f2, |buf| buf.size())
    })
});

engine_test_case!(a_scattered_read_woken_without_data_polls_again, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b, mut c) = pair_and_dup();
        let mut x = [0u8; 4];
        let mut y = [0u8; 4];
        let vx = vec![Iovec { iov_base: x.as_mut_ptr().cast::<c_void>(), iov_len: x.len() }];
        let vy = vec![Iovec { iov_base: y.as_mut_ptr().cast::<c_void>(), iov_len: y.len() }];
        let (f1, f2) = (b.read_some_iov(&vx), c.read_some_iov(&vy));
        race(&mut a, f1, f2, |n| n)
    })
});

engine_test_case!(reads_that_fill_their_buffer_speculate_more_data, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b, _c) = pair_and_dup();
        // Waiting reads, each filled exactly by the bytes sent while it waits.
        let mut x = [0u8; 3];
        // SAFETY: the buffer outlives the read, resolved below.
        let f = unsafe { b.read_some_bytes(x.as_mut_ptr(), x.len()) };
        yield_().get()?;
        send(&mut a, b"abc")?;
        assert_eq!(f.get()?, 3);
        assert_eq!(&x, b"abc");
        let mut alloc = Fixed(2);
        let p: *mut dyn BufferAllocator = &mut alloc;
        // SAFETY: the allocator outlives the read, resolved below.
        let f = unsafe { b.read_some(p) };
        yield_().get()?;
        send(&mut a, b"de")?;
        assert_eq!(f.get()?.as_slice(), b"de");
        // SAFETY: as above.
        let f = unsafe { b.recv_some(p) };
        yield_().get()?;
        send(&mut a, b"fg")?;
        assert_eq!(f.get()?.as_slice(), b"fg");
        // Readiness: the writing end is writeable, the reading end readable once data is there.
        a.writeable().get()?;
        send(&mut a, b"h")?;
        b.readable_or_writeable().get()?;
        let mut z = [0u8; 1];
        // SAFETY: as above.
        assert_eq!(unsafe { b.read_some_bytes(z.as_mut_ptr(), 1) }.get()?, 1);
        Ok(())
    })
});
