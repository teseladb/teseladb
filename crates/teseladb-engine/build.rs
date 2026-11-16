//! Links the native libraries that are not named by a `#[link]` attribute.
//!
//! gnutls, c-ares, lz4 and hwloc carry `#[link(name = ..)]` on their `extern "C"` blocks;
//! gnutls is linked only with the feature `tls` and c-ares only with the feature `dns`. DPDK
//! is a set of libraries whose names and order depend on how it was built, so with the
//! feature `dpdk` the link line comes from `pkg-config --libs --static libdpdk`.
//!
//! It also reads `rte_build_config.h` to learn whether DPDK
//! was built with `RTE_MBUF_REFCNT_ATOMIC`: without it, `cfg(dpdk_mbuf_refcnt_nonatomic)`
//! makes `net/dpdk_rte.rs` update the mbuf reference count without atomic operations, as the
//! library's own inline functions then do.

use std::env;
use std::path::Path;
use std::process::Command;

/// The optional build configurations the sources test with `cfg` (debug checks, alternative
/// allocators, task backtraces, and so on). None is set by default; enable one with
/// `RUSTFLAGS="--cfg <name>"`. They are declared here so that `check-cfg` accepts them.
const BUILD_CONFIG_CFGS: &[&str] = &[
    "no_exception_intercept",
    "engine_backtrace_unimplemented",
    "engine_build_shared_libs",
    "engine_debug",
    "engine_debug_allocations",
    "engine_debug_promise",
    "engine_debug_shared_ptr",
    "engine_default_allocator",
    "engine_enable_alloc_failure_injection",
    "engine_gate_holder_debug",
    "engine_have_asan_fiber_support",
    "engine_heapprof",
    "engine_semaphore_debug",
    "engine_shuffle_task_queue",
    "engine_task_backtrace",
    "engine_task_histogram",
    "engine_testing_with_networking",
    "engine_thread_stack_guards",
    "engine_type_erase_more",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo::rustc-check-cfg=cfg(dpdk_mbuf_refcnt_nonatomic)");
    // `engine_asan_enabled`: set by the builder (RUSTFLAGS="--cfg engine_asan_enabled") for a
    // build with `-Zsanitizer=address`, which has no stable cfg of its own.
    println!("cargo::rustc-check-cfg=cfg(engine_asan_enabled)");
    // The other build configurations (`engine_debug`, `engine_thread_stack_guards`,
    // `no_exception_intercept`, ...), set the same way with RUSTFLAGS="--cfg <name>".
    for name in BUILD_CONFIG_CFGS {
        println!("cargo::rustc-check-cfg=cfg({name})");
    }
    detect_cares_version();
    if env::var_os("CARGO_FEATURE_DPDK").is_none() {
        return;
    }
    if let Err(e) = link_dpdk().and_then(|()| detect_refcnt_atomic()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Sets the c-ares API cfgs used by net/dns.rs: `cares_dnsrec` from c-ares 1.28
/// (`ares_query_dnsrec`) and `cares_process_fds` from 1.34 (`ares_process_fds`), from the
/// version in the installed `ares_version.h` (include dirs of `pkg-config libcares`, then /usr/include and
/// /usr/local/include). Without the header (a host that only checks the crate) neither is set,
/// the branches every supported c-ares takes. Without the feature `dns` nothing uses c-ares
/// and its headers are not looked for.
fn detect_cares_version() {
    println!("cargo::rustc-check-cfg=cfg(cares_dnsrec)");
    println!("cargo::rustc-check-cfg=cfg(cares_process_fds)");
    if env::var_os("CARGO_FEATURE_DNS").is_none() {
        return;
    }
    let mut dirs: Vec<String> = Command::new("pkg-config")
        .args(["--cflags-only-I", "libcares"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.split_whitespace().filter_map(|f| f.strip_prefix("-I")).map(str::to_owned).collect()
        })
        .unwrap_or_default();
    dirs.extend(["/usr/include".to_owned(), "/usr/local/include".to_owned()]);
    for dir in dirs {
        let header = Path::new(&dir).join("ares_version.h");
        let Ok(text) = std::fs::read_to_string(&header) else { continue };
        println!("cargo:rerun-if-changed={}", header.display());
        let field = |name: &str| -> u32 {
            text.lines()
                .find_map(|l| {
                    let mut w = l.split_whitespace();
                    (w.next() == Some("#define") && w.next() == Some(name))
                        .then(|| w.next())
                        .flatten()
                })
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        let version = (field("ARES_VERSION_MAJOR") << 16)
            | (field("ARES_VERSION_MINOR") << 8)
            | field("ARES_VERSION_PATCH");
        if version >= 0x011c00 {
            println!("cargo:rustc-cfg=cares_dnsrec");
        }
        if version >= 0x012200 {
            println!("cargo:rustc-cfg=cares_process_fds");
        }
        return;
    }
}

/// Sets `dpdk_mbuf_refcnt_nonatomic` when `rte_build_config.h` does not define
/// `RTE_MBUF_REFCNT_ATOMIC`.
fn detect_refcnt_atomic() -> Result<(), String> {
    let out = Command::new("pkg-config")
        .args(["--cflags-only-I", "libdpdk"])
        .output()
        .map_err(|e| format!("feature `dpdk`: cannot run pkg-config: {e}"))?;
    let flags = String::from_utf8(out.stdout)
        .map_err(|e| format!("feature `dpdk`: pkg-config output: {e}"))?;
    for dir in flags.split_whitespace().filter_map(|f| f.strip_prefix("-I")) {
        let header = Path::new(dir).join("rte_build_config.h");
        if let Ok(text) = std::fs::read_to_string(&header) {
            println!("cargo:rerun-if-changed={}", header.display());
            let atomic = text.lines().any(|l| {
                let mut w = l.split_whitespace();
                w.next() == Some("#define") && w.next() == Some("RTE_MBUF_REFCNT_ATOMIC")
            });
            if !atomic {
                println!("cargo:rustc-cfg=dpdk_mbuf_refcnt_nonatomic");
            }
            return Ok(());
        }
    }
    // Not found: keep the atomic updates, which are correct for either build of DPDK.
    Ok(())
}

/// Emits the `-L`, `-l` and other linker flags of `libdpdk`.
fn link_dpdk() -> Result<(), String> {
    let out = Command::new("pkg-config")
        .args(["--libs", "--static", "libdpdk"])
        .output()
        .map_err(|e| format!("feature `dpdk`: cannot run pkg-config: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "feature `dpdk`: pkg-config cannot find libdpdk: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let flags = String::from_utf8(out.stdout)
        .map_err(|e| format!("feature `dpdk`: pkg-config output: {e}"))?;
    for flag in flags.split_whitespace() {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else if let Some(lib) = flag.strip_prefix("-l:") {
            println!("cargo:rustc-link-arg=-l:{lib}");
        } else if let Some(lib) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={lib}");
        } else {
            // --whole-archive, -Wl,... and the like keep their position on the link line.
            println!("cargo:rustc-link-arg={flag}");
        }
    }
    Ok(())
}
