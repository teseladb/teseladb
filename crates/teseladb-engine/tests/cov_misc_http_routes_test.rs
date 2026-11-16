//! Tests of the HTTP routing layer driven without a connection: status lines and classes,
//! MIME types, the HTTP error varieties and their replies, request helpers (addresses, URLs,
//! keep-alive, query strings), path parameters, function handlers of every form, mandatory
//! parameters, exception handlers, the default handler, aliases, path descriptions, the
//! API documentation registries and the file and directory handlers up to the body writer.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::path::PathBuf;

use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::iostream::OutputStream;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::http::api_docs::{
    ApiDoc, ApiDocs, ApiRegistryBuilder, ApiRegistryBuilder20, ApiRegistryBuilderBase, DEFAULT_DIR,
    DEFAULT_PATH, DocEntry,
};
use teseladb::engine::http::common::httpd::{OperationType, Parameters, str2type, type2str};
use teseladb::engine::http::exception::{
    BaseException, ExceptionKind, JsonException, bad_chunk_exception, bad_param_exception,
    bad_request_exception, missing_param_exception, not_found_exception,
    not_found_exception_default, redirect_exception, redirect_exception_default,
    response_parsing_exception, server_error_exception, unexpected_status_error,
};
use teseladb::engine::http::file_handler::{
    DirectoryHandler, FileHandler, FileInteractionHandler, FileTransformer,
};
use teseladb::engine::http::function_handlers::FunctionHandler;
use teseladb::engine::http::handlers::{HandlerBase, HandlerBaseData};
use teseladb::engine::http::json_path::{PathDescription, PathPart, UrlComponentType};
use teseladb::engine::http::matchrules::MatchRule;
use teseladb::engine::http::mime_types::mime_types::extension_to_type;
use teseladb::engine::http::reply::{Reply, StatusClass, StatusType};
use teseladb::engine::http::request::{Ctclass, Request};
use teseladb::engine::http::routes::{Routes, Url};
use teseladb::engine::http::transformers::ContentReplace;
use teseladb::engine::http::url::internal::{path_decode, url_decode};
use teseladb::engine::json::json_elements::{JsonBase, JsonReturnType, Jsonable};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine_thread_test_case;

fn ss(s: &str) -> Sstring {
    Sstring::from(s)
}

/// Routes a request for `url` (with an optional query) through `routes`, as the server does.
fn route(routes: &mut Routes, method: &str, url: &str) -> Box<Reply> {
    let mut req = Request::make(ss(method), ss("localhost"), ss(url));
    req._version = ss("1.1");
    let path = req.parse_query_param();
    routes.handle(&path, Box::new(req), Box::new(Reply::new())).get().expect("a reply")
}

/// The message of a runtime error.
fn msg(e: &ExceptionPtr) -> String {
    e.downcast_ref::<RuntimeError>().map(|r| r.0.clone()).unwrap_or_default()
}

fn content(rep: &Reply) -> String {
    rep._content.as_str().to_owned()
}

// ---------------------------------------------------------------------------------------
// Replies, statuses and MIME types

#[test]
fn every_status_has_its_reason_phrase() {
    let all = [
        (StatusType::CONTINUE, "100 Continue"),
        (StatusType::SWITCHING_PROTOCOLS, "101 Switching Protocols"),
        (StatusType::OK, "200 OK"),
        (StatusType::CREATED, "201 Created"),
        (StatusType::ACCEPTED, "202 Accepted"),
        (StatusType::NONAUTHORITATIVE_INFORMATION, "203 Non-Authoritative Information"),
        (StatusType::NO_CONTENT, "204 No Content"),
        (StatusType::RESET_CONTENT, "205 Reset Content"),
        (StatusType::PARTIAL_CONTENT, "206 Partial Content"),
        (StatusType::MULTIPLE_CHOICES, "300 Multiple Choices"),
        (StatusType::MOVED_PERMANENTLY, "301 Moved Permanently"),
        (StatusType::MOVED_TEMPORARILY, "302 Moved Temporarily"),
        (StatusType::SEE_OTHER, "303 See Other"),
        (StatusType::NOT_MODIFIED, "304 Not Modified"),
        (StatusType::USE_PROXY, "305 Use Proxy"),
        (StatusType::TEMPORARY_REDIRECT, "307 Temporary Redirect"),
        (StatusType::BAD_REQUEST, "400 Bad Request"),
        (StatusType::UNAUTHORIZED, "401 Unauthorized"),
        (StatusType::PAYMENT_REQUIRED, "402 Payment Required"),
        (StatusType::FORBIDDEN, "403 Forbidden"),
        (StatusType::NOT_FOUND, "404 Not Found"),
        (StatusType::METHOD_NOT_ALLOWED, "405 Method Not Allowed"),
        (StatusType::NOT_ACCEPTABLE, "406 Not Acceptable"),
        (StatusType::REQUEST_TIMEOUT, "408 Request Timeout"),
        (StatusType::CONFLICT, "409 Conflict"),
        (StatusType::GONE, "410 Gone"),
        (StatusType::LENGTH_REQUIRED, "411 Length Required"),
        (StatusType::PAYLOAD_TOO_LARGE, "413 Payload Too Large"),
        (StatusType::URI_TOO_LONG, "414 URI Too Long"),
        (StatusType::UNSUPPORTED_MEDIA_TYPE, "415 Unsupported Media Type"),
        (StatusType::EXPECTATION_FAILED, "417 Expectation Failed"),
        (StatusType::PAGE_EXPIRED, "419 Page Expired"),
        (StatusType::UNPROCESSABLE_ENTITY, "422 Unprocessable Entity"),
        (StatusType::UPGRADE_REQUIRED, "426 Upgrade Required"),
        (StatusType::TOO_MANY_REQUESTS, "429 Too Many Requests"),
        (StatusType::LOGIN_TIMEOUT, "440 Login Timeout"),
        (StatusType::INTERNAL_SERVER_ERROR, "500 Internal Server Error"),
        (StatusType::NOT_IMPLEMENTED, "501 Not Implemented"),
        (StatusType::BAD_GATEWAY, "502 Bad Gateway"),
        (StatusType::SERVICE_UNAVAILABLE, "503 Service Unavailable"),
        (StatusType::GATEWAY_TIMEOUT, "504 Gateway Timeout"),
        (StatusType::HTTP_VERSION_NOT_SUPPORTED, "505 HTTP Version Not Supported"),
        (StatusType::INSUFFICIENT_STORAGE, "507 Insufficient Storage"),
        (StatusType::BANDWIDTH_LIMIT_EXCEEDED, "509 Bandwidth Limit Exceeded"),
        (StatusType::NETWORK_READ_TIMEOUT, "598 Network Read Timeout"),
        (StatusType::NETWORK_CONNECT_TIMEOUT, "599 Network Connect Timeout"),
    ];
    for (st, text) in all {
        assert_eq!(st.to_string(), text);
        let mut rep = Reply::new();
        rep.set_version(&ss("1.1")).set_status(st, Sstring::new());
        assert_eq!(rep.response_line(), format!("HTTP/1.1 {text}\r\n").as_str());
    }
    // An unknown code is reported as an internal error.
    assert_eq!(StatusType(299).to_string(), "500 Internal Server Error");

    assert_eq!(Reply::classify_status(StatusType(99)), StatusClass::Unclassified);
    assert_eq!(Reply::classify_status(StatusType(600)), StatusClass::Unclassified);
    assert_eq!(Reply::classify_status(StatusType::CONTINUE), StatusClass::Informational);
    assert_eq!(Reply::classify_status(StatusType::SEE_OTHER), StatusClass::Redirection);
}

#[test]
fn replies_default_to_html_and_keep_their_headers() {
    let mut rep = Reply::default();
    assert_eq!(rep._status, StatusType::OK);
    rep.set_content_type_default();
    assert_eq!(rep.get_header("content-type"), "text/html");
    rep.add_header(&ss("X-Test"), &ss("1"));
    assert_eq!(rep.get_header("x-test"), "1");
    assert_eq!(rep.get_header("missing"), "");
    // Setting a status with empty content keeps the previous content.
    rep.set_status(StatusType::OK, ss("body"));
    rep.set_status(StatusType::CREATED, Sstring::new());
    assert_eq!(content(&rep), "body");
    rep.set_mime_type(&ss("application/x-custom")).done();
    assert_eq!(rep.get_header("Content-Type"), "application/x-custom");
    assert!(rep._response_line.as_str().contains("201 Created"));
}

#[test]
#[allow(deprecated)]
fn file_extensions_map_to_mime_types() {
    let all = [
        ("json", "application/json"),
        ("xml", "application/xml"),
        ("gif", "image/gif"),
        ("htm", "text/html"),
        ("css", "text/css"),
        ("js", "text/javascript"),
        ("html", "text/html"),
        ("jpg", "image/jpeg"),
        ("svg", "image/svg+xml"),
        ("png", "image/png"),
        ("txt", "text/plain"),
        ("ico", "image/x-icon"),
        ("bin", "application/octet-stream"),
    ];
    for (ext, mime) in all {
        assert_eq!(extension_to_type(ext), mime);
    }
    assert!(extension_to_type("proto").starts_with("application/vnd.google.protobuf"));
    assert_eq!(extension_to_type("unknown"), "text/plain");
    assert_eq!(extension_to_type(""), "text/plain");
    assert_eq!(
        teseladb::engine::http::mime_types::httpd::mime_types::extension_to_type("png"),
        "image/png"
    );
}

// ---------------------------------------------------------------------------------------
// HTTP errors

#[test]
fn http_errors_have_kinds_statuses_and_messages() {
    let base = BaseException::new("plain", StatusType::CONFLICT);
    assert_eq!(base.what(), "plain");
    assert_eq!(base.str(), "plain");
    assert_eq!(base.status(), StatusType::CONFLICT);
    assert_eq!(base.kind(), &ExceptionKind::Base);
    assert!(!base.is_bad_request());
    assert!(!base.is_server_error());
    assert_eq!(base.url(), None);
    assert_eq!(base.to_string(), "plain (409 Conflict)");

    let nf = not_found_exception_default();
    assert_eq!(nf.what(), "Not found");
    assert_eq!(nf.status(), StatusType::NOT_FOUND);
    assert_eq!(nf.kind(), &ExceptionKind::NotFound);
    assert_eq!(not_found_exception("gone").what(), "gone");

    for e in [
        bad_request_exception("r"),
        bad_param_exception("p"),
        missing_param_exception("id"),
        bad_chunk_exception("c"),
    ] {
        assert!(e.is_bad_request(), "{e}");
        assert!(!e.is_server_error());
        assert_eq!(e.status(), StatusType::BAD_REQUEST);
    }
    assert_eq!(bad_param_exception("p").kind(), &ExceptionKind::BadParam);
    assert_eq!(missing_param_exception("id").what(), "Missing mandatory parameter 'id'");
    assert!(bad_chunk_exception("c").what().contains("'c'"));

    for e in [server_error_exception("s"), response_parsing_exception("r")] {
        assert!(e.is_server_error());
        assert!(!e.is_bad_request());
        assert_eq!(e.status(), StatusType::INTERNAL_SERVER_ERROR);
    }

    let r = redirect_exception_default("/elsewhere");
    assert_eq!(r.url(), Some("/elsewhere"));
    assert_eq!(r.status(), StatusType::MOVED_PERMANENTLY);
    assert_eq!(redirect_exception("/t", StatusType::TEMPORARY_REDIRECT).status().0, 307);

    let u = unexpected_status_error(StatusType::GONE);
    assert_eq!(u.what(), "410 Gone");
    assert_eq!(u.kind(), &ExceptionKind::UnexpectedStatus);

    let j = JsonException::from_base(&nf);
    assert!(j.element(2).is_none());
    let text = Jsonable::to_json(&j).unwrap();
    assert!(text.as_str().contains("\"message\": \"Not found\""), "{text}");
    assert!(text.as_str().contains("\"code\": 404"), "{text}");
    let j = JsonException::from_exception_ptr(&ExceptionPtr::new(RuntimeError::new("boom")));
    assert_eq!(*j._code.get(), 500);
    assert!(j._msg.get().as_str().contains("boom"));
}

// ---------------------------------------------------------------------------------------
// Requests, parameters and URLs

#[test]
fn operation_names_round_trip() {
    for op in [
        OperationType::Get,
        OperationType::Post,
        OperationType::Put,
        OperationType::Delete,
        OperationType::Head,
        OperationType::Options,
        OperationType::Trace,
        OperationType::Connect,
        OperationType::Patch,
    ] {
        assert_eq!(str2type(type2str(op)), op);
    }
    assert_eq!(str2type("BREW"), OperationType::Get);
}

#[test]
#[allow(deprecated)]
fn path_parameters_are_stored_with_their_slash() {
    let mut p = Parameters::default();
    p.set(&ss("name"), &ss("/a%20b"));
    p.set(&ss("bad"), &ss("/%2"));
    assert!(p.exists(&ss("name")));
    assert!(!p.exists(&ss("other")));
    assert_eq!(p.index(&ss("name")).unwrap(), "a%20b");
    assert!(p.index(&ss("other")).is_none());
    assert_eq!(p.at(&ss("name")).unwrap().as_str(), "/a%20b");
    assert_eq!(p.get_decoded_param(&ss("name")), "a b");
    assert_eq!(p.get_decoded_param(&ss("bad")), "");
    assert_eq!(p.get_decoded_param(&ss("other")), "");
    p.clear();
    assert!(!p.exists(&ss("name")));

    let mut out = Sstring::new();
    assert!(url_decode(b"%2f%2F%41", &mut out));
    assert_eq!(out, "//A");
    let mut out = Sstring::new();
    assert!(path_decode(b"x%2fy", &mut out));
    assert_eq!(out, "x/y");
}

#[test]
fn requests_report_addresses_urls_and_body_kinds() {
    let mut req =
        Request::make_op(OperationType::Post, ss("example.org"), ss("/p?flag&a=1&b=&%z=1"));
    assert_eq!(req._method, "POST");
    assert_eq!(req.get_url(), "http://example.org/p?flag&a=1&b=&%z=1");
    let path = req.parse_query_param();
    assert_eq!(path, "/p");
    assert_eq!(req.get_query_param("flag"), "");
    assert!(req.query_parameters.contains_key("flag".as_bytes()));
    assert_eq!(req.get_query_param("a"), "1");
    assert!(req.query_parameters.contains_key("b".as_bytes()));
    assert_eq!(req.query_parameters.len(), 3, "the undecodable key is dropped");

    let addr = SocketAddress::from_ipv4_port(0x7f00_0001, 8080);
    req._client_address = addr;
    req._server_address = addr;
    assert_eq!(*req.get_client_address(), addr);
    assert_eq!(*req.get_server_address(), addr);
    req.param.set(&ss("id"), &ss("/42"));
    assert_eq!(req.get_path_param(&ss("id")), "42");
    assert_eq!(req.get_protocol_name(), "http");

    assert!(!req.is_multi_part());
    assert!(!req.is_form_post());
    req.content_type_class = Ctclass::Multipart;
    assert!(req.is_multi_part());
    req.content_type_class = Ctclass::AppXWwwUrlencoded;
    assert!(req.is_form_post());

    // Without a Host header the URL has an empty authority.
    let bare = Request { _url: ss("/x"), ..Request::default() };
    assert_eq!(bare.get_url(), "http:///x");
}

#[test]
fn keep_alive_depends_on_the_version_and_connection_header() {
    let mut req = Request { _version: ss("0.9"), ..Request::default() };
    assert!(!req.should_keep_alive());
    req._version = ss("1.0");
    assert!(!req.should_keep_alive());
    req._headers.insert(
        teseladb::engine::util::string_utils::internal::CaseInsensitiveKey(ss("Connection")),
        ss("Keep-Alive"),
    );
    assert!(req.should_keep_alive());
    req._version = ss("1.1");
    assert!(req.should_keep_alive());
    req._headers.insert(
        teseladb::engine::util::string_utils::internal::CaseInsensitiveKey(ss("Connection")),
        ss("close"),
    );
    assert!(!req.should_keep_alive());
}

// ---------------------------------------------------------------------------------------
// Routing

fn text_handler(text: &'static str) -> Box<dyn HandlerBase> {
    Box::new(FunctionHandler::from_handle_function(
        Box::new(move |_req: &Request, _rep: &mut Reply| Ok(ss(text))),
        &ss("txt"),
    ))
}

engine_thread_test_case!(function_handlers_of_every_form_fill_the_reply, {
    let mut routes = Routes::default();
    routes.put(OperationType::Get, "/handle", text_handler("handled")).unwrap();
    routes
        .put(
            OperationType::Get,
            "/handle_err",
            Box::new(FunctionHandler::from_handle_function(
                Box::new(|_req: &Request, _rep: &mut Reply| {
                    Err(ExceptionPtr::new(bad_param_exception("bad value")))
                }),
                &ss("txt"),
            )),
        )
        .unwrap();
    routes
        .put(
            OperationType::Get,
            "/request",
            Box::new(FunctionHandler::from_request_function(
                Box::new(|req: &Request| Ok(req.get_query_param("q"))),
                &ss("json"),
            )),
        )
        .unwrap();
    routes
        .put(
            OperationType::Get,
            "/request_err",
            Box::new(FunctionHandler::from_request_function(
                Box::new(|_req: &Request| Err(ExceptionPtr::new(RuntimeError::new("req fail")))),
                &ss("json"),
            )),
        )
        .unwrap();
    routes
        .put(
            OperationType::Get,
            "/json_err",
            Box::new(FunctionHandler::from_json_request_function(Box::new(|_req: &Request| {
                Err(ExceptionPtr::new(not_found_exception("no such item")))
            }))),
        )
        .unwrap();
    routes
        .put(
            OperationType::Get,
            "/future_json",
            Box::new(FunctionHandler::from_future_json_function(Box::new(|req: Box<Request>| {
                match JsonReturnType::new(&req.get_query_param("v")) {
                    Ok(r) => make_ready_future(r),
                    Err(e) => make_exception_future(e),
                }
            }))),
        )
        .unwrap();

    let rep = route(&mut routes, "GET", "/handle");
    assert_eq!(rep._status, StatusType::OK);
    assert_eq!(content(&rep), "handled");
    assert_eq!(rep.get_header("Content-Type"), "text/plain");

    let rep = route(&mut routes, "GET", "/handle_err");
    assert_eq!(rep._status, StatusType::BAD_REQUEST);
    assert!(content(&rep).contains("bad value"), "{}", content(&rep));

    let rep = route(&mut routes, "GET", "/request?q=hi");
    assert_eq!(content(&rep), "\"hi\"");
    assert_eq!(rep.get_header("Content-Type"), "application/json");

    let rep = route(&mut routes, "GET", "/request_err");
    assert_eq!(rep._status, StatusType::INTERNAL_SERVER_ERROR);
    assert!(content(&rep).contains("req fail"));

    let rep = route(&mut routes, "GET", "/json_err");
    assert_eq!(rep._status, StatusType::NOT_FOUND);
    assert!(content(&rep).contains("no such item"));

    let rep = route(&mut routes, "GET", "/future_json?v=x");
    assert_eq!(content(&rep), "\"x\"");

    // A trailing slash is ignored by the lookup; an unknown path is a JSON 404.
    assert_eq!(content(&route(&mut routes, "GET", "/handle/")), "handled");
    let rep = route(&mut routes, "GET", "/nowhere");
    assert_eq!(rep._status, StatusType::NOT_FOUND);
    assert!(content(&rep).contains("Not found"));
    // The method is part of the key.
    assert_eq!(route(&mut routes, "POST", "/handle")._status, StatusType::NOT_FOUND);
});

engine_thread_test_case!(mandatory_parameters_and_exception_handlers_shape_errors, {
    let mut routes = Routes::new();
    let mut h = text_handler("with id");
    h.mandatory(&ss("id"));
    routes.put(OperationType::Get, "/needs_id", h).unwrap();
    routes
        .put(
            OperationType::Get,
            "/redirect",
            Box::new(FunctionHandler::from_handle_function(
                Box::new(|_req: &Request, _rep: &mut Reply| {
                    Err(ExceptionPtr::new(redirect_exception_default("/target")))
                }),
                &ss("txt"),
            )),
        )
        .unwrap();
    routes
        .put(
            OperationType::Get,
            "/fail",
            Box::new(FunctionHandler::from_handle_function(
                Box::new(|_req: &Request, _rep: &mut Reply| {
                    Err(ExceptionPtr::new(RuntimeError::new("first")))
                }),
                &ss("txt"),
            )),
        )
        .unwrap();

    let rep = route(&mut routes, "GET", "/needs_id");
    assert_eq!(rep._status, StatusType::BAD_REQUEST);
    assert!(content(&rep).contains("Missing mandatory parameter 'id'"));
    assert_eq!(content(&route(&mut routes, "GET", "/needs_id?id=3")), "with id");

    let rep = route(&mut routes, "GET", "/redirect");
    assert_eq!(rep._status, StatusType::MOVED_PERMANENTLY);
    assert_eq!(rep.get_header("Location"), "/target");

    // The first handler replaces the error, the second turns the new one into a reply.
    let first = routes.register_exeption_handler(Box::new(|e: ExceptionPtr| {
        assert!(e.is::<RuntimeError>());
        Err(ExceptionPtr::new(BaseException::new("second", StatusType::CONFLICT)))
    }));
    let second = routes.register_exeption_handler(Box::new(|e: ExceptionPtr| {
        let b = e.downcast_ref::<BaseException>().expect("the replaced error");
        let mut rep = Box::new(Reply::new());
        rep.set_status(StatusType::GONE, Sstring::from(b.what()));
        Ok(rep)
    }));
    assert_ne!(first, second);
    let rep = route(&mut routes, "GET", "/fail");
    assert_eq!(rep._status, StatusType::GONE);
    assert_eq!(content(&rep), "second");

    routes.remove_exception_handler(second);
    let rep = route(&mut routes, "GET", "/fail");
    assert_eq!(rep._status, StatusType::CONFLICT);
    routes.remove_exception_handler(first);
    let rep = routes.exception_reply(ExceptionPtr::new(RuntimeError::new("direct")));
    assert_eq!(rep._status, StatusType::INTERNAL_SERVER_ERROR);
    assert!(content(&rep).contains("direct"));
});

engine_thread_test_case!(default_handler_and_rules_catch_other_paths, {
    let mut routes = Routes::new();
    let mut fallback = text_handler("fallback");
    // SAFETY: `fallback` outlives the routes' use of it (the routes are dropped first).
    unsafe { routes.add_default_handler(&mut *fallback) };
    let mut url = Url::new(&ss("/files"));
    url.remainder(&ss("path"));
    routes.add(OperationType::Get, &url, text_handler("rule"));
    assert_eq!(content(&route(&mut routes, "GET", "/files/a/b")), "rule");
    assert_eq!(content(&route(&mut routes, "GET", "/anything")), "fallback");
    assert_eq!(content(&route(&mut routes, "DELETE", "/files/x")), "fallback");
    assert!(routes.drop(OperationType::Get, "/missing").is_none());
    drop(routes);
    drop(fallback);

    // A rule without matchers matches every URL.
    let rule = MatchRule::new(text_handler("any"));
    let mut params = Parameters::default();
    assert!(rule.get(&ss("/whatever"), &mut params).is_some());
});

engine_thread_test_case!(aliases_copy_function_handlers, {
    let mut routes = Routes::new();
    let old = PathDescription::new(&ss("/old"), OperationType::Get, &ss("old"), &[], &[]);
    old.set_json_request_function(
        &mut routes,
        Box::new(|_req: &Request| JsonReturnType::new(&ss("v1"))),
    )
    .unwrap();
    let new = PathDescription::new(&ss("/new"), OperationType::Get, &ss("new"), &[], &[]);
    routes.add_alias(&old, &new).unwrap();
    assert_eq!(content(&route(&mut routes, "GET", "/new")), "\"v1\"");

    // A path with a fixed part and a parameter.
    let mut with_params = PathDescription::with_parts(
        &ss("/item"),
        OperationType::Get,
        &ss("item"),
        &[PathPart { name: ss("/details"), type_: UrlComponentType::FixedString }],
        &[],
    );
    with_params.pushparam(&ss("id"), false);
    with_params
        .set_json_request_function(
            &mut routes,
            Box::new(|req: &Request| JsonReturnType::new(&req.get_path_param(&ss("id")))),
        )
        .unwrap();
    assert_eq!(content(&route(&mut routes, "GET", "/item/details/7")), "\"7\"");
    let mut alias = PathDescription::new(
        &ss("/thing"),
        OperationType::Get,
        &ss("thing"),
        &[(ss("id"), false)],
        &[],
    );
    alias.push_static_path_part(&ss("/x"));
    routes.add_alias(&with_params, &alias).unwrap();

    // Errors: no handler at the old path, or a handler that is not a function handler.
    let missing = PathDescription::new(&ss("/none"), OperationType::Get, &ss("n"), &[], &[]);
    let e = routes.add_alias(&missing, &new).unwrap_err();
    assert!(msg(&e).contains("not found"), "{}", msg(&e));
    routes.put(OperationType::Get, "/plain", Box::new(Plain::default())).unwrap();
    let plain = PathDescription::new(&ss("/plain"), OperationType::Get, &ss("p"), &[], &[]);
    let e = routes.add_alias(&plain, &new).unwrap_err();
    assert!(msg(&e).contains("not a function_handler"));

    // Setting the same plain path twice fails; unsetting removes either kind of entry.
    let e = old
        .set_json_request_function(&mut routes, Box::new(|_req: &Request| JsonReturnType::new(&1)))
        .unwrap_err();
    assert!(msg(&e).contains("already exists"));
    old.unset(&mut routes);
    assert_eq!(route(&mut routes, "GET", "/old")._status, StatusType::NOT_FOUND);
    with_params.unset(&mut routes);
    assert_eq!(route(&mut routes, "GET", "/item/details/7")._status, StatusType::NOT_FOUND);

    let fut = PathDescription::new(
        &ss("/fut"),
        OperationType::Post,
        &ss("fut"),
        &[(ss("rest"), true)],
        &[ss("must")],
    );
    assert_eq!(fut.mandatory_queryparams.len(), 1);
    fut.set_future_json_function(
        &mut routes,
        Box::new(|req: Box<Request>| match JsonReturnType::new(&req.get_path_param(&ss("rest"))) {
            Ok(r) => make_ready_future(r),
            Err(e) => make_exception_future(e),
        }),
    )
    .unwrap();
    assert_eq!(route(&mut routes, "POST", "/fut/a/b")._status, StatusType::BAD_REQUEST);
    assert_eq!(content(&route(&mut routes, "POST", "/fut/a/b?must=1")), "\"a/b\"");
});

/// A handler that is not a function handler.
#[derive(Default)]
struct Plain {
    base: HandlerBaseData,
}

impl HandlerBase for Plain {
    fn base(&self) -> &HandlerBaseData {
        &self.base
    }
    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.base
    }
    fn handle(
        &mut self,
        _path: &Sstring,
        _req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        rep._content = ss("plain");
        rep.done_with("txt");
        make_ready_future(rep)
    }
}

// ---------------------------------------------------------------------------------------
// API documentation

#[test]
fn api_doc_objects_format_as_json() {
    let mut doc = ApiDoc::default();
    doc.path.set(ss("/p"));
    doc.description.set(ss("desc"));
    let mut copy = ApiDoc::default();
    copy.assign_from(&doc);
    assert!(copy.element(2).is_none());
    let text = Jsonable::to_json(&copy).unwrap();
    assert!(text.as_str().contains("\"path\": \"/p\""), "{text}");
    assert!(text.as_str().contains("\"description\": \"desc\""), "{text}");
    let via_to_json = teseladb::engine::json::formatter::ToJson::to_json(&copy).unwrap();
    assert_eq!(via_to_json, text);

    let mut docs = ApiDocs::default();
    docs.apis.push(doc);
    let mut other = ApiDocs::default();
    other.assign_from(&docs);
    assert!(other.element(3).is_none());
    let text = Jsonable::to_json(&other).unwrap();
    assert!(text.as_str().contains("\"swaggerVersion\": \"1.2\""), "{text}");
    assert!(text.as_str().contains("\"apiVersion\": \"0.0.1\""), "{text}");
    assert!(text.as_str().contains("/p"), "{text}");

    let b = ApiRegistryBuilderBase::default();
    assert_eq!(b._file_directory, DEFAULT_DIR);
    assert_eq!(b._base_path, DEFAULT_PATH);
}

engine_thread_test_case!(api_registries_list_their_apis, {
    let mut routes = Routes::new();
    let builder = ApiRegistryBuilder::new(&ss("/docs/"), &ss("/api-doc"));
    // Without a registry, registering a function does nothing.
    builder.register_function(&mut routes, &ss("early"), &ss("e"), &Sstring::new()).unwrap();
    builder.set_api_doc(&mut routes).unwrap();
    // A second registry at the same path is refused.
    assert!(builder.set_api_doc(&mut routes).is_err());
    builder.register_function(&mut routes, &ss("api1"), &ss("first api"), &Sstring::new()).unwrap();
    builder
        .register_function(&mut routes, &ss("api2"), &ss("second api"), &ss("/elsewhere/a2.json"))
        .unwrap();
    let rep = route(&mut routes, "GET", "/api-doc");
    assert_eq!(rep._status, StatusType::OK);
    let body = content(&rep);
    assert!(body.contains("\"/api1\"") && body.contains("first api"), "{body}");
    assert!(body.contains("\"/api2\"") && body.contains("second api"), "{body}");
    assert_eq!(rep.get_header("Content-Type"), "application/json");

    // The API files are served with a forced trailing slash.
    let rep = route(&mut routes, "GET", "/api-doc/api1");
    assert_eq!(rep._status, StatusType::MOVED_PERMANENTLY);
    assert_eq!(rep.get_header("Location"), "http://localhost/api-doc/api1/");
    let rep = route(&mut routes, "GET", "/api-doc/api2/");
    assert_eq!(rep._status, StatusType::OK);
    assert_eq!(rep.get_header("Content-Type"), "application/json");

    // Another kind of handler at the base path is an error.
    let mut other = Routes::new();
    other.put(OperationType::Get, "/api-doc", Box::new(Plain::default())).unwrap();
    let e = builder.register_function(&mut other, &ss("x"), &ss("x"), &Sstring::new());
    assert!(msg(&e.unwrap_err()).contains("is not an api_registry"));

    // The 2.0 builder: registering before the registry exists is ignored, a second
    // registry is refused, and the registry answers with a streamed JSON body.
    let b20 = ApiRegistryBuilder20::new(&ss("/docs20"), &ss("/v2/api-doc"));
    b20.register_function(&mut routes, DocEntry::new(|os| os.write_str("{")));
    b20.set_api_doc(&mut routes).unwrap();
    assert!(b20.set_api_doc(&mut routes).is_err());
    b20.register_function(&mut routes, DocEntry::new(|os| os.write_str("{\"paths\": {")));
    b20.add_definition(&mut routes, DocEntry::new(|os| os.write_str("\"d\": {}")));
    b20.register_api_file(&mut routes, &ss("file_api"));
    b20.add_definitions_file(&mut routes, &ss("/file_defs"));
    let rep = route(&mut routes, "GET", "/v2/api-doc");
    assert_eq!(rep._status, StatusType::OK);
    assert_eq!(rep.get_header("Content-Type"), "application/json");
    // With a non-registry handler at its path, the 2.0 builder ignores registrations.
    let b20_other = ApiRegistryBuilder20::new(&ss("/d"), &ss("/api-doc"));
    b20_other.register_function(&mut other, DocEntry::new(|os| os.write_str("x")));
    b20_other.add_definition(&mut other, DocEntry::new(|os| os.write_str("x")));
    assert_eq!(content(&route(&mut other, "GET", "/api-doc")), "plain");
});

// ---------------------------------------------------------------------------------------
// File handlers

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
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A transformer that passes the stream through and counts its uses.
struct CountingTransformer(std::rc::Rc<std::cell::Cell<u32>>);

impl FileTransformer for CountingTransformer {
    fn transform(
        &mut self,
        _req: Box<Request>,
        _extension: &Sstring,
        s: OutputStream<u8>,
    ) -> OutputStream<u8> {
        self.0.set(self.0.get() + 1);
        s
    }
}

engine_thread_test_case!(file_handlers_choose_the_file_and_the_reply_kind, {
    let d = TmpDir::new("files");
    std::fs::write(d.0.join("page.HTML"), "<p>hi</p>").unwrap();
    std::fs::create_dir(d.0.join("sub")).unwrap();
    std::fs::write(d.0.join("sub").join("index.html"), "index").unwrap();

    let mut routes = Routes::new();
    let mut url = Url::new(&ss("/static"));
    url.remainder(&ss("path"));
    // The handler appends the path after the prefix, without its leading slash, to the root.
    let doc_root = format!("{}/", d.root());
    routes.add(OperationType::Get, &url, Box::new(DirectoryHandler::new(&ss(&doc_root), None)));
    let single = format!("{}/page.HTML", d.root());
    routes
        .put(OperationType::Get, "/one", Box::new(FileHandler::new(&ss(&single), None, false)))
        .unwrap();
    routes
        .put(
            OperationType::Get,
            "/forced",
            Box::new(FileHandler::new(
                &ss(&single),
                Some(Box::new(ContentReplace::new(&ss("html")))),
                true,
            )),
        )
        .unwrap();

    let rep = route(&mut routes, "GET", "/static/page.HTML");
    assert_eq!(rep._status, StatusType::OK);
    assert_eq!(rep.get_header("Content-Type"), "text/html", "the extension is lower-cased");
    let rep = route(&mut routes, "GET", "/static/missing.txt");
    assert_eq!(rep._status, StatusType::NOT_FOUND);
    let rep = route(&mut routes, "GET", "/static/sub");
    assert_eq!(rep._status, StatusType::MOVED_PERMANENTLY);
    assert_eq!(rep.get_header("Location"), "http://localhost/static/sub/");
    let rep = route(&mut routes, "GET", "/static/sub/");
    assert_eq!(rep._status, StatusType::OK);
    assert_eq!(rep.get_header("Content-Type"), "text/html");

    assert_eq!(route(&mut routes, "GET", "/one")._status, StatusType::OK);
    assert_eq!(route(&mut routes, "GET", "/forced")._status, StatusType::MOVED_PERMANENTLY);
    assert_eq!(route(&mut routes, "GET", "/forced/")._status, StatusType::OK);

    assert_eq!(FileInteractionHandler::get_extension(&ss("/a/b.TXT")), "txt");
    assert_eq!(FileInteractionHandler::get_extension(&ss("noslash.txt")), "");
    assert_eq!(FileInteractionHandler::get_extension(&ss("/dir.d/file")), "");
    assert_eq!(FileInteractionHandler::get_extension(&ss("/a/b.")), "");

    let uses = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut fih = FileInteractionHandler::new(None);
    let s = fih.get_stream(Box::new(Request::default()), &ss("txt"), OutputStream::default());
    drop(s);
    fih.set_transformer(Some(Box::new(CountingTransformer(uses.clone()))));
    let s = fih.get_stream(Box::new(Request::default()), &ss("txt"), OutputStream::default());
    drop(s);
    assert_eq!(uses.get(), 1);
    let mut rep = Reply::new();
    let req = Request { _url: ss("/dir"), ..Request::default() };
    assert!(fih.redirect_if_needed(&req, &mut rep));
    let req = Request { _url: ss("/dir/"), ..Request::default() };
    let mut rep = Reply::new();
    assert!(!fih.redirect_if_needed(&req, &mut rep));
    let rep =
        fih.read(ss(&single), Box::new(Request::default()), Box::new(Reply::new())).get().unwrap();
    assert_eq!(rep.get_header("Content-Type"), "text/html");
    fih.set_transformer(None);
});
