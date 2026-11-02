//! Change streams as Server-Sent Events (019 FR-012): the `aggregate` each route opens with its
//! options from the URL or the body, `Last-Event-ID` as `resumeAfter`, and the events of a
//! batch with their resume tokens.

use teseladb_bson::{Document, Value, extjson};
use teseladb_rest::changestream::{
    AWAIT_MS, ERROR_EVENT, Scope, cursor_collection, error_event, events, get_more, open,
};
use teseladb_rest::routes::Params;

fn d(t: &str) -> Document {
    extjson::parse(t).unwrap()
}

fn params() -> Params {
    Params { db: "app".into(), coll: "c".into(), ..Params::default() }
}

/// The `$changeStream` stage and the stages after it of `cmd`.
fn stages(cmd: &Document) -> (Document, Vec<Value>) {
    let Some(Value::Array(p)) = cmd.get("pipeline") else { panic!("{cmd:?}") };
    let first =
        p[0].as_document().and_then(|s| s.get("$changeStream")).and_then(Value::as_document);
    (first.cloned().unwrap(), p[1..].to_vec())
}

#[test]
fn each_scope_opens_its_aggregate_with_the_options_of_the_url_or_the_body() {
    let q = "fullDocument=updateLookup&showExpandedEvents=true&pipeline=%5B%7B%22%24match%22%3A%7B%7D%7D%5D";
    let (db, cmd) = open(Scope::Collection, &params(), q, &Document::new(), true, None).unwrap();
    assert_eq!((db.as_str(), cmd.get("aggregate")), ("app", Some(&Value::from("c"))));
    let (opts, rest) = stages(&cmd);
    assert_eq!(opts.get("fullDocument"), Some(&Value::from("updateLookup")));
    assert_eq!(opts.get("showExpandedEvents"), Some(&Value::Boolean(true)));
    assert_eq!(rest.len(), 1);
    let (db, cmd) = open(Scope::Database, &params(), "", &Document::new(), true, None).unwrap();
    assert_eq!((db.as_str(), cmd.get("aggregate")), ("app", Some(&Value::Int32(1))));
    let body = d(r#"{"fullDocument":"required"}"#);
    let (db, cmd) = open(Scope::Cluster, &params(), "x=1", &body, false, None).unwrap();
    let (opts, _) = stages(&cmd);
    assert_eq!(db, "admin");
    assert_eq!(opts.get("allChangesForCluster"), Some(&Value::Boolean(true)));
    assert_eq!(opts.get("fullDocument"), Some(&Value::from("required")));
    // `Last-Event-ID` resumes after its token, in place of where the request asked to start.
    let body =
        d(r#"{"startAfter":{"_data":"01"},"startAtOperationTime":{"$timestamp":{"t":1,"i":1}}}"#);
    let (_, cmd) =
        open(Scope::Collection, &params(), "", &body, false, Some(r#"{"_data":"02"}"#)).unwrap();
    let (opts, _) = stages(&cmd);
    assert_eq!(opts.get("resumeAfter"), Some(&Value::Document(d(r#"{"_data":"02"}"#))));
    assert!(opts.get("startAfter").is_none() && opts.get("startAtOperationTime").is_none());
}

#[test]
fn what_a_stream_refuses() {
    let refused = |q: &str, body: &Document, url: bool, last: Option<&str>| {
        open(Scope::Collection, &params(), q, body, url, last).unwrap_err().errmsg
    };
    let none = Document::new();
    assert!(refused("nope=1", &none, true, None).contains("unknown change stream option nope"));
    assert!(refused("%ZZ=1", &none, true, None).contains("percent-encoding"));
    assert!(refused("pipeline=3", &none, true, None).contains("pipeline must be an array"));
    assert!(refused("", &none, true, Some("{not json")).contains("Last-Event-ID"));
    assert!(open(Scope::Collection, &params(), "", &d(r#"{"lsid":{}}"#), false, None).is_err());
}

#[test]
fn a_batch_gives_one_event_per_change_with_its_resume_token() {
    let reply = d(
        r#"{"cursor":{"id":{"$numberLong":"7"},"ns":"app.c","firstBatch":[{"_id":{"_data":"0A"},"operationType":"insert"},3]},"ok":1.0}"#,
    );
    let (id, evs) = events(&reply).unwrap();
    assert_eq!((id, evs.len()), (7, 1));
    assert_eq!(evs[0].id.as_deref(), Some(r#"{"_data":"0A"}"#));
    assert!(evs[0].data.contains("insert"));
    assert_eq!(cursor_collection(&reply), "c");
    let next = d(r#"{"cursor":{"id":0,"ns":"app.$cmd.aggregate","nextBatch":[]},"ok":1.0}"#);
    assert_eq!(events(&next).unwrap(), (0, Vec::new()));
    assert_eq!(cursor_collection(&next), "$cmd.aggregate");
    assert_eq!(cursor_collection(&d(r#"{"cursor":{"ns":"nodot"}}"#)), "nodot");
    assert_eq!(cursor_collection(&Document::new()), "");
    let no_batch = d(r#"{"cursor":{},"ok":1.0}"#);
    assert_eq!(events(&no_batch).unwrap(), (0, Vec::new()));
    let failed = d(r#"{"ok":0.0,"code":280,"errmsg":"lost"}"#);
    assert_eq!(events(&failed).unwrap_err(), failed);
    let more = get_more(7, "c");
    assert_eq!(more.get("maxTimeMS"), Some(&Value::Int64(AWAIT_MS)));
    let e = error_event(&d(r#"{"status":500}"#));
    assert_eq!(e.event.as_deref(), Some(ERROR_EVENT));
    assert!(e.data.contains("500"));
}
