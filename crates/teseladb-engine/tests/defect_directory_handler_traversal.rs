#![cfg(target_os = "linux")]
//! A directory handler never serves a file outside its document root: a path with a `..`
//! segment, raw (`/../secret`) or percent-encoded (`/%2e%2e%2fsecret`), or a doubled leading
//! slash, is answered with "not found" before the filesystem is touched, so a sibling of the
//! document root stays unreachable although it exists. Names that only start with `..` are
//! still served.

#[path = "common/tmpdir.rs"]
mod tmpdir;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::http::file_handler::DirectoryHandler;
use teseladb::engine::http::handlers::HandlerBase;
use teseladb::engine::http::reply::{Reply, StatusType};
use teseladb::engine::http::request::Request;
use teseladb::engine_test_case;
use tmpdir::Tmpdir;

engine_test_case!(test_directory_handler_path_traversal, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let tmp = Tmpdir::new();
        let doc_root = tmp.path().join("public");
        std::fs::create_dir(&doc_root).expect("doc root");
        std::fs::write(doc_root.join("index.html"), "public").expect("index");
        std::fs::write(doc_root.join("..foo"), "dots").expect("..foo");
        // A file next to the document root, which a traversal would try to reach.
        std::fs::write(tmp.path().join("secret"), "secret").expect("secret");

        // The root ends with '/': get_decoded_param() strips the path matcher's leading '/'
        // from the parameter, so the handler appends the name to the root.
        let root = Sstring::from(format!("{}/", doc_root.display()).as_str());
        let mut handler = DirectoryHandler::new(&root, None);
        let mut handle = |param: &str| -> Result<StatusType, ExceptionPtr> {
            let mut req = Box::new(Request::default());
            req.param.set(&Sstring::from("path"), &Sstring::from(param));
            let rep = handler.handle(&Sstring::new(), req, Box::new(Reply::new())).get()?;
            Ok(rep._status)
        };

        // Requests inside the document root are served.
        assert_eq!(handle("/index.html")?, StatusType::OK);
        assert_eq!(handle("/..foo")?, StatusType::OK);
        // Raw and percent-encoded traversals do not escape it.
        for param in ["/../secret", "/%2e%2e%2fsecret", "/subdir/../../secret", "//etc/passwd"] {
            assert_eq!(handle(param)?, StatusType::NOT_FOUND, "{param} was served");
        }
        Ok(())
    })
});
