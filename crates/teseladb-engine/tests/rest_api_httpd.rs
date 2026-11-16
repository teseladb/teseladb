//! A small REST API server for the HTTP tests, with the routes that `tests/data/api.json`
//! describes (`common/api_json.rs`). A program with its own `main()` (`harness = false`),
//! driven by `tests/data/json2code_test.py`.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/api_json.rs"]
mod api_json_hh;
#[path = "common/stop_signal.rs"]
mod stop_signal;

use std::io::Write;
use std::sync::Arc;

use api_json_hh::api_json;
use api_json_hh::api_json::ns_hello_world::{QueryEnum, str2query_enum};
use stop_signal::StopSignal;
use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, OutOfRange, make_exception_ptr};
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::api_docs::ApiRegistryBuilder;
use teseladb::engine::http::exception::bad_param_exception;
use teseladb::engine::http::handlers::ConstReq;
use teseladb::engine::http::httpd::httpd::HttpServerControl;
use teseladb::engine::http::routes::Routes;
use teseladb::engine::json::json_elements::{JsonReturnType, stream_object};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::util::defer::defer;

/// The value of a map lookup, or an out-of-range error when the key is missing.
fn at(v: Option<&Sstring>) -> Result<&Sstring, ExceptionPtr> {
    v.ok_or_else(|| make_exception_ptr(OutOfRange::new("_Map_base::at")))
}

fn set_routes(r: &mut Routes) {
    let handler = |req: ConstReq<'_>| -> Result<JsonReturnType, ExceptionPtr> {
        let mut obj = api_json::MyObject::new();
        obj.var1.set(at(req.param.at(&Sstring::from("var1")))?.clone());
        obj.var2.set(at(req.param.at(&Sstring::from("var2")))?.clone());
        let v: QueryEnum =
            str2query_enum(at(req.query_parameters.get(&Sstring::from("query_enum")))?);
        // This demonstrate enum conversion
        obj.enum_var.set(v);
        let vec123: Vec<i32> = vec![1, 2, 3];
        obj.array_var.assign(vec123.iter().copied());
        obj.chunked_array_var.assign(vec123.iter().copied());
        let use_streaming = at(req.query_parameters.get(&Sstring::from("use_streaming")))?.clone();
        if use_streaming != "false" && use_streaming != "true" {
            return Err(make_exception_ptr(bad_param_exception(
                "param use_streaming must be true or false",
            )));
        }
        if use_streaming == "false" {
            JsonReturnType::new(&obj)
        } else {
            Ok(JsonReturnType::from_body_writer(stream_object(obj)))
        }
    };
    if let Err(e) = api_json::hello_world().set_json_request_function(r, Box::new(handler)) {
        panic!("{}", e);
    }
}

pub(crate) fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut app = AppTemplate::new(Default::default());
    app.add_options().opt("port", bpo::value::<u16>().default_value(10000), "HTTP Server port");
    let appp: *mut AppTemplate = &mut *app;
    let status = app.run_with_exit_code(args, move || {
        async_(move || -> Result<i32, ExceptionPtr> {
            let mut stop_signal = StopSignal::new();
            // SAFETY: the application outlives run(), which waits for this thread.
            let config = unsafe { (*appp).configuration() };
            let port: u16 = config.get::<u16>("port").copied().unwrap_or_default();
            let mut server = Box::new(HttpServerControl::new());
            let rb = Arc::new(ApiRegistryBuilder::new(
                &Sstring::from("apps/httpd/"),
                &Sstring::from("/api-doc"),
            ));

            server.start(&HttpServerControl::generate_server_name()).get()?;

            let serverp: *mut HttpServerControl = &mut *server;
            let _stop_server = defer(move || {
                // Printing must not fail: a failed write (the reader of the pipe is
                // gone) is ignored, where println! panics.
                let _ = writeln!(std::io::stdout(), "Stopping HTTP server"); // This can throw, but won't.
                let _ = std::io::stdout().flush();
                // SAFETY: the server box outlives this deferred action, declared after it.
                let _ = unsafe { (*serverp).stop() }.get();
            });

            server.set_routes(set_routes).get()?;
            let rb1 = rb.clone();
            server
                .set_routes(move |r: &mut Routes| {
                    if let Err(e) = rb1.set_api_doc(r) {
                        panic!("{}", e);
                    }
                })
                .get()?;
            let rb2 = rb.clone();
            server
                .set_routes(move |r: &mut Routes| {
                    if let Err(e) = rb2.register_function(
                        r,
                        &Sstring::from("demo"),
                        &Sstring::from("rest api test"),
                        &Sstring::new(),
                    ) {
                        panic!("{}", e);
                    }
                })
                .get()?;
            server.listen(SocketAddress::from_port(port)).get()?;

            println!("{}", port);
            let _ = std::io::stdout().flush();

            stop_signal.wait().get()?;
            Ok(0)
        })
    });
    std::process::exit(status);
}
