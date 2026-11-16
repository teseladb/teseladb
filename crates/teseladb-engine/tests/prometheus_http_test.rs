//! Tests of the Prometheus exposition over HTTP: the body served for registered metrics
//! contains the expected families and samples.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/loopback_socket.rs"]
mod loopback_socket;

use loopback_socket::{LoopbackConnectionFactory, LoopbackSocketImpl};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::metrics::{self, Description, Label};
use teseladb::engine::core::metrics_registration::MetricGroups;
use teseladb::engine::core::prometheus::{self, add_prometheus_routes};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::httpd::httpd::{HttpServer, HttpServerTester};
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress, Transport};
use teseladb::engine::net::stack::SocketImpl;
use teseladb::engine::util::defer::defer;

struct TestMetrics {
    metrics: MetricGroups,
}

impl TestMetrics {
    fn new() -> Self {
        TestMetrics { metrics: MetricGroups::default() }
    }

    fn setup_metrics(&mut self) -> Result<(), ExceptionPtr> {
        let somelabel = Label::new("somekey");
        self.metrics.add_group(
            "aaaa",
            vec![
                metrics::make_gauge(
                    "escaped_label_value_test",
                    || 10,
                    Description::new("test that special characters are escaped"),
                    vec![somelabel.call(r#"special"\nvalue"#)],
                ),
                metrics::make_gauge(
                    "int_test",
                    || 10,
                    Description::new("simple minimal test"),
                    vec![],
                ),
                metrics::make_gauge(
                    "double_test",
                    || 1234567654321.0,
                    Description::new(
                        "test that a long double is printed fully and not in scientific notation",
                    ),
                    vec![],
                ),
                metrics::make_counter(
                    "counter_test",
                    || -> i64 { 1234567654321 },
                    Description::new("test with a long counter value"),
                    vec![],
                ),
            ],
        )?;
        Ok(())
    }
}

/// Asserts that `resp_str` contains `s`, printing the response when it does not.
fn require_contains(resp_str: &str, s: &str) {
    assert!(resp_str.contains(s), "Response: {}", resp_str);
}

fn test_prometheus_metrics_body() -> Future<()> {
    make_coroutine(async move {
        let mut metrics = TestMetrics::new();
        metrics.setup_metrics()?;

        async_(|| -> Result<(), ExceptionPtr> {
            let mut lcf = LoopbackConnectionFactory::new(1);
            let mut server = HttpServer::new(&Sstring::from("test"));
            let mut lsi = LoopbackSocketImpl::new(&mut lcf, None);
            let server_socket = lcf.get_server_socket();
            HttpServerTester::listeners(&mut server).push(server_socket);
            let ctx = prometheus::Config::default();
            add_prometheus_routes(&mut server, ctx).get()?;

            let lsi_ptr: *mut LoopbackSocketImpl = &mut lsi;
            let client = async_(move || -> Result<(), ExceptionPtr> {
                // SAFETY: `lsi` outlives `client`, which is joined below.
                let lsi = unsafe { &mut *lsi_ptr };
                let mut c_socket = lsi
                    .connect(
                        SocketAddress::from_ipv4_addr(Ipv4Addr::default()),
                        SocketAddress::from_ipv4_addr(Ipv4Addr::default()),
                        Transport::TCP,
                    )
                    .get()?;
                let mut input = c_socket.input(Default::default());
                let input_p: *mut InputStream<u8> = &mut input;
                // Close the input on scope exit.
                // SAFETY: `input` is declared before the guard and so outlives it.
                let _close_input = defer(move || drop(unsafe { (*input_p).close() }.get()));
                let mut output = c_socket.output(8192);
                let output_p: *mut OutputStream<u8> = &mut output;
                // SAFETY: as above.
                let _close_output = defer(move || drop(unsafe { (*output_p).close() }.get()));
                output.write_str("GET /metrics HTTP/1.1\r\nHost: test\r\n\r\n").get()?;
                output.flush().get()?;
                let resp = input.read().get()?;
                let resp_str = String::from_utf8_lossy(resp.as_slice()).into_owned();
                assert!(resp_str.contains("200 OK"));
                require_contains(&resp_str, r#"engine_aaaa_escaped_label_value_test{shard="0",somekey="special\"\\nvalue"} 10.000000"#);
                require_contains(&resp_str, r#"engine_aaaa_int_test{shard="0"} 10.000000"#);
                require_contains(&resp_str, r#"engine_aaaa_double_test{shard="0"} 1234567654321.000000"#);
                require_contains(&resp_str, r#"engine_aaaa_counter_test{shard="0"} 1234567654321"#);
                Ok(())
            });

            // The server is pinned from its first accept on (the connections point at it).
            let mut server = std::pin::pin!(server);
            // The server's setup, at the server's final place.
            server.as_mut().start();
            server.as_mut().do_accepts(0).get()?;

            client.get()?;
            // SAFETY: stop() does not move the server out of its pin.
            unsafe { server.as_mut().get_unchecked_mut() }.stop().get()?;
            Ok(())
        })
        .await?;
        drop(metrics);
        Ok(())
    })
}

teseladb::engine_test_case!(test_prometheus_metrics, { test_prometheus_metrics_body() });
