//! `effective_uid`: the user id the process runs as, which decides the startup warning of a node
//! run as `root` (measured on the reference server and the reference router 9.0.4).

use std::process::Command;

#[test]
fn effective_uid_is_the_one_id_reports() {
    let out = Command::new("id").arg("-u").output().expect("id -u");
    let shown: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().expect("a number");
    assert_eq!(teseladb_sys::effective_uid(), shown);
}
