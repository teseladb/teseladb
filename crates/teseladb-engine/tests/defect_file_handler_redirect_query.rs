#![cfg(target_os = "linux")]
//! The redirect of a directory path to its form with a trailing slash puts the slash at the
//! end of the path, before the query: before, it was appended to the whole target (after the
//! query), and a slash at the end of the query was taken for one at the end of the path.

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::http::file_handler::FileInteractionHandler;
use teseladb::engine::http::reply::{Reply, StatusType};
use teseladb::engine::http::request::Request;
use teseladb::engine::util::string_utils::internal::CaseInsensitiveKey;
use teseladb::engine_test_case;

fn request(target: &str) -> Request {
    let mut req = Request::default();
    req._headers.insert(CaseInsensitiveKey(Sstring::from("Host")), Sstring::from("xx.yy"));
    req._url = Sstring::from(target);
    req
}

engine_test_case!(test_file_handler_redirect_preserves_query, {
    let handler = FileInteractionHandler::new(None);
    for (target, location) in [
        ("/docs?lang=zh", "http://xx.yy/docs/?lang=zh"),
        ("/docs?next=/", "http://xx.yy/docs/?next=/"),
        ("/docs?lang=zh/", "http://xx.yy/docs/?lang=zh/"),
        ("/doc", "http://xx.yy/doc/"),
    ] {
        let mut rep = Reply::new();
        assert!(handler.redirect_if_needed(&request(target), &mut rep), "{target}");
        assert_eq!(rep._status, StatusType::MOVED_PERMANENTLY);
        assert_eq!(rep.get_header("Location"), Sstring::from(location), "{target}");
    }

    // A slash at the end of the path prevents a redirect whatever the query holds.
    let mut rep = Reply::new();
    assert!(!handler.redirect_if_needed(&request("/docs/?lang=zh"), &mut rep));
    assert!(rep.get_header("Location").empty());
    make_ready_future(())
});
