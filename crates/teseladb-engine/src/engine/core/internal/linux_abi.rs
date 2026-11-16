//! Linux ABI of the supported architectures (x86_64 and aarch64).
//!
//! The crate links no C headers, so every system call number, flag value and structure layout
//! whose number or layout differs between architectures lives here once, checked against the
//! glibc 2.36 and Linux 6.1 uapi headers of both architectures, and every module of the runtime
//! takes it from here. Values that are the same on both (asm-generic) stay next to their users;
//! their layouts are still asserted at the end of this file, so the whole ABI is verified in
//! one place.

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("engine supports only the Linux ABIs of x86_64 and aarch64");

/// System call numbers (`__NR_*` of `<asm/unistd.h>`).
pub mod nr {
    #[cfg(target_arch = "x86_64")]
    mod arch {
        /// `io_setup`
        pub const IO_SETUP: i64 = 206;
        /// `io_destroy`
        pub const IO_DESTROY: i64 = 207;
        /// `io_getevents`
        pub const IO_GETEVENTS: i64 = 208;
        /// `io_submit`
        pub const IO_SUBMIT: i64 = 209;
        /// `io_cancel`
        pub const IO_CANCEL: i64 = 210;
        /// `io_pgetevents`
        pub const IO_PGETEVENTS: i64 = 333;
        /// `gettid`
        pub const GETTID: i64 = 186;
        /// `getdents64`
        pub const GETDENTS64: i64 = 217;
        /// `perf_event_open`
        pub const PERF_EVENT_OPEN: i64 = 298;
        /// `membarrier`
        pub const MEMBARRIER: i64 = 324;
        /// `mbind`
        pub const MBIND: i64 = 237;
    }
    #[cfg(target_arch = "aarch64")]
    mod arch {
        /// `io_setup`
        pub const IO_SETUP: i64 = 0;
        /// `io_destroy`
        pub const IO_DESTROY: i64 = 1;
        /// `io_submit`
        pub const IO_SUBMIT: i64 = 2;
        /// `io_cancel`
        pub const IO_CANCEL: i64 = 3;
        /// `io_getevents`
        pub const IO_GETEVENTS: i64 = 4;
        /// `io_pgetevents`
        pub const IO_PGETEVENTS: i64 = 292;
        /// `gettid`
        pub const GETTID: i64 = 178;
        /// `getdents64`
        pub const GETDENTS64: i64 = 61;
        /// `perf_event_open`
        pub const PERF_EVENT_OPEN: i64 = 241;
        /// `membarrier`
        pub const MEMBARRIER: i64 = 283;
        /// `mbind`
        pub const MBIND: i64 = 235;
    }
    /// The numbers that differ per architecture.
    pub use arch::*;

    /// `__NR_io_uring_setup` (the same number on every architecture since Linux 5.1).
    pub const IO_URING_SETUP: i64 = 425;
    /// `__NR_io_uring_enter`
    pub const IO_URING_ENTER: i64 = 426;
    /// `__NR_io_uring_register`
    pub const IO_URING_REGISTER: i64 = 427;
    /// `__NR_pidfd_open`
    pub const PIDFD_OPEN: i64 = 434;
}

/// `O_RDONLY`
pub const O_RDONLY: i32 = 0;
/// `O_WRONLY`
pub const O_WRONLY: i32 = 0o1;
/// `O_RDWR`
pub const O_RDWR: i32 = 0o2;
/// `O_ACCMODE`
pub const O_ACCMODE: i32 = 0o3;
/// `O_CREAT`
pub const O_CREAT: i32 = 0o100;
/// `O_EXCL`
pub const O_EXCL: i32 = 0o200;
/// `O_TRUNC`
pub const O_TRUNC: i32 = 0o1000;
/// `O_APPEND`
pub const O_APPEND: i32 = 0o2000;
/// `O_NONBLOCK`
pub const O_NONBLOCK: i32 = 0o4000;
/// `O_DSYNC`
pub const O_DSYNC: i32 = 0o10000;
/// `O_CLOEXEC`
pub const O_CLOEXEC: i32 = 0o2000000;
/// `O_LARGEFILE` of glibc: 0 on LP64, where every file is large.
pub const O_LARGEFILE: i32 = 0;
/// `O_DIRECT`
#[cfg(target_arch = "x86_64")]
pub const O_DIRECT: i32 = 0o40000;
/// `O_DIRECT`
#[cfg(target_arch = "aarch64")]
pub const O_DIRECT: i32 = 0o200000;
/// `O_DIRECTORY`
#[cfg(target_arch = "x86_64")]
pub const O_DIRECTORY: i32 = 0o200000;
/// `O_DIRECTORY`
#[cfg(target_arch = "aarch64")]
pub const O_DIRECTORY: i32 = 0o40000;
/// `O_NOFOLLOW`
#[cfg(target_arch = "x86_64")]
pub const O_NOFOLLOW: i32 = 0o400000;
/// `O_NOFOLLOW`
#[cfg(target_arch = "aarch64")]
pub const O_NOFOLLOW: i32 = 0o100000;

/// `SIGSTKSZ` of the kernel uapi, used when `sysconf(_SC_SIGSTKSZ)` gives nothing.
#[cfg(target_arch = "x86_64")]
pub const KERNEL_SIGSTKSZ: usize = 8192;
/// `SIGSTKSZ` of the kernel uapi, used when `sysconf(_SC_SIGSTKSZ)` gives nothing.
#[cfg(target_arch = "aarch64")]
pub const KERNEL_SIGSTKSZ: usize = 16384;

/// `_SC_SIGSTKSZ` of glibc.
const SC_SIGSTKSZ: i32 = 250;

unsafe extern "C" {
    fn sysconf(name: i32) -> i64;
}

/// The signal stack size, as glibc 2.34 and later define `SIGSTKSZ` with `_GNU_SOURCE`:
/// `sysconf(_SC_SIGSTKSZ)`, the size the running kernel needs for a signal frame (it grows
/// with SVE and AVX-512 state), or [`KERNEL_SIGSTKSZ`] when `sysconf` reports nothing.
#[inline]
pub fn sigstksz() -> usize {
    // SAFETY: sysconf only reads a configuration value.
    let v = unsafe { sysconf(SC_SIGSTKSZ) };
    if v > 0 { v as usize } else { KERNEL_SIGSTKSZ }
}

/// `pthread_attr_t`: opaque, with glibc's size (`__SIZEOF_PTHREAD_ATTR_T`) and the alignment of
/// its `long int __align` member.
#[repr(C, align(8))]
pub struct PthreadAttrT(pub [u8; PTHREAD_ATTR_T_SIZE]);

/// `__SIZEOF_PTHREAD_ATTR_T`
#[cfg(target_arch = "x86_64")]
pub const PTHREAD_ATTR_T_SIZE: usize = 56;
/// `__SIZEOF_PTHREAD_ATTR_T`
#[cfg(target_arch = "aarch64")]
pub const PTHREAD_ATTR_T_SIZE: usize = 64;

impl PthreadAttrT {
    /// Storage for `pthread_attr_init` to fill.
    #[inline]
    pub const fn zeroed() -> Self {
        PthreadAttrT([0; PTHREAD_ATTR_T_SIZE])
    }
}

/// `sigset_t` of glibc (1024 bits on both architectures).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SigsetT {
    /// `__val`
    pub __val: [u64; 16],
}

impl Default for SigsetT {
    #[inline]
    fn default() -> Self {
        SigsetT { __val: [0; 16] }
    }
}

/// `struct epoll_event`: packed on x86_64 (`__EPOLL_PACKED`), naturally aligned elsewhere.
#[cfg_attr(target_arch = "x86_64", repr(C, packed))]
#[cfg_attr(not(target_arch = "x86_64"), repr(C))]
#[derive(Clone, Copy, Default)]
pub struct EpollEvent {
    /// `events`
    pub events: u32,
    /// `data` (the `u64` member of `epoll_data_t`)
    pub data: u64,
}

/// `struct timespec`
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timespec {
    /// `tv_sec`
    pub tv_sec: i64,
    /// `tv_nsec`
    pub tv_nsec: i64,
}

/// `struct stat` (x86_64 layout of `<bits/struct_stat.h>`).
#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: i32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atim: Timespec,
    pub st_mtim: Timespec,
    pub st_ctim: Timespec,
    pub __unused: [i64; 3],
}

/// `struct stat` (generic layout of `<bits/struct_stat.h>`, used by aarch64).
#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub __pad1: u64,
    pub st_size: i64,
    pub st_blksize: i32,
    pub __pad2: i32,
    pub st_blocks: i64,
    pub st_atim: Timespec,
    pub st_mtim: Timespec,
    pub st_ctim: Timespec,
    pub __unused: [i32; 2],
}

// Sizes and alignments measured with the C compiler and glibc headers of each architecture.
#[cfg(target_arch = "x86_64")]
const _: () = {
    assert!(std::mem::size_of::<PthreadAttrT>() == 56);
    assert!(std::mem::size_of::<EpollEvent>() == 12 && std::mem::align_of::<EpollEvent>() == 1);
    assert!(std::mem::size_of::<Stat>() == 144);
    assert!(std::mem::offset_of!(Stat, st_mode) == 24);
};
#[cfg(target_arch = "aarch64")]
const _: () = {
    assert!(std::mem::size_of::<PthreadAttrT>() == 64);
    assert!(std::mem::size_of::<EpollEvent>() == 16 && std::mem::align_of::<EpollEvent>() == 8);
    assert!(std::mem::size_of::<Stat>() == 128);
    assert!(std::mem::offset_of!(Stat, st_mode) == 16);
};
const _: () = {
    assert!(std::mem::align_of::<PthreadAttrT>() == 8);
    assert!(std::mem::size_of::<SigsetT>() == 128);
    assert!(std::mem::offset_of!(Stat, st_size) == 48);
    assert!(std::mem::offset_of!(Stat, st_blocks) == 64);
    assert!(std::mem::offset_of!(Stat, st_mtim) == 88);
};

// The layouts that are the same on both architectures but are declared next to their users:
// checked here so that the whole ABI is verified in one place.
const _: () = {
    use crate::engine::core::linux_aio::internal::linux_abi::{IoEvent, Iocb};
    use crate::engine::core::reactor::{
        PosixSpawnFileActionsT, PosixSpawnattrT, Sigaction, StackT,
    };
    use crate::engine::core::reactor_backend::uring::{IoUringCqe, IoUringParams, IoUringSqe};
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Iocb>() == 64);
    assert!(size_of::<IoEvent>() == 32);
    assert!(size_of::<IoUringSqe>() == 64);
    assert!(size_of::<IoUringCqe>() == 16);
    assert!(size_of::<IoUringParams>() == 120);
    assert!(size_of::<Sigaction>() == 152);
    assert!(offset_of!(Sigaction, sa_flags) == 136);
    assert!(offset_of!(Sigaction, sa_restorer) == 144);
    assert!(size_of::<StackT>() == 24);
    assert!(size_of::<PosixSpawnattrT>() == 336);
    assert!(size_of::<PosixSpawnFileActionsT>() == 80);
};
