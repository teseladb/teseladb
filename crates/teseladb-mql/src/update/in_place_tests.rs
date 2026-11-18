//! An update applied on the stored bytes (`Update::apply_in_place`) gives the bytes the whole update
//! gives, and runs for the `$inc` and `$mul` that keep their field's type and width.

use teseladb_bson::{Document, RawDoc, Value};

use super::{Now, Update, UpdateContext};
use crate::ext::NoExtensions;
use crate::matcher::MatchContext;

fn context() -> UpdateContext<'static> {
    UpdateContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: None,
        now: Now { millis: 0, timestamp: (0, 0) },
        insert: false,
    }
}

fn parsed(u: Document) -> (Vec<u8>, Update) {
    let bytes = Document::new().with("u", u).to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let update = Update::parse(raw.get("u").expect("u"), None).expect("an update");
    (bytes, update)
}

fn op(name: &str, field: &str, v: Value) -> Document {
    Document::new().with(name, Document::new().with(field, v))
}

fn documents() -> Vec<Document> {
    let doc = |a: Value, b: Value, c: f64| {
        Document::new()
            .with("_id", 1)
            .with("a", a)
            .with("b", b)
            .with("c", c)
            .with("s", "text")
            .with("pad", "x".repeat(40))
    };
    vec![
        doc(Value::Int32(5), Value::Int64(7), 1.5),
        doc(Value::Int32(i32::MAX), Value::Int64(i64::MAX), -0.0),
        doc(Value::Int32(i32::MIN), Value::Int64(i64::MIN), f64::MAX),
        doc(Value::Int32(0), Value::Int64(0), f64::NAN),
    ]
}

fn updates() -> Vec<Document> {
    let mut out = Vec::new();
    for name in ["$inc", "$mul"] {
        for field in ["a", "b", "c", "s", "missing", "_id", "a.x", "pad"] {
            for v in [
                Value::Int32(1),
                Value::Int32(-3),
                Value::Int32(i32::MAX),
                Value::Int64(1),
                Value::Int64(i64::MAX),
                Value::Double(0.5),
                Value::Double(2.0),
            ] {
                out.push(op(name, field, v));
            }
        }
    }
    out.push(Document::new().with("$inc", Document::new().with("a", 1).with("c", 2.5)));
    out.push(
        Document::new()
            .with("$inc", Document::new().with("a", 1))
            .with("$mul", Document::new().with("b", 2i64)),
    );
    out.push(
        Document::new()
            .with("$inc", Document::new().with("a", 1))
            .with("$set", Document::new().with("s", "y")),
    );
    out
}

#[test]
fn an_update_on_the_bytes_gives_the_bytes_of_the_whole_update() {
    let ctx = context();
    let mut fast = 0;
    for d in documents() {
        let bytes = d.to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&bytes).expect("valid");
        for u in updates() {
            let (_, update) = parsed(u.clone());
            let Some(in_place) = update.apply_in_place(raw) else { continue };
            let whole = update.apply(raw, &ctx).expect("an update the bytes took applies");
            assert_eq!(in_place, whole.doc.to_bytes().expect("small"), "{u:?} on {d:?}");
            fast += 1;
        }
    }
    assert!(fast > 40, "only {fast} updates went on the bytes");
}

#[test]
fn the_updates_of_the_one_node_load_go_on_the_bytes() {
    let bytes = documents()[0].to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let both = Document::new().with("$inc", Document::new().with("a", 3).with("a2", 1));
    let (_, missing) = parsed(both);
    assert!(missing.apply_in_place(raw).is_none(), "a field the document lacks");
    // update_inc of the load is `{$inc: {v: d, ver: 1}}` on int32 fields.
    for u in [
        op("$inc", "a", Value::Int32(3)),
        op("$inc", "b", Value::Int32(1)),
        op("$inc", "c", Value::Double(0.25)),
        op("$mul", "a", Value::Int32(2)),
        Document::new().with("$inc", Document::new().with("a", 1).with("c", 2.5)),
    ] {
        let (_, update) = parsed(u.clone());
        assert!(update.apply_in_place(raw).is_some(), "{u:?}");
    }
    for u in [
        op("$inc", "a", Value::Int64(1)),
        op("$inc", "a", Value::Double(0.5)),
        op("$inc", "s", Value::Int32(1)),
        op("$inc", "_id", Value::Int32(1)),
        op("$inc", "a.x", Value::Int32(1)),
        Document::new()
            .with("$inc", Document::new().with("a", 1))
            .with("$set", Document::new().with("s", "y")),
    ] {
        let (_, update) = parsed(u.clone());
        assert!(update.apply_in_place(raw).is_none(), "{u:?} changes a type, a width or more");
    }
}
