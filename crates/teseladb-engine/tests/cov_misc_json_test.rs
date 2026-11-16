//! Tests of the JSON formatter and object model beyond the common paths: string escapes,
//! dates, slices and hash maps, non-finite numbers, mandatory members, objects with unset or
//! failing members, the default and empty object writers, copied body writers, a streamed
//! object written twice, and every write that fails on an unformattable value.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::collections::HashMap;

use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError};
use teseladb::engine::core::iostream::{DataSink, OutputStream, OutputStreamOptions};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::vector_data_sink::VectorDataSink;
use teseladb::engine::json::formatter::{
    DateTime, Formatter, JsonPair, JsonWrite, JsonWriteIn, State, ToJson,
};
use teseladb::engine::json::json_elements::{
    BodyWriter, JsonBase, JsonBaseElement, JsonBaseElementData, JsonElement, JsonReturnType,
    JsonVoid, Jsonable, json_base_add, stream_object,
};
use teseladb::engine::net::packet::Packet;
use teseladb::engine_thread_test_case;

/// Runs `f` on a stream that collects what is written into memory, closes the stream when
/// `close` is set, and returns `f`'s result with the text written.
fn with_stream<R>(f: impl FnOnce(&mut OutputStream<u8>) -> R, close: bool) -> (R, String) {
    let mut vec: Vec<Packet> = Vec::new();
    let r;
    {
        // SAFETY: `vec` outlives the stream (dropped at the end of this block, after its
        // close) and is not touched until then.
        let sink = unsafe { VectorDataSink::new(&mut vec) };
        let mut out = OutputStream::<u8>::with_options(
            DataSink::new(Box::new(sink)),
            8,
            OutputStreamOptions::default(),
        );
        r = f(&mut out);
        if close {
            out.close().get().expect("close");
        }
    }
    let mut packets = Packet::default();
    for p in vec.drain(..) {
        packets.append(p);
    }
    packets.linearize_all();
    let text = packets.release().first().map(|b| b.as_slice().to_vec()).unwrap_or_default();
    (r, String::from_utf8(text).unwrap())
}

/// An object with a mandatory integer `a` and an optional float `b`.
#[derive(Clone)]
struct Obj {
    a: JsonElement<i32>,
    b: JsonElement<f64>,
}

impl Obj {
    fn new() -> Self {
        let mut o = Obj { a: JsonElement::default(), b: JsonElement::default() };
        json_base_add(&mut o.a, "a", true);
        json_base_add(&mut o.b, "b", false);
        o
    }
}

impl JsonBase for Obj {
    fn element_count(&self) -> usize {
        2
    }
    fn element(&self, i: usize) -> Option<&dyn JsonBaseElement> {
        match i {
            0 => Some(&self.a),
            1 => Some(&self.b),
            _ => None,
        }
    }
}

/// A value with its own text that may fail to format.
struct Custom(bool);

impl Jsonable for Custom {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        if self.0 {
            return Err(ExceptionPtr::new(RuntimeError::new("custom failure")));
        }
        Ok(Sstring::from("\"custom\""))
    }
}

#[test]
fn strings_escape_quotes_backslashes_and_controls() {
    let s = Sstring::from("q\"b\\s\u{8}f\u{c}n\nr\rt\t\u{1}");
    assert_eq!(ToJson::to_json(&s).unwrap(), r#""q\"b\\s\bf\fn\nr\rt\t\u0001""#);
    assert_eq!(ToJson::to_json(&Sstring::from("plain")).unwrap(), "\"plain\"");
}

#[test]
fn dates_ranges_and_maps_format() {
    let d = DateTime {
        tm_year: 123,
        tm_mon: 0,
        tm_mday: 2,
        tm_hour: 3,
        tm_min: 4,
        tm_sec: 5,
        ..DateTime::default()
    };
    assert_eq!(d.to_json().unwrap(), "\"2023-01-02T03:04:05Z\"");
    assert_eq!(ToJson::to_json(&[1i32, 2][..]).unwrap(), "[1,2]");
    let mut m: HashMap<Sstring, i32> = HashMap::new();
    m.insert(Sstring::from("k"), 1);
    assert_eq!(m.to_json().unwrap(), "{\"k\":1}");
    assert_eq!(Formatter::begin(State::None), "");
    assert_eq!(Formatter::end(State::None), "");
    assert!(f64::NAN.to_json().is_err());
    assert!(f32::INFINITY.to_json().is_err());
}

#[test]
fn objects_check_mandatory_members_and_skip_unset_ones() {
    let mut data = JsonBaseElementData::default();
    assert!(!data._set && !data._mandatory && data._name.is_empty());
    let set = JsonBaseElementData { _name: "x".to_owned(), _mandatory: true, _set: true };
    data.assign_from(&set);
    assert!(data._set);
    assert!(data._name.is_empty(), "the name is not copied");

    let mut o = Obj::new();
    assert!(!o.a.is_verify());
    assert!(o.b.is_verify());
    assert!(!o.is_verify());
    o.a.set(1);
    assert!(o.is_verify());
    assert_eq!(o.base_to_json().unwrap(), "{\"a\": 1}");

    o.b.set(f64::NAN);
    let e = o.base_to_json().unwrap_err();
    let r = e.downcast_ref::<RuntimeError>().expect("a runtime error");
    assert_eq!(r.0, "Json generation failed for field: b");
    assert!(e.nested_ptr().is_some());

    assert_eq!(Jsonable::to_json(&JsonVoid).unwrap(), "");
    assert_eq!(Jsonable::to_json(&Custom(false)).unwrap(), "\"custom\"");

    let bw: BodyWriter = Box::new(|s: OutputStream<u8>| -> Future<()> {
        let mut s = s;
        s.close()
    });
    let copy = bw.clone();
    let r = JsonReturnType::from_body_writer(copy);
    assert!(r._body_writer.is_some());
    assert!(r._res.empty());
    drop(bw);
}

engine_thread_test_case!(unformattable_values_fail_their_writes, {
    let (r, _) = with_stream(|out| f64::NAN.write_json(out).get(), true);
    assert!(r.is_err());

    let (r, _) = with_stream(|out| JsonPair(f64::NAN, 1i32).write_in(out, State::Map).get(), true);
    assert!(r.is_err());
    let (r, text) = with_stream(|out| JsonPair(2.5f64, 1i32).write_in(out, State::Map).get(), true);
    assert!(r.is_ok());
    assert_eq!(text, "2.5:1");

    let mut e = JsonElement::<f64>::default();
    e.set(f64::INFINITY);
    // SAFETY: `e` stays alive and unchanged until the write resolves (waited here).
    let (r, _) = with_stream(|out| unsafe { e.write(out) }.get(), true);
    assert!(r.is_err());

    let (r, text) = with_stream(|out| Box::new(Custom(false)).write(out).get(), true);
    assert!(r.is_ok());
    assert_eq!(text, "\"custom\"");
    let (r, _) = with_stream(|out| Box::new(Custom(true)).write(out).get(), true);
    assert!(r.is_err());
    let (r, text) = with_stream(
        |out| {
            let b: Box<dyn Jsonable> = Box::new(Custom(false));
            b.write_json(out).get()
        },
        true,
    );
    assert!(r.is_ok());
    assert_eq!(text, "\"custom\"");
});

engine_thread_test_case!(objects_and_empty_values_stream_to_the_output, {
    // The empty object closes the stream itself.
    let (r, text) = with_stream(|out| Box::new(JsonVoid).write(out).get(), false);
    assert!(r.is_ok());
    assert_eq!(text, "");

    let mut o = Obj::new();
    o.a.set(7);
    let (r, text) = with_stream(|out| o.clone().base_write(out).get(), true);
    assert!(r.is_ok());
    assert_eq!(text, "{\"a\":7}");

    // A streamed object is written by the first call only; a second call only closes.
    let mut bw = stream_object(5i32);
    let (r, text) = with_stream(|out| bw(std::mem::take(out)).get(), false);
    assert!(r.is_ok());
    assert_eq!(text, "5");
    let (r, text) = with_stream(|out| bw(std::mem::take(out)).get(), false);
    assert!(r.is_ok());
    assert_eq!(text, "");
});
