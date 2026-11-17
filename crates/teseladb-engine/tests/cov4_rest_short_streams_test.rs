#![allow(unsafe_code)]
//! The deprecated whole-stream readers of `http::short_streams::httpd` forward to the ones in
//! `util::short_streams`: all the chunks, one contiguous string, or nothing (skipped).
#![cfg(target_os = "linux")]
#![allow(unsafe_code, deprecated)]

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::http::short_streams::httpd;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::packet_data_source::as_input_stream;
use teseladb::engine_test_case;

const DATA: &[u8] = b"short streams read whole";

engine_test_case!(deprecated_whole_stream_readers, {
    make_coroutine(async move {
        let mut s = as_input_stream(Packet::from_static_data(DATA));
        // SAFETY: the stream outlives the read and nothing else uses it.
        let chunks = unsafe { httpd::read_entire_stream(&mut s) }.await?;
        let all: Vec<u8> = chunks.iter().flat_map(|b| b.as_slice().iter().copied()).collect();
        assert_eq!(all, DATA);

        let mut s = as_input_stream(Packet::from_static_data(DATA));
        // SAFETY: as above.
        let joined = unsafe { httpd::read_entire_stream_contiguous(&mut s) }.await?;
        assert_eq!(joined.to_string().as_bytes(), DATA);

        let mut s = as_input_stream(Packet::from_static_data(DATA));
        // SAFETY: as above.
        unsafe { httpd::skip_entire_stream(&mut s) }.await?;
        assert!(s.read().await?.empty());
        Ok(())
    })
});
