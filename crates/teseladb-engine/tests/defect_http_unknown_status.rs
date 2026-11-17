#![cfg(target_os = "linux")]
//! A reply with a status code that has no known reason phrase is sent with that code and an
//! empty reason phrase (`HTTP/1.1 299 \r\n`, the space after the code kept, RFC 9112 4).
//! Before, it went out as `500 Internal Server Error`: the client got another status.

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::http::reply::{Reply, StatusType};
use teseladb::engine_test_case;

engine_test_case!(test_reply_unknown_status_line, {
    let mut rep = Reply::new();
    rep.set_version(&Sstring::from("1.1")).set_status(StatusType(299), Sstring::new());
    assert_eq!(rep.response_line(), "HTTP/1.1 299 \r\n");
    assert_eq!(StatusType(299).to_string(), "299");
    make_ready_future(())
});
