//! Tests of the kernel version checks: which `uname` releases have the fixes for non-blocking
//! AIO and for XFS concurrency.

#![cfg(target_os = "linux")]

use teseladb::engine::core::uname::parse_uname;

#[test]
fn test_nowait_aio_fix() {
    let check =
        |uname: &str| parse_uname(uname).whitelisted(&["5.1", "5.0.8", "4.19.35", "4.14.112"]);
    assert!(check("5.1.0"));
    assert!(check("5.1.1"));
    assert!(check("5.1.1-44.distro"));
    assert!(check("5.1.1-44.7.distro"));
    assert!(!check("5.0.0"));
    assert!(!check("5.0.7"));
    assert!(!check("5.0.7-55.el19"));
    assert!(check("5.0.8"));
    assert!(check("5.0.9"));
    assert!(check("5.0.8-200.fedora"));
    assert!(check("5.0.9-200.fedora"));
    assert!(check("5.2.0"));
    assert!(check("5.2.9"));
    assert!(check("5.2.9-77.el153"));
    assert!(check("6.0.0"));
    assert!(!check("3.9.0"));
    assert!(!check("4.19"));
    assert!(!check("4.19.34"));
    assert!(check("4.19.35"));
    assert!(check("4.19.36"));
    assert!(!check("4.20.36"));
    assert!(!check("4.14.111"));
    assert!(check("4.14.112"));
    assert!(check("4.14.113"));
}

#[test]
fn test_xfs_concurrency_fix() {
    let check = |uname: &str| parse_uname(uname).whitelisted(&["3.15", "3.10.0-325.el7"]);
    assert!(check("3.15.0"));
    assert!(check("5.1.0"));
    assert!(!check("3.14.0"));
    assert!(!check("3.10.0"));
    assert!(!check("3.10.14"));
    assert!(!check("3.10.0-325.ubuntu"));
    assert!(!check("3.10.0-325"));
    assert!(check("3.10.0-325.el7"));
    assert!(check("3.10.0-326.el7"));
    assert!(!check("3.10.0-324.el7"));
    assert!(check("3.10.0-325.665.el7"));
}
