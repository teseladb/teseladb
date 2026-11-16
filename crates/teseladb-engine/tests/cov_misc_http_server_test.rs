#![cfg(target_os = "linux")]
//! Tests of the HTTP handlers whose body is streamed when the reply is written: files and
//! directory indexes served from disk, the streamed API documentation with entries read
//! from files, and the JSON errors, all fetched over TCP from a real server.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::http::api_docs::{ApiRegistryBuilder, ApiRegistryBuilder20, DocEntry};
use teseladb::engine::http::common::httpd::OperationType;
use teseladb::engine::http::file_handler::{DirectoryHandler, FileHandler};
use teseladb::engine::http::httpd::httpd::HttpServerControl;
use teseladb::engine::http::routes::{Routes, Url};
use teseladb::engine::http::transformers::ContentReplace;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

struct Response {
    head: String,
    body: Vec<u8>,
}

impl Response {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").expect("chunk size line");
        let size =
            usize::from_str_radix(std::str::from_utf8(&b[..eol]).unwrap().trim(), 16).unwrap();
        b = &b[eol + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

fn blocking_get(port: u16, target: &str) -> Response {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("end of headers");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let rest = &raw[split + 4..];
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        dechunk(rest)
    } else {
        rest.to_vec()
    };
    Response { head, body }
}

/// Sends the request from another OS thread, so that the reactor keeps serving it while this
/// engine thread waits.
fn get(port: u16, target: &str) -> Response {
    let (tx, rx) = mpsc::channel();
    let target = target.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(blocking_get(port, &target));
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                sleep(Duration::from_millis(2)).get().unwrap();
            }
            Err(e) => panic!("no response: {e:?}"),
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A fresh directory under the system's temporary directory, removed when dropped.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cov_misc_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn root(&self) -> String {
        self.0.display().to_string()
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.0.join(name), text).unwrap();
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ss(s: &str) -> Sstring {
    Sstring::from(s)
}

/// Installs the handlers of these tests on every shard.
fn install(routes: &mut Routes, root: &str) {
    let mut url = Url::new(&ss("/static"));
    url.remainder(&ss("path"));
    routes.add(OperationType::Get, &url, Box::new(DirectoryHandler::new(&ss(root), None)));
    let mut url = Url::new(&ss("/replaced"));
    url.remainder(&ss("path"));
    routes.add(
        OperationType::Get,
        &url,
        Box::new(DirectoryHandler::new(
            &ss(root),
            Some(Box::new(ContentReplace::new(&ss("json")))),
        )),
    );
    let page = format!("{root}/page.html");
    routes
        .put(OperationType::Get, "/page", Box::new(FileHandler::new(&ss(&page), None, false)))
        .unwrap();

    // Swagger 2.0: an inline entry, an API file and a definitions file.
    let b20 = ApiRegistryBuilder20::new(&ss(root), &ss("/v2/api-doc"));
    b20.set_api_doc(routes).unwrap();
    b20.register_function(routes, DocEntry::new(|os| os.write_str("{\"paths\": {")));
    b20.register_api_file(routes, &ss("file_api"));
    b20.add_definitions_file(routes, &ss("/file_defs"));

    // Swagger 1.2: the listing and an API file served with the host substituted.
    let dir = format!("{root}/");
    let b12 = ApiRegistryBuilder::new(&ss(&dir), &ss("/api-doc"));
    b12.set_api_doc(routes).unwrap();
    b12.register_function(routes, &ss("api1"), &ss("first"), &Sstring::new()).unwrap();
}

thread_case!(files_and_api_documents_are_streamed, {
    let d = TmpDir::new("srv");
    d.write("page.html", "<p>hi</p>");
    std::fs::create_dir(d.0.join("sub")).unwrap();
    d.write("sub/index.html", "index page");
    d.write("file_api.json", "\"/f\": {}");
    d.write("file_defs.def.json", "\"d\": {}");
    d.write("api1.json", "{\"basePath\": \"{{Protocol}}://{{Host}}/v1\"}");
    d.write("data.json", "{\"host\": \"{{Host}}\", \"keep\": \"{x}\"}");

    let mut server = Box::new(HttpServerControl::new());
    server.start(&ss("cov_misc_files")).get().unwrap();
    let root = d.root();
    server.set_routes(move |routes: &mut Routes| install(routes, &root)).get().unwrap();
    let port = free_port();
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    server.listen(SocketAddress::from_inet_address(&ip, port)).get().unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let r = get(port, "/static/page.html");
        assert!(r.head.starts_with("HTTP/1.1 200"), "{}", r.head);
        assert!(r.head.to_ascii_lowercase().contains("content-type: text/html"), "{}", r.head);
        assert_eq!(r.text(), "<p>hi</p>");

        let r = get(port, "/static/sub/");
        assert_eq!(r.text(), "index page");
        let r = get(port, "/static/sub");
        assert!(r.head.starts_with("HTTP/1.1 301"), "{}", r.head);
        let r = get(port, "/static/nothing");
        assert!(r.head.starts_with("HTTP/1.1 404"), "{}", r.head);

        let r = get(port, "/page");
        assert_eq!(r.text(), "<p>hi</p>");

        let r = get(port, "/replaced/data.json");
        assert_eq!(r.text(), "{\"host\": \"localhost\", \"keep\": \"{x}\"}");

        let r = get(port, "/v2/api-doc");
        assert!(r.head.starts_with("HTTP/1.1 200"), "{}", r.head);
        assert_eq!(r.text(), "{\"paths\": {\"/f\": {}},\"definitions\": {\"d\": {}}}");

        let r = get(port, "/api-doc");
        assert!(r.text().contains("\"/api1\""), "{}", r.text());
        let r = get(port, "/api-doc/api1/");
        assert_eq!(r.text(), "{\"basePath\": \"http://localhost/v1\"}");
    }));
    server.stop().get().unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
});
