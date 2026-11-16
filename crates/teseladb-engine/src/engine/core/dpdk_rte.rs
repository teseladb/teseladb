//! Start-up of the DPDK Environment Abstraction Layer (EAL), with the `dpdk` feature.
//!
//! When the DPDK network driver is used, the shards run on DPDK lcores instead of threads the
//! engine creates. `eal::init` initializes the EAL once per process for a CPU set and an optional
//! hugetlbfs mount, and `launch_thread_loops` starts one shard thread loop on each worker lcore.
//! Without the `dpdk` feature this module is empty.

/// The EAL bindings; compiled only with the `dpdk` feature.
#[cfg(feature = "dpdk")]
pub mod dpdk {
    use std::ffi::{c_char, c_int, c_void};

    // DPDK must be built with `CONFIG_RTE_MBUF_REFCNT_ATOMIC` disabled: mbufs are only touched by
    // the shard that owns them.

    /// Maximum number of lcores (`RTE_MAX_LCORE` of the default DPDK configuration); the size of
    /// [`eal::Cpuset`].
    pub const RTE_MAX_LCORE: usize = 128;

    unsafe extern "C" {
        fn rte_eal_init(argc: c_int, argv: *mut *mut c_char) -> c_int;
        fn rte_exit(exit_code: c_int, format: *const c_char, ...) -> !;
    }

    /// Returns the bytes of packet-buffer pool memory one queue pair of the DPDK driver needs.
    ///
    /// Forwards to [`net::dpdk`](crate::engine::net::dpdk), which the `dpdk` feature always
    /// compiles together with this module. `hugetlbfs_membackend` says whether the pools are backed
    /// by hugetlbfs memory.
    #[inline]
    pub fn qp_mempool_obj_size(hugetlbfs_membackend: bool) -> u32 {
        crate::engine::net::dpdk::qp_mempool_obj_size(hugetlbfs_membackend)
    }

    /// Whether the EAL has been initialized; re-exported for the network driver.
    pub use self::eal::INITIALIZED;

    /// Process-wide state and initialization of the EAL.
    pub mod eal {
        use super::{RTE_MAX_LCORE, qp_mempool_obj_size, rte_eal_init, rte_exit};
        use crate::engine::core::future::ExceptionPtr;
        use std::ffi::{c_char, c_int};
        use std::fmt::Write as _;
        use std::sync::atomic::{AtomicBool, Ordering};

        /// A set of lcore numbers below [`RTE_MAX_LCORE`], as a bit mask (bit `i` of word `i / 64`
        /// is lcore `i`).
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct Cpuset(pub [u64; RTE_MAX_LCORE / 64]);

        impl Cpuset {
            /// Sets (`value == true`) or clears bit `pos`, and returns `self` for chaining.
            ///
            /// # Panics
            /// Panics if `pos >= RTE_MAX_LCORE`.
            #[inline]
            pub fn set(&mut self, pos: usize, value: bool) -> &mut Self {
                let (w, b) = (pos / 64, pos % 64);
                if value {
                    self.0[w] |= 1u64 << b;
                } else {
                    self.0[w] &= !(1u64 << b);
                }
                self
            }

            /// Returns whether any bit is set.
            #[inline]
            pub fn any(&self) -> bool {
                self.0.iter().any(|&w| w != 0)
            }

            /// Returns the number of bits set.
            #[inline]
            pub fn count(&self) -> usize {
                self.0.iter().map(|w| w.count_ones() as usize).sum()
            }

            /// Shifts the whole mask right by four bits (one hex digit).
            fn shr4(&mut self) {
                let n = self.0.len();
                for i in 0..n {
                    let hi = if i + 1 < n { self.0[i + 1] << 60 } else { 0 };
                    self.0[i] = (self.0[i] >> 4) | hi;
                }
            }
        }

        /// Set once [`init`] has initialized the EAL; later calls of [`init`] do nothing.
        pub static INITIALIZED: AtomicBool = AtomicBool::new(false);

        /// Returns `s` as a NUL-terminated byte vector, for building `argv`.
        fn string2vector(s: &str) -> Vec<u8> {
            let mut v = Vec::with_capacity(s.len() + 1);
            v.extend_from_slice(s.as_bytes());
            v.push(0);
            v
        }

        /// Initializes the EAL for the lcores in `cpus`, once per process.
        ///
        /// Builds an EAL command line with `argv0`, the core mask of `cpus` and one memory channel.
        /// With `hugepages_path`, the EAL uses that hugetlbfs mount and is given [`mem_size`] bytes
        /// for the CPU count; without it, huge pages are disabled unless `dpdk_pmd` (DPDK poll-mode
        /// drivers requested) asks for the EAL's default huge page configuration.
        ///
        /// # Errors
        /// Never returns `Err`: a failure of `rte_eal_init` ends the process through `rte_exit`
        /// with the message "Cannot init EAL". The `Result` matches the reactor's start-up code.
        pub fn init(
            mut cpus: Cpuset,
            argv0: &str,
            hugepages_path: Option<String>,
            dpdk_pmd: bool,
        ) -> Result<(), ExceptionPtr> {
            if INITIALIZED.load(Ordering::Relaxed) {
                return Ok(());
            }

            let cpu_count = cpus.count();
            let mut mask = String::new();
            while cpus.any() {
                // Writing to a String cannot fail.
                let _ = write!(mask, "{:x}", cpus.0[0] & 0xF);
                cpus.shr4();
            }

            let mask_str: String = mask.chars().rev().collect();

            let mut args: Vec<Vec<u8>> = vec![
                string2vector(argv0),
                string2vector("-c"),
                string2vector(&mask_str),
                string2vector("-n"),
                string2vector("1"),
            ];

            // If "hugepages" is not provided and DPDK PMD drivers mode is requested -
            // use the default DPDK huge tables configuration.
            if let Some(path) = &hugepages_path {
                args.push(string2vector("--huge-dir"));
                args.push(string2vector(path));

                //
                // We don't know what is going to be our networking configuration so we
                // assume there is going to be a queue per-CPU. Plus we'll give a DPDK
                // 64MB for "other stuff".
                //
                let size_mb = mem_size(cpu_count as u32) >> 20;
                args.push(string2vector("-m"));
                args.push(string2vector(&size_mb.to_string()));
            } else if !dpdk_pmd {
                args.push(string2vector("--no-huge"));
            }

            let mut cargs: Vec<*mut c_char> =
                args.iter_mut().map(|a| a.as_mut_ptr() as *mut c_char).collect();

            // SAFETY: every element of `cargs` points to a NUL-terminated string in `args`, which
            // outlives the call; the EAL may reorder the array but does not free it.
            let ret = unsafe { rte_eal_init(cargs.len() as c_int, cargs.as_mut_ptr()) };
            if ret < 0 {
                // SAFETY: the format is a NUL-terminated literal with no conversions, so no
                // variadic arguments are read.
                unsafe {
                    rte_exit(1 /* EXIT_FAILURE */, c"Cannot init EAL\n".as_ptr())
                };
            }

            INITIALIZED.store(true, Ordering::Relaxed);
            Ok(())
        }

        /// Returns the bytes of memory the EAL needs for `num_cpus` shards, with pools backed by
        /// hugetlbfs (see [`mem_size_with`]).
        #[inline]
        pub fn mem_size(num_cpus: u32) -> usize {
            mem_size_with(num_cpus, true)
        }

        /// Returns the bytes of memory the EAL needs for `num_cpus` shards: one queue pair's pool
        /// memory per shard (the network configuration is not known yet, so one queue per CPU is
        /// assumed) plus 64 MiB for everything else.
        pub fn mem_size_with(num_cpus: u32, hugetlbfs_membackend: bool) -> usize {
            let mut memsize: usize = 0;
            //
            // PMD mempool memory:
            //
            // We don't know what is going to be our networking configuration so we
            // assume there is going to be a queue per-CPU.
            //
            memsize += num_cpus as usize * qp_mempool_obj_size(hugetlbfs_membackend) as usize;

            // Plus we'll give a DPDK 64MB for "other stuff".
            memsize += 64usize << 20;

            memsize
        }
    }

    unsafe extern "C" {
        fn rte_get_next_lcore(i: u32, skip_main: c_int, wrap: c_int) -> u32;
        fn rte_eal_remote_launch(
            f: unsafe extern "C" fn(*mut c_void) -> c_int,
            arg: *mut c_void,
            worker_id: u32,
        ) -> c_int;
    }

    /// Entry point of a worker lcore: runs the thread loop `f` points to and returns 0.
    unsafe extern "C" fn dpdk_thread_adaptor(f: *mut c_void) -> c_int {
        // SAFETY: `f` is the address of a thread loop that `launch_thread_loops` handed to
        // this lcore; the loops outlive the lcores (`smp::join_all` waits for them).
        unsafe { (*f.cast::<Box<dyn Fn() + Send>>())() };
        0
    }

    /// Starts the shard thread loops on the DPDK worker lcores: loop `k` on the `k`-th worker
    /// lcore, until either runs out.
    ///
    /// A launch that fails (the lcore is busy) is ignored and leaves that shard unstarted.
    ///
    /// # Safety
    /// The EAL is initialized (`eal::init`), and the slice and every loop in it stay in
    /// place, neither moved, freed nor touched by anyone else, until
    /// `rte_eal_mp_wait_lcore()` returns: the lcores keep pointers to them and call them on
    /// their own threads. The type cannot express that lifetime.
    pub unsafe fn launch_thread_loops(thread_loops: &mut [Box<dyn Fn() + Send>]) {
        let mut it = thread_loops.iter_mut();
        // Iterate over the worker lcores, skipping the main one.
        // SAFETY: the EAL is initialized (guaranteed by the caller).
        let mut i = unsafe { rte_get_next_lcore(u32::MAX, 1, 0) };
        while (i as usize) < RTE_MAX_LCORE {
            let Some(f) = it.next() else { break };
            let arg: *mut Box<dyn Fn() + Send> = f;
            // The result is ignored: a failed launch (-EBUSY) leaves that shard unstarted.
            // SAFETY: the loop stays in place until the lcores are joined (guaranteed by the
            // caller), and it is `Send`, so calling it on the lcore's thread is sound.
            unsafe { rte_eal_remote_launch(dpdk_thread_adaptor, arg.cast(), i) };
            // SAFETY: as above.
            i = unsafe { rte_get_next_lcore(i, 1, 0) };
        }
    }
}

#[cfg(feature = "dpdk")]
pub use dpdk::{eal, launch_thread_loops};
