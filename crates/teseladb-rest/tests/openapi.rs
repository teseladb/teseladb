//! The OpenAPI document against the route table (019 T014, FR-014): every route of the table is
//! an operation of the document and the document has no other, and the table holds the routes of
//! contract `rest-api.md` that the server serves.

use teseladb_http::http1::Request;
use teseladb_json::{Value, parse};
use teseladb_rest::openapi::{OPENAPI_VERSION, document, operation_id};
use teseladb_rest::routes::{BASE, ROUTES};

fn doc() -> Value {
    parse(document().as_bytes()).expect("the document is JSON")
}

#[test]
fn document_and_table_agree() {
    let d = doc();
    assert_eq!(d.get("openapi").and_then(Value::as_str), Some(OPENAPI_VERSION));
    let servers = d.get("servers").and_then(Value::as_array).unwrap();
    assert_eq!(servers[0].get("url").and_then(Value::as_str), Some(BASE));
    let paths = d.get("paths").and_then(Value::as_object).unwrap();
    let mut in_doc = Vec::new();
    for (path, ops) in paths.iter() {
        for (method, op) in ops.as_object().unwrap().iter() {
            in_doc.push((method.to_ascii_uppercase(), path.to_owned()));
            let route = ROUTES
                .iter()
                .find(|r| r.method.eq_ignore_ascii_case(method) && r.path == path)
                .unwrap_or_else(|| panic!("{method} {path} is in the document, not in the table"));
            assert_eq!(
                op.get("operationId").and_then(Value::as_str),
                Some(operation_id(route).as_str())
            );
            assert_eq!(op.get("requestBody").is_some(), route.body, "{method} {path}");
            let params = op.get("parameters").and_then(Value::as_array).map_or(0, <[Value]>::len);
            assert_eq!(params, path.matches('{').count(), "{method} {path}");
        }
    }
    let in_table: Vec<(String, String)> =
        ROUTES.iter().map(|r| (r.method.to_owned(), r.path.to_owned())).collect();
    let mut a = in_doc.clone();
    let mut b = in_table;
    a.sort();
    b.sort();
    assert_eq!(a, b);
    let mut ids: Vec<String> = ROUTES.iter().map(operation_id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), ROUTES.len(), "operationIds are unique");
}

#[test]
fn served_at_its_route() {
    struct Nothing;
    impl teseladb_rest::api::Executor for Nothing {
        fn run(&mut self, _: &str, _: teseladb_bson::Document) -> teseladb_bson::Document {
            unreachable!()
        }
        fn node(&self) -> u64 {
            1
        }
        fn cursor_ns(&mut self, _: i64) -> Option<String> {
            None
        }
        fn object_id(&mut self) -> [u8; 12] {
            [0; 12]
        }
        fn forward(&mut self, _: u64, _: &Request) -> Option<teseladb_http::server::Response> {
            None
        }
        fn random(&mut self, out: &mut [u8]) {
            out.fill(1);
        }
        fn now_ms(&self) -> u64 {
            0
        }
        fn principal(&self) -> String {
            String::new()
        }
        fn run_internal(
            &mut self,
            db: &str,
            command: teseladb_bson::Document,
        ) -> teseladb_bson::Document {
            self.run(db, command)
        }
    }
    let r = teseladb_rest::api::handle(&Request::new("GET", "/v1/openapi.json"), &mut Nothing);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, document().into_bytes());
}
