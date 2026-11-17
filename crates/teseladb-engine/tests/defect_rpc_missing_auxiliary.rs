#![cfg(target_os = "linux")]
//! A handler that asks a client's info for auxiliary data that was never attached gets an rpc
//! error (and the connection is aborted), instead of an assertion that aborts the whole
//! process: the data is attached by a verb the peer sends, which may have failed, so a missing
//! key is the peer's doing, not a programming error.

use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::api::make_ipv4_address_ip_port;
use teseladb::engine::rpc::rpc_types::{ClientInfo, ConnectionId, Error};

#[test]
fn retrieving_missing_auxiliary_data_is_an_error() {
    let mut info = ClientInfo {
        addr: make_ipv4_address_ip_port(0x7f00_0001, 5),
        server: std::ptr::null_mut(),
        conn_id: ConnectionId::make_id(1, 0),
        user_data: Default::default(),
    };
    info.attach_auxiliary(&Sstring::from("present"), 7i32);
    assert_eq!(*info.retrieve_auxiliary::<i32>(&Sstring::from("present")).expect("present"), 7);
    assert!(info.retrieve_auxiliary_opt::<i32>(&Sstring::from("missing")).is_none());

    let e = info.retrieve_auxiliary::<i32>(&Sstring::from("never_attached")).expect_err("missing");
    let rpc_error = e.downcast_ref::<Error>().expect("an rpc error");
    assert!(rpc_error.what().contains("missing auxiliary data 'never_attached'"), "{}", rpc_error.what());
}
