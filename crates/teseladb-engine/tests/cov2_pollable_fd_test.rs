//! Scattered reads and plain sends on a pollable descriptor, in every reactor backend (the
//! original's `pollable_fd::read_some(const std::vector<iovec>&)` and
//! `write_all(const char*, size_t)`, which the backends serve with `recvmsg` and `send`): a
//! read waiting on an empty socket completes when data arrives and fills the vectors in order;
//! a read whose vectors are filled exactly leaves the socket speculated readable; data already
//! there is read at once; and a send from a fresh descriptor arrives whole.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::internal::pollable_fd::PollableFd;
use teseladb::engine::core::posix::FileDesc;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::thread::async_;
use teseladb::engine::util::internal::iovec_utils::Iovec;
use teseladb::engine_test_case;

/// Two connected non-blocking stream sockets as pollable descriptors.
fn pair() -> (PollableFd, PollableFd) {
    let (a, b) = UnixStream::pair().expect("socketpair");
    a.set_nonblocking(true).unwrap();
    b.set_nonblocking(true).unwrap();
    (
        PollableFd::from_file_desc(FileDesc::from_fd(a.into_raw_fd())),
        PollableFd::from_file_desc(FileDesc::from_fd(b.into_raw_fd())),
    )
}

fn iov(buf: &mut [u8]) -> Iovec {
    Iovec {
        iov_base: buf.as_mut_ptr().cast::<c_void>(),
        iov_len: buf.len(),
    }
}

fn send(fd: &mut PollableFd, data: &[u8]) -> Result<(), ExceptionPtr> {
    // SAFETY: `data` outlives the write, resolved here.
    unsafe { fd.write_all_bytes(data.as_ptr(), data.len()) }.get()
}

engine_test_case!(a_waiting_scattered_read_completes_when_data_arrives, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b) = pair();
        let mut head = [0u8; 3];
        let mut tail = [0u8; 10];
        let v = vec![iov(&mut head), iov(&mut tail)];
        let pending = b.read_some_iov(&v);
        yield_().get()?;
        assert!(!pending.available(), "nothing to read yet");
        send(&mut a, b"hello")?;
        assert_eq!(pending.get()?, 5);
        assert_eq!(&head, b"hel");
        assert_eq!(&tail[..2], b"lo");
        Ok(())
    })
});

engine_test_case!(exactly_filled_vectors_and_data_already_there, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b) = pair();
        send(&mut a, b"0123456789")?;
        let mut x = [0u8; 4];
        let mut y = [0u8; 2];
        let v = vec![iov(&mut x), iov(&mut y)];
        // Six of the ten bytes fill both vectors exactly.
        assert_eq!(b.read_some_iov(&v).get()?, 6);
        assert_eq!((&x, &y), (b"0123", b"45"));
        // The rest is there (the socket was left speculated readable).
        let mut z = [0u8; 8];
        let v = vec![iov(&mut z)];
        assert_eq!(b.read_some_iov(&v).get()?, 4);
        assert_eq!(&z[..4], b"6789");
        Ok(())
    })
});

engine_test_case!(a_send_from_a_fresh_descriptor_arrives_whole, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (mut a, mut b) = pair();
        let msg: Vec<u8> = (0..200u8).collect();
        send(&mut a, &msg)?;
        let mut got = Vec::new();
        while got.len() < msg.len() {
            let mut buf = [0u8; 64];
            let v = vec![iov(&mut buf)];
            let n = b.read_some_iov(&v).get()?;
            assert!(n > 0, "the peer is open");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, msg);
        // Back the other way, after the first send made the descriptor writeable.
        send(&mut b, b"ack")?;
        let mut r = [0u8; 3];
        let v = vec![iov(&mut r)];
        assert_eq!(a.read_some_iov(&v).get()?, 3);
        assert_eq!(&r, b"ack");
        Ok(())
    })
});
