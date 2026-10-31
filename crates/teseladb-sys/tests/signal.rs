//! A stop signal sets the flag instead of killing the process. In a binary of its own: the signal
//! interrupts the system calls of every thread of the process, which would disturb other tests.

use teseladb_sys::signal::{Stop, install_stop_handler, raise_stop, send_stop, stop_requested};

#[test]
fn a_stop_signal_sets_the_flag_and_keeps_the_process_alive() {
    install_stop_handler().unwrap();
    assert!(!stop_requested());
    raise_stop().unwrap();
    assert!(stop_requested());
}

#[test]
fn stop_signals_reach_other_processes() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    send_stop(child.id(), Stop::Terminate).unwrap();
    assert!(!child.wait().unwrap().success());
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    send_stop(child.id(), Stop::Interrupt).unwrap();
    assert!(!child.wait().unwrap().success());
    // A pid that does not fit, and one that does not exist.
    assert!(send_stop(u32::MAX, Stop::Terminate).is_err());
    assert!(send_stop(i32::MAX as u32, Stop::Terminate).is_err());
}
