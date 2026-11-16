#![cfg(target_os = "linux")]
//! The stream `ContentReplace::transform` returns replaces `{{Host}}` and `{{Protocol}}` in
//! what is written through it and passes the result to the stream it wraps. Data written
//! without copying (as a packet or a temporary buffer) goes through the same replacement,
//! and closing the transformed stream closes the stream it wraps.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::iostream::{DataSink, DataSinkImpl, OutputStream, OutputStreamOptions};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::http::file_handler::FileTransformer;
use teseladb::engine::http::request::Request;
use teseladb::engine::http::transformers::ContentReplace;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::util::string_utils::internal::CaseInsensitiveKey;
use teseladb::engine_thread_test_case;

/// A sink that records what it receives and whether it was closed.
struct Recorder {
    data: Rc<RefCell<Vec<u8>>>,
    closed: Rc<Cell<bool>>,
}

impl DataSinkImpl for Recorder {
    fn put(&mut self, mut p: Packet) -> Future<()> {
        for b in p.release() {
            self.data.borrow_mut().extend_from_slice(b.as_slice());
        }
        make_ready_future(())
    }
    fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
        self.data.borrow_mut().extend_from_slice(buf.as_slice());
        make_ready_future(())
    }
    fn flush(&mut self) -> Future<()> {
        make_ready_future(())
    }
    fn close(&mut self) -> Future<()> {
        self.closed.set(true);
        make_ready_future(())
    }
}

/// A transformed stream over a recorder, for a request to host `localhost`.
fn transformed(
    cr: &mut ContentReplace,
    data: &Rc<RefCell<Vec<u8>>>,
    closed: &Rc<Cell<bool>>,
) -> OutputStream<u8> {
    let mut req = Box::new(Request::default());
    req._headers.insert(CaseInsensitiveKey(Sstring::from("Host")), Sstring::from("localhost"));
    let inner = OutputStream::with_options(
        DataSink::new(Box::new(Recorder { data: data.clone(), closed: closed.clone() })),
        32000,
        OutputStreamOptions { trim_to_size: true, ..Default::default() },
    );
    cr.transform(req, &Sstring::from("json"), inner)
}

engine_thread_test_case!(zero_copy_writes_are_replaced_and_close_reaches_the_inner_stream, {
    let mut cr = ContentReplace::new(&Sstring::from("json"));

    // A copying write: replaced, as before.
    let (data, closed) = (Rc::new(RefCell::new(Vec::new())), Rc::new(Cell::new(false)));
    let mut os = transformed(&mut cr, &data, &closed);
    os.write(b"a={{Host}}").get().unwrap();
    os.close().get().unwrap();
    assert_eq!(String::from_utf8_lossy(&data.borrow()), "a=localhost");
    assert!(closed.get(), "closing the transformed stream leaves the inner one open");

    // A write without copying reaches the sink as a packet.
    let (data, closed) = (Rc::new(RefCell::new(Vec::new())), Rc::new(Cell::new(false)));
    let mut os = transformed(&mut cr, &data, &closed);
    os.write_buffer(TemporaryBuffer::copy_from(b"b={{Host}}")).get().unwrap();
    os.close().get().unwrap();
    assert_eq!(String::from_utf8_lossy(&data.borrow()), "b=localhost", "the packet was dropped");
    assert!(closed.get());
});
