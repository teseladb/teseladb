//! Tests of the file-descriptor wrapper and the POSIX helpers outside a reactor: temporary
//! files, duplication and reassignment, sizes and truncation, positional and gathered I/O,
//! memory mappings, socket options, addresses, listening and accepting, signal masks, kernel
//! errors and threads with their own stack and affinity.
#![cfg(target_os = "linux")]

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use teseladb::engine::core::future::SystemError;
use teseladb::engine::core::posix::{
    CpuSetT, FileDesc, PosixThread, PosixThreadAttr, StackSize, kernel_error,
    make_full_sigset_mask, make_sigset_mask, throw_kernel_error, throw_pthread_error,
    throw_system_error_on,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use teseladb::engine::util::internal::iovec_utils::Iovec;

const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0o100;
const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;
const SOCK_NONBLOCK: i32 = 0o4000;
const SOL_SOCKET: i32 = 1;
const SO_REUSEADDR: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_RCVBUF: i32 = 8;
const SO_BINDTODEVICE: i32 = 25;
const IPPROTO_TCP: i32 = 6;
const TCP_NODELAY: i32 = 1;
const TCP_CONGESTION: i32 = 13;
const SHUT_WR: i32 = 1;
const EPOLL_CLOEXEC: i32 = 0o2000000;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

fn tmp_dir() -> Sstring {
    Sstring::from(std::env::temp_dir().display().to_string().as_str())
}

#[test]
fn temporary_files_hold_data_and_resize() {
    let f = FileDesc::temporary(tmp_dir()).unwrap();
    assert!(f.get() >= 0);
    assert_eq!(f.size().unwrap(), 0);
    let data = b"hello posix";
    // SAFETY: the buffer is valid for its length.
    assert_eq!(unsafe { f.write(data.as_ptr().cast(), data.len()) }.unwrap(), Some(data.len()));
    let mut a = *b"AAA";
    let mut b = *b"BB";
    let iov = [
        Iovec { iov_base: a.as_mut_ptr().cast::<c_void>(), iov_len: a.len() },
        Iovec { iov_base: b.as_mut_ptr().cast::<c_void>(), iov_len: b.len() },
    ];
    // SAFETY: both buffers are valid for their lengths.
    assert_eq!(unsafe { f.writev(&iov) }.unwrap(), Some(5));
    assert_eq!(f.size().unwrap(), data.len() + 5);
    let mut buf = [0u8; 5];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { f.pread(buf.as_mut_ptr().cast(), buf.len(), 6) }.unwrap();
    assert_eq!(&buf[..n], b"posix");
    f.truncate(4).unwrap();
    assert_eq!(f.size().unwrap(), 4);

    // A missing directory cannot hold a temporary file.
    assert!(FileDesc::temporary(Sstring::from("/nonexistent/dir")).is_err());
}

#[test]
fn duplicates_and_reassignment_share_the_file() {
    let f = FileDesc::temporary(tmp_dir()).unwrap();
    f.truncate(10).unwrap();
    let d = f.dup().unwrap();
    assert_ne!(d.get(), f.get());
    assert_eq!(d.size().unwrap(), 10);
    let mut target = FileDesc::temporary(tmp_dir()).unwrap();
    target.assign(d).unwrap();
    assert_eq!(target.size().unwrap(), 10);
    target.close().unwrap();
    // The target of the descriptor: the unlinked temporary file.
    let info = f.fdinfo();
    assert!(info.as_str().ends_with("(deleted)"), "{info}");
    let closed = FileDesc::from_fd(-1);
    assert!(closed.fdinfo().as_str().starts_with("error("), "{}", closed.fdinfo());
    std::mem::forget(closed);
}

#[test]
fn files_map_into_memory() {
    let path = std::env::temp_dir().join(format!("cov_mmap_{}", std::process::id()));
    let f = FileDesc::open(path.to_str().unwrap(), O_RDWR | O_CREAT, 0o600).unwrap();
    f.truncate(4096).unwrap();
    {
        let rw = f.map_shared_rw(4096, 0).unwrap();
        // SAFETY: the mapping is 4096 writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(b"mapped".as_ptr(), rw.get(), 6) };
    }
    let ro = f.map_shared_ro(4096, 0).unwrap();
    // SAFETY: the mapping is 4096 readable bytes.
    assert_eq!(unsafe { std::slice::from_raw_parts(ro.get(), 6) }, b"mapped");
    let private = f.map_private_rw(4096, 0).unwrap();
    // SAFETY: a private writable mapping of 4096 bytes.
    unsafe { *private.get() = b'M' };
    let private_ro = f.map_private_ro(4096, 0).unwrap();
    // SAFETY: a readable mapping; the private write is not visible in the file.
    assert_eq!(unsafe { *private_ro.get() }, b'm');
    let raw = private.release();
    assert!(!raw.is_null());
    assert!(f.map_shared_rw(4096, 1).is_err(), "unaligned offset");
    std::fs::remove_file(&path).unwrap();
    assert!(FileDesc::open("/nonexistent/file", O_RDWR, 0).is_err());
}

fn loopback() -> SocketAddress {
    SocketAddress::from_ipv4_addr(Ipv4Addr::from_string("127.0.0.1:0").unwrap())
}

#[test]
fn sockets_options_addresses_and_connections() {
    let listener = FileDesc::socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0).unwrap();
    listener.setsockopt_int(SOL_SOCKET, SO_REUSEADDR, 1).unwrap();
    assert_eq!(listener.getsockopt_int(SOL_SOCKET, SO_REUSEADDR).unwrap(), 1);
    // SAFETY: SO_TYPE is an int.
    assert_eq!(unsafe { listener.getsockopt::<i32>(SOL_SOCKET, SO_TYPE) }.unwrap(), SOCK_STREAM);
    listener.setsockopt(SOL_SOCKET, SO_RCVBUF, 65536i32).unwrap();
    assert!(listener.getsockopt_int(SOL_SOCKET, SO_RCVBUF).unwrap() >= 65536);
    let one = 1i32;
    // SAFETY: an int option with its size.
    unsafe { listener.setsockopt_raw(IPPROTO_TCP, TCP_NODELAY, (&one as *const i32).cast(), 4) }
        .unwrap();
    listener.setsockopt_str(IPPROTO_TCP, TCP_CONGESTION, "cubic").unwrap();
    let mut cc = [0u8; 16];
    // The call returns the system call's result; the name is NUL-terminated in the buffer.
    assert_eq!(listener.getsockopt_buf(IPPROTO_TCP, TCP_CONGESTION, &mut cc).unwrap(), 0);
    assert!(cc.starts_with(b"cubic"));
    assert!(listener.setsockopt_str(SOL_SOCKET, SO_BINDTODEVICE, "nosuchdev0").is_err());

    let addr = loopback();
    // SAFETY: a sockaddr of the given length.
    unsafe { listener.bind(addr.as_posix_sockaddr(), addr.length()) }.unwrap();
    listener.listen(16).unwrap();
    let bound = listener.get_address().unwrap();
    assert!(bound.port() != 0);
    let mut peer = SocketAddress::default();
    assert!(listener.try_accept(&mut peer, 0).unwrap().is_none(), "nobody connected yet");
    // A listening socket has no remote end.
    assert!(listener.get_remote_address().is_err());

    let client = FileDesc::socket(AF_INET, SOCK_STREAM, 0).unwrap();
    // SAFETY: a sockaddr of the given length.
    unsafe { client.connect(bound.as_posix_sockaddr(), bound.length()) }.unwrap();
    assert_eq!(client.get_remote_address().unwrap().port(), bound.port());
    let server = listener.accept(&mut peer, 0).unwrap();
    assert_eq!(peer.port(), client.get_address().unwrap().port());

    // SAFETY: the buffers are valid for their lengths.
    unsafe {
        assert_eq!(client.send(b"ping".as_ptr().cast(), 4, 0).unwrap(), Some(4));
        let mut buf = [0u8; 8];
        assert_eq!(server.recv(buf.as_mut_ptr().cast(), buf.len(), 0).unwrap(), Some(4));
        assert_eq!(&buf[..4], b"ping");
    }
    client.shutdown(SHUT_WR).unwrap();
    let mut buf = [0u8; 4];
    // SAFETY: as above.
    assert_eq!(
        unsafe { server.read(buf.as_mut_ptr().cast(), 4) }.unwrap(),
        Some(0),
        "end of stream"
    );
    // A socket that is not connected cannot be connected to an unbound port twice.
    let refused = FileDesc::socket(AF_INET, SOCK_STREAM, 0).unwrap();
    let closed = loopback();
    // SAFETY: a sockaddr of the given length.
    assert!(unsafe { refused.connect(closed.as_posix_sockaddr(), closed.length()) }.is_err());
    // Socket calls on a file fail.
    let file = FileDesc::temporary(tmp_dir()).unwrap();
    assert!(file.getsockopt_int(SOL_SOCKET, SO_TYPE).is_err());
    assert!(file.listen(1).is_err());
    assert!(file.accept(&mut peer, 0).is_err());

    let ep = FileDesc::epoll_create(EPOLL_CLOEXEC).unwrap();
    assert!(ep.get() >= 0);
}

#[test]
fn signal_masks_and_kernel_errors() {
    let one = make_sigset_mask(SIGINT);
    let bit = |set: &teseladb::engine::core::reactor::SigsetT, sig: i32| {
        let n = (sig - 1) as usize;
        set.__val[n / 64] & (1 << (n % 64)) != 0
    };
    assert!(bit(&one, SIGINT));
    assert!(!bit(&one, SIGTERM));
    let all = make_full_sigset_mask();
    assert!(bit(&all, SIGINT) && bit(&all, SIGTERM));

    assert!(throw_kernel_error(5).is_ok());
    let e = throw_kernel_error(-2).unwrap_err();
    assert_eq!(e.downcast_ref::<SystemError>().unwrap().code(), 2);
    assert_eq!(kernel_error(-13).downcast_ref::<SystemError>().unwrap().code(), 13);
    assert!(throw_pthread_error(0).is_ok());
    assert_eq!(
        throw_pthread_error(11).unwrap_err().downcast_ref::<SystemError>().unwrap().code(),
        11
    );
    assert!(throw_system_error_on(false, "nothing").is_ok());
    let e = throw_system_error_on(true, "something").unwrap_err();
    assert!(e.to_string().contains("something"), "{e}");
}

#[test]
fn posix_threads_run_with_their_attributes() {
    let ran = Arc::new(AtomicBool::new(false));
    let r = ran.clone();
    PosixThread::new(Box::new(move || r.store(true, Ordering::SeqCst))).unwrap().join();
    assert!(ran.load(Ordering::SeqCst));

    let mut cpus = CpuSetT::default();
    cpus.set(0);
    assert!(cpus.is_set(0));
    assert!(!cpus.is_set(1));
    assert!(!cpus.is_set(CpuSetT::SETSIZE));
    assert_eq!(cpus.count(), 1);
    let attr =
        PosixThreadAttr::default().set_stack_size(StackSize { size: 4 << 20 }).set_affinity(cpus);
    let on_cpu = Arc::new(std::sync::atomic::AtomicI32::new(-1));
    let o = on_cpu.clone();
    PosixThread::with_attr(
        attr,
        Box::new(move || {
            // A deep frame fits in the larger stack.
            let big = [1u8; 1 << 20];
            std::hint::black_box(&big);
            // SAFETY: plain libc call.
            o.store(unsafe { sched_getcpu() }, Ordering::SeqCst);
        }),
    )
    .unwrap()
    .join();
    assert_eq!(on_cpu.load(Ordering::SeqCst), 0);
}

unsafe extern "C" {
    fn sched_getcpu() -> i32;
}
