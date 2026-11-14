//! Errors (006 FR-021, contract `error-codes.md`): the reply document of an error, labels, the
//! translation of every error of the range (005) and the round trip of a mutation's error.

use teseladb_bson::{Document, Value};
use teseladb_hlc::Hlc;
use teseladb_range::host::{MutationError, RangeError};
use teseladb_range::intents::{IntentOwner, TxnId};
use teseladb_types::NodeId;
use teseladb_wire::WireError;
use teseladb_wire::error::{
    self, CommandError, RETRYABLE_WRITE_ERROR, RangeContext, TRANSIENT_TRANSACTION_ERROR,
    from_range,
};

#[test]
fn error_document_order_and_labels() {
    let mut e = CommandError::new(error::DUPLICATE_KEY, "E11000 duplicate key error")
        .with_label(TRANSIENT_TRANSACTION_ERROR)
        .with_label(TRANSIENT_TRANSACTION_ERROR);
    e.extra.push("keyPattern", Document::new().with("_id", 1));
    let d = e.to_document();
    let keys: Vec<&str> = d.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["ok", "errmsg", "code", "codeName", "keyPattern", "errorLabels"]);
    assert_eq!(d.get("codeName"), Some(&Value::from("DuplicateKey")));
    assert_eq!(
        d.get("errorLabels"),
        Some(&Value::Array(vec![Value::from(TRANSIENT_TRANSACTION_ERROR)]))
    );
    let plain = CommandError::new(error::BAD_VALUE, "x").to_document();
    assert_eq!(plain.get("errorLabels"), None);
}

#[test]
fn every_range_error_has_the_reference_code() {
    let plain = RangeContext::default();
    let retry = RangeContext { retryable_write: true, ..RangeContext::default() };
    let expired = RangeContext { max_time_expired: true, ..retry };
    // (error, context, code, labelled as retryable)
    let cases = [
        (RangeError::NotLeader { hint: NodeId(2) }, retry, error::NOT_WRITABLE_PRIMARY, true),
        (RangeError::NotLeader { hint: NodeId(0) }, plain, error::NOT_WRITABLE_PRIMARY, false),
        (RangeError::Timeout, retry, error::NETWORK_TIMEOUT, true),
        (RangeError::Timeout, expired, error::MAX_TIME_MS_EXPIRED, false),
        (RangeError::Overloaded, retry, error::EXCEEDED_TIME_LIMIT, true),
        (RangeError::OutOfRange, retry, error::INTERNAL_ERROR, false),
        (RangeError::Unsatisfiable, retry, error::UNSATISFIABLE_WRITE_CONCERN, false),
        (RangeError::TooLarge, retry, error::BSON_OBJECT_TOO_LARGE, false),
        (RangeError::Unavailable, retry, error::INTERRUPTED_AT_SHUTDOWN, true),
        (RangeError::Unavailable, plain, error::INTERRUPTED_AT_SHUTDOWN, false),
        (RangeError::SnapshotTooOld, retry, error::SNAPSHOT_TOO_OLD, false),
    ];
    for (e, ctx, code, labelled) in cases {
        let c = from_range(&e, ctx);
        assert_eq!(c.code, code, "{e:?}");
        assert_eq!(c.labels.contains(&RETRYABLE_WRITE_ERROR), labelled, "{e:?}");
    }
}

#[test]
fn transaction_conflicts_are_transient_transaction_errors() {
    let owner = IntentOwner {
        txn: TxnId(1),
        record_key: vec![2],
        key: vec![1],
        ts: Hlc { physical: 1, logical: 0 },
    };
    let cases = [
        (RangeError::WriteConflict, error::WRITE_CONFLICT),
        (RangeError::IntentConflict(owner), error::WRITE_CONFLICT),
        (RangeError::NoSuchTransaction, error::NO_SUCH_TRANSACTION),
    ];
    for (e, code) in cases {
        let retry = RangeContext { retryable_write: true, ..RangeContext::default() };
        let c = from_range(&e, retry);
        assert_eq!(c.code, code, "{e:?}");
        assert_eq!(c.labels, [TRANSIENT_TRANSACTION_ERROR], "{e:?}");
    }
    let c = from_range(&RangeError::WriteConflict, RangeContext::default());
    assert_eq!(c.errmsg, error::WRITE_CONFLICT_MESSAGE);
}

#[test]
fn mutation_errors_keep_their_code() {
    let original = CommandError::new(
        error::IMMUTABLE_FIELD,
        "Performing an update on the path '_id' would modify the immutable field '_id'",
    );
    let carried = original.to_mutation();
    let back = from_range(&RangeError::Mutation(carried.clone()), RangeContext::default());
    assert_eq!((back.code, back.errmsg.as_str()), (original.code, original.errmsg.as_str()));
    assert_eq!(CommandError::from_mutation(&carried), original);
    // A mutation error that does not come from this crate is an internal error with its text.
    for text in ["free text", "99999\u{0}unknown code", "x\u{0}not a number"] {
        let e = CommandError::from_mutation(&MutationError(text.into()));
        assert_eq!((e.code, e.errmsg.as_str()), (error::INTERNAL_ERROR, text));
    }
}

#[test]
fn wire_errors_name_their_fault() {
    let cases = [
        WireError::MessageLength(48_000_001),
        WireError::UnsupportedOpcode(2002),
        WireError::IllegalFlags(1 << 5),
        WireError::Checksum,
        WireError::UnknownCompressor(0),
        WireError::Compression(teseladb_compress::Error::Truncated),
        WireError::Bson(teseladb_bson::BsonError::TooLarge { size: 1, max: 0 }),
        WireError::Malformed("cut"),
    ];
    for e in cases {
        assert!(!e.to_string().is_empty(), "{e:?}");
    }
    assert_eq!(
        WireError::MessageLength(10).to_string(),
        "message length 10 is invalid. Min 16 Max: 48000000"
    );
    assert_eq!(
        WireError::UnauthenticatedLength(16_385).to_string(),
        "message length 16385 is too long for a connection that has not authenticated. Max: 16384"
    );
}

#[test]
fn every_mql_code_survives_the_trip_through_the_range() {
    use teseladb_mql::codes as m;
    let all = [
        m::INTERNAL_ERROR,
        m::BAD_VALUE,
        m::FAILED_TO_PARSE,
        m::TYPE_MISMATCH,
        m::INDEX_NOT_FOUND,
        m::PATH_NOT_VIABLE,
        m::CONFLICTING_UPDATE_OPERATORS,
        m::DOLLAR_PREFIXED_FIELD_NAME,
        m::NOT_SINGLE_VALUE_FIELD,
        m::EMPTY_FIELD_NAME,
        m::IMMUTABLE_FIELD,
        m::DOCUMENT_VALIDATION_FAILURE,
        m::NO_QUERY_EXECUTION_PLANS,
        m::BSON_OBJECT_TOO_LARGE,
        m::REGEX_INVALID,
        m::REGEX_INVALID_OPTION,
        m::PROJECTION_PATH_COLLISION,
        m::PROJECTION_INCLUSION_IN_EXCLUSION,
        m::PROJECTION_EXCLUSION_IN_INCLUSION,
    ];
    for c in all {
        let wire = error::code(c.code).expect("a code of the wire crate");
        assert_eq!((wire.code, wire.name), (c.code, c.name));
        let e = CommandError::new(wire, "m");
        assert_eq!(CommandError::from_mutation(&e.to_mutation()), e);
    }
}
