//! The `$changeStream` stage's options and the stages after it (014 FR-001), with the errors of a
//! `refrouter` 9.0.4 (measured on 01-11).

use teseladb_bson::{Document, Value};
use teseladb_changestream::stage::{
    BeforeChange, ChangeStreamSpec, FullDocument, Scope, Start, check_rest, codes,
};
use teseladb_changestream::token::ResumeToken;

const NS: &str = "lane1errs.c";

fn parse(d: Document) -> Result<ChangeStreamSpec, (i32, String)> {
    ChangeStreamSpec::parse(&Value::Document(d), Scope::Collection, NS)
        .map_err(|e| (e.code, e.message))
}

#[test]
fn the_options_are_read() {
    let token =
        ResumeToken::decode("01000000000000000100000001000000000000000000000000000000000001006B")
            .unwrap();
    let data = Document::new().with("_data", token.encode());
    let spec = parse(
        Document::new()
            .with("fullDocument", "updateLookup")
            .with("fullDocumentBeforeChange", "required")
            .with("resumeAfter", data.clone())
            .with("showExpandedEvents", true),
    )
    .unwrap();
    assert_eq!(spec.full_document, FullDocument::UpdateLookup);
    assert_eq!(spec.before, BeforeChange::Required);
    assert_eq!(spec.start, Start::ResumeAfter(token.clone()));
    assert!(spec.expanded && !spec.all_for_cluster);
    let after = parse(Document::new().with("startAfter", data)).unwrap();
    assert_eq!(after.start, Start::StartAfter(token));
    let at = parse(
        Document::new().with("startAtOperationTime", Value::Timestamp { time: 1, increment: 1 }),
    );
    assert!(matches!(at.unwrap().start, Start::At(_)));
    let all = ChangeStreamSpec::parse(
        &Value::Document(Document::new().with("allChangesForCluster", true)),
        Scope::Cluster,
        "admin.$cmd.aggregate",
    );
    assert!(all.unwrap().all_for_cluster);
}

#[test]
fn bad_options_fail_as_on_a_refrouter() {
    let cases = [
        (
            Document::new().with("bogus", 1),
            codes::UNKNOWN_FIELD,
            "BSON field '$changeStream.bogus' is an unknown field.",
        ),
        (
            Document::new().with("fullDocument", "x"),
            codes::BAD_VALUE,
            "Enumeration value 'x' for field '$changeStream.fullDocument' is not a valid value.",
        ),
        (
            Document::new().with("fullDocumentBeforeChange", "x"),
            codes::BAD_VALUE,
            "Enumeration value 'x' for field '$changeStream.fullDocumentBeforeChange' is not a valid value.",
        ),
        (
            Document::new().with("resumeAfter", Document::new().with("_data", "zz")),
            codes::FAILED_TO_PARSE,
            "resume token string was not a valid hex string",
        ),
        (
            Document::new()
                .with("resumeAfter", Document::new().with("_data", "82"))
                .with("startAfter", Document::new().with("_data", "82")),
            codes::BOTH_STARTS,
            "Do not specify both 'resumeAfter' and 'startAfter' in a $changeStream stage",
        ),
        (
            Document::new().with("allChangesForCluster", true),
            codes::INVALID_OPTIONS,
            "A $changeStream with 'allChangesForCluster:true' may only be opened on the 'admin' database, and with no collection name; found lane1errs.c",
        ),
        (
            Document::new().with("showExpandedEvents", "x"),
            codes::TYPE_MISMATCH,
            "Field 'showExpandedEvents' should be a boolean value, but found: string",
        ),
    ];
    let bad = Document::new().with("resumeAfter", Document::new().with("_data", "82"));
    assert_eq!(parse(bad).unwrap_err().0, codes::FAILED_TO_PARSE);
    for (d, code, msg) in cases {
        assert_eq!(parse(d.clone()), Err((code, msg.to_owned())), "{d:?}");
    }
    let not_object = ChangeStreamSpec::parse(&Value::Int32(1), Scope::Collection, NS).unwrap_err();
    assert_eq!(
        (not_object.code, not_object.message.as_str()),
        (
            codes::NOT_AN_OBJECT,
            "$changeStream must take a nested object but found: $changeStream: 1"
        )
    );
}

#[test]
fn only_some_stages_follow_and_the_split_goes_last() {
    assert_eq!(check_rest(&["$match", "$project", "$changeStreamSplitLargeEvent"]), Ok(()));
    let e = check_rest(&["$group"]).unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (codes::ILLEGAL_OPERATION, "$group is not permitted in a $changeStream pipeline")
    );
    let e = check_rest(&["$changeStreamSplitLargeEvent", "$match"]).unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            codes::SPLIT_NOT_LAST,
            "$changeStreamSplitLargeEvent must be the last stage in the pipeline"
        )
    );
    let e = check_rest(&["$changeStream"]).unwrap_err();
    assert_eq!(e.code, codes::NOT_FIRST);
}
